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
use crate::servers::upstream::UPSTREAM_CONFIG_PATH;
use crate::servers::{CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::share::clone::{clone_configs, ShareNodeSpec, ShareSource};
use crate::share::model::*;
use crate::share::naming::*;
use crate::share::plan;
use crate::share::plan::{
    reserved_share_ports_cmd, tcp_port_probe_cmd, udp_port_probe_cmd,
};
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
    // 把刚刚核验/自动检测到的域名带进创建编排：服务器上可能没有显式配置
    // `share_domain`，只有证书。若这里不带，下面会再次报「未配置分享域名」。
    create_share_inner_with_domain(state, input, Some(&tls.domain)).await
}

/// 创建编排本体。仅测试直接调用以绕过 TLS 外部依赖；生产路径必须经
/// [`create_share`]（它会先查证书，并把自动检测到的域名传进来）。
#[cfg(test)]
pub(crate) async fn create_share_inner(
    state: &Arc<AppState>,
    input: CreateShareInput,
) -> Result<ShareInfo, String> {
    create_share_inner_with_domain(state, input, None).await
}

async fn create_share_inner_with_domain(
    state: &Arc<AppState>,
    input: CreateShareInput,
    resolved_domain: Option<&str>,
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

    // 上游出口的入站不在主 Xray 配置里，而在专用的 `xray-upstream` 实例。
    // 先按 node_id 认出哪些选中节点来自上游，克隆时才选对配置来源。
    let upstream_node_ids: HashSet<String> = {
        let conn = state.db.lock();
        crate::db::repos::upstreams::list_by_server(&conn, input.server_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|u| u.node_id)
            .collect()
    };

    // 2. 读服务器/凭据并建立 SSH 会话。
    let (auth, server) =
        crate::servers::load_auth(state, input.server_id).map_err(|e| e.to_string())?;
    let domain = match resolved_domain.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => crate::servers::normalize_share_domain(d)?,
        None => crate::servers::normalize_share_domain(&server.share_domain)?,
    };
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

    // 直接调用本函数（测试/旧调用）时没有 TLS 层传来的域名：再从服务器证书兜底
    // 检测一次。生产入口已经由 `tls_status` 核验过证书，不会走到第二次 SSH 读取。
    let domain = if domain.is_empty() {
        tls::detect_cert_domain(&mut sess, &cancel).await?
    } else {
        domain
    };
    if domain.is_empty() {
        return Err(
            "该服务器未配置分享域名，服务器上也没有可自动检测的分享证书（服务器 → 编辑 → 分享域名）"
                .into(),
        );
    }

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
    let specs = build_specs(&chosen, &allocated.pairs, &upstream_node_ids)?;

    // 4. 读配置。需要哪个协议/来源才读哪个；文件缺失即报错，不静默跳过。
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
    let upstream_xray =
        read_main(&mut sess, &cancel, &specs, "upstream", UPSTREAM_CONFIG_PATH).await?;

    // 5. 克隆：只改端口、监听地址与凭据，其余流设置/出站原样保留。
    let cloned = clone_configs(
        main_xray.as_deref(),
        upstream_xray.as_deref(),
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
        // 旧字段写成上下行之和，便于降级展示；判停以 quota_up/down 为准。
        quota_bytes: input
            .options
            .quota_up_bytes
            .saturating_add(input.options.quota_down_bytes),
        quota_up_bytes: Some(input.options.quota_up_bytes),
        quota_down_bytes: Some(input.options.quota_down_bytes),
        rate_up_mbps: input.options.rate_up_mbps,
        rate_down_mbps: input.options.rate_down_mbps,
        expires_at_unix,
        http_port: allocated.http,
        domain: domain.clone(),
        nodes: share_nodes,
    };
    let gateway = GatewayConfig {
        quota_bytes: manifest.quota_bytes,
        quota_up_bytes: manifest.quota_up_bytes,
        quota_down_bytes: manifest.quota_down_bytes,
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
    let split = m.uses_split_quota();
    let (used_up, used_down) = u.direction_counts(split);
    let quota_hit = if split {
        (m.quota_up() > 0 && used_up >= m.quota_up())
            || (m.quota_down() > 0 && used_down >= m.quota_down())
    } else {
        m.quota_bytes > 0 && u.used_bytes >= m.quota_bytes
    };
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

/// 状态提示：配额停用时点名是哪个方向超了，其它停用原因保持原样。
pub(crate) fn status_detail_from(u: &UsageState) -> Option<String> {
    match (u.blocked.as_deref(), u.blocked_direction.as_deref()) {
        (Some("quota"), Some("up")) => Some("上行配额已用完".to_string()),
        (Some("quota"), Some("down")) => Some("下行配额已用完".to_string()),
        (Some("quota"), _) => Some("配额已用完".to_string()),
        (other, _) => other.map(str::to_string),
    }
}

pub(crate) fn info_from_manifest(
    m: &ShareManifest,
    u: &UsageState,
    server: &Server,
    active: bool,
) -> ShareInfo {
    let status = status_from(m, u, active);
    let split = m.uses_split_quota();
    let (used_up, used_down) = u.direction_counts(split);
    let used_bytes = if split {
        used_up.saturating_add(used_down)
    } else {
        u.used_bytes
    };
    let quota_bytes = if split {
        m.quota_up().saturating_add(m.quota_down())
    } else {
        m.quota_bytes
    };
    let status_detail = status_detail_from(u);
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
        status_detail,
        quota_bytes,
        used_bytes,
        quota_up_bytes: m.quota_up(),
        quota_down_bytes: m.quota_down(),
        used_up_bytes: used_up,
        used_down_bytes: used_down,
        split_quota: split,
        rate_up_mbps: m.rate_up_mbps,
        rate_down_mbps: m.rate_down_mbps,
        expires_at: chrono::DateTime::from_timestamp(m.expires_at_unix, 0)
            .map(|t| t.to_rfc3339()),
        last_access_at: None,
        last_access_ip: None,
    }
}

/// 只有当选中节点里出现该协议族/来源时才读配置；文件不存在即报错，不静默跳过。
///
/// `family = "upstream"` 读的是上游出口专用的 `xray-upstream` 配置；其余 family
/// 只服务主服务配置，因此必须排除上游节点（它们的端口与协议在主配置里并不存在）。
pub(crate) async fn read_main(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    specs: &[ShareNodeSpec],
    family: &str,
    path: &str,
) -> Result<Option<String>, String> {
    let needed = specs.iter().any(|s| match family {
        "upstream" => s.source == ShareSource::Upstream,
        "vless" => {
            s.source == ShareSource::Main
                && matches!(s.protocol.as_str(), "vless" | "trojan" | "vmess")
        }
        "hysteria2" => {
            s.source == ShareSource::Main && matches!(s.protocol.as_str(), "hysteria2" | "tuic")
        }
        "ss" => s.source == ShareSource::Main && s.protocol == "ss",
        _ => false,
    });
    if !needed {
        return Ok(None);
    }
    match read_remote_file(sess, cancel, "读取", path, PROBE_TIMEOUT).await? {
        RemoteFile::Absent if family == "upstream" => Err(format!(
            "上游出口配置不存在：{path}（请先在这台服务器上部署上游出口）"
        )),
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
    reserved: &HashSet<u16>,
) -> Result<u16, String> {
    while *cursor <= end {
        let port = *cursor;
        *cursor += 1;
        if reserved.contains(&port) {
            continue;
        }
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

/// 读取已有分享占用的端口；输出为空表示没有历史分享。
async fn reserved_share_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<HashSet<u16>, String> {
    let out = run_cmd(
        sess,
        cancel,
        "探测",
        &reserved_share_ports_cmd(),
        PROBE_TIMEOUT,
    )
    .await?;
    if out.exit_code != 0 {
        return Err(format!(
            "读取已有分享端口失败: {}",
            out.stderr.trim()
        ));
    }
    Ok(out
        .stdout
        .lines()
        .filter_map(|s| s.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
        .collect())
}

/// 订阅端口、公网数据端口、回环内部端口三段各自独立分配；每段都从段首开始，
/// 实际占用情况由远端 `ss` 探测决定；已停止分享配置过的端口也跳过。
pub async fn allocate_share_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    transports: &[&str],
) -> Result<AllocatedPorts, String> {
    let reserved = reserved_share_ports(sess, cancel).await?;
    let mut http_cursor = PORT_RANGE_START;
    let http = find_free_port(
        sess,
        cancel,
        &mut http_cursor,
        PORT_RANGE_END,
        "tcp",
        &reserved,
    )
    .await?;
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
            &reserved,
        )
        .await?;
        let internal = find_free_port(
            sess,
            cancel,
            &mut internal_cursor,
            INTERNAL_PORT_END,
            transport,
            &reserved,
        )
        .await?;
        pairs.push((public, internal));
    }
    Ok(AllocatedPorts { http, pairs })
}

/// 由节点与端口对生成克隆规格；凭据在这里一次性生成。
///
/// `upstream_node_ids` 是这台服务器上由上游出口写回的节点 id。它们虽然也表现为
/// `self-built` 的 VLESS 节点，入站却在专用 `xray-upstream` 配置里，克隆来源不同。
pub fn build_specs(
    nodes: &[Node],
    pairs: &[(u16, u16)],
    upstream_node_ids: &HashSet<String>,
) -> Result<Vec<ShareNodeSpec>, String> {
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
            let source = if upstream_node_ids.contains(&n.id) {
                // 上游出口只有 VLESS+REALITY 一种客户端形态；真出现别的协议说明
                // 节点被别处改过，宁可显式拒绝，也不要拿错来源的配置去猜。
                if n.protocol != "vless" {
                    return Err(format!(
                        "上游出口节点 {} 的协议为 {}，当前只支持分享 VLESS 上游",
                        n.name, n.protocol
                    ));
                }
                ShareSource::Upstream
            } else {
                ShareSource::Main
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
                source,
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
    /// 最近一次取订阅的格式（yaml/singbox）；列表目前不展示，保留以完整映射摘要。
    #[serde(default)]
    #[allow(dead_code)]
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
        quota_up_bytes: 0,
        quota_down_bytes: 0,
        used_up_bytes: 0,
        used_down_bytes: 0,
        split_quota: false,
        rate_up_mbps: 0,
        rate_down_mbps: 0,
        expires_at: None,
        last_access_at: None,
        last_access_ip: None,
    }
}

// ---------- 编辑 / 重置用量 / 作废 ----------

/// 编辑配额、速率与到期。**保留已用字节**，只清除「已停用」标记——用户提高配额
/// 或续期后网关重新开始转发；若新配额仍低于已用量，状态仍会显示超额（下次计费
/// 时网关会立刻再次停用）。
#[allow(dead_code)] // 兼容旧调用方/真机测试；命令层已改用 update_share_with_nodes
pub async fn update_share(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
    options: ShareOptions,
) -> Result<ShareInfo, String> {
    update_share_with_nodes(state, server_id, id, options, None).await
}

/// 编辑分享。`node_ids = Some(...)` 时同时增删节点；`None` 保持旧行为，只改配额/速率/到期。
///
/// 增删节点是**增量替换**：保留仍被选中的节点的公网/回环端口与凭据，链接和 token
/// 不变；新节点分配端口并生成新凭据；未选中的节点及其端口/单元/防火墙规则一并移除。
pub async fn update_share_with_nodes(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
    options: ShareOptions,
    node_ids: Option<Vec<String>>,
) -> Result<ShareInfo, String> {
    if !is_valid_id(id) {
        return Err("分享 id 非法".into());
    }
    validate_options(&options)?;
    let (auth, server) =
        crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
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

    let mut manifest = read_new_manifest(&mut sess, &cancel, id).await?;
    if manifest.server_id != server_id {
        return Err("分享不属于该服务器".into());
    }
    let mut usage = read_usage(&mut sess, &cancel, id).await?;

    let expires_at_unix =
        server_now_unix(&mut sess, &cancel).await? + options.expires_in_days as i64 * 86400;
    manifest.quota_bytes = options
        .quota_up_bytes
        .saturating_add(options.quota_down_bytes);
    manifest.quota_up_bytes = Some(options.quota_up_bytes);
    manifest.quota_down_bytes = Some(options.quota_down_bytes);
    manifest.rate_up_mbps = options.rate_up_mbps;
    manifest.rate_down_mbps = options.rate_down_mbps;
    manifest.expires_at_unix = expires_at_unix;
    let gateway = gateway_from_manifest(&manifest);

    usage.blocked = None;
    usage.blocked_at = None;
    usage.blocked_direction = None;
    usage.updated_at = chrono::Utc::now().to_rfc3339();

    if let Some(node_ids) = node_ids {
        return update_share_node_set(
            state, &mut sess, &cancel, &server, id, manifest, usage, node_ids,
        )
        .await;
    }

    let gw_unit = gateway_unit_name(id);
    run_cmd_ok(
        &mut sess,
        &cancel,
        "停止网关",
        &format!("systemctl stop {gw_unit}"),
        CONFIG_TIMEOUT,
    )
    .await?;

    let write_result = async {
        // 网关脚本是所有分享共用的。旧分享在服务器重启/单元重启后也会加载它，
        // 因此升级脚本必须保持对旧 gateway.json（只有 quota_bytes）的兼容。
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
            &manifest_path(id),
            &serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &gateway_config_path(id),
            &serde_json::to_string_pretty(&gateway).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "写入",
            &usage_path(id),
            &serde_json::to_string(&usage).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        Ok::<(), String>(())
    }
    .await;
    // 配置写失败也要试着把网关拉起来：半成品配置可能起不来，但让它一直停着
    // 更糟（用户失去所有连接且没有任何自动恢复）。
    let start_result = run_cmd_ok(
        &mut sess,
        &cancel,
        "启动网关",
        &format!("systemctl start {gw_unit}"),
        CONFIG_TIMEOUT,
    )
    .await;
    if let Err(e) = write_result {
        return Err(format!("更新分享配置失败：{e}（已尝试重启网关）"));
    }
    start_result.map_err(|e| format!("启动网关失败：{e}"))?;
    ensure_gateway_active(&mut sess, &cancel, &gw_unit).await?;

    Ok(info_from_manifest(&manifest, &usage, &server, true))
}

/// 节点集合编辑：保留仍被选中节点的端口/凭据，新增节点分配端口并生成凭据，
/// 未选中的节点连同配置、单元与防火墙放行一起移除。
///
/// 任何一步失败都会用编辑前的备份恢复整份分享（目录 + 单元文件 + 防火墙），
/// 避免「配额已改、节点只换了一半」的半成品状态。
async fn update_share_node_set(
    state: &Arc<AppState>,
    sess: &mut SshSession,
    cancel: &AtomicBool,
    server: &Server,
    id: &str,
    mut manifest: ShareManifest,
    usage: UsageState,
    node_ids: Vec<String>,
) -> Result<ShareInfo, String> {
    if node_ids.is_empty() {
        return Err("至少选择一个节点".into());
    }

    let old_nodes = manifest.nodes.clone();
    let candidates = plan::share_candidates(state, manifest.server_id)?;
    let mut old_by_key: HashMap<(String, String), ShareNode> = old_nodes
        .iter()
        .cloned()
        .map(|n| ((n.name.clone(), n.protocol.clone()), n))
        .collect();

    // 仍被选中的节点按 (name, protocol) 认领旧端口/凭据；旧节点即使现在不可分享
    // （例如后来被停用/挂上中转），也允许用户保留，只有**新增**节点要求通过准入。
    let mut seen = HashSet::new();
    let mut chosen: Vec<(Node, Option<ShareNode>)> = Vec::new();
    for node_id in node_ids {
        if !seen.insert(node_id.clone()) {
            return Err(format!("节点重复选择: {node_id}"));
        }
        let candidate = candidates
            .iter()
            .find(|c| c.node.id == node_id)
            .ok_or_else(|| format!("节点不存在: {node_id}"))?;
        let key = (candidate.node.name.clone(), candidate.node.protocol.clone());
        let retained = old_by_key.remove(&key);
        if retained.is_none() {
            if let Some(reason) = &candidate.reason {
                return Err(format!("节点 {} 不可分享: {reason}", candidate.node.name));
            }
        }
        chosen.push((candidate.node.clone(), retained));
    }
    if chosen.iter().filter(|(n, _)| n.protocol == "ss").count() > 1 {
        return Err("Shadowsocks 分享一次只允许一个节点".into());
    }

    let upstream_node_ids: HashSet<String> = {
        let conn = state.db.lock();
        crate::db::repos::upstreams::list_by_server(&conn, manifest.server_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|u| u.node_id)
            .collect()
    };

    // 只给新增节点分配端口；被保留的节点沿用原公网/回环端口，已发出去的订阅
    // 里这些端口仍然有效。
    let new_transports: Vec<&str> = chosen
        .iter()
        .filter(|(_, old)| old.is_none())
        .map(|(n, _)| transport_for(&n.protocol))
        .collect();
    let allocated = if new_transports.is_empty() {
        None
    } else {
        Some(allocate_share_ports(sess, cancel, &new_transports).await?)
    };
    let mut next_new_pair = 0usize;
    let mut pairs = Vec::with_capacity(chosen.len());
    for (_, old) in &chosen {
        match old {
            Some(n) => pairs.push((n.public_port, n.internal_port)),
            None => {
                let pair = allocated
                    .as_ref()
                    .and_then(|a| a.pairs.get(next_new_pair))
                    .copied()
                    .ok_or("分配新节点端口失败")?;
                next_new_pair += 1;
                pairs.push(pair);
            }
        }
    }

    let chosen_nodes: Vec<Node> = chosen.iter().map(|(n, _)| n.clone()).collect();
    let mut specs = build_specs(&chosen_nodes, &pairs, &upstream_node_ids)?;
    for (spec, (_, old)) in specs.iter_mut().zip(chosen.iter()) {
        if let Some(old) = old {
            let (credential, password) = existing_share_credential(old)?;
            spec.new_credential = credential;
            spec.new_password = password;
        }
    }

    let main_xray = read_main(sess, cancel, &specs, "vless", XRAY_CONFIG_PATH).await?;
    let main_sb = read_main(sess, cancel, &specs, "hysteria2", SINGBOX_CONFIG_PATH).await?;
    let main_ss = read_main(sess, cancel, &specs, "ss", SS_CONFIG_PATH).await?;
    let upstream_xray = read_main(sess, cancel, &specs, "upstream", UPSTREAM_CONFIG_PATH).await?;
    let cloned = clone_configs(
        main_xray.as_deref(),
        upstream_xray.as_deref(),
        main_sb.as_deref(),
        main_ss.as_deref(),
        &specs,
        effective_host(server),
        id,
        &[],
    )?;
    manifest.nodes = specs
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
    let gateway = gateway_from_manifest(&manifest);
    let dir = share_dir(id);
    let bind = if server.addr_family == "v6" {
        "::"
    } else {
        "0.0.0.0"
    };
    let new_pairs = spec_public_pairs(&specs);
    let old_pairs = share_node_public_pairs(&old_nodes);
    let old_families = share_node_families(&old_nodes);
    let new_families = share_node_families(&manifest.nodes);
    let old_instance_units = instance_units_for(id, &old_families);
    let new_instance_units = instance_units_for(id, &new_families);

    let backup = format!("/srv/vpnshare/.update-{id}");
    run_cmd_ok(
        sess,
        cancel,
        "备份",
        &format!("rm -rf {backup} && mkdir -p {backup}/units && cp -a {dir} {backup}/data"),
        CONFIG_TIMEOUT,
    )
    .await
    .map_err(|e| format!("备份现有分享失败，未做任何修改：{e}"))?;
    let _ = run_cmd_ok(
        sess,
        cancel,
        "备份",
        &format!(
            "cp -a /etc/systemd/system/vpnshare-{id}*.service {backup}/units/ 2>/dev/null || true"
        ),
        CONFIG_TIMEOUT,
    )
    .await;
    run_cmd_ok(
        sess,
        cancel,
        "备份",
        &format!(
            "test -f {backup}/units/vpnshare-{id}.service && \
             test -f {backup}/units/vpnshare-{id}-gateway.service"
        ),
        CONFIG_TIMEOUT,
    )
    .await
    .map_err(|e| format!("备份现有单元失败，未做任何修改：{e}"))?;

    let mut old_start_units = vec![unit_name(id), gateway_unit_name(id)];
    old_start_units.extend(old_instance_units.iter().cloned());

    let mut stop_units: Vec<String> = Vec::new();
    let mut stop_seen = HashSet::new();
    for unit in std::iter::once(gateway_unit_name(id))
        .chain(std::iter::once(unit_name(id)))
        .chain(old_instance_units.iter().cloned())
        .chain(new_instance_units.iter().cloned())
    {
        if stop_seen.insert(unit.clone()) {
            stop_units.push(unit);
        }
    }
    for unit in &stop_units {
        run_cmd_ok(
            sess,
            cancel,
            "停止旧分享",
            &format!("systemctl stop {unit} 2>/dev/null || true"),
            CONFIG_TIMEOUT,
        )
        .await?;
    }
    // `|| true` 会把真实失败也吞掉；写配置前必须确认每个单元真的停了，
    // 否则旧实例可能继续占用回环端口，或把已删除的节点留在内存里运行。
    let mut still_active = Vec::new();
    for unit in &stop_units {
        let state = run_cmd(
            sess,
            cancel,
            "停止旧分享",
            &format!("systemctl is-active {unit}"),
            PROBE_TIMEOUT,
        )
        .await?
        .stdout
        .trim()
        .to_string();
        if matches!(state.as_str(), "active" | "activating" | "deactivating") {
            still_active.push(format!("{unit}（{state}）"));
        }
    }
    if !still_active.is_empty() {
        let leftovers = restore_share_after_failed_node_update(
            sess,
            cancel,
            id,
            &backup,
            &old_nodes,
            &manifest.nodes,
            &old_start_units,
        )
        .await;
        return Err(format!(
            "单元未能停止：{}，未修改节点{}",
            still_active.join("、"),
            rollback_suffix(&leftovers)
        ));
    }

    let setup: Result<(), String> = async {
        write_remote_file(
            sess,
            cancel,
            "写入",
            &share_gateway_script_path(),
            &scripts::share_gateway_script(),
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            sess,
            cancel,
            "写入",
            &manifest_path(id),
            &serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            sess,
            cancel,
            "写入",
            &gateway_config_path(id),
            &serde_json::to_string_pretty(&gateway).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            sess,
            cancel,
            "写入",
            &usage_path(id),
            &serde_json::to_string(&usage).map_err(|e| e.to_string())?,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            sess,
            cancel,
            "写入",
            &yaml_path(id, &manifest.token),
            &cloned.yaml,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            sess,
            cancel,
            "写入",
            &singbox_path(id, &manifest.token),
            &cloned.singbox_json,
            CONFIG_TIMEOUT,
        )
        .await?;

        // 已不再需要的协议族：停用、删单元与实例配置；sing-box 的证书副本一并清理。
        for family in old_families.difference(&new_families) {
            let unit = instance_unit_name(id, family);
            run_cmd_ok(
                sess,
                cancel,
                "移除旧节点",
                &format!("systemctl disable {unit} 2>/dev/null || true"),
                CONFIG_TIMEOUT,
            )
            .await?;
            run_cmd_ok(
                sess,
                cancel,
                "移除旧节点",
                &format!(
                    "rm -f {} {}",
                    unit_path(&unit),
                    instance_config_path(id, family)
                ),
                CONFIG_TIMEOUT,
            )
            .await?;
            if *family == "singbox" {
                run_cmd_ok(
                    sess,
                    cancel,
                    "移除旧节点",
                    &format!("rm -f {dir}/singbox.crt {dir}/singbox.key"),
                    CONFIG_TIMEOUT,
                )
                .await?;
            }
        }

        if let Some(cfg) = &cloned.xray_config {
            write_remote_file(
                sess,
                cancel,
                "写入",
                &instance_config_path(id, "xray"),
                cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            let unit = instance_unit_name(id, "xray");
            write_remote_file(
                sess,
                cancel,
                "写入",
                &unit_path(&unit),
                &instance_unit(id, &unit, "xray", &dir, &instance_config_path(id, "xray")),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        if let Some(cfg) = &cloned.singbox_config {
            run_cmd_ok(
                sess,
                cancel,
                "准备",
                &format!(
                    "install -m 600 /etc/sing-box/self.crt {dir}/singbox.crt && \
                     install -m 600 /etc/sing-box/self.key {dir}/singbox.key"
                ),
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                sess,
                cancel,
                "写入",
                &instance_config_path(id, "singbox"),
                cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            let unit = instance_unit_name(id, "singbox");
            write_remote_file(
                sess,
                cancel,
                "写入",
                &unit_path(&unit),
                &instance_unit(
                    id,
                    &unit,
                    "singbox",
                    &dir,
                    &instance_config_path(id, "singbox"),
                ),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        if let Some(cfg) = &cloned.ss_config {
            write_remote_file(
                sess,
                cancel,
                "写入",
                &instance_config_path(id, "ss"),
                cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            let unit = instance_unit_name(id, "ss");
            write_remote_file(
                sess,
                cancel,
                "写入",
                &unit_path(&unit),
                &instance_unit(id, &unit, "ss", &dir, &instance_config_path(id, "ss")),
                CONFIG_TIMEOUT,
            )
            .await?;
        }

        let share_units = unit_contents(id, manifest.http_port, bind, &dir, &new_pairs);
        write_remote_file(
            sess,
            cancel,
            "写入",
            &unit_path(&unit_name(id)),
            &share_units.http,
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            sess,
            cancel,
            "写入",
            &unit_path(&gateway_unit_name(id)),
            &share_units.gateway,
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            sess,
            cancel,
            "准备",
            &format!("chown -R vpnshare:vpnshare {dir} && chmod 600 {dir}/*.json {dir}/sub/*"),
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            sess,
            cancel,
            "准备",
            "systemctl daemon-reload",
            CONFIG_TIMEOUT,
        )
        .await?;

        // 旧网关停止时已按旧单元关掉全部旧端口；这里再显式关闭新集合之外的
        // 端口，防止旧单元缺失 ExecStopPost 时留下永久放行规则。
        let removed_pairs: Vec<(u16, &str)> = old_pairs
            .iter()
            .filter(|p| !new_pairs.contains(p))
            .cloned()
            .collect();
        close_public_pairs(sess, cancel, id, &removed_pairs).await?;

        // 先起实例再起网关：网关开始转发时回环目标已经就绪。
        let mut start_units = new_instance_units.clone();
        start_units.push(gateway_unit_name(id));
        start_units.push(unit_name(id));
        for unit in &start_units {
            run_cmd_ok(
                sess,
                cancel,
                "启动",
                &format!("systemctl enable --now {unit}"),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        for unit in &start_units {
            let out = run_cmd(
                sess,
                cancel,
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
            sess,
            cancel,
            "自测",
            &format!(
                "curl -s -o /dev/null -w '%{{http_code}}' --resolve {domain}:{port}:127.0.0.1 \
                 https://{domain}:{port}/sub/{token} --max-time 10",
                domain = manifest.domain,
                port = manifest.http_port,
                token = manifest.token
            ),
            PROBE_TIMEOUT,
        )
        .await?
        .stdout;
        if code.trim() != "200" {
            return Err(format!("分享链接自测返回 {}（期望 200）", code.trim()));
        }
        Ok(())
    }
    .await;

    if let Err(e) = setup {
        let leftovers = restore_share_after_failed_node_update(
            sess,
            cancel,
            id,
            &backup,
            &old_nodes,
            &manifest.nodes,
            &old_start_units,
        )
        .await;
        return Err(format!(
            "{e}；{}",
            if leftovers.is_empty() {
                "已恢复编辑前的节点配置".to_string()
            } else {
                format!("回滚后仍有残留：{leftovers:?}")
            }
        ));
    }

    let _ = run_cmd_ok(
        sess,
        cancel,
        "清理备份",
        &format!("rm -rf {backup}"),
        CONFIG_TIMEOUT,
    )
    .await;
    Ok(info_from_manifest(&manifest, &usage, server, true))
}

fn rollback_suffix(leftovers: &[String]) -> String {
    if leftovers.is_empty() {
        String::new()
    } else {
        format!("；回滚后仍有残留：{leftovers:?}")
    }
}

/// 失败回滚：停掉新旧全部单元，恢复分享目录与单元文件，再按旧节点重开端口/启动。
/// 所有步骤尽力而为，返回无法确认恢复的项目。
async fn restore_share_after_failed_node_update(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    id: &str,
    backup: &str,
    old_nodes: &[ShareNode],
    new_nodes: &[ShareNode],
    old_units: &[String],
) -> Vec<String> {
    let mut leftovers = Vec::new();
    for unit in [
        unit_name(id),
        gateway_unit_name(id),
        instance_unit_name(id, "xray"),
        instance_unit_name(id, "singbox"),
        instance_unit_name(id, "ss"),
    ] {
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
    }
    let dir = share_dir(id);
    if run_cmd_ok(
        sess,
        cancel,
        "回滚",
        &format!("rm -f /etc/systemd/system/vpnshare-{id}*.service"),
        CONFIG_TIMEOUT,
    )
    .await
    .is_err()
    {
        leftovers.push("旧单元文件清理".into());
    }
    let _ = run_cmd_ok(
        sess,
        cancel,
        "回滚",
        &format!(
            "cp -a {backup}/units/vpnshare-{id}*.service /etc/systemd/system/ 2>/dev/null || true"
        ),
        CONFIG_TIMEOUT,
    )
    .await;
    if run_cmd_ok(
        sess,
        cancel,
        "回滚",
        &format!("rm -rf {dir} && cp -a {backup}/data {dir}"),
        CONFIG_TIMEOUT,
    )
    .await
    .is_err()
    {
        leftovers.push(format!("目录 {dir}"));
    }
    if run_cmd_ok(
        sess,
        cancel,
        "回滚",
        "systemctl daemon-reload",
        CONFIG_TIMEOUT,
    )
    .await
    .is_err()
    {
        leftovers.push("daemon-reload".into());
    }

    let old_pairs = share_node_public_pairs(old_nodes);
    let new_pairs = share_node_public_pairs(new_nodes);
    let added: Vec<(u16, &str)> = new_pairs
        .iter()
        .filter(|p| !old_pairs.contains(p))
        .cloned()
        .collect();
    if let Err(e) = close_public_pairs(sess, cancel, id, &added).await {
        leftovers.push(format!("撤新端口：{e}"));
    }
    if let Err(e) = open_public_pairs(sess, cancel, id, &old_pairs).await {
        leftovers.push(format!("恢复旧端口：{e}"));
    }

    for unit in old_units {
        if run_cmd_ok(
            sess,
            cancel,
            "回滚",
            &format!("systemctl enable --now {unit}"),
            CONFIG_TIMEOUT,
        )
        .await
        .is_err()
        {
            leftovers.push(format!("启动 {unit}"));
        }
    }
    let _ = run_cmd_ok(
        sess,
        cancel,
        "回滚",
        &format!("rm -rf {backup}"),
        CONFIG_TIMEOUT,
    )
    .await;
    leftovers
}

fn transport_for(protocol: &str) -> &'static str {
    match protocol {
        "hysteria2" | "tuic" => "udp",
        "ss" => "tcp_and_udp",
        _ => "tcp",
    }
}

fn share_node_families(nodes: &[ShareNode]) -> HashSet<&'static str> {
    nodes
        .iter()
        .filter_map(|n| match n.protocol.as_str() {
            "vless" | "trojan" | "vmess" => Some("xray"),
            "hysteria2" | "tuic" => Some("singbox"),
            "ss" => Some("ss"),
            _ => None,
        })
        .collect()
}

fn instance_units_for(id: &str, families: &HashSet<&'static str>) -> Vec<String> {
    let mut units = Vec::new();
    for family in ["xray", "singbox", "ss"] {
        if families.contains(family) {
            units.push(instance_unit_name(id, family));
        }
    }
    units
}

fn share_node_public_pairs(nodes: &[ShareNode]) -> Vec<(u16, &'static str)> {
    nodes
        .iter()
        .flat_map(|n| match n.transport.as_str() {
            "tcp_and_udp" => vec![(n.public_port, "tcp"), (n.public_port, "udp")],
            "tcp" => vec![(n.public_port, "tcp")],
            "udp" => vec![(n.public_port, "udp")],
            _ => Vec::new(),
        })
        .collect()
}

fn spec_public_pairs(specs: &[ShareNodeSpec]) -> Vec<(u16, &'static str)> {
    specs
        .iter()
        .flat_map(|s| match s.transport {
            "tcp_and_udp" => vec![(s.public_port, "tcp"), (s.public_port, "udp")],
            other => vec![(s.public_port, other)],
        })
        .collect()
}

async fn close_public_pairs(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    id: &str,
    pairs: &[(u16, &str)],
) -> Result<(), String> {
    if pairs.is_empty() {
        return Ok(());
    }
    run_cmd_ok(
        sess,
        cancel,
        "撤防火墙",
        &format!(
            "{} close vpnshare-{id} {}",
            firewall_script_path(),
            firewall_specs(pairs)
        ),
        CONFIG_TIMEOUT,
    )
    .await
    .map(|_| ())
}

async fn open_public_pairs(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    id: &str,
    pairs: &[(u16, &str)],
) -> Result<(), String> {
    if pairs.is_empty() {
        return Ok(());
    }
    run_cmd_ok(
        sess,
        cancel,
        "恢复防火墙",
        &format!(
            "{} open vpnshare-{id} {}",
            firewall_script_path(),
            firewall_specs(pairs)
        ),
        CONFIG_TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// 从现有分享的节点参数里取回凭据；编辑节点集合时保留这些凭据，
/// 已导入的客户端不需要换订阅也能继续用。
fn existing_share_credential(node: &ShareNode) -> Result<(String, Option<String>), String> {
    let params: serde_json::Value = serde_json::from_str(&node.params_json)
        .map_err(|e| format!("节点 {} 的分享参数损坏: {e}", node.name))?;
    let field = |key: &str| {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("节点 {} 的分享参数缺少 {key}", node.name))
    };
    match node.protocol.as_str() {
        "vless" | "vmess" => Ok((field("uuid")?, None)),
        "tuic" => Ok((field("uuid")?, Some(field("password")?))),
        _ => Ok((field("password")?, None)),
    }
}

/// 重置用量：把 `usage.json` 归零并重启网关。到期时间与配额不变；已到期的分享
/// 会在网关启动时立刻再次停用。
pub async fn reset_usage(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
) -> Result<ShareInfo, String> {
    if !is_valid_id(id) {
        return Err("分享 id 非法".into());
    }
    let (auth, server) =
        crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
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
    let manifest = read_new_manifest(&mut sess, &cancel, id).await?;
    if manifest.server_id != server_id {
        return Err("分享不属于该服务器".into());
    }
    let usage = UsageState::default();
    let gw_unit = gateway_unit_name(id);
    run_cmd_ok(
        &mut sess,
        &cancel,
        "停止网关",
        &format!("systemctl stop {gw_unit}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    let write_result = write_remote_file(
        &mut sess,
        &cancel,
        "重置用量",
        &usage_path(id),
        &serde_json::to_string(&usage).map_err(|e| e.to_string())?,
        CONFIG_TIMEOUT,
    )
    .await;
    let start_result = run_cmd_ok(
        &mut sess,
        &cancel,
        "启动网关",
        &format!("systemctl start {gw_unit}"),
        CONFIG_TIMEOUT,
    )
    .await;
    if let Err(e) = write_result {
        return Err(format!("重置用量失败：{e}（已尝试重启网关）"));
    }
    start_result.map_err(|e| format!("启动网关失败：{e}"))?;
    ensure_gateway_active(&mut sess, &cancel, &gw_unit).await?;
    Ok(info_from_manifest(&manifest, &usage, &server, true))
}

/// 作废分享：停单元、删单元/目录、撤端口放行。幂等——重复调用不报错。
/// 旧分享没有 `share.json`，端口从单元文件解析；新旧两种防火墙注释都清。
pub async fn delete_share(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
) -> Result<(), String> {
    if !is_valid_id(id) {
        return Err("分享 id 非法".into());
    }
    let (auth, server) =
        crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
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

    let manifest: Option<ShareManifest> = match read_remote_file(
        &mut sess,
        &cancel,
        "读取",
        &manifest_path(id),
        PROBE_TIMEOUT,
    )
    .await?
    {
        RemoteFile::Absent => None,
        RemoteFile::Content(c) => serde_json::from_str(&c).ok(),
    };
    let mut ports: Vec<(u16, &'static str)> = Vec::new();
    match &manifest {
        Some(m) => {
            for n in &m.nodes {
                match n.transport.as_str() {
                    "tcp_and_udp" => {
                        ports.push((n.public_port, "tcp"));
                        ports.push((n.public_port, "udp"));
                    }
                    "tcp" => ports.push((n.public_port, "tcp")),
                    "udp" => ports.push((n.public_port, "udp")),
                    _ => {}
                }
            }
        }
        None => {
            // 旧分享：端口的事实源是单元文件。
            if let RemoteFile::Content(unit) = read_remote_file(
                &mut sess,
                &cancel,
                "读取",
                &unit_path(&unit_name(id)),
                PROBE_TIMEOUT,
            )
            .await?
            {
                if let Some(p) = crate::share::parse_port_from_unit(&unit) {
                    ports.push((p, "tcp"));
                }
            }
        }
    }
    // 单元文件也没了（旧分享被手工清理过）时，从残留的防火墙规则里找回端口，
    // 否则那条 ACCEPT 会永远留在链上。
    if ports.is_empty() {
        let rules = run_cmd(
            &mut sess,
            &cancel,
            "读取",
            &format!("iptables -S INPUT | grep -- 'vpnshare-{id}'"),
            PROBE_TIMEOUT,
        )
        .await
        .map(|o| o.stdout)
        .unwrap_or_default();
        for line in rules.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let proto = words
                .iter()
                .position(|w| *w == "-p")
                .and_then(|i| words.get(i + 1))
                .copied();
            let port = words
                .iter()
                .position(|w| *w == "--dport")
                .and_then(|i| words.get(i + 1))
                .and_then(|p| p.parse().ok());
            match (proto, port) {
                (Some("tcp"), Some(p)) => ports.push((p, "tcp")),
                (Some("udp"), Some(p)) => ports.push((p, "udp")),
                _ => {}
            }
        }
    }
    ports.sort();
    ports.dedup();

    for unit in [
        unit_name(id),
        gateway_unit_name(id),
        instance_unit_name(id, "xray"),
        instance_unit_name(id, "singbox"),
        instance_unit_name(id, "ss"),
    ] {
        // 单元可能本来就不存在，所以 disable 失败可以忽略；但**停没停**必须核验：
        // 进程还在跑就删目录会把「作废」变成一句空话（已下载配置仍能连）。
        run_cmd_ok(
            &mut sess,
            &cancel,
            "作废",
            &format!("systemctl disable --now {unit} 2>/dev/null || true"),
            CONFIG_TIMEOUT,
        )
        .await?;
        let state = run_cmd(
            &mut sess,
            &cancel,
            "作废",
            &format!("systemctl is-active {unit}"),
            PROBE_TIMEOUT,
        )
        .await?
        .stdout
        .trim()
        .to_string();
        if state == "active" || state == "activating" || state == "deactivating" {
            return Err(format!(
                "单元 {unit} 未能停止（systemctl：{state}），作废中止，请稍后重试"
            ));
        }
        run_cmd_ok(
            &mut sess,
            &cancel,
            "作废",
            &format!("rm -f {}", unit_path(&unit)),
            CONFIG_TIMEOUT,
        )
        .await?;
    }
    run_cmd_ok(
        &mut sess,
        &cancel,
        "作废",
        &format!("rm -rf {}", share_dir(id)),
        CONFIG_TIMEOUT,
    )
    .await?;

    for (port, proto) in ports {
        // 新格式注释带端口/proto；旧格式注释只有 vpnshare-<id>。两种都清，
        // 否则升级过的旧分享会在端口上留一条永久放行规则。
        for comment in [
            format!("vpnshare-{id}-{port}-{proto}"),
            format!("vpnshare-{id}"),
        ] {
            let _ = run_cmd(
                &mut sess,
                &cancel,
                "撤防火墙",
                &format!(
                    "while iptables -C INPUT -p {proto} --dport {port} -m comment --comment \"{comment}\" -j ACCEPT 2>/dev/null; do \
                     iptables -D INPUT -p {proto} --dport {port} -m comment --comment \"{comment}\" -j ACCEPT || break; done"
                ),
                CONFIG_TIMEOUT,
            )
            .await;
        }
    }
    run_cmd_ok(
        &mut sess,
        &cancel,
        "作废",
        "systemctl daemon-reload",
        CONFIG_TIMEOUT,
    )
    .await?;
    Ok(())
}

async fn read_new_manifest(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    id: &str,
) -> Result<ShareManifest, String> {
    match read_remote_file(sess, cancel, "读取", &manifest_path(id), PROBE_TIMEOUT).await? {
        RemoteFile::Absent => Err("分享不存在或不是新版分享".into()),
        RemoteFile::Content(c) => {
            let manifest: ShareManifest =
                serde_json::from_str(&c).map_err(|e| format!("分享清单损坏: {e}"))?;
            if manifest.id != id {
                return Err("分享清单与 id 不一致".into());
            }
            Ok(manifest)
        }
    }
}

/// 用量文件缺失或内容损坏时按 0 处理；**读取命令本身失败必须上报**——把读失败
/// 当成 0 会在编辑时静默清空已用字节。
async fn read_usage(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    id: &str,
) -> Result<UsageState, String> {
    match read_remote_file(sess, cancel, "读取", &usage_path(id), PROBE_TIMEOUT).await? {
        RemoteFile::Content(c) => Ok(serde_json::from_str(&c).unwrap_or_default()),
        RemoteFile::Absent => Ok(UsageState::default()),
    }
}

fn gateway_from_manifest(m: &ShareManifest) -> GatewayConfig {
    GatewayConfig {
        quota_bytes: m.quota_bytes,
        quota_up_bytes: m.quota_up_bytes,
        quota_down_bytes: m.quota_down_bytes,
        rate_up_mbps: m.rate_up_mbps,
        rate_down_mbps: m.rate_down_mbps,
        expires_at_unix: m.expires_at_unix,
        ports: m
            .nodes
            .iter()
            .map(|n| GatewayPort {
                public: n.public_port,
                target: n.internal_port,
                proto: n.transport.clone(),
            })
            .collect(),
    }
}

async fn ensure_gateway_active(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    gw_unit: &str,
) -> Result<(), String> {
    let out = run_cmd(
        sess,
        cancel,
        "确认",
        &format!("systemctl is-active {gw_unit}"),
        PROBE_TIMEOUT,
    )
    .await?;
    if out.stdout.trim() != "active" {
        return Err(format!(
            "网关 {gw_unit} 未进入 active 状态（systemctl：{}）",
            out.stdout.trim()
        ));
    }
    Ok(())
}
