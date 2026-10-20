//! 分享的创建编排：准入复检 → 端口分配 → 读取主配置 → 克隆 → 落盘 → 启服务 → 自测。
//!
//! **服务器是唯一事实源**：本地不建分享表；本模块每次操作都 SSH 到目标机。
//! **分享是快照**：克隆发生在创建时刻，主配置后续变化不会回写已有分享。
//!
//! 失败必须回滚（A9）：步骤 7-8 的任一 `?` 都不能把半成品留在服务器上，
//! 因此写入、启动、验证整体包在同一个 `setup` future 里，失败统一走
//! [`rollback_created`]——停单元、删单元、删目录、撤端口放行。

use crate::domain::{effective_host, Node, Server};
use crate::servers::deploy::{
    run_cmd, run_cmd_ok, write_remote_file, CONFIG_TIMEOUT, SINGBOX_CONFIG_PATH, SS_CONFIG_PATH,
    XRAY_CONFIG_PATH,
};
use crate::servers::remote_file::{read_remote_file, RemoteFile};
use crate::servers::{CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::share::clone::{clone_configs, ShareNodeSpec};
use crate::share::model::*;
use crate::share::naming::*;
use crate::share::plan;
use crate::share::plan::{tcp_port_probe_cmd, udp_port_probe_cmd};
use crate::share::{scripts, tls};
use crate::ssh::SshSession;
use crate::state::AppState;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CreateShareInput {
    pub server_id: i64,
    pub node_ids: Vec<String>,
    pub options: ShareOptions,
}

/// 生产入口：先确认 TLS 就绪，再进入创建编排。
///
/// **绝不静默退回 HTTP**：证书没准备好就直接拒绝，不连接服务器做任何写入。
pub async fn create_share(
    state: &Arc<AppState>,
    input: CreateShareInput,
) -> Result<ShareInfo, String> {
    validate_options(&input.options)?;
    if input.node_ids.is_empty() {
        return Err("至少选择一个节点".into());
    }
    let tls = tls::tls_status(state, input.server_id).await?;
    if !tls.ready {
        return Err(format!("分享 HTTPS 未就绪: {}", tls.detail));
    }
    create_share_inner(state, input).await
}

/// 创建编排本体。测试直接调用以绕过 TLS 的外部依赖；生产路径必须经
/// [`create_share`]（它会先查证书）。
pub(crate) async fn create_share_inner(
    state: &Arc<AppState>,
    input: CreateShareInput,
) -> Result<ShareInfo, String> {
    validate_options(&input.options)?;
    if input.node_ids.is_empty() {
        return Err("至少选择一个节点".into());
    }

    // 1. 后端重新做准入：拒绝不可分享节点必须发生在连服务器之前。
    let candidates = plan::share_candidates(state, input.server_id)?;
    let mut seen = HashSet::new();
    let chosen: Vec<Node> = input
        .node_ids
        .iter()
        .map(|id| {
            if !seen.insert(id.clone()) {
                return Err(format!("节点重复选择: {id}"));
            }
            candidates
                .iter()
                .find(|c| &c.node.id == id)
                .ok_or_else(|| format!("节点不存在: {id}"))
                .and_then(|c| match &c.reason {
                    Some(r) => Err(format!("节点 {} 不可分享: {r}", c.node.name)),
                    None => Ok(c.node.clone()),
                })
        })
        .collect::<Result<_, _>>()?;
    if chosen.iter().filter(|n| n.protocol == "ss").count() > 1 {
        return Err("Shadowsocks 分享一次只允许一个节点".into());
    }

    // 2. 读服务器/凭据并建立 SSH 会话。
    let (auth, server) =
        crate::servers::load_auth(state, input.server_id).map_err(|e| e.to_string())?;
    let domain = crate::servers::normalize_share_domain(&server.share_domain)?;
    if domain.is_empty() {
        return Err("该服务器未配置分享域名（服务器 → 编辑 → 分享域名）".into());
    }
    let mut sess = SshSession::connect(
        effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;
    let cancel = AtomicBool::new(false);
    tls::probe_python3(&mut sess, &cancel).await?;

    // 3. 端口分配与克隆规格（凭据在这里一次性生成）。
    let transports: Vec<&str> = chosen
        .iter()
        .map(|n| match n.protocol.as_str() {
            "hysteria2" | "tuic" => "udp",
            "ss" => "tcp_and_udp",
            _ => "tcp",
        })
        .collect();
    let allocated = allocate_share_ports(&mut sess, &cancel, &transports).await?;
    let specs = build_specs(&chosen, &allocated.pairs)?;

    // 4. 读主配置。需要哪个协议才读哪个；文件缺失即报错，不静默跳过。
    let id = new_id();
    let token = new_token();
    let main_xray = read_main(&mut sess, &cancel, &specs, "vless", XRAY_CONFIG_PATH).await?;
    let main_sb = read_main(
        &mut sess,
        &cancel,
        &specs,
        "hysteria2",
        SINGBOX_CONFIG_PATH,
    )
    .await?;
    let main_ss = read_main(&mut sess, &cancel, &specs, "ss", SS_CONFIG_PATH).await?;

    // 5. 克隆：只改端口、监听地址与凭据，其余流设置/出站原样保留。
    let cloned = clone_configs(
        main_xray.as_deref(),
        main_sb.as_deref(),
        main_ss.as_deref(),
        &specs,
        effective_host(&server),
        &id,
        &[],
    )?;

    // 6. 清单/网关配置。到期时间用**服务器时钟**——客户端时钟不可信。
    let dir = share_dir(&id);
    let bind = if server.addr_family == "v6" {
        "::"
    } else {
        "0.0.0.0"
    };
    let expires_at_unix =
        server_now_unix(&mut sess, &cancel).await? + input.options.expires_in_days as i64 * 86400;
    let share_nodes: Vec<ShareNode> = specs
        .iter()
        .zip(cloned.share_nodes.iter())
        .map(|(s, n)| ShareNode {
            name: s.name.clone(),
            protocol: s.protocol.clone(),
            public_port: s.public_port,
            internal_port: s.internal_port,
            transport: s.transport.into(),
            params_json: n.params_json.clone(),
        })
        .collect();
    let manifest = ShareManifest {
        version: 1,
        id: id.clone(),
        token: token.clone(),
        server_id: server.id,
        created_at: chrono::Utc::now().to_rfc3339(),
        quota_bytes: input.options.quota_bytes,
        rate_up_mbps: input.options.rate_up_mbps,
        rate_down_mbps: input.options.rate_down_mbps,
        expires_at_unix,
        http_port: allocated.http,
        domain: domain.clone(),
        nodes: share_nodes,
    };
    let gateway = GatewayConfig {
        quota_bytes: manifest.quota_bytes,
        rate_up_mbps: manifest.rate_up_mbps,
        rate_down_mbps: manifest.rate_down_mbps,
        expires_at_unix,
        ports: specs
            .iter()
            .map(|s| GatewayPort {
                public: s.public_port,
                target: s.internal_port,
                proto: s.transport.into(),
            })
            .collect(),
    };

    // tcp_and_udp 拆成两条防火墙规则：脚本里的 -p 只认 tcp/udp 单值。
    let public_pairs: Vec<(u16, &str)> = specs
        .iter()
        .flat_map(|s| match s.transport {
            "tcp_and_udp" => vec![(s.public_port, "tcp"), (s.public_port, "udp")],
            other => vec![(s.public_port, other)],
        })
        .collect();

    // 单元名先全部确定：写文件中途失败时，回滚仍知道要删哪些单元。
    let mut units = vec![unit_name(&id), gateway_unit_name(&id)];
    if cloned.xray_config.is_some() {
        units.push(instance_unit_name(&id, "xray"));
    }
    if cloned.singbox_config.is_some() {
        units.push(instance_unit_name(&id, "singbox"));
    }
    if cloned.ss_config.is_some() {
        units.push(instance_unit_name(&id, "ss"));
    }

    // 7-8. 写入、启动、验证。任一失败统一回滚（A9）。
    let setup: Result<(), String> = async {
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &share_server_script_path(),
            &scripts::share_server_script(),
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &share_gateway_script_path(),
            &scripts::share_gateway_script(),
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &firewall_script_path(),
            &scripts::firewall_script(),
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            &mut sess,
            &cancel,
            "准备",
            &format!(
                "chmod 755 {} {} {}",
                share_server_script_path(),
                share_gateway_script_path(),
                firewall_script_path()
            ),
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            &mut sess,
            &cancel,
            "准备",
            "id -u vpnshare >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin vpnshare 2>/dev/null || adduser -S -D -H -s /sbin/nologin vpnshare",
            CONFIG_TIMEOUT,
        )
        .await?;
        // 先建 0700 目录再写秘密：写文件时父目录已经只有 vpnshare 可进。
        run_cmd_ok(
            &mut sess,
            &cancel,
            "准备",
            &format!(
                "mkdir -p {dir} {dir}/sub && chown vpnshare:vpnshare {dir} {dir}/sub && chmod 700 {dir} {dir}/sub"
            ),
            CONFIG_TIMEOUT,
        )
        .await?;

        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &manifest_path(&id),
            &serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &gateway_config_path(&id),
            &serde_json::to_string_pretty(&gateway).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &usage_path(&id),
            &serde_json::to_string(&UsageState::default()).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &yaml_path(&id, &token),
            &cloned.yaml,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &singbox_path(&id, &token),
            &cloned.singbox_json,
            CONFIG_TIMEOUT,
        )
        .await?;

        if let Some(cfg) = &cloned.xray_config {
            let unit = instance_unit_name(&id, "xray");
            let path = instance_config_path(&id, "xray");
            write_remote_file(&mut sess, &cancel, "写入", &path, cfg, CONFIG_TIMEOUT).await?;
            write_remote_file(
                &mut sess,
                &cancel,
                "写入",
                &unit_path(&unit),
                &instance_unit(&id, &unit, "xray", &dir, &path),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        if let Some(cfg) = &cloned.singbox_config {
            // 分享实例复用主服务的自签证书副本；证书只落在分享目录内。
            run_cmd_ok(
                &mut sess,
                &cancel,
                "准备",
                &format!(
                    "install -m 600 /etc/sing-box/self.crt {dir}/singbox.crt && \
                     install -m 600 /etc/sing-box/self.key {dir}/singbox.key"
                ),
                CONFIG_TIMEOUT,
            )
            .await?;
            let unit = instance_unit_name(&id, "singbox");
            let path = instance_config_path(&id, "singbox");
            write_remote_file(&mut sess, &cancel, "写入", &path, cfg, CONFIG_TIMEOUT).await?;
            write_remote_file(
                &mut sess,
                &cancel,
                "写入",
                &unit_path(&unit),
                &instance_unit(&id, "singbox", "singbox", &dir, &path),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        if let Some(cfg) = &cloned.ss_config {
            let unit = instance_unit_name(&id, "ss");
            let path = instance_config_path(&id, "ss");
            write_remote_file(&mut sess, &cancel, "写入", &path, cfg, CONFIG_TIMEOUT).await?;
            write_remote_file(
                &mut sess,
                &cancel,
                "写入",
                &unit_path(&unit),
                &instance_unit(&id, &unit, "ss", &dir, &path),
                CONFIG_TIMEOUT,
            )
            .await?;
        }

        let share_units = unit_contents(&id, allocated.http, bind, &dir, &public_pairs);
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &unit_path(&unit_name(&id)),
            &share_units.http,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &unit_path(&gateway_unit_name(&id)),
            &share_units.gateway,
            CONFIG_TIMEOUT,
        )
        .await?;

        run_cmd_ok(
            &mut sess,
            &cancel,
            "准备",
            &format!(
                "chown -R vpnshare:vpnshare {dir} && chmod 600 {dir}/*.json {dir}/sub/*"
            ),
            CONFIG_TIMEOUT,
        )
        .await?;

        run_cmd_ok(
            &mut sess,
            &cancel,
            "启动",
            "systemctl daemon-reload",
            CONFIG_TIMEOUT,
        )
        .await?;
        for unit in &units {
            run_cmd_ok(
                &mut sess,
                &cancel,
                "启动",
                &format!("systemctl enable --now {unit}"),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        for unit in &units {
            let out = run_cmd(
                &mut sess,
                &cancel,
                "确认",
                &format!("systemctl is-active {unit}"),
                PROBE_TIMEOUT,
            )
            .await?;
            if out.stdout.trim() != "active" {
                return Err(format!(
                    "单元 {unit} 未进入 active 状态（systemctl：{}）",
                    out.stdout.trim()
                ));
            }
        }

        let code = run_cmd(
            &mut sess,
            &cancel,
            "自测",
            &format!(
                "curl -s -o /dev/null -w '%{{http_code}}' --resolve {domain}:{port}:127.0.0.1 \
                 https://{domain}:{port}/sub/{token} --max-time 10",
                port = allocated.http
            ),
            PROBE_TIMEOUT,
        )
        .await?
        .stdout;
        if code.trim() != "200" {
            return Err(format!(
                "分享链接自测返回 {}（期望 200）",
                code.trim()
            ));
        }
        Ok(())
    }
    .await;

    if let Err(e) = setup {
        let left = rollback_created(&mut sess, &cancel, &id, &units, &public_pairs).await;
        return Err(if left.is_empty() {
            format!("{e}；已回滚本次创建的单元、目录与端口")
        } else {
            format!("{e}；回滚后仍有残留：{left:?}")
        });
    }

    Ok(info_from_manifest(
        &manifest,
        &UsageState::default(),
        &server,
        true,
    ))
}

/// 展示用状态：配额按 `used >= quota` 判定（不依赖时钟）；到期用本机时钟近似，
/// 真正的断开由网关按服务器时钟执行。
pub(crate) fn status_from(m: &ShareManifest, u: &UsageState, active: bool) -> ShareStatus {
    let quota_hit = m.quota_bytes > 0 && u.used_bytes >= m.quota_bytes;
    let expired =
        m.expires_at_unix > 0 && chrono::Utc::now().timestamp() >= m.expires_at_unix;
    if u.blocked.as_deref() == Some("quota") || quota_hit {
        ShareStatus::BlockedQuota
    } else if u.blocked.as_deref() == Some("expired") || expired {
        ShareStatus::BlockedExpired
    } else if !active {
        ShareStatus::Degraded
    } else {
        ShareStatus::Active
    }
}

pub(crate) fn info_from_manifest(
    m: &ShareManifest,
    u: &UsageState,
    server: &Server,
    active: bool,
) -> ShareInfo {
    let status = status_from(m, u, active);
    ShareInfo {
        id: m.id.clone(),
        server_id: m.server_id,
        server_name: server.name.clone(),
        host: effective_host(server).to_string(),
        token: m.token.clone(),
        port: m.http_port,
        active,
        url: url_yaml(&m.domain, m.http_port, &m.token),
        url_singbox: url_singbox(&m.domain, m.http_port, &m.token),
        node_names: m.nodes.iter().map(|n| n.name.clone()).collect(),
        legacy: false,
        status,
        status_detail: u.blocked.clone(),
        quota_bytes: m.quota_bytes,
        used_bytes: u.used_bytes,
        rate_up_mbps: m.rate_up_mbps,
        rate_down_mbps: m.rate_down_mbps,
        expires_at: chrono::DateTime::from_timestamp(m.expires_at_unix, 0)
            .map(|t| t.to_rfc3339()),
        last_access_at: None,
        last_access_ip: None,
    }
}

/// 只有当选中节点里出现该协议族时才读主配置；文件不存在即报错，不静默跳过。
pub(crate) async fn read_main(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    specs: &[ShareNodeSpec],
    family: &str,
    path: &str,
) -> Result<Option<String>, String> {
    let needed = specs.iter().any(|s| match family {
        "vless" => matches!(s.protocol.as_str(), "vless" | "trojan" | "vmess"),
        "hysteria2" => matches!(s.protocol.as_str(), "hysteria2" | "tuic"),
        "ss" => s.protocol == "ss",
        _ => false,
    });
    if !needed {
        return Ok(None);
    }
    match read_remote_file(sess, cancel, "读取", path, PROBE_TIMEOUT).await? {
        RemoteFile::Absent => Err(format!("主服务配置不存在：{path}（请先部署对应协议）")),
        RemoteFile::Content(c) => Ok(Some(c)),
    }
}

async fn server_now_unix(sess: &mut SshSession, cancel: &AtomicBool) -> Result<i64, String> {
    let out = run_cmd(sess, cancel, "授时", "date +%s", PROBE_TIMEOUT).await?;
    out.stdout
        .trim()
        .parse()
        .map_err(|_| "服务器时间解析失败".to_string())
}

pub struct AllocatedPorts {
    pub http: u16,
    /// (public, internal)，与传入 transports 同序。
    pub pairs: Vec<(u16, u16)>,
}

async fn find_free_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    cursor: &mut u16,
    end: u16,
    transport: &str,
) -> Result<u16, String> {
    while *cursor <= end {
        let port = *cursor;
        *cursor += 1;
        let mut free = true;
        if transport != "udp" {
            let out = run_cmd(
                sess,
                cancel,
                "探测",
                &tcp_port_probe_cmd(port),
                PROBE_TIMEOUT,
            )
            .await?;
            free &= out.exit_code != 0; // 非 0 = 空闲（与 port_probe_cmd 同方向）
        }
        if transport != "tcp" {
            let out = run_cmd(
                sess,
                cancel,
                "探测",
                &udp_port_probe_cmd(port),
                PROBE_TIMEOUT,
            )
            .await?;
            free &= out.exit_code != 0;
        }
        if free {
            return Ok(port);
        }
    }
    Err(format!("端口段到 {end} 已耗尽"))
}

/// 订阅端口、公网数据端口、回环内部端口三段各自独立分配；每段都从段首开始，
/// 实际占用情况由远端 `ss` 探测决定。
pub async fn allocate_share_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    transports: &[&str],
) -> Result<AllocatedPorts, String> {
    let mut http_cursor = PORT_RANGE_START;
    let http = find_free_port(sess, cancel, &mut http_cursor, PORT_RANGE_END, "tcp").await?;
    let mut public_cursor = DATA_PORT_START;
    let mut internal_cursor = INTERNAL_PORT_START;
    let mut pairs = Vec::new();
    for transport in transports {
        let public = find_free_port(
            sess,
            cancel,
            &mut public_cursor,
            DATA_PORT_END,
            transport,
        )
        .await?;
        let internal = find_free_port(
            sess,
            cancel,
            &mut internal_cursor,
            INTERNAL_PORT_END,
            transport,
        )
        .await?;
        pairs.push((public, internal));
    }
    Ok(AllocatedPorts { http, pairs })
}

/// 由节点与端口对生成克隆规格；凭据在这里一次性生成。
pub fn build_specs(nodes: &[Node], pairs: &[(u16, u16)]) -> Result<Vec<ShareNodeSpec>, String> {
    if nodes.len() != pairs.len() {
        return Err("端口数量与节点数量不一致".into());
    }
    nodes
        .iter()
        .zip(pairs)
        .map(|(n, (public, internal))| {
            let transport = match n.protocol.as_str() {
                "hysteria2" | "tuic" => "udp",
                "ss" => "tcp_and_udp",
                "vless" | "trojan" | "vmess" => "tcp",
                other => return Err(format!("不支持分享的协议: {other}")),
            };
            let original_port = u16::try_from(n.port)
                .map_err(|_| format!("节点 {} 端口非法: {}", n.name, n.port))?;
            let original_params = serde_json::from_str(&n.params_json)
                .map_err(|e| format!("节点 {} 参数解析失败: {e}", n.name))?;
            let (new_credential, new_password) = if n.protocol == "tuic" {
                (
                    uuid::Uuid::new_v4().to_string(),
                    Some(uuid::Uuid::new_v4().to_string()),
                )
            } else if n.protocol == "ss" {
                let key: [u8; 32] = rand::random();
                (
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key),
                    None,
                )
            } else {
                (uuid::Uuid::new_v4().to_string(), None)
            };
            Ok(ShareNodeSpec {
                name: n.name.clone(),
                protocol: n.protocol.clone(),
                original_port,
                public_port: *public,
                internal_port: *internal,
                transport,
                original_params,
                new_credential,
                new_password,
            })
        })
        .collect()
}

// ---------- 单元与防火墙 ----------

fn firewall_specs(pairs: &[(u16, &str)]) -> String {
    pairs
        .iter()
        .map(|(p, proto)| format!("{p}:{proto}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn hardening(dir: &str) -> String {
    format!(
        "User=vpnshare\nGroup=vpnshare\nNoNewPrivileges=yes\nProtectSystem=strict\n\
         ProtectHome=yes\nPrivateTmp=yes\nReadWritePaths={dir}\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK\n"
    )
}

pub struct ShareUnits {
    pub http: String,
    pub gateway: String,
}

pub fn unit_contents(
    id: &str,
    port: u16,
    bind: &str,
    dir: &str,
    pairs: &[(u16, &str)],
) -> ShareUnits {
    let http_script = share_server_script_path();
    let gateway_script = share_gateway_script_path();
    let firewall = firewall_script_path();
    let tls = tls_dir();
    let h = hardening(dir);
    let http = format!(
        "[Unit]\nDescription=vpn-ctl share {id} subscription\nAfter=network.target\n\n[Service]\n{h}\
         ExecStart=/usr/bin/env python3 {http_script} {port} {dir} --bind {bind} \
--cert {tls}/fullchain.pem --key {tls}/privkey.pem\n\
         ExecStartPost=+{firewall} open vpnshare-{id} {port}:tcp\n\
         ExecStopPost=+{firewall} close vpnshare-{id} {port}:tcp\n\
         Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n"
    );
    let specs = firewall_specs(pairs);
    let gateway = format!(
        "[Unit]\nDescription=vpn-ctl share {id} gateway\nAfter=network.target\n\n[Service]\n{h}\
         ExecStart=/usr/bin/env python3 {gateway_script} --config {dir}/gateway.json \
--usage {dir}/usage.json --bind {bind}\n\
         ExecStartPost=+{firewall} open vpnshare-{id} {specs}\n\
         ExecStopPost=+{firewall} close vpnshare-{id} {specs}\n\
         Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n"
    );
    ShareUnits { http, gateway }
}

fn instance_unit(id: &str, unit: &str, kind: &str, dir: &str, config: &str) -> String {
    let exec = match kind {
        "xray" => format!("/usr/local/bin/xray run -config {config}"),
        "singbox" => format!("/usr/local/bin/sing-box run -c {config}"),
        _ => format!("/usr/local/bin/ssserver -c {config}"),
    };
    let h = hardening(dir);
    format!(
        "[Unit]\nDescription=vpn-ctl share {id} {kind} ({unit})\nAfter=network.target\n\n[Service]\n{h}\
         ExecStart={exec}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

/// 回滚一次失败的创建。所有步骤幂等：单元/目录/规则不存在也不算错。
/// 返回无法确认清理干净的项目，调用方把它带进错误信息（A9 的透明度）。
pub(crate) async fn rollback_created(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    id: &str,
    units: &[String],
    ports: &[(u16, &str)],
) -> Vec<String> {
    let mut leftovers = Vec::new();
    for unit in units {
        if run_cmd_ok(
            sess,
            cancel,
            "回滚",
            &format!("systemctl disable --now {unit} 2>/dev/null || true"),
            CONFIG_TIMEOUT,
        )
        .await
        .is_err()
        {
            leftovers.push(format!("unit {unit}"));
        }
        if run_cmd_ok(
            sess,
            cancel,
            "回滚",
            &format!("rm -f {}", unit_path(unit)),
            CONFIG_TIMEOUT,
        )
        .await
        .is_err()
        {
            leftovers.push(format!("unit file {}", unit_path(unit)));
        }
    }
    if run_cmd_ok(
        sess,
        cancel,
        "回滚",
        &format!("rm -rf {}", share_dir(id)),
        CONFIG_TIMEOUT,
    )
    .await
    .is_err()
    {
        leftovers.push(format!("dir {}", share_dir(id)));
    }
    for (port, proto) in ports {
        if run_cmd_ok(
            sess,
            cancel,
            "回滚",
            &format!(
                "iptables -D INPUT -p {proto} --dport {port} -m comment \
                 --comment vpnshare-{id}-{port}-{proto} -j ACCEPT 2>/dev/null || true"
            ),
            CONFIG_TIMEOUT,
        )
        .await
        .is_err()
        {
            leftovers.push(format!("rule {port}/{proto}"));
        }
    }
    let _ = run_cmd_ok(
        sess,
        cancel,
        "回滚",
        "systemctl daemon-reload",
        CONFIG_TIMEOUT,
    )
    .await;
    leftovers
}

// ---------- 列举 ----------

/// `share_summary.py` 的一条记录。新分享带 manifest/usage/last_access/units；
/// 旧分享只有 unit/snapshot/active。
#[derive(Debug, Deserialize)]
pub struct SummaryEntry {
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub manifest: Option<ShareManifest>,
    #[serde(default)]
    pub usage: Option<UsageState>,
    #[serde(default)]
    pub last_access: Option<LastAccess>,
    #[serde(default)]
    pub units: Option<HashMap<String, Option<String>>>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LastAccess {
    pub at: String,
    pub ip: String,
    #[serde(default)]
    pub format: Option<String>,
}

pub fn parse_summary(json: &str) -> Result<Vec<SummaryEntry>, String> {
    serde_json::from_str(json).map_err(|e| format!("分享摘要解析失败: {e}"))
}

/// 逐服务器并发列举。某台连不上不整体失败：好那台照常列出，坏那台进
/// `unreachable` 并带原因——否则用户会以为分享凭空消失了。
pub async fn list_shares(state: &Arc<AppState>) -> Result<ShareList, String> {
    let servers =
        crate::db::repos::servers::list(&state.db.lock()).map_err(|e| e.to_string())?;

    let mut set = tokio::task::JoinSet::new();
    for (idx, server) in servers.into_iter().enumerate() {
        let state = state.clone();
        set.spawn(async move {
            let cancel = AtomicBool::new(false);
            let name = server.name.clone();
            (
                idx,
                name,
                read_server_summaries(&state, &server, &cancel).await,
            )
        });
    }

    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(t) => results.push(t),
            Err(e) => return Err(format!("读取分享的任务异常结束: {e}")),
        }
    }
    results.sort_by_key(|(idx, _, _)| *idx);

    let mut shares = Vec::new();
    let mut unreachable = Vec::new();
    for (_, name, r) in results {
        match r {
            Ok(mut list) => shares.append(&mut list),
            Err(reason) => unreachable.push(format!("{name}：{reason}")),
        }
    }
    Ok(ShareList {
        shares,
        unreachable,
    })
}

async fn read_server_summaries(
    state: &Arc<AppState>,
    server: &Server,
    cancel: &AtomicBool,
) -> Result<Vec<ShareInfo>, String> {
    let (auth, _) = crate::servers::load_auth(state, server.id).map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        effective_host(server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;

    // 摘要脚本只读；每次列举幂等重写，服务器重装/首次列举都能自愈。
    let summary_script = share_summary_script_path();
    write_remote_file(
        &mut sess,
        cancel,
        "写入",
        &summary_script,
        &scripts::share_summary_script(),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        cancel,
        "准备",
        &format!("chmod 755 {summary_script}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    let out = run_cmd_ok(
        &mut sess,
        cancel,
        "列举",
        &format!("python3 {summary_script} {SHARE_ROOT}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    let entries = parse_summary(&out.stdout)?;

    let mut shares = Vec::new();
    for entry in entries {
        match entry.kind.as_str() {
            "new" => {
                if let Some(info) = new_info(server, entry) {
                    shares.push(info);
                }
            }
            "legacy" => {
                // `/srv/vpnshare/tls` 等非分享目录也会出现在摘要里；不是 16 位 id
                // 的一律不是旧分享。
                if !is_valid_id(&entry.id) {
                    continue;
                }
                let unit = entry.unit.clone().unwrap_or_default();
                let mut active = entry.active.unwrap_or(false);
                if !unit.is_empty() && crate::share::unit_needs_upgrade(&unit) {
                    let port = crate::share::parse_port_from_unit(&unit).unwrap_or(0);
                    if port != 0 {
                        crate::share::upgrade_share_unit(&mut sess, cancel, &entry.id, port)
                            .await?;
                        active = run_cmd(
                            &mut sess,
                            cancel,
                            "确认",
                            &format!("systemctl is-active {}", unit_name(&entry.id)),
                            PROBE_TIMEOUT,
                        )
                        .await
                        .map(|o| o.stdout.trim() == "active")
                        .unwrap_or(false);
                        // 升级替换了正在服务的进程：必须验链接真取得到，不能只看
                        // restart 退出码（2026-09-21 的死链就是只看了进程状态）。
                        let code = run_cmd(
                            &mut sess,
                            cancel,
                            "升级",
                            &format!(
                                "curl -s -o /dev/null -w '%{{http_code}}' --max-time 10 \
                                 http://127.0.0.1:{port}/sub/{}",
                                entry.id
                            ),
                            PROBE_TIMEOUT,
                        )
                        .await?
                        .stdout;
                        if code.trim() != "200" {
                            return Err(format!(
                                "旧分享 {} 升级后链接自测返回 {}（期望 200）",
                                entry.id,
                                code.trim()
                            ));
                        }
                    }
                }
                shares.push(legacy_info(
                    server,
                    &entry.id,
                    &unit,
                    entry.snapshot.as_deref(),
                    active,
                ));
            }
            _ => {}
        }
    }
    Ok(shares)
}

fn new_info(server: &Server, entry: SummaryEntry) -> Option<ShareInfo> {
    let manifest = entry.manifest?;
    // 清单里的 id/token 会进 URL 与命令；非法值直接跳过，不显示也不执行。
    if !is_valid_id(&manifest.id) || !is_valid_token(&manifest.token) {
        return None;
    }
    let usage = entry.usage.unwrap_or_default();
    let units = entry.units.unwrap_or_default();
    let active = required_units(&manifest)
        .iter()
        .all(|unit| units.get(unit).and_then(|v| v.as_deref()) == Some("active"));
    let mut info = info_from_manifest(&manifest, &usage, server, active);
    if let Some(last) = entry.last_access {
        info.last_access_at = Some(last.at);
        info.last_access_ip = Some(last.ip);
    }
    Some(info)
}

/// 一份分享需要同时处于 active 的单元。清单里没有的实例单元（例如 SS 分享没有
/// xray/singbox）不参与判断——摘要脚本对不存在的单元返回 null。
fn required_units(m: &ShareManifest) -> Vec<String> {
    let mut units = vec![unit_name(&m.id), gateway_unit_name(&m.id)];
    if m.nodes
        .iter()
        .any(|n| matches!(n.protocol.as_str(), "vless" | "trojan" | "vmess"))
    {
        units.push(instance_unit_name(&m.id, "xray"));
    }
    if m.nodes
        .iter()
        .any(|n| matches!(n.protocol.as_str(), "hysteria2" | "tuic"))
    {
        units.push(instance_unit_name(&m.id, "singbox"));
    }
    if m.nodes.iter().any(|n| n.protocol == "ss") {
        units.push(instance_unit_name(&m.id, "ss"));
    }
    units
}

/// 旧分享：明文 HTTP、无配额/限速/到期，只有端口与快照有意义。
/// 状态固定为 [`ShareStatus::Legacy`]——UI 用「旧版·无管控」明确区分。
pub(crate) fn legacy_info(
    server: &Server,
    id: &str,
    unit: &str,
    snapshot: Option<&str>,
    active: bool,
) -> ShareInfo {
    let port = crate::share::parse_port_from_unit(unit).unwrap_or(0);
    let host = effective_host(server).to_string();
    ShareInfo {
        id: id.to_string(),
        server_id: server.id,
        server_name: server.name.clone(),
        host: host.clone(),
        token: id.to_string(),
        port,
        active,
        url: legacy_url(&host, port, id),
        url_singbox: String::new(), // 旧分享没有 sing-box 与 HTTPS
        node_names: snapshot.map(crate::share::proxy_names).unwrap_or_default(),
        legacy: true,
        status: ShareStatus::Legacy,
        status_detail: Some("旧版·无管控".into()),
        quota_bytes: 0,
        used_bytes: 0,
        rate_up_mbps: 0,
        rate_down_mbps: 0,
        expires_at: None,
        last_access_at: None,
        last_access_ip: None,
    }
}
