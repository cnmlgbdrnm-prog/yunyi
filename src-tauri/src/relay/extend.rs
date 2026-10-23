//! 接管链路的扩展端口：复用**已有** WireGuard 隧道（例如手工建的 wg1），
//! 只在中转机的 PREROUTING 上补 DNAT，不写 wg 配置、不重启隧道。
//!
//! 背景：`adopt` 只登记既有链路，工具不知道那条隧道的接口名，也不能按 wg0
//! 重建它（会撞地址/冲掉手工配置）。节点后于链路部署时，「补全中转」如果
//! 走 `build` 就会另起一条 wg0——对用户就是「已经有中转线路还要重新建立连接」。
//! 这里把缺的端口补成 `forward_mode='extended'` 的 imported 行：
//! - 每行一个节点端口，`forward_port == landing_port == port`，路由语义与 single 相同；
//! - 只增加 DNAT（原端口 → 落地 WG IP:原端口）与一次性的 POSTROUTING MASQUERADE；
//! - 规则写进工具自己的 systemd unit，重启后由 unit 恢复，**不碰** `/etc/wireguard`。
//!
//! 不做自动端口发现：只处理请求里点名的补全，端口来自落地机的自建节点表。
//! 不允许覆盖中转机上已有的、指向别处的 DNAT——那可能是用户的另一条服务。

use crate::db::repos;
use crate::domain::{JobProgress, LinkTeardownReq, RelayRule};
use crate::jobs::EmitFn;
use crate::relay::build::{find_iface_by_peer, valid_iface_name};
use crate::relay::{parse_dnat_pairs, RelayError};
use crate::servers::deploy::{run_cmd, run_cmd_ok, write_remote_file, CONFIG_TIMEOUT};
use crate::servers::{load_auth, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::atomic::AtomicBool;

/// 扩展规则的单位：一条 imported 链路 + 落地机上的一个节点端口。
///
/// `forward_mode` 用 `extended` 而不是 `same_port`：后者表示「整机所有端口都已被
/// 这条链路覆盖」，会把**尚未补 DNAT** 的新节点也判成已覆盖，界面于是不再给补全
/// 入口、节点却连不上。extended 仍按 landing_port 精确匹配，新增节点会重新出现
/// 补全按钮。
pub const EXTENDED_MODE: &str = "extended";

/// 生成并下发扩展端口规则。返回本次涉及的规则行（含此前已存在的同端口行）。
pub async fn extend_taken_link(
    state: &AppState,
    anchor_rule_id: i64,
) -> Result<Vec<RelayRule>, RelayError> {
    let (anchor, landing, nodes, rules) = {
        let conn = state.db.lock();
        let anchor = repos::relay::get(&conn, anchor_rule_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?;
        let landing = repos::servers::get(&conn, anchor.landing_server_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?;
        let nodes = repos::nodes::find_self_built_by_host(&conn, &landing.host, &state.creds)
            .map_err(|e| RelayError::Db(e.to_string()))?;
        let rules = repos::relay::list(&conn).map_err(|e| RelayError::Db(e.to_string()))?;
        (anchor, landing, nodes, rules)
    };
    validate_anchor(&anchor)?;
    let landing_ip: Ipv4Addr = anchor
        .landing_wg_ip
        .parse()
        .map_err(|_| RelayError::Validation("落地机 WG 地址不是合法 IPv4，拒绝生成 DNAT".into()))?;
    let landing_ip = landing_ip.to_string();
    let landing_pubkey = anchor
        .landing_pubkey
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RelayError::Validation(
                "接管链路缺少落地公钥，无法定位中转机上实际承载的 WireGuard 接口".into(),
            )
        })?;

    // 这台落地机上所有自建节点端口；已有可用链路（含整机/单端口/扩展）覆盖的不再处理。
    let mut node_ports: Vec<i64> = nodes.iter().map(|n| n.port).collect();
    node_ports.sort_unstable();
    node_ports.dedup();
    let missing: Vec<i64> = node_ports
        .into_iter()
        .filter(|p| (1..=65535).contains(p))
        .filter(|p| {
            !rules.iter().any(|r| {
                r.landing_server_id == anchor.landing_server_id
                    && r.converged
                    && matches!(r.status.as_str(), "imported" | "built")
                    && (r.forward_mode == "same_port" || r.landing_port == *p)
            })
        })
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    // 同一落地端口上若有一条**尚未就绪**的链路（failed/building/未收敛），不能再插
    // 一行同端口的扩展规则：两行会争同一份路由记录，节点挂到哪条全看遍历顺序。
    // 这类残局必须先由用户处理（删除/重建），补端口不能替它做决定。
    for port in &missing {
        if let Some(r) = rules.iter().find(|r| {
            r.landing_server_id == anchor.landing_server_id
                && r.landing_port == *port
                && r.status != "not-built"
        }) {
            return Err(RelayError::Validation(format!(
                "落地端口 {port} 已被链路「{}」（状态 {}）占用；请先处理该链路，再补端口",
                r.name, r.status
            )));
        }
    }

    let (relay_auth, relay_server) =
        load_auth(state, anchor.relay_server_id).map_err(crate::relay::map_servers_error)?;
    let mut sess = SshSession::connect(
        &relay_server.host,
        relay_server.port as u16,
        &relay_server.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()))?;
    let cancel = AtomicBool::new(false);

    // 1. 找到实际承载这条链路的接口（不假设 wg0；用户的手工隧道常是 wg1）。
    let iface = find_iface_by_peer(&mut sess, &cancel, landing_pubkey)
        .await
        .map_err(RelayError::Ssh)?
        .ok_or_else(|| {
            RelayError::Validation(
                "在中转机上找不到包含该落地公钥的 WireGuard 接口，无法扩展端口".into(),
            )
        })?;
    if !valid_iface_name(&iface) {
        return Err(RelayError::Validation(format!("非法 WG 接口名: {iface}")));
    }

    // 2. 公网入口网卡（DNAT 只匹配公网入，避免把隧道内流量再转一圈）。
    let out = run_cmd_ok(
        &mut sess,
        &cancel,
        "补全",
        "ip route get 8.8.8.8 | awk '{for(i=1;i<=NF;i++) if($i==\"dev\"){print $(i+1); exit}}'",
        PROBE_TIMEOUT,
    )
    .await
    .map_err(RelayError::Ssh)?;
    let ingress = out.stdout.trim().to_string();
    if !valid_iface_name(&ingress) {
        return Err(RelayError::Validation(format!(
            "中转机公网入口网卡名不可信，已中止：{ingress:?}"
        )));
    }

    // 3. 回读中转机本机监听端口与现有 DNAT。
    //
    //    入口端口**不必等于落地端口**：真机上 4443 已经被锚点链路映射到 8388，
    //    若落地机上的 trojan 也监听 4443，再往 4443 上加一条 DNAT 既装不下也会
    //    覆盖用户服务。这里为每个落地端口挑一个空闲入口端口（优先原端口），
    //    再把「入口端口」写进扩展行，客户端按 forward_port 拨号。
    let out = run_cmd(
        &mut sess,
        &cancel,
        "补全",
        "ss -tln | awk '$1==\"LISTEN\"{print $4}'",
        PROBE_TIMEOUT,
    )
    .await
    .map_err(RelayError::Ssh)?;
    if out.exit_code != 0 {
        return Err(RelayError::Ssh(format!(
            "读取中转机监听端口失败: {}",
            crate::util::stderr_tail(&out.stderr)
        )));
    }
    let listened = parse_listen_ports(&out.stdout);

    let out = run_cmd(
        &mut sess,
        &cancel,
        "补全",
        "iptables -t nat -S PREROUTING",
        PROBE_TIMEOUT,
    )
    .await
    .map_err(RelayError::Ssh)?;
    if out.exit_code != 0 {
        return Err(RelayError::Ssh(format!(
            "读取中转机 DNAT 失败: {}",
            crate::util::stderr_tail(&out.stderr)
        )));
    }
    let mut all_pairs: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    let mut own_pairs: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    for p in parse_dnat_pairs(&out.stdout, &ingress) {
        all_pairs.insert(p.forward_port, (p.target_ip.clone(), p.target_port));
        if p.target_ip == landing_ip {
            own_pairs.insert(p.forward_port, (p.target_ip, p.target_port));
        }
    }

    // 4. 给每个缺失的落地端口分配入口端口。已被本机监听或已有 DNAT 的端口
    //    不可再用（DNAT 会劫走本机服务，覆盖既有 DNAT 会改坏用户服务）。
    let mut used: BTreeSet<i64> = listened;
    used.extend(all_pairs.keys().copied());
    let mut planned: BTreeSet<i64> = BTreeSet::new();
    let mut plan: Vec<(i64, i64)> = Vec::new(); // (入口端口, 落地端口)
    for &landing_port in &missing {
        // 已经恰好是「入口 P → 本落地 IP:P」的规则：直接复用，不重复下发。
        let already = all_pairs
            .get(&landing_port)
            .is_some_and(|(ip, tp)| ip == &landing_ip && *tp == landing_port);
        let forward = if already && !planned.contains(&landing_port) {
            landing_port
        } else {
            allocate_forward_port(landing_port, &used, &planned).ok_or_else(|| {
                RelayError::Validation(format!(
                    "中转机上找不到可用入口端口来转发落地端口 {landing_port}"
                ))
            })?
        };
        planned.insert(forward);
        plan.push((forward, landing_port));
    }

    // 5. 下发 DNAT（幂等），并确保回程 MASQUERADE 存在。回程伪装是「端口通但客户端
    //    一直等不到响应」的根因；手工链路通常有 `-o wg1 -j MASQUERADE`，这里只在
    //    缺失时补上，不重复堆积。
    for &(forward, landing_port) in &plan {
        let already = all_pairs
            .get(&forward)
            .is_some_and(|(ip, tp)| ip == &landing_ip && *tp == landing_port);
        if !already {
            let cmd = format!(
                "iptables -t nat -C PREROUTING -i '{ingress}' -p tcp --dport {forward} \
                 -j DNAT --to-destination {landing_ip}:{landing_port} 2>/dev/null || \
                 iptables -t nat -A PREROUTING -i '{ingress}' -p tcp --dport {forward} \
                 -j DNAT --to-destination {landing_ip}:{landing_port}"
            );
            run_cmd_ok(&mut sess, &cancel, "补全", &cmd, CONFIG_TIMEOUT)
                .await
                .map_err(RelayError::Ssh)?;
        }
        let target = (landing_ip.clone(), landing_port);
        all_pairs.insert(forward, target.clone());
        own_pairs.insert(forward, target);
    }
    let cmd = format!(
        "iptables -t nat -C POSTROUTING -o '{iface}' -j MASQUERADE 2>/dev/null || \
         iptables -t nat -A POSTROUTING -o '{iface}' -j MASQUERADE"
    );
    run_cmd_ok(&mut sess, &cancel, "补全", &cmd, CONFIG_TIMEOUT)
        .await
        .map_err(RelayError::Ssh)?;

    // 6. 持久化：工具自己的 unit 在开机时重放同一份 DNAT 表。脚本还包括这条
    //    落地 IP 上原有的 DNAT（例如手工的 8443→8444），让用户的既有服务也能
    //    在重启后存活；不修改 /etc/wireguard/wg1.conf。
    let (script_path, unit_name, unit_path) =
        ports_paths(anchor.relay_server_id, anchor.landing_server_id);
    let script = render_script(&ingress, &iface, &own_pairs);
    write_remote_file(
        &mut sess,
        &cancel,
        "补全",
        &script_path,
        &script,
        CONFIG_TIMEOUT,
    )
    .await
    .map_err(RelayError::Ssh)?;
    let unit = render_unit(&script_path);
    write_remote_file(
        &mut sess,
        &cancel,
        "补全",
        &unit_path,
        &unit,
        CONFIG_TIMEOUT,
    )
    .await
    .map_err(RelayError::Ssh)?;
    run_cmd_ok(
        &mut sess,
        &cancel,
        "补全",
        &format!(
            "systemctl daemon-reload && systemctl enable {unit_name} && systemctl restart {unit_name}"
        ),
        CONFIG_TIMEOUT,
    )
    .await
    .map_err(RelayError::Ssh)?;

    // 7. 写回规则行并挂节点。SSH 已成功，这里失败时重试会复用已存在的 DNAT。
    let conn = state.db.lock();
    let mut result = Vec::new();
    for &(forward, landing_port) in &plan {
        let id =
            match repos::relay::find_by_landing_port(&conn, anchor.landing_server_id, landing_port)
                .map_err(|e| RelayError::Db(e.to_string()))?
            {
                // 只复用上一次补端口留下的扩展行；其它任何行都意味着端口身份冲突。
                Some(r) if r.forward_mode == EXTENDED_MODE => {
                    // 入口端口可能因上一次的占用情况而不同：以本次实际下发的为准，
                    // 否则客户端会按旧 forward_port 拨号。
                    if r.forward_port != Some(forward) {
                        repos::relay::set_mode_and_ports(
                            &conn,
                            r.id,
                            &r.client_mode,
                            Some(forward),
                            landing_port,
                        )
                        .map_err(|e| RelayError::Db(e.to_string()))?;
                    }
                    r.id
                }
                Some(r) => {
                    return Err(RelayError::Validation(format!(
                        "落地端口 {landing_port} 已被链路「{}」（状态 {}）占用，未写入登记",
                        r.name, r.status
                    )))
                }
                None => {
                    let rule = RelayRule {
                        id: 0,
                        name: format!("{} · 端口 {}", anchor.name, landing_port),
                        relay_server_id: anchor.relay_server_id,
                        landing_server_id: anchor.landing_server_id,
                        client_mode: "forward".into(),
                        wg_port: anchor.wg_port,
                        forward_port: Some(forward),
                        landing_port,
                        subnet: anchor.subnet.clone(),
                        relay_wg_ip: anchor.relay_wg_ip.clone(),
                        landing_wg_ip: anchor.landing_wg_ip.clone(),
                        relay_pubkey: anchor.relay_pubkey.clone(),
                        landing_pubkey: anchor.landing_pubkey.clone(),
                        converged: anchor.converged,
                        status: "imported".into(),
                        forward_mode: EXTENDED_MODE.into(),
                    };
                    repos::relay::insert(&conn, &rule).map_err(|e| RelayError::Db(e.to_string()))?
                }
            };
        let attached =
            repos::nodes::find_for_landing_server(&conn, &landing, landing_port, &state.creds)
                .map_err(|e| RelayError::Db(e.to_string()))?;
        for n in attached {
            repos::nodes::set_relay(&conn, &n.id, Some(id))
                .map_err(|e| RelayError::Db(e.to_string()))?;
        }
        if let Some(r) = repos::relay::get(&conn, id).map_err(|e| RelayError::Db(e.to_string()))? {
            result.push(r);
        }
    }
    Ok(result)
}

/// 远端脚本/unit 的固定路径（按中转/落地对命名，扩展端口与移除共用同一个 unit）。
fn ports_paths(relay_id: i64, landing_id: i64) -> (String, String, String) {
    let unit_name = format!("yunyi-relay-ports-{relay_id}-{landing_id}");
    let script_path = format!("/usr/local/lib/yunyi/relay-ports-{relay_id}-{landing_id}.sh");
    let unit_path = format!("/etc/systemd/system/{unit_name}.service");
    (script_path, unit_name, unit_path)
}

/// `ss -tln | awk '$1=="LISTEN"{print $4}'` 的输出（每行一个 `地址:端口`）
/// → 监听端口集合。IPv4/IPv6/通配地址都取最后一个冒号之后的部分。
fn parse_listen_ports(stdout: &str) -> BTreeSet<i64> {
    stdout
        .lines()
        .filter_map(|line| {
            let (_, port) = line.trim().rsplit_once(':')?;
            port.parse::<i64>().ok().filter(|p| (1..=65535).contains(p))
        })
        .collect()
}

/// 给一个落地端口挑入口端口：优先原端口（客户端最容易理解），其次 +10000 系列，
/// 最后全范围找第一个空闲端口。`used` 已包含中转机监听端口与现有 DNAT 入口，
/// `planned` 是本次已经分配出去的入口端口。
fn allocate_forward_port(
    landing_port: i64,
    used: &BTreeSet<i64>,
    planned: &BTreeSet<i64>,
) -> Option<i64> {
    let mut candidates = Vec::with_capacity(6);
    for delta in [0i64, 10_000, 20_000, 30_000, 40_000, 50_000] {
        let p = landing_port.saturating_add(delta);
        if (1..=65535).contains(&p) {
            candidates.push(p);
        }
    }
    candidates
        .into_iter()
        .chain(1..=65535)
        .find(|p| !used.contains(p) && !planned.contains(p))
}

/// 移除一个扩展端口：只删 DNAT、重写持久化脚本、删登记行并解开节点挂载；
/// 不碰 WireGuard（调用方已在 run_teardown 里把扩展行与普通拆链分流）。
pub async fn remove_extended_port(
    state: &AppState,
    req: &LinkTeardownReq,
    rule: &RelayRule,
    cancel: &AtomicBool,
    emit: &EmitFn,
) -> Result<(), String> {
    if rule.forward_mode != EXTENDED_MODE {
        return Err(RelayError::Validation("只有扩展端口行可以移除端口".into()).to_string());
    }
    let landing_ip: Ipv4Addr = rule
        .landing_wg_ip
        .parse()
        .map_err(|_| "落地机 WG 地址不是合法 IPv4，拒绝操作".to_string())?;
    let landing_ip = landing_ip.to_string();
    let landing_pubkey = rule
        .landing_pubkey
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "接管链路缺少落地公钥，无法定位 WireGuard 接口".to_string())?;
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
    emit_progress("移除端口", "连接中转机", 10);
    let (relay_auth, relay_server) =
        load_auth(state, rule.relay_server_id).map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        &relay_server.host,
        relay_server.port as u16,
        &relay_server.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;

    let iface = find_iface_by_peer(&mut sess, cancel, landing_pubkey)
        .await?
        .ok_or_else(|| "找不到承载该链路的 WireGuard 接口".to_string())?;
    if !valid_iface_name(&iface) {
        return Err(format!("非法 WG 接口名: {iface}"));
    }
    let out = run_cmd_ok(
        &mut sess,
        cancel,
        "移除端口",
        "ip route get 8.8.8.8 | awk '{for(i=1;i<=NF;i++) if($i==\"dev\"){print $(i+1); exit}}'",
        PROBE_TIMEOUT,
    )
    .await?;
    let ingress = out.stdout.trim().to_string();
    if !valid_iface_name(&ingress) {
        return Err(format!("中转机公网入口网卡名不可信: {ingress:?}"));
    }

    // 删除失败不中止：规则可能已经被人手工删掉；后面的重建脚本以实际表为准。
    // 入口端口用 forward_port——扩展行允许它和落地端口不同（原端口被既有链路占用时）。
    let forward = rule.forward_port.unwrap_or(rule.landing_port);
    emit_progress("移除端口", "删除 DNAT 规则", 40);
    let _ = run_cmd(
        &mut sess,
        cancel,
        "移除端口",
        &format!(
            "iptables -t nat -D PREROUTING -i '{ingress}' -p tcp --dport {forward} \
             -j DNAT --to-destination {landing_ip}:{landing_port}",
            landing_port = rule.landing_port
        ),
        CONFIG_TIMEOUT,
    )
    .await?;

    // 以删除后的实际 DNAT 表重建持久化脚本：只保留仍指向本落地 WG IP 的规则。
    emit_progress("移除端口", "更新持久化规则", 70);
    let out = run_cmd(
        &mut sess,
        cancel,
        "移除端口",
        "iptables -t nat -S PREROUTING",
        PROBE_TIMEOUT,
    )
    .await?;
    if out.exit_code != 0 {
        return Err(format!(
            "读取中转机 DNAT 失败: {}",
            crate::util::stderr_tail(&out.stderr)
        ));
    }
    let mut own_pairs: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    for p in parse_dnat_pairs(&out.stdout, &ingress) {
        if p.target_ip == landing_ip {
            own_pairs.insert(p.forward_port, (p.target_ip, p.target_port));
        }
    }
    let (script_path, unit_name, unit_path) =
        ports_paths(rule.relay_server_id, rule.landing_server_id);
    write_remote_file(
        &mut sess,
        cancel,
        "移除端口",
        &script_path,
        &render_script(&ingress, &iface, &own_pairs),
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        &mut sess,
        cancel,
        "移除端口",
        &unit_path,
        &render_unit(&script_path),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        cancel,
        "移除端口",
        &format!(
            "systemctl daemon-reload && systemctl enable {unit_name} && systemctl restart {unit_name}"
        ),
        CONFIG_TIMEOUT,
    )
    .await?;

    emit_progress("移除端口", "更新节点挂载", 90);
    let conn = state.db.lock();
    repos::relay::delete(&conn, rule.id).map_err(|e| e.to_string())?;
    let nodes = repos::nodes::list(&conn, &state.creds).map_err(|e| e.to_string())?;
    for n in nodes.iter().filter(|n| n.relay_rule_id == Some(rule.id)) {
        repos::nodes::set_relay(&conn, &n.id, None).map_err(|e| e.to_string())?;
    }
    emit_progress("移除端口", "完成", 100);
    Ok(())
}

fn validate_anchor(anchor: &RelayRule) -> Result<(), RelayError> {
    if anchor.client_mode != "forward" {
        return Err(RelayError::Validation(
            "只有模式 A（端口转发）的接管链路可以补端口".into(),
        ));
    }
    if anchor.status != "imported" {
        return Err(RelayError::Validation(
            "只有「已接管」的链路可以补端口；工具自建链路请用建链/重建流程".into(),
        ));
    }
    if anchor.forward_mode == EXTENDED_MODE {
        return Err(RelayError::Validation(
            "请选择原始接管链路，而不是扩展端口行".into(),
        ));
    }
    if !anchor.converged {
        return Err(RelayError::Validation(
            "该链路尚未收敛（落地机仍在公网监听），补端口不会产生中转效果；先重建收敛".into(),
        ));
    }
    Ok(())
}

fn render_script(ingress: &str, iface: &str, pairs: &BTreeMap<i64, (String, i64)>) -> String {
    let mut s = String::from(
        "#!/bin/sh\n\
         # 由云驿自动生成：接管链路扩展端口（复用现有 WireGuard 隧道，勿手改）\n\
         # 只补 DNAT 与回程 MASQUERADE；不修改 /etc/wireguard 下的配置。\n\
         set -eu\n",
    );
    s.push_str(&format!("INGRESS='{ingress}'\nWG='{iface}'\n"));
    s.push_str(
        "add() {\n\
         \x20 iptables -t nat -C PREROUTING -i \"$INGRESS\" -p tcp --dport \"$1\" \\\n\
         \x20   -j DNAT --to-destination \"$2\" 2>/dev/null \\\n\
         \x20   || iptables -t nat -A PREROUTING -i \"$INGRESS\" -p tcp --dport \"$1\" \\\n\
         \x20   -j DNAT --to-destination \"$2\"\n\
         }\n",
    );
    for (port, (ip, target_port)) in pairs {
        // ip/port 来自已校验的 IPv4 与 u16；仍用单引号包裹，杜绝字符注入。
        s.push_str(&format!("add {port} '{ip}:{target_port}'\n"));
    }
    s.push_str(
        "iptables -t nat -C POSTROUTING -o \"$WG\" -j MASQUERADE 2>/dev/null \\\n\
         \x20 || iptables -t nat -A POSTROUTING -o \"$WG\" -j MASQUERADE\n",
    );
    s
}

fn render_unit(script_path: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Yunyi relay port forwarding\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/bin/sh {script_path}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}
