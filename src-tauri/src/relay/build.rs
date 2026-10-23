//! 建链/拆链执行器（job runner）：SSH 全序列 —— 探测/安装 → 密钥 → 配置渲染
//! 与段级合并 → 启动 → 握手等待 → 收敛 → 写回；拆链为对称逆过程。
//! 输入：LinkBuildReq/LinkTeardownReq；输出：进度事件（kind=link）与规则行状态。
//! WG 私钥两端服务器上 wg genkey 生成，本进程不落库、不进事件与错误文本；
//! 凭据经 servers::load_auth 解密即用即弃。依赖 relay::config 渲染与
//! relay::mod 解析函数、servers::deploy 命令 helper。

use crate::db::repos;
use crate::domain::{JobProgress, LinkBuildReq, LinkTeardownReq, RelayRule};
use crate::jobs::EmitFn;
use crate::relay::config::{
    alloc_subnet, dnat_plan, interface_private_key, masquerade_pair, merge_relay_conf,
    peer_section_pubkey, remove_peer_section, render_dnat_pair, render_landing_conf,
};
use crate::relay::resolve_forward_mode;
use crate::relay::{
    parse_latest_handshakes, RelayError, STATUS_TIMEOUT, WG_BAK_PATH, WG_CONF_PATH, WG_IFACE,
};
use crate::servers::deploy::{
    port_probe_cmd, read_remote_file, run_cmd, run_cmd_ok, write_remote_file, RemoteFile,
    CONFIG_TIMEOUT, INSTALL_TIMEOUT, SS_CONFIG_PATH, VERIFY_TIMEOUT, XRAY_CONFIG_PATH,
};
use crate::servers::{load_auth, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use rusqlite::Connection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 收敛/恢复：把 Xray/SS 配置的监听地址改为 target（"0.0.0.0" 即恢复）。
/// 支持本工具部署的 inbounds 数组形态与单 inbound 对象形态。
pub fn rewrite_listen(protocol: &str, cfg_json: &str, target: &str) -> Result<String, String> {
    let mut v: serde_json::Value =
        serde_json::from_str(cfg_json).map_err(|e| format!("配置解析失败: {e}"))?;
    match protocol {
        "vless" | "vmess" | "trojan" => {
            if let Some(inbound) = v.get_mut("inbound") {
                inbound["listen"] = serde_json::json!(target);
            } else if let Some(inbounds) = v.get_mut("inbounds").and_then(|s| s.as_array_mut()) {
                let first = inbounds.first_mut().ok_or("缺少 inbounds 项")?;
                first["listen"] = serde_json::json!(target);
            } else {
                return Err("缺少 inbound".into());
            }
        }
        "ss" => {
            let servers = v
                .get_mut("servers")
                .and_then(|s| s.as_array_mut())
                .ok_or("缺少 servers")?;
            servers[0]["server"] = serde_json::json!(target);
        }
        other => return Err(format!("不支持的协议: {other}")),
    }
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

/// 建链主线（worker 调用）。失败时规则行落 failed（行不存在则先建行）。
pub async fn run_build(
    state: &AppState,
    req: LinkBuildReq,
    cancel: &Arc<AtomicBool>,
    emit: &EmitFn,
) -> Result<(), String> {
    validate_build_input(&req)?;
    let rule = load_or_create_rule(state, &req).map_err(|e| e.to_string())?;
    let rule_id = rule.id;
    let emit_progress = |step: &str, message: &str, progress: i64| {
        emit(JobProgress {
            job_id: req.job_id,
            kind: "link".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    let result = build_main(state, &req, &rule, cancel, &emit_progress).await;
    if let Err(e) = &result {
        // 失败语义：行落 failed（仅对已建行；装载阶段的校验错误不动状态）
        let _ = repos::relay::update_status(&state.db.lock(), rule_id, "failed");
        return Err(e.clone());
    }
    Ok(())
}

fn validate_build_input(req: &LinkBuildReq) -> Result<(), String> {
    if !matches!(req.client_mode.as_str(), "forward" | "peer") {
        return Err(
            RelayError::Validation("client_mode 只能是 forward 或 peer".into()).to_string(),
        );
    }
    if req.client_mode == "forward" && req.forward_port.is_none() {
        return Err(RelayError::Validation("forward 模式必须提供 forward_port".into()).to_string());
    }
    for p in [Some(req.wg_port), req.forward_port, Some(req.landing_port)]
        .into_iter()
        .flatten()
    {
        if !(1..=65535).contains(&p) {
            return Err(RelayError::Validation("端口必须在 1-65535".into()).to_string());
        }
    }
    if let Some(m) = req.forward_mode.as_deref() {
        if !matches!(m, "single" | "same_port") {
            return Err(RelayError::Validation(format!(
                "forward_mode 只能是 single 或 same_port：{m}"
            ))
            .to_string());
        }
    }
    Ok(())
}

/// 装载规则行：不存在则建（网段 alloc_subnet、wg_ip 派生、status=building）；
/// 已存在（上次失败/接管）则复用并置 building；rebuild 要求已存在且 built/imported。
fn load_or_create_rule(state: &AppState, req: &LinkBuildReq) -> Result<RelayRule, RelayError> {
    let conn = state.db.lock();
    // 按 (落地机, 落地端口) 找行：一台落地机上可以有多条链路（一个服务一条），
    // 只按落地机找会在多链路时随手挑一条，重建就改错服务
    let existing =
        repos::relay::find_by_landing_port(&conn, req.landing_server_id, req.landing_port as i64)
            .map_err(|e| RelayError::Db(e.to_string()))?;
    let rule = match existing {
        Some(r) => {
            // 扩展端口是「接管链路 + 额外 DNAT」的记账行，WireGuard 配置不属于本工具；
            // 重建会落到 wg0 全序列，既可能冲掉手工隧道，也会把扩展行的语义改坏。
            if r.forward_mode == "extended" {
                return Err(RelayError::Validation(
                    "扩展端口链路不能重建；请在链路页「移除端口」，或新建一条工具管理的链路".into(),
                ));
            }
            // 扩展端口挂在当前隧道上。整条重建会落到 wg0 全序列，可能冲掉手工隧道，
            // 那些 DNAT 也会指向一个不存在的接口——必须先逐条移除。
            let has_extended = repos::relay::list(&conn)
                .map_err(|e| RelayError::Db(e.to_string()))?
                .iter()
                .any(|x| {
                    x.forward_mode == "extended"
                        && x.relay_server_id == r.relay_server_id
                        && x.landing_server_id == r.landing_server_id
                });
            if has_extended {
                return Err(RelayError::Validation(
                    "该链路下还有扩展端口：请先在链路页逐一「移除端口」，再重建链路".into(),
                ));
            }
            if req.action == "build" && r.status == "built" {
                return Err(RelayError::LinkBuilt);
            }
            if req.action == "rebuild" && !matches!(r.status.as_str(), "built" | "imported") {
                return Err(RelayError::Validation("重建要求链路已建立或已接管".into()));
            }
            repos::relay::update_status(&conn, r.id, "building")
                .map_err(|e| RelayError::Db(e.to_string()))?;
            // 重建可以改语义（single ↔ same_port），但**只在请求明说时**才改：
            // 不带 forward_mode 的调用方（重试、老前端）沿用原粒度，不静默降级
            let mode = resolve_forward_mode(Some(&r.forward_mode), req.forward_mode.as_deref());
            repos::relay::set_forward_mode(&conn, r.id, &mode)
                .map_err(|e| RelayError::Db(e.to_string()))?;
            // 模式与端口也按请求改写：重建可以把模式 B 改成模式 A（或反过来），
            // 行不改写的话后续判定（路由目标、挂载原因、UI 提示）全都看的是旧语义
            let fwd_port = req.forward_port.map(|p| p as i64);
            repos::relay::set_mode_and_ports(
                &conn,
                r.id,
                &req.client_mode,
                fwd_port,
                req.landing_port as i64,
            )
            .map_err(|e| RelayError::Db(e.to_string()))?;
            RelayRule {
                client_mode: req.client_mode.clone(),
                forward_port: fwd_port,
                landing_port: req.landing_port as i64,
                forward_mode: mode,
                ..r
            }
        }
        None => {
            if req.action == "rebuild" {
                return Err(RelayError::Validation("重建要求链路已存在".into()));
            }
            // 工具建的链路独占落地机的 wg0.conf（整份渲染覆盖写）：一台落地机只能
            // 建一条，否则第二条会把第一条的落地配置冲掉。接管来的链路不写服务器
            // 配置，不在此列——「一台机器多个服务」靠它们共存
            if repos::relay::has_built_link(&conn, req.landing_server_id)
                .map_err(|e| RelayError::Db(e.to_string()))?
            {
                return Err(RelayError::Validation(
                    "该落地机已有本工具建立的链路（它独占落地机配置）；其余服务请用「接管」接入既有链路".into(),
                ));
            }
            let taken: Vec<String> = repos::relay::list(&conn)
                .map_err(|e| RelayError::Db(e.to_string()))?
                .into_iter()
                .map(|r| r.subnet)
                .collect();
            let subnet = alloc_subnet(&taken);
            let prefix = subnet.trim_end_matches(".0/24");
            let rule = RelayRule {
                id: 0,
                name: req.name.clone(),
                relay_server_id: req.relay_server_id,
                landing_server_id: req.landing_server_id,
                client_mode: req.client_mode.clone(),
                wg_port: req.wg_port as i64,
                forward_port: req.forward_port.map(|p| p as i64),
                landing_port: req.landing_port as i64,
                subnet: subnet.clone(),
                relay_wg_ip: format!("{prefix}.1"),
                landing_wg_ip: format!("{prefix}.2"),
                relay_pubkey: None,
                landing_pubkey: None,
                converged: req.converged,
                status: "building".into(),
                forward_mode: resolve_forward_mode(None, req.forward_mode.as_deref()),
            };
            let id =
                repos::relay::insert(&conn, &rule).map_err(|e| RelayError::Db(e.to_string()))?;
            RelayRule { id, ..rule }
        }
    };
    Ok(rule)
}

async fn build_main(
    state: &AppState,
    req: &LinkBuildReq,
    rule: &RelayRule,
    cancel: &Arc<AtomicBool>,
    emit_progress: &(dyn Fn(&str, &str, i64) + Sync),
) -> Result<(), String> {
    // 2. 服务器装载与类型校验
    let (relay_srv, landing_srv) = {
        let conn = state.db.lock();
        let relay_srv = repos::servers::get(&conn, rule.relay_server_id)
            .map_err(|e| e.to_string())?
            .ok_or("中转服务器不存在")?;
        let landing_srv = repos::servers::get(&conn, rule.landing_server_id)
            .map_err(|e| e.to_string())?
            .ok_or("落地服务器不存在")?;
        (relay_srv, landing_srv)
    };
    if relay_srv.kind != "relay" || landing_srv.kind != "landing" {
        return Err(RelayError::Validation(
            "服务器类型不符：中转机须为 relay、落地机须为 landing".into(),
        )
        .to_string());
    }
    let (relay_auth, _) = load_auth(state, rule.relay_server_id).map_err(|e| e.to_string())?;
    let (landing_auth, _) = load_auth(state, rule.landing_server_id).map_err(|e| e.to_string())?;
    emit_progress("探测", "连接服务器", 10);
    let mut relay = SshSession::connect(
        &relay_srv.host,
        relay_srv.port as u16,
        &relay_srv.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    let mut landing = SshSession::connect(
        &landing_srv.host,
        landing_srv.port as u16,
        &landing_srv.user,
        &landing_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    check_cancel(cancel)?;

    // 3. 探测：modprobe + wg 工具链（缺失则安装）+ 出口网卡 + forward 模式端口占用
    ensure_wg_ready(&mut relay, cancel, "中转机").await?;
    ensure_wg_ready(&mut landing, cancel, "落地机").await?;
    let egress_if = run_cmd_ok(
        &mut relay,
        cancel,
        "探测",
        "ip route get 8.8.8.8 | awk '{for(i=1;i<=NF;i++) if($i==\"dev\"){print $(i+1); exit}}'",
        PROBE_TIMEOUT,
    )
    .await?
    .stdout
    .trim()
    .to_string();
    if egress_if.is_empty() {
        return Err("无法确定中转机出口网卡".into());
    }
    // 只判空不够：这个值随后进 iptables 命令与 wg0.conf 的 PostUp（wg-quick 以 root 执行），
    // 而它是远端 awk 的输出——SSH 被中间人接管时就是一条 root 命令注入路径（#10）。
    if !valid_iface_name(&egress_if) {
        return Err(format!(
            "中转机出口网卡名不可信，已中止：{egress_if:?}（只允许字母数字、连字符、下划线，≤15 字符）"
        ));
    }
    if req.client_mode == "forward" {
        let fp = req.forward_port.unwrap_or(443);
        let busy = run_cmd(
            &mut relay,
            cancel,
            "探测",
            &port_probe_cmd(fp),
            PROBE_TIMEOUT,
        )
        .await?
        .exit_code
            == 0;
        if busy {
            return Err(RelayError::PortInUse { port: fp }.to_string());
        }
    }
    check_cancel(cancel)?;

    // 4. 密钥：读回两端既有 conf；rebuild 复用既有私钥（不轮换——私钥更换会
    //    使同中转机其他链路与已导出设备配置全部失效），仅缺失时 wg genkey。
    //    读失败一律 `?` 中止：把读失败当成「没有旧配置」，正是密钥轮换与整文件覆盖的入口。
    emit_progress("密钥", "读取并准备 WireGuard 密钥", 40);
    let old_relay = read_conf(&mut relay, cancel).await?;
    let old_landing = read_conf(&mut landing, cancel).await?;
    let relay_privkey = match interface_private_key(old_relay.content()) {
        Some(k) => k,
        None => gen_wg_key(&mut relay, cancel).await?,
    };
    let relay_pubkey = derive_wg_pubkey(&mut relay, cancel, &relay_privkey).await?;
    let landing_privkey = match interface_private_key(old_landing.content()) {
        Some(k) => k,
        None => gen_wg_key(&mut landing, cancel).await?,
    };
    let landing_pubkey = derive_wg_pubkey(&mut landing, cancel, &landing_privkey).await?;
    if req.action == "rebuild" {
        // 备份旧 conf；文件不存在（cp 非零退出）跳过且不报错
        backup_conf(&mut relay, cancel).await?;
        backup_conf(&mut landing, cancel).await?;
    }

    // 5. 配置：中转侧段级合并（Interface 保留、本链规则行替换、他链/设备段保留），
    //    落地侧整份渲染（本工具独有，两端私钥均复用）
    emit_progress("配置", "渲染并下发 wg0.conf", 60);
    // 本次链路的完整转发计划：single 一对；same_port 是整机——该落地机每个自建节点
    // 端口按**原端口号**转发。配置（PostUp，重启后自动恢复的那份）与随后的幂等
    // iptables 命令共用这一份，避免只把主端口写进配置。
    let nat_ports: Vec<(u16, u16)> = if rule.forward_mode == "same_port" {
        let plan = same_port_plan(state, &landing_srv.host);
        if plan.is_empty() {
            return Err(format!(
                "整机转发没有可转的端口：{} 上还没有已部署的自建节点",
                landing_srv.host
            ));
        }
        plan
    } else {
        dnat_plan(rule)
    };
    let relay_conf = merge_relay_conf(
        rule,
        old_relay.content(),
        &relay_privkey,
        &landing_pubkey,
        &egress_if,
        &nat_ports,
    );
    let landing_conf = render_landing_conf(rule, &landing_privkey, &relay_pubkey, &relay_srv.host);
    write_remote_file(
        &mut relay,
        cancel,
        "配置",
        WG_CONF_PATH,
        &relay_conf,
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        &mut landing,
        cancel,
        "配置",
        WG_CONF_PATH,
        &landing_conf,
        CONFIG_TIMEOUT,
    )
    .await?;
    check_cancel(cancel)?;

    // 6. 启动：两端 wg-quick + 中转转发；forward 模式显式幂等执行 DNAT 增
    emit_progress("启动", "启动 wg-quick 与转发", 85);
    start_wg(&mut relay, cancel, "中转机").await?;
    start_wg(&mut landing, cancel, "落地机").await?;
    run_cmd_ok(
        &mut relay,
        cancel,
        "启动",
        "sysctl -w net.ipv4.ip_forward=1 && mkdir -p /etc/sysctl.d && echo 'net.ipv4.ip_forward=1' > /etc/sysctl.d/99-vpn-ctl.conf",
        CONFIG_TIMEOUT,
    )
    .await?;
    if req.client_mode == "forward" {
        // 计划在「配置」那一步已经算好：配置里有什么，这里就补什么（幂等下发）
        for (fwd_port, tgt_port) in &nat_ports {
            let (ups, _downs) =
                render_dnat_pair(&egress_if, *fwd_port, &rule.landing_wg_ip, *tgt_port);
            for rule_text in &ups {
                let idem = format!(
                    "{} 2>/dev/null || {}",
                    rule_text.replace("-A", "-C"),
                    rule_text
                );
                run_cmd_ok(&mut relay, cancel, "启动", &idem, CONFIG_TIMEOUT).await?;
            }
        }
        // 回程伪装同样幂等补一条：只改目的地址的 DNAT 回包出不去（见 masquerade_pair）
        let (m_up, _m_down) = masquerade_pair(WG_IFACE);
        let idem = format!("{} 2>/dev/null || {}", m_up.replace("-A", "-C"), m_up);
        run_cmd_ok(&mut relay, cancel, "启动", &idem, CONFIG_TIMEOUT).await?;
    }
    check_cancel(cancel)?;

    // 7. 握手等待：ping 触发 + latest-handshakes 轮询（≤30s，每 2s）
    emit_progress("握手", "等待握手建立", 90);
    if let Err(e) = wait_handshake(&mut relay, cancel, &landing_pubkey, &rule.landing_wg_ip).await {
        // 取消原样上抛；真超时则补一次现场取证——「握手超时」四个字会把用户引向
        // 分流/防火墙，而真相往往是启动那一步其实没成，或对面根本没回包
        if !e.contains("握手超时") {
            return Err(e);
        }
        let why = handshake_failure_reason(&mut relay, &mut landing, cancel).await;
        return Err(format!("{e}：{why}"));
    }

    // 8. 收敛（双向对账）：converged=true 改指 WG 内网；rebuild 由开转关
    //    （!converged 且行旧值 converged=1）时恢复监听 —— 否则行 converged=0
    //    后拆链跳过恢复，服务永久绑在已不存在的 WG 内网地址
    if req.converged {
        emit_progress("收敛", "收敛落地机监听地址", 95);
        converge_listen(state, &mut landing, cancel, &landing_srv, rule).await?;
    } else if req.action == "rebuild" && rule.converged {
        emit_progress("收敛", "恢复落地机监听地址", 95);
        converge_restore(state, &mut landing, cancel, rule).await?;
    }

    // 9. 写回：状态 + 收敛标记 + 节点改指 + deploys 记录（无凭据）
    emit_progress("写回", "写回链路状态与节点", 100);
    {
        // 锁作用域：await 前释放，避免 MutexGuard 跨 await 持有
        let conn = state.db.lock();
        repos::relay::update_status(&conn, rule.id, "built").map_err(|e| e.to_string())?;
        repos::relay::set_converged(&conn, rule.id, req.converged).map_err(|e| e.to_string())?;
        // 公钥写回：状态查询/拆链按公钥定位 peer，不依赖接口名
        repos::relay::set_pubkeys(&conn, rule.id, Some(&relay_pubkey), Some(&landing_pubkey))
            .map_err(|e| e.to_string())?;
        let nodes = crate::relay::landing_nodes_for_rule(&conn, &landing_srv, rule, &state.creds)
            .map_err(|e| e.to_string())?;
        for n in nodes {
            // 只落标记，**不改地址**（同 reroute_landing_nodes 的说明）。生效地址由
            // crate::config::effective_node 在生成侧一处算出，这里再写一遍就是第二份
            // 真相：转发模式（single/same_port）与 client_mode（forward/wg）的组合
            // 各自算一遍，总有对不上的时候。
            repos::nodes::set_relay(&conn, &n.id, Some(rule.id)).map_err(|e| e.to_string())?;
        }
        let summary = serde_json::json!({
            "kind": "link",
            "client_mode": rule.client_mode,
            "wg_port": rule.wg_port,
            "forward_port": rule.forward_port,
            "landing_port": rule.landing_port,
            "subnet": rule.subnet,
        })
        .to_string();
        repos::deploys::insert(&conn, rule.relay_server_id, "link", &summary, "ok")
            .map_err(|e| e.to_string())?;
    }
    // 节点改指后重建合并配置缓存（导出/订阅服务立即生效）
    state
        .subs
        .rebuild_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 拆链主线：两端 down → 中转 DNAT 删 + 段移除 → 收敛恢复 → 节点直连恢复 → not-built。
/// action 分流：unregister = 纯登记解除（仅 imported）；full = 真实拆除
/// （built/failed 走 wg0 全序列；imported 按公钥定位实际接口拆除）。
pub async fn run_teardown(
    state: &AppState,
    req: LinkTeardownReq,
    cancel: &Arc<AtomicBool>,
    emit: &EmitFn,
) -> Result<(), String> {
    if !matches!(req.action.as_str(), "unregister" | "full" | "port") {
        return Err(RelayError::Validation(format!("未知拆链动作: {}", req.action)).to_string());
    }
    let rule = {
        let conn = state.db.lock();
        repos::relay::get(&conn, req.rule_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| RelayError::NotFound.to_string())?
    };
    // 扩展端口只删 DNAT 与登记行，绝不碰 WireGuard；它必须走专用分支，
    // 否则会落进 imported 的「拆除接口」，把用户的 wg1 整条拆掉。
    if rule.forward_mode == crate::relay::extend::EXTENDED_MODE || req.action == "port" {
        return crate::relay::extend::remove_extended_port(state, &req, &rule, cancel, emit).await;
    }
    if rule.status == "not-built" {
        return Err(RelayError::LinkNotBuilt.to_string());
    }
    if rule.status == "building" {
        return Err(RelayError::Validation("链路正在建链，请稍后".into()).to_string());
    }
    // 接管（imported）的链路接口非本工具所建：默认拆链为纯登记解除，不发任何 SSH；
    // action=full 时按公钥定位实际接口做真实拆除
    if rule.status == "imported" {
        return match req.action.as_str() {
            "unregister" => teardown_imported(state, req, &rule, emit).await,
            _ => teardown_full_imported(state, req, &rule, cancel, emit).await,
        };
    }
    if req.action == "unregister" {
        return Err(
            RelayError::Validation("仅接管链路可解除登记；自建链路请用拆链".into()).to_string(),
        );
    }
    let emit_progress = |step: &str, message: &str, progress: i64| {
        emit(JobProgress {
            job_id: req.job_id,
            kind: "link".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    // 两端装载：任一端失败即 Err，规则行状态保持不变（幂等重试语义）
    emit_progress("拆链", "连接服务器", 10);
    let (relay_srv, landing_srv) = {
        let conn = state.db.lock();
        let relay_srv = repos::servers::get(&conn, rule.relay_server_id)
            .map_err(|e| e.to_string())?
            .ok_or("中转服务器不存在")?;
        let landing_srv = repos::servers::get(&conn, rule.landing_server_id)
            .map_err(|e| e.to_string())?
            .ok_or("落地服务器不存在")?;
        (relay_srv, landing_srv)
    };
    let (relay_auth, _) = load_auth(state, rule.relay_server_id).map_err(|e| e.to_string())?;
    let (landing_auth, _) = load_auth(state, rule.landing_server_id).map_err(|e| e.to_string())?;
    let mut relay = SshSession::connect(
        &relay_srv.host,
        relay_srv.port as u16,
        &relay_srv.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    let mut landing = SshSession::connect(
        &landing_srv.host,
        landing_srv.port as u16,
        &landing_srv.user,
        &landing_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;

    // 中转：down（已拆除视为成功）→ 显式 DNAT 删 → 段移除 → 空 conf 删除/非空写回
    emit_progress("拆链", "拆除中转机接口", 40);
    teardown_wg_down(&mut relay, cancel).await?;
    if rule.client_mode == "forward" {
        remove_dnat_rules(&mut relay, cancel, &rule).await?;
    }
    // 读失败即中止（`?`）：否则 `old_conf` 会被当成空串，走到下面「已无任何 peer」
    // 的分支 `rm -f wg0.conf`，把同机上其它链路的配置整个删掉。
    let old_conf = read_conf(&mut relay, cancel).await?;
    let old_conf = old_conf.content();
    // 落地段定位：新格式 landing_wg_ip/32；旧格式（前版）peer 模式落地段为 subnet/24
    let relay_drop = format!("{}/32", rule.landing_wg_ip);
    let landing_pubkey = peer_section_pubkey(old_conf, &relay_drop)
        .or_else(|| peer_section_pubkey(old_conf, &rule.subnet));
    let updated = landing_pubkey
        .map(|pubkey| remove_peer_section(old_conf, &pubkey))
        .unwrap_or_else(|| old_conf.to_string());
    if updated.lines().any(|l| l.trim() == "[Peer]") {
        write_remote_file(
            &mut relay,
            cancel,
            "拆链",
            WG_CONF_PATH,
            &updated,
            CONFIG_TIMEOUT,
        )
        .await?;
    } else {
        // 已无任何 peer：整机配置删除（接口已 down）
        run_cmd_ok(
            &mut relay,
            cancel,
            "拆链",
            &format!("rm -f {WG_CONF_PATH}"),
            CONFIG_TIMEOUT,
        )
        .await?;
    }

    // 落地：down + 整机配置删除（含 rebuild 备份）
    emit_progress("拆链", "拆除落地机接口", 60);
    teardown_wg_down(&mut landing, cancel).await?;
    run_cmd_ok(
        &mut landing,
        cancel,
        "拆链",
        &format!("rm -f {WG_CONF_PATH} {WG_BAK_PATH}"),
        CONFIG_TIMEOUT,
    )
    .await?;

    // 收敛恢复：监听地址回 0.0.0.0
    if rule.converged {
        emit_progress("拆链", "恢复落地机监听地址", 80);
        converge_restore(state, &mut landing, cancel, &rule).await?;
    }

    // 节点恢复：读 params_json.direct 回写直连地址，解除 relay_rule_id 关联
    emit_progress("拆链", "恢复节点直连地址", 90);
    {
        // 锁作用域：await 前释放，避免 MutexGuard 跨 await 持有
        let conn = state.db.lock();
        clear_device_rows(&conn, rule.id).map_err(|e| e.to_string())?;
        restore_nodes_direct(&conn, rule.id, &state.creds).map_err(|e| e.to_string())?;
        repos::relay::update_status(&conn, rule.id, "not-built").map_err(|e| e.to_string())?;
    }
    state
        .subs
        .rebuild_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 接管链路的拆链：纯登记解除——接口非本工具所建，不发任何 SSH；
/// 对称于 adopt 的纯 DB 语义：清设备行 → 节点回指直连 → status=not-built。
async fn teardown_imported(
    state: &AppState,
    req: LinkTeardownReq,
    rule: &RelayRule,
    emit: &EmitFn,
) -> Result<(), String> {
    let emit_progress = |step: &str, message: &str, progress: i64| {
        emit(JobProgress {
            job_id: req.job_id,
            kind: "link".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    emit_progress("拆链", "解除登记（接口非本工具所建，不修改服务器）", 10);
    {
        let conn = state.db.lock();
        clear_device_rows(&conn, rule.id).map_err(|e| e.to_string())?;
        restore_nodes_direct(&conn, rule.id, &state.creds).map_err(|e| e.to_string())?;
        repos::relay::update_status(&conn, rule.id, "not-built").map_err(|e| e.to_string())?;
    }
    emit_progress("拆链", "登记已解除", 100);
    state
        .subs
        .rebuild_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 接管链路的真实拆除（action=full）：按公钥定位实际接口（不假设 wg0），
/// 删两端 peer 段（无剩余 peer 则 down+删 conf）→ 中转 DNAT 按行幂等删 →
/// 收敛恢复（仅有关联节点时）→ DB 复位。接口名来自远端输出，进路径前白名单校验。
async fn teardown_full_imported(
    state: &AppState,
    req: LinkTeardownReq,
    rule: &RelayRule,
    cancel: &Arc<AtomicBool>,
    emit: &EmitFn,
) -> Result<(), String> {
    // 公钥定位是安全前提：缺公钥（历史行为）不猜接口名，拒绝并要求走解除登记
    let relay_peer_pk = rule
        .landing_pubkey
        .as_deref()
        .ok_or("规则缺少落地公钥，无法定位 peer（历史行为请用解除登记）".to_string())?;
    let landing_peer_pk = rule
        .relay_pubkey
        .as_deref()
        .ok_or("规则缺少中转公钥，无法定位 peer（历史行为请用解除登记）".to_string())?;
    let emit_progress = |step: &str, message: &str, progress: i64| {
        emit(JobProgress {
            job_id: req.job_id,
            kind: "link".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    emit_progress("拆除", "连接服务器", 10);
    let (relay_srv, landing_srv) = {
        let conn = state.db.lock();
        let relay_srv = repos::servers::get(&conn, rule.relay_server_id)
            .map_err(|e| e.to_string())?
            .ok_or("中转服务器不存在")?;
        let landing_srv = repos::servers::get(&conn, rule.landing_server_id)
            .map_err(|e| e.to_string())?
            .ok_or("落地服务器不存在")?;
        (relay_srv, landing_srv)
    };
    let (relay_auth, _) = load_auth(state, rule.relay_server_id).map_err(|e| e.to_string())?;
    let (landing_auth, _) = load_auth(state, rule.landing_server_id).map_err(|e| e.to_string())?;
    let mut relay = SshSession::connect(
        &relay_srv.host,
        relay_srv.port as u16,
        &relay_srv.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    let mut landing = SshSession::connect(
        &landing_srv.host,
        landing_srv.port as u16,
        &landing_srv.user,
        &landing_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    check_cancel(cancel)?;

    // 中转：按落地公钥定位接口 → 删 peer 段 → 模式 A 删 DNAT
    emit_progress("拆除", "拆除中转机 peer 段", 40);
    let relay_iface = find_iface_by_peer(&mut relay, cancel, relay_peer_pk)
        .await?
        .ok_or("中转机未找到对端公钥所在的 WireGuard 接口")?;
    remove_peer_and_sync(&mut relay, cancel, &relay_iface, relay_peer_pk).await?;
    if rule.client_mode == "forward" {
        remove_dnat_rules(&mut relay, cancel, rule).await?;
    }

    // 落地：按中转公钥定位接口 → 删 peer 段 → 收敛恢复（仅有关联节点时）
    emit_progress("拆除", "拆除落地机 peer 段", 60);
    let landing_iface = find_iface_by_peer(&mut landing, cancel, landing_peer_pk)
        .await?
        .ok_or("落地机未找到对端公钥所在的 WireGuard 接口")?;
    remove_peer_and_sync(&mut landing, cancel, &landing_iface, landing_peer_pk).await?;
    if rule.converged {
        let has_nodes = !repos::nodes::find_by_rule(&state.db.lock(), rule.id, &state.creds)
            .map_err(|e| e.to_string())?
            .is_empty();
        if has_nodes {
            emit_progress("拆除", "恢复落地机监听地址", 80);
            converge_restore(state, &mut landing, cancel, rule).await?;
        }
    }

    // DB 复位（与解除登记同语义）
    emit_progress("拆除", "复位登记", 90);
    {
        let conn = state.db.lock();
        clear_device_rows(&conn, rule.id).map_err(|e| e.to_string())?;
        restore_nodes_direct(&conn, rule.id, &state.creds).map_err(|e| e.to_string())?;
        repos::relay::update_status(&conn, rule.id, "not-built").map_err(|e| e.to_string())?;
    }
    emit_progress("拆除", "拆除完成", 100);
    state
        .subs
        .rebuild_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 接口名白名单：远端输出拼接进文件路径/命令前校验（防路径穿越与命令注入）。
///
/// 长度上限 15 是内核 IFNAMSIZ-1。字符集比内核实际允许的更严（内核还允许 `.`、`@`
/// 之类），但本模块只处理 `ip route` 报出来的物理/veth 名——宁可把罕见名字判不合法
/// 而报错，也不放元字符进来：这个值会进 iptables 命令与 `wg0.conf` 的 `PostUp`，
/// 而后者由 wg-quick 以 root 执行。
pub(crate) fn valid_iface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 枚举 WG 接口并按 peer 公钥定位所在接口（接口名不假设 wg0）。
pub(crate) async fn find_iface_by_peer(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    peer_pubkey: &str,
) -> Result<Option<String>, String> {
    let out = run_cmd(sess, cancel, "拆除", "wg show interfaces", STATUS_TIMEOUT).await?;
    if out.exit_code != 0 {
        return Ok(None);
    }
    for iface in out.stdout.split_whitespace() {
        let hs = run_cmd(
            sess,
            cancel,
            "拆除",
            &format!("wg show {iface} latest-handshakes"),
            STATUS_TIMEOUT,
        )
        .await?;
        if hs.exit_code == 0 && parse_latest_handshakes(&hs.stdout).contains_key(peer_pubkey) {
            return Ok(Some(iface.to_string()));
        }
    }
    Ok(None)
}

/// 删指定接口的 peer 段并热加载：无剩余 [Peer] → down + 删 conf；否则写回 + syncconf。
async fn remove_peer_and_sync(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    iface: &str,
    peer_pubkey: &str,
) -> Result<(), String> {
    if !valid_iface_name(iface) {
        return Err(RelayError::Validation(format!("非法接口名: {iface}")).to_string());
    }
    let conf_path = format!("/etc/wireguard/{iface}.conf");
    let old_conf = match run_cmd(
        sess,
        cancel,
        "拆除",
        &format!("cat {conf_path}"),
        CONFIG_TIMEOUT,
    )
    .await
    {
        Ok(out) if out.exit_code == 0 => out.stdout,
        _ => return Err(format!("读取 {conf_path} 失败")),
    };
    let updated = remove_peer_section(&old_conf, peer_pubkey);
    if updated.lines().any(|l| l.trim() == "[Peer]") {
        write_remote_file(sess, cancel, "拆除", &conf_path, &updated, CONFIG_TIMEOUT).await?;
        run_cmd_ok(
            sess,
            cancel,
            "拆除",
            &format!("wg syncconf {iface} {conf_path}"),
            CONFIG_TIMEOUT,
        )
        .await?;
    } else {
        run_cmd_ok(
            sess,
            cancel,
            "拆除",
            &format!("wg-quick down {iface}"),
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            sess,
            cancel,
            "拆除",
            &format!("rm -f {conf_path}"),
            CONFIG_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

/// 级联清理设备行：拆链后设备已无隧道，行随之删除；否则规则行被
/// device_peers 的 FK 引用锁死，删除规则报错（I3）。
fn clear_device_rows(conn: &Connection, rule_id: i64) -> rusqlite::Result<()> {
    let devices = repos::device_peers::list_by_rule(conn, rule_id)?;
    for d in devices {
        repos::device_peers::delete(conn, d.id)?;
    }
    Ok(())
}

/// 节点回指：读 params_json.direct 回写直连地址，解除 relay_rule_id 关联
/// （拆链、接管解除与「单点开关关闭中转」共用；无 direct 记录的节点保持原样）。
///
/// 逐节点走 [`repos::nodes::restore_direct`]：还原口径只有一处实现，免得拆链与单点
/// 开关对「direct 缺失时怎么办」给出不同答案。
fn restore_nodes_direct(
    conn: &Connection,
    rule_id: i64,
    creds: &crate::crypto::CredentialService,
) -> rusqlite::Result<()> {
    for n in repos::nodes::find_by_rule(conn, rule_id, creds)? {
        repos::nodes::restore_direct(conn, &n.id, creds)?;
    }
    Ok(())
}

/// 单端 WG 就绪：modprobe wireguard（失败 → WgNotSupported）→ wg 缺失则 apt 安装。
async fn ensure_wg_ready(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    side: &str,
) -> Result<(), String> {
    let modprobe = run_cmd(sess, cancel, "探测", "modprobe wireguard", PROBE_TIMEOUT).await?;
    if modprobe.exit_code != 0 {
        return Err(RelayError::WgNotSupported(format!("{side} 内核不支持 WireGuard")).to_string());
    }
    let have_wg = run_cmd(sess, cancel, "探测", "command -v wg", PROBE_TIMEOUT)
        .await?
        .exit_code
        == 0;
    if !have_wg {
        run_cmd_ok(
            sess,
            cancel,
            "安装",
            "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq wireguard-tools",
            INSTALL_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

async fn gen_wg_key(sess: &mut SshSession, cancel: &AtomicBool) -> Result<String, String> {
    let out = run_cmd_ok(sess, cancel, "密钥", "wg genkey", PROBE_TIMEOUT).await?;
    let key = out.stdout.trim().to_string();
    if key.is_empty() {
        return Err("wg genkey 无输出".into());
    }
    Ok(key)
}

/// 公钥推导：私钥经管道回送同一会话，不落日志。
async fn derive_wg_pubkey(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    privkey: &str,
) -> Result<String, String> {
    let out = run_cmd_ok(
        sess,
        cancel,
        "密钥",
        &format!("echo '{privkey}' | wg pubkey"),
        PROBE_TIMEOUT,
    )
    .await?;
    let key = out.stdout.trim().to_string();
    if key.is_empty() {
        return Err("wg pubkey 无输出".into());
    }
    Ok(key)
}

/// 备份旧 conf：文件不存在（cp 非零退出）跳过不报错；命令级错误（超时）仍传播。
async fn backup_conf(sess: &mut SshSession, cancel: &AtomicBool) -> Result<(), String> {
    let out = run_cmd(
        sess,
        cancel,
        "备份",
        &format!("cp {WG_CONF_PATH} {WG_BAK_PATH}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    if out.exit_code != 0 {
        // 目标不存在属正常（首次建链无旧 conf），跳过
    }
    Ok(())
}

/// 读回远端 conf。**读失败必须上报（Err）**：早先这里把「读失败」与「无文件」都
/// 归成空串，一次瞬时 SSH 抖动或权限被改就会：`interface_private_key` 取不到 →
/// 重新 `wg genkey`（作废该机上**所有**已建链路与已导出设备 peer）→ merge 走整文件
/// 渲染分支（抹掉其它链路的 `[Peer]` 与 DNAT）。全程不报任何错，用户只看到
/// 「链路重建了」。`Absent` 才是真正的首次建链。
async fn read_conf(sess: &mut SshSession, cancel: &AtomicBool) -> Result<RemoteFile, String> {
    read_remote_file(sess, cancel, "配置", WG_CONF_PATH, CONFIG_TIMEOUT).await
}

/// 整机转发的转发计划：该落地机上每个自建节点 → 原端口号转发。
/// 端口集合**派生**自节点表，不额外维护端口清单——节点部署/下架即自动跟随。
fn same_port_plan(state: &AppState, landing_host: &str) -> Vec<(u16, u16)> {
    let nodes =
        match repos::nodes::find_self_built_by_host(&state.db.lock(), landing_host, &state.creds) {
            Ok(n) => n,
            Err(_) => return Vec::new(),
        };
    let mut ports: Vec<u16> = nodes
        .iter()
        .filter(|n| (1..=65535).contains(&n.port))
        .map(|n| n.port as u16)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports.into_iter().map(|p| (p, p)).collect()
}

/// 一侧的 WG 接口此刻在不在（`ip -br link` 非空即算在）。
async fn iface_present(sess: &mut SshSession, cancel: &AtomicBool) -> Result<bool, String> {
    let out = run_cmd(
        sess,
        cancel,
        "探测",
        &format!("ip -br link show {}", WG_IFACE),
        PROBE_TIMEOUT,
    )
    .await?;
    Ok(out.exit_code == 0 && !out.stdout.trim().is_empty())
}

/// unit 状态短摘要（失败提示用）：`ActiveState: active / SubState: exited` 这种。
/// 读不到给「状态未知」——提示里宁可少一句，也不能编。
async fn unit_state(sess: &mut SshSession, cancel: &AtomicBool) -> String {
    let cmd = format!("systemctl show -p ActiveState,SubState wg-quick@{WG_IFACE}");
    match run_cmd(sess, cancel, "启动", &cmd, PROBE_TIMEOUT).await {
        Ok(out) if out.exit_code == 0 => {
            let parts: Vec<String> = out
                .stdout
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            if parts.is_empty() {
                "状态未知".into()
            } else {
                parts.join(" / ")
            }
        }
        _ => "状态未知".into(),
    }
}

/// unit 最近的日志尾部（失败提示用）：wg-quick 自己的报错都在这里。
/// 拿不到（无 journald / 读失败）返回空串，调用方跳过。
async fn unit_log_tail(sess: &mut SshSession, cancel: &AtomicBool) -> String {
    let cmd = format!("journalctl -u wg-quick@{WG_IFACE} -n 3 --no-pager -o cat");
    match run_cmd(sess, cancel, "启动", &cmd, PROBE_TIMEOUT).await {
        Ok(out) if out.exit_code == 0 => out
            .stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" / "),
        _ => String::new(),
    }
}

/// 启一侧的 WG 接口，并**核验接口真的起来了**。
///
/// 为什么不能只看 `systemctl enable --now` 的退出码：wg-quick@.service 是
/// oneshot + RemainAfterExit —— 进程退出后 unit 仍算 active。接口一旦消失
/// （掉线、手工 ip link del、异常退出），systemd 认定「已经激活」，那条命令就是
/// **空操作且返回 0**，接口永远不会再回来。真机现场（2026-09-18，落地机 112）：
/// unit 自 8/29 起一直是 active(exited)、接口早没了，两次建链都栽在这里，而用户
/// 看到的只有一句「握手超时」。
///
/// 所以这里先清僵尸态（停 → 删残留接口 → 清 failed 标记），再 start 强制跑一遍
/// ExecStart，最后核验接口；不在就把 unit 状态与日志尾部一并报出来。
async fn start_wg(sess: &mut SshSession, cancel: &AtomicBool, side: &str) -> Result<(), String> {
    let cmd = format!(
        "systemctl enable wg-quick@{iface} >/dev/null 2>&1 || true; \
         systemctl stop wg-quick@{iface} >/dev/null 2>&1 || true; \
         ip link del {iface} >/dev/null 2>&1 || true; \
         systemctl reset-failed wg-quick@{iface} >/dev/null 2>&1 || true; \
         systemctl start wg-quick@{iface}",
        iface = WG_IFACE
    );
    run_cmd_ok(sess, cancel, "启动", &cmd, CONFIG_TIMEOUT).await?;
    if iface_present(sess, cancel).await? {
        return Ok(());
    }
    let state = unit_state(sess, cancel).await;
    let tail = unit_log_tail(sess, cancel).await;
    let mut msg = format!("{side} {iface} 没起来（unit {state}）", iface = WG_IFACE);
    if !tail.is_empty() {
        msg.push_str(&format!("；日志：{tail}"));
    }
    Err(msg)
}

/// 握手超时的现场取证：两端接口在不在、中转侧那个 peer 收到过回包没有。
/// 目的只有一个——把「握手超时」翻译成能下手的一句话。
async fn handshake_failure_reason(
    relay: &mut SshSession,
    landing: &mut SshSession,
    cancel: &AtomicBool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (sess, side) in [(&mut *relay, "中转机"), (&mut *landing, "落地机")] {
        parts.push(match iface_present(sess, cancel).await {
            Ok(true) => format!("{side}接口在"),
            Ok(false) => format!("{side}接口不在"),
            Err(_) => format!("{side}接口状态未知"),
        });
    }
    if let Ok(out) = run_cmd(
        relay,
        cancel,
        "握手",
        &format!("wg show {} transfer", WG_IFACE),
        PROBE_TIMEOUT,
    )
    .await
    {
        if out.exit_code == 0 {
            let (rx, tx) = parse_transfer_totals(&out.stdout);
            if rx == 0 && tx == 0 {
                parts.push("中转机该 peer 收发均为 0（无回包，对端没应答）".into());
            } else if rx == 0 {
                parts.push(format!("中转机发出过 {tx} 字节但无回包（对端没应答）"));
            }
        }
    }
    parts.join("；")
}

/// `wg show <if> transfer` 输出（`<pubkey>\t<接收>\t<发送>`）的收发合计。
fn parse_transfer_totals(stdout: &str) -> (i64, i64) {
    let mut rx = 0i64;
    let mut tx = 0i64;
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        rx += cols[1].parse::<i64>().unwrap_or(0);
        tx += cols[2].parse::<i64>().unwrap_or(0);
    }
    (rx, tx)
}

/// ping 触发握手并轮询最新握手（至多 30s，每 2s，取消感知）；超时 → HandshakeTimeout。
async fn wait_handshake(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    landing_pubkey: &str,
    landing_wg_ip: &str,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        check_cancel(cancel)?;
        // ping 仅用于触发握手，失败继续
        let _ = run_cmd(
            sess,
            cancel,
            "握手",
            &format!("ping -c1 -W2 {landing_wg_ip}"),
            PROBE_TIMEOUT,
        )
        .await;
        let hs = run_cmd(
            sess,
            cancel,
            "握手",
            &format!("wg show {WG_IFACE} latest-handshakes"),
            PROBE_TIMEOUT,
        )
        .await?;
        if hs.exit_code == 0 {
            let map = parse_latest_handshakes(&hs.stdout);
            if map.get(landing_pubkey).copied().unwrap_or(0) > 0 {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(RelayError::HandshakeTimeout.to_string());
        }
        // 取消感知的 2s 间隔
        for _ in 0..40 {
            if cancel.load(Ordering::SeqCst) {
                return Err("已取消".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// 收敛：把落地机 Xray/SS 服务监听地址改为 WG 内网 IP 并重启验证。
async fn converge_listen(
    state: &AppState,
    sess: &mut SshSession,
    cancel: &AtomicBool,
    landing: &crate::domain::Server,
    rule: &RelayRule,
) -> Result<(), String> {
    let nodes = crate::relay::landing_nodes_for_rule(&state.db.lock(), landing, rule, &state.creds)
        .map_err(|e| e.to_string())?;
    let node = nodes.first().ok_or("落地机没有可收敛的自建节点")?;
    let (cfg_path, service) = match node.protocol.as_str() {
        "vless" | "vmess" | "trojan" => (XRAY_CONFIG_PATH, "xray"),
        "ss" => (SS_CONFIG_PATH, "ssserver"),
        other => return Err(format!("不支持的收敛协议: {other}")),
    };
    let out = run_cmd(
        sess,
        cancel,
        "收敛",
        &format!("cat {cfg_path}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    if out.exit_code != 0 {
        return Err(format!("读取落地服务配置失败（{cfg_path}）"));
    }
    let updated = rewrite_listen(node.protocol.as_str(), &out.stdout, &rule.landing_wg_ip)?;
    write_remote_file(sess, cancel, "收敛", cfg_path, &updated, CONFIG_TIMEOUT).await?;
    run_cmd_ok(
        sess,
        cancel,
        "收敛",
        &format!("systemctl restart {service}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    let listening = run_cmd(
        sess,
        cancel,
        "收敛",
        &format!(
            "ss -tln | grep -q '{}:{}'",
            rule.landing_wg_ip, rule.landing_port
        ),
        VERIFY_TIMEOUT,
    )
    .await?;
    if listening.exit_code != 0 {
        return Err(format!(
            "落地机未监听 {}:{}",
            rule.landing_wg_ip, rule.landing_port
        ));
    }
    Ok(())
}

/// 收敛恢复：监听地址回 0.0.0.0（按 relay_rule_id 定位节点取协议）。
async fn converge_restore(
    state: &AppState,
    sess: &mut SshSession,
    cancel: &AtomicBool,
    rule: &RelayRule,
) -> Result<(), String> {
    let nodes = repos::nodes::find_by_rule(&state.db.lock(), rule.id, &state.creds)
        .map_err(|e| e.to_string())?;
    // 没有挂在链条上的节点 = 没有要恢复的对象（节点先被删掉了），跳过即可。
    // 这里报错会让整条拆链中止在 80%，规则行永远停在 built——界面上留一条两端
    // 都没有接口的幽灵链路（2026-09-18 实测）。
    let Some(node) = nodes.first() else {
        return Ok(());
    };
    let (cfg_path, service) = match node.protocol.as_str() {
        "vless" | "vmess" | "trojan" => (XRAY_CONFIG_PATH, "xray"),
        "ss" => (SS_CONFIG_PATH, "ssserver"),
        other => return Err(format!("不支持的恢复协议: {other}")),
    };
    let out = run_cmd(
        sess,
        cancel,
        "拆链",
        &format!("cat {cfg_path}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    if out.exit_code != 0 {
        return Err(format!("读取落地服务配置失败（{cfg_path}）"));
    }
    let updated = rewrite_listen(node.protocol.as_str(), &out.stdout, "0.0.0.0")?;
    write_remote_file(sess, cancel, "拆链", cfg_path, &updated, CONFIG_TIMEOUT).await?;
    run_cmd_ok(
        sess,
        cancel,
        "拆链",
        &format!("systemctl restart {service}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// wg-quick down：已拆除（stderr 含 "not a WireGuard interface"）视为成功。
async fn teardown_wg_down(sess: &mut SshSession, cancel: &AtomicBool) -> Result<(), String> {
    let out = run_cmd(
        sess,
        cancel,
        "拆链",
        &format!("wg-quick down {WG_IFACE}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    // 容错两种「其实已经拆了」：接口不在（not a WireGuard interface）、
    // conf 文件已被上一次拆除删掉（does not exist）。二者都不是失败——
    // 否则重试卡死在这里，规则行留在 built（2026-09-18 实测）。
    let already_gone =
        out.stderr.contains("not a WireGuard interface") || out.stderr.contains("does not exist");
    if out.exit_code != 0 && !already_gone {
        return Err(RelayError::WgCommandFailed {
            step: "拆链".into(),
            stderr_tail: crate::util::stderr_tail(&out.stderr),
        }
        .to_string());
    }
    Ok(())
}

/// 从 `iptables -S` 的一行里**解析**出重建撤销命令所需的字段，并按本模块自己的规范
/// 形状重建命令——**不整行重放**。
///
/// 为什么必须重建：那一行里的 `--comment` 是任意文本，而 `-S` 会把它原样回显
/// （`--comment "x; id -A PREROUTING"`）。整行重放等于把远端可控的字符串当 root 命令
/// 执行；原实现叠的 `line.replace("-A", …)` 更糟——连注释里的 `-A` 也一起替换，
/// 命令直接被改坏（既不安全也不正确）。重建还有一个好处：`-C`/`-D` 与新增时的
/// 规则文本**逐字一致**，匹配失败导致删不掉（＝端口一直开着）的风险随之消失。
pub(crate) struct ScannedRule {
    /// 目的地址：nat 取 `--to-destination` 的 IP，filter 取 `-d` 的 IP。
    pub dest_ip: String,
    /// **目的端口**（注意不是规则的 `--dport`）：nat 取 `--to-destination` 的端口
    /// （`--dport` 那边是转发入口端口，不是落地端口），filter 取 `--dport`。
    /// 调用方按「落地机 IP + 落地端口」匹配，取的必须是这个。
    pub dest_port: u16,
    /// 已重建好的幂等撤销命令（`-C 命中 && -D || true`）。
    pub undo: String,
}

/// 去掉 iptables 回显时可能加的一层双引号。
fn unquote(tok: &str) -> &str {
    tok.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(tok)
}

/// 取 `flag` 后面那个 token 的值（`-i eth0` / `--dport 443`）。
fn value_after<'a>(tokens: &[&'a str], flag: &str) -> Option<&'a str> {
    tokens
        .iter()
        .position(|t| *t == flag)
        .and_then(|i| tokens.get(i + 1))
        .map(|t| unquote(t))
}

/// 解析一行 nat PREROUTING 规则。不是本模块加的 DNAT 规则、或任一字段不合法 → None
/// （调用方跳过，不猜也不跑）。
pub(crate) fn scan_dnat_prerouting_line(line: &str) -> Option<ScannedRule> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if value_after(&tokens, "-j") != Some("DNAT") {
        return None;
    }
    let iface = value_after(&tokens, "-i")?;
    if !valid_iface_name(iface) {
        return None;
    }
    // 规则本身的入口端口（重建成 -C/-D 时要原样带上）
    let dport: u16 = value_after(&tokens, "--dport")?.parse().ok()?;
    let dest = value_after(&tokens, "--to-destination")?;
    let (dest_ip, dest_port) = dest.rsplit_once(':')?;
    // 目的地址必须是 IPv4 字面量：DNAT 目标是 wg 内网地址，域名/其他形态一概不是我们写的
    let ip: std::net::Ipv4Addr = dest_ip.parse().ok()?;
    let dest_port: u16 = dest_port.parse().ok()?;
    let cmd = |action: &str| {
        format!(
            "iptables -t nat {action} PREROUTING -i '{iface}' -p tcp --dport {dport} \
             -j DNAT --to-destination '{ip}:{dest_port}'"
        )
    };
    Some(ScannedRule {
        dest_ip: ip.to_string(),
        dest_port,
        undo: format!("{} && {} || true", cmd("-C"), cmd("-D")),
    })
}

/// 解析一行 filter FORWARD 规则；同样只认本模块自己写的那种（`-j ACCEPT`）。
pub(crate) fn scan_forward_line(line: &str) -> Option<ScannedRule> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if value_after(&tokens, "-j") != Some("ACCEPT") {
        return None;
    }
    let daddr = value_after(&tokens, "-d")?;
    let ip: std::net::Ipv4Addr = daddr.parse().ok()?;
    // 这条规则里 `--dport` 就是落地端口（FORWARD 只放行到「目的地 + 该端口」）
    let dest_port: u16 = value_after(&tokens, "--dport")?.parse().ok()?;
    let cmd = |action: &str| {
        format!("iptables {action} FORWARD -p tcp -d '{ip}' --dport {dest_port} -j ACCEPT")
    };
    Some(ScannedRule {
        dest_ip: ip.to_string(),
        dest_port,
        undo: format!("{} && {} || true", cmd("-C"), cmd("-D")),
    })
}

/// 显式 DNAT 删除：回读当前规则，按目标匹配后幂等删除（-C 存在检查 && -D || true）。
/// nat 分支重建命令补 `-t nat`：与新增命令同表同形态（默认 filter 表无 PREROUTING 链，
/// 缺表名的 -C 必然失败 → -D 永不执行 → 规则残留）。
///
/// 回读到的行**先解析再重建**（见 [`scan_dnat_prerouting_line`]），不整行重放。
async fn remove_dnat_rules(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    rule: &RelayRule,
) -> Result<(), String> {
    let nat = run_cmd(
        sess,
        cancel,
        "拆链",
        "iptables -t nat -S PREROUTING",
        CONFIG_TIMEOUT,
    )
    .await?;
    for line in nat.stdout.lines() {
        let Some(scanned) = scan_dnat_prerouting_line(line) else {
            continue;
        };
        if scanned.dest_ip == rule.landing_wg_ip
            && i64::from(scanned.dest_port) == rule.landing_port
        {
            run_cmd_ok(sess, cancel, "拆链", &scanned.undo, CONFIG_TIMEOUT).await?;
        }
    }
    let fwd = run_cmd(sess, cancel, "拆链", "iptables -S FORWARD", CONFIG_TIMEOUT).await?;
    for line in fwd.stdout.lines() {
        let Some(scanned) = scan_forward_line(line) else {
            continue;
        };
        if scanned.dest_ip == rule.landing_wg_ip
            && i64::from(scanned.dest_port) == rule.landing_port
        {
            run_cmd_ok(sess, cancel, "拆链", &scanned.undo, CONFIG_TIMEOUT).await?;
        }
    }
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("已取消".into())
    } else {
        Ok(())
    }
}
