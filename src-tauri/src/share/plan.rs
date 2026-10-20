//! 分享准入与端口计划：哪些节点能分享、向导要列哪些候选、端口探测命令。
//!
//! 准入必须在**后端**重算，不能只信前端置灰：创建请求可能来自旧界面或直接 IPC。

use crate::domain::{direct_addr, server_owns_addr, Node, Privacy, Server};
use crate::share::model::ShareCandidate;
use crate::state::AppState;
use std::collections::HashSet;
use std::sync::Arc;

/// 单节点准入。`Err` 直接作为前端置灰原因；创建时同样用它拒绝。
pub fn check_eligibility(
    node: &Node,
    server: &Server,
    upstream_ids: &HashSet<String>,
    privacies: &[Privacy],
) -> Result<(), String> {
    if node.source != "self-built" {
        return Err("不是本应用部署的自建节点".into());
    }
    if !server_owns_addr(server, &direct_addr(node)) {
        return Err("节点不属于所选服务器".into());
    }
    if !node.enabled {
        return Err("节点未启用".into());
    }
    if node.protocol == "ssr" {
        return Err("mihomo 与 sing-box 已移除 SSR 支持".into());
    }
    if crate::servers::privacy::effective_privacy(&node.id, privacies).is_some() {
        return Err("隐私线不可分享（需要本机 Tor）".into());
    }
    if upstream_ids.contains(&node.id) {
        return Err("上游出口节点不可分享".into());
    }
    if node.relay_rule_id.is_some() {
        return Err("挂中转链路的节点不可分享（会绕过中转）".into());
    }
    Ok(())
}

/// 向导候选：只列属于该服务器的节点；不可分享的也返回，带上原因供置灰。
pub fn share_candidates(
    state: &Arc<AppState>,
    server_id: i64,
) -> Result<Vec<ShareCandidate>, String> {
    let (server, upstream_ids, privacies) = {
        let conn = state.db.lock();
        let server = crate::db::repos::servers::get(&conn, server_id)
            .map_err(|e| e.to_string())?
            .ok_or("服务器不存在")?;
        let upstream_ids = crate::db::repos::upstreams::list_by_server(&conn, server_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|u| u.node_id)
            .collect::<HashSet<_>>();
        let privacies = crate::db::repos::privacies::list_all(&conn).map_err(|e| e.to_string())?;
        (server, upstream_ids, privacies)
    };
    let nodes = state.subs.visible_nodes().map_err(|e| e.to_string())?;
    Ok(nodes
        .into_iter()
        // 向导只列属于该服务器的节点；别把别的机器/机场节点混进来
        .filter(|node| server_owns_addr(&server, &direct_addr(node)))
        .map(|node| {
            let reason = check_eligibility(&node, &server, &upstream_ids, &privacies).err();
            ShareCandidate {
                shareable: reason.is_none(),
                reason,
                node,
            }
        })
        .collect())
}

/// 退出码 0 = 已占用（与 deploy_spec::port_probe_cmd 同方向）。
/// **必须只匹配数据行**：无匹配时 ss 仍会打印表头，`grep -q .` 会把空闲端口误报为
/// 占用（deploy_spec::port_probe_cmd 已踩过这个坑）。
pub fn tcp_port_probe_cmd(port: u16) -> String {
    format!("ss -tln 'sport = :{port}' | grep -q '^LISTEN'")
}
pub fn udp_port_probe_cmd(port: u16) -> String {
    format!("ss -uln 'sport = :{port}' | grep -q '^UNCONN'")
}

/// 读出**已配置**分享占用的端口（含当前已停止的单元）。
///
/// 只看 `ss` 不够：停着的分享不监听，但它的单元/清单仍占着这个端口，新分享
/// 一旦复用，旧分享下次启动就会 `bind: address already in use`。新格式读
/// `share.json`，旧格式从单元 `ExecStart` 的 `share_server.py <port>` 里取。
pub fn reserved_share_ports_cmd() -> String {
    concat!(
        "{ ",
        "for f in /srv/vpnshare/*/share.json; do ",
        "[ -f \"$f\" ] || continue; ",
        "python3 -c 'import json,sys;m=json.load(open(sys.argv[1]));print(m.get(\"http_port\") or 0);",
        "[print(n.get(k) or 0) for n in m.get(\"nodes\",[]) for k in (\"public_port\",\"internal_port\")]' ",
        "\"$f\" 2>/dev/null; ",
        "done; ",
        "grep -hoE 'share_server\\.py [0-9]+' /etc/systemd/system/vpnshare-*.service 2>/dev/null | awk '{print $2}'; ",
        "} | grep -v '^0$' | sort -un"
    )
    .to_string()
}
