//! 部署节点写回：按服务器归属复用历史行，消除名称冲突并记录部署结果。
//! 所有本地变更在同一事务内完成，提交后才重建配置和热重载客户端。

use crate::db::repos;
use crate::domain::{Node, Server};
use crate::state::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use super::deploy::{firewall_ports, matches_deployed_name};

pub(super) fn writeback(
    state: &Arc<AppState>,
    server: &Server,
    ok_protocols: &[&(String, u16, String)],
    bbr_note: Option<&str>,
) -> Result<(), String> {
    let mut guard = state.db.lock();
    let conn = guard.transaction().map_err(|e| e.to_string())?;
    let upstream_rows =
        repos::upstreams::list_by_server(&conn, server.id).map_err(|e| e.to_string())?;
    let upstream_ids: HashSet<String> = upstream_rows
        .iter()
        .filter_map(|u| u.node_id.clone())
        .collect();
    let landing_rule_ids: HashSet<i64> = repos::relay::list(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|r| r.landing_server_id == server.id)
        .map(|r| r.id)
        .collect();
    // 主协议写回后的 node id/端口/参数：复用入口的出口要用它们生成自己的客户端节点。
    let mut main_entries: HashMap<String, (String, u16, String)> = HashMap::new();
    for (proto, port, params) in ok_protocols {
        let name = format!("{}-{}", server.name, proto);
        // 地址会变化，不能仅凭当前 addr 判断归属。历史中转节点先还原 direct.addr；
        // 服务器换地址后，通过 relay_rule 的 landing_server_id 仍能认出同一条线。
        // 只合并同服务器、同协议的默认名及自动消歧副本，不吞并改名或上游出口节点。
        let rows: Vec<Node> = repos::nodes::list(&conn, &state.creds)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|n| {
                n.source == "self-built"
                    && n.protocol == *proto
                    && matches_deployed_name(&n.name, &name)
                    && (crate::domain::server_owns_addr(server, &crate::domain::direct_addr(n))
                        || n.relay_rule_id
                            .is_some_and(|id| landing_rule_ids.contains(&id)))
                    && !upstream_ids.contains(&n.id)
            })
            .collect();
        // 优先保留挂了中转的那行：链路是按节点行 id/端口建的，换行即丢挂载。
        let keep = rows
            .iter()
            .position(|n| n.relay_rule_id.is_some())
            .unwrap_or(0);
        let mut node = if rows.is_empty() {
            Node {
                id: Uuid::new_v4().simple().to_string(),
                name: name.clone(),
                source: "self-built".into(),
                protocol: proto.clone(),
                addr: String::new(),
                port: 0,
                params_json: String::new(),
                group_name: "自建".into(),
                sub_id: None,
                relay_rule_id: None,
                enabled: true,
                latency: None,
                bandwidth: None,
                last_checked_at: None,
                last_check_error: None,
                direct_latency: None,
                hop_relay_ms: None,
                hop_landing_ms: None,
                hop_tunnel_ms: None,
            }
        } else {
            rows[keep].clone()
        };
        node.addr = crate::domain::effective_host(server).to_string();
        node.port = *port as i64;
        node.params_json = params.clone();
        node.enabled = true;
        for (i, extra) in rows.iter().enumerate() {
            if i != keep {
                repos::nodes::delete(&conn, &extra.id).map_err(|e| e.to_string())?;
            }
        }
        repos::nodes::upsert(&conn, &node, &state.creds).map_err(|e| e.to_string())?;
        main_entries.insert(proto.clone(), (node.id.clone(), *port, params.clone()));
    }
    // 复用主入口的上游出口：服务端已由主配置加上 `up-<id>` 用户/路由，这里写回
    // 客户端节点。凭据必须用出口自己的（material），参数里的公钥/SNI/WS path 等
    // 共享主入口那份——这是它与主节点唯一的共同点。
    for u in upstream_rows.iter().filter(|u| !u.entry_proto.is_empty()) {
        let Some((main_node_id, main_port, main_params)) = main_entries.get(&u.entry_proto) else {
            let msg = format!("入口协议 {} 本次未部署成功，出口节点未更新", u.entry_proto);
            let _ = repos::upstreams::set_probe(&conn, u.id, u.egress_ip.as_deref(), Some(&msg));
            continue;
        };
        let material_json = match repos::upstreams::secrets(&conn, u.id) {
            Ok(Some((_, m))) => m,
            Ok(None) => continue,
            Err(e) => {
                let _ = repos::upstreams::set_probe(
                    &conn,
                    u.id,
                    u.egress_ip.as_deref(),
                    Some(&format!("读取身份材料失败: {e}")),
                );
                continue;
            }
        };
        let material =
            match crate::servers::upstream::material_for_reuse(&conn, u.id, &material_json) {
                Ok(m) => m,
                Err(e) => {
                    let _ = repos::upstreams::set_probe(
                        &conn,
                        u.id,
                        u.egress_ip.as_deref(),
                        Some(&format!("生成复用入口凭据失败: {e}")),
                    );
                    continue;
                }
            };
        let params_json = match crate::servers::upstream::reused_node_params(
            &u.entry_proto,
            main_params,
            &material,
        ) {
            Ok(p) => p,
            Err(e) => {
                let _ = repos::upstreams::set_probe(
                    &conn,
                    u.id,
                    u.egress_ip.as_deref(),
                    Some(&format!("生成出口节点参数失败: {e}")),
                );
                continue;
            }
        };
        let node = Node {
            id: u
                .node_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
            name: format!("{}-{}", server.name, u.name),
            source: "self-built".into(),
            protocol: u.entry_proto.clone(),
            addr: crate::domain::effective_host(server).to_string(),
            port: i64::from(*main_port),
            params_json,
            group_name: "自建".into(),
            sub_id: None,
            relay_rule_id: None,
            enabled: true,
            latency: None,
            bandwidth: None,
            last_checked_at: None,
            last_check_error: None,
            direct_latency: None,
            hop_relay_ms: None,
            hop_landing_ms: None,
            hop_tunnel_ms: None,
        };
        repos::nodes::upsert(&conn, &node, &state.creds).map_err(|e| e.to_string())?;
        if u.node_id.is_none() {
            repos::upstreams::set_node_id(&conn, u.id, &node.id).map_err(|e| e.to_string())?;
        }
        repos::upstreams::set_entry(&conn, u.id, Some(main_node_id), i64::from(*main_port))
            .map_err(|e| e.to_string())?;
        if u.last_check_error
            .as_deref()
            .is_some_and(|e| e.starts_with("入口协议"))
        {
            let _ = repos::upstreams::set_probe(&conn, u.id, u.egress_ip.as_deref(), None);
        }
    }
    // 部署也可能撞上导入节点或同名服务器；在同一事务内消歧并同步 raw YAML/隐私名，
    // 保证下一步导出缓存和运行客户端使用同一组唯一名称。
    repos::nodes::ensure_unique_names(&conn, &state.creds).map_err(|e| e.to_string())?;
    // 自动补挂中转：建链只改指「当时已存在」的节点，之后部署上来的节点
    // relay_rule_id 恒为 None，客户端就按直连拨——落地机直连不通时表现为
    // 测速 504，节点行上却毫无线索。这里补上，并把挂不上的原因写进摘要。
    let deployed: Vec<(String, i64)> = ok_protocols
        .iter()
        .map(|(p, port, _)| (p.clone(), *port as i64))
        .collect();
    let attaches = crate::relay::attach_deployed_nodes(&conn, server, &deployed, &state.creds)
        .unwrap_or_else(|e| {
            // 补挂失败不该让整次部署失败：节点已在库，链路问题照实报出来
            vec![crate::relay::AttachOutcome {
                node: server.name.clone(),
                attached: false,
                detail: format!("自动补挂中转时出错：{e}"),
            }]
        });
    let ports_map: serde_json::Map<String, serde_json::Value> = ok_protocols
        .iter()
        .map(|(p, port, _)| (p.clone(), serde_json::Value::from(*port)))
        .collect();
    let mut summary = serde_json::json!({
        "protocols": ok_protocols.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(),
        "ports": serde_json::Value::Object(ports_map),
        "relay": attaches.iter().map(|a| serde_json::json!({
            "node": a.node, "attached": a.attached, "detail": a.detail
        })).collect::<Vec<_>>()
    });
    // BBR 结果（有才写）：摘要只作部署历史记录，协议列表以节点行为准，
    // 因此附加字段不会影响 UI 的已部署协议展示
    if let Some(note) = bbr_note {
        summary["bbr"] = serde_json::Value::from(note);
    }
    let summary = summary.to_string();
    repos::deploys::insert(&conn, server.id, "deploy", &summary, "ok")
        .map_err(|e| e.to_string())?;
    conn.commit().map_err(|e| e.to_string())
}

/// 部署「复用既有入站」时可选的节点（按协议索引，每协议一条）。
///
/// **必须排除上游出口节点**：同一个协议名（vless）在两类节点上重名，而上游节点
/// 的 sni/公钥属于各自的 xray-upstream 入口。混进来会让主入站复用错公钥或拿错
/// 伪装域名——重复 key 时旧代码的 `collect::<HashMap>` 由哈希顺序决定谁留下，
/// 表现为时好时坏、极难复现。这里统一用「先到先得」且显式排除，行为确定。
pub fn deploy_managed_nodes(
    nodes: Vec<crate::domain::Node>,
    server: &crate::domain::Server,
    upstream_node_ids: &HashSet<String>,
) -> HashMap<String, crate::domain::Node> {
    let mut out = HashMap::new();
    for n in nodes {
        if n.source != "self-built"
            || upstream_node_ids.contains(&n.id)
            || !crate::domain::server_owns_addr(server, &crate::domain::direct_addr(&n))
        {
            continue;
        }
        out.entry(n.protocol.clone()).or_insert(n);
    }
    out
}

/// 从库里收集 [`firewall_ports`] 的输入（读库失败按空表处理：放行是增益，
/// 不能因为读不到节点就让整次部署失败）。
pub(crate) fn deployed_node_ports(
    state: &Arc<AppState>,
    server: &crate::domain::Server,
) -> Vec<(String, u16)> {
    let conn = state.db.lock();
    let upstreams = repos::upstreams::list_by_server(&conn, server.id).unwrap_or_default();
    let nodes = repos::nodes::list(&conn, &state.creds).unwrap_or_default();
    firewall_ports(&nodes, &upstreams, server)
}
