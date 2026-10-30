//! 从主服务配置克隆出分享专属入站：只改端口、监听地址与凭据，其余流设置/出站
//! 保持与主服务一致。分享是快照，克隆完成后主服务再改不会回写这里。

use crate::domain::{Node, Privacy};
use crate::share::naming::share_dir;
use std::collections::HashSet;

/// 入站来自哪份服务配置。上游出口在独立的 `xray-upstream` 实例里，不能去主配置找。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareSource {
    Main,
    Upstream,
}

#[derive(Debug)]
pub struct ShareNodeSpec {
    pub name: String,
    pub protocol: String,
    pub original_port: u16,
    pub public_port: u16,
    pub internal_port: u16,
    pub transport: &'static str, // tcp | udp | tcp_and_udp
    pub original_params: serde_json::Value,
    pub new_credential: String, // vless/vmess: uuid; trojan/hy2/ss: password; tuic: uuid
    pub new_password: Option<String>, // 仅 tuic 需要
    pub source: ShareSource,
}

pub struct ClonedConfigs {
    pub xray_config: Option<String>,
    pub singbox_config: Option<String>,
    pub ss_config: Option<String>,
    pub share_nodes: Vec<Node>,
    pub yaml: String,
    pub singbox_json: String,
}

// 参数对应主配置、上游、sing-box、SS、节点规格与服务端上下文，拆结构体会牵动调用点。
#[allow(clippy::too_many_arguments)]
pub fn clone_configs(
    main_xray: Option<&str>,
    upstream_xray: Option<&str>,
    main_singbox: Option<&str>,
    main_ss: Option<&str>,
    specs: &[ShareNodeSpec],
    server_host: &str,
    share_id: &str, // sing-box 证书副本落在 /srv/vpnshare/<share_id>/
    privacies: &[Privacy],
) -> Result<ClonedConfigs, String> {
    let xray_specs: Vec<_> = specs
        .iter()
        .filter(|s| matches!(s.protocol.as_str(), "vless" | "trojan" | "vmess"))
        .collect();
    let sb_specs: Vec<_> = specs
        .iter()
        .filter(|s| matches!(s.protocol.as_str(), "hysteria2" | "tuic"))
        .collect();
    let ss_specs: Vec<_> = specs.iter().filter(|s| s.protocol == "ss").collect();

    let xray_config = if xray_specs.is_empty() {
        None
    } else {
        Some(clone_xray(main_xray, upstream_xray, &xray_specs)?)
    };
    let singbox_config = if sb_specs.is_empty() {
        None
    } else {
        Some(clone_singbox(
            main_singbox.ok_or("缺少 sing-box 主配置")?,
            &sb_specs,
            share_id,
        )?)
    };
    let ss_config = if ss_specs.is_empty() {
        None
    } else {
        Some(clone_ss(
            main_ss.ok_or("缺少 Shadowsocks 主配置")?,
            &ss_specs,
        )?)
    };

    let mut share_nodes = Vec::new();
    for s in specs {
        let mut params = s.original_params.clone();
        match s.protocol.as_str() {
            "vless" | "vmess" => {
                params["uuid"] = serde_json::Value::String(s.new_credential.clone());
            }
            "trojan" | "hysteria2" => {
                params["password"] = serde_json::Value::String(s.new_credential.clone());
            }
            "tuic" => {
                params["uuid"] = serde_json::Value::String(s.new_credential.clone());
                params["password"] =
                    serde_json::Value::String(s.new_password.clone().ok_or("tuic 缺新密码")?);
            }
            "ss" => {
                params["password"] = serde_json::Value::String(s.new_credential.clone());
            }
            _ => return Err(format!("不支持分享的协议: {}", s.protocol)),
        }
        share_nodes.push(Node {
            id: format!("share:{}:{}", s.public_port, s.protocol),
            name: s.name.clone(),
            source: "self-built".into(),
            protocol: s.protocol.clone(),
            addr: server_host.into(),
            port: s.public_port as i64,
            params_json: params.to_string(),
            group_name: "分享".into(),
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
        });
    }
    let (yaml, _) =
        crate::config::build_yaml_with_privacy(&share_nodes, false, &[], &[], privacies)
            .map_err(|e| e.to_string())?;
    let (singbox_json, _) =
        crate::config::singbox::build_singbox_with_privacy(&share_nodes, privacies)
            .map_err(|e| e.to_string())?;
    Ok(ClonedConfigs {
        xray_config,
        singbox_config,
        ss_config,
        share_nodes,
        yaml,
        singbox_json,
    })
}

/// 主配置里给复用上游出口用的出站 tag（见 `servers::deploy_spec::upstream_outbound_tag`）。
fn is_upstream_outbound_tag(tag: &str) -> bool {
    tag.starts_with("out-up-")
}

/// 取规则里的字符串列表字段：Xray/sing-box 都允许 string 或 string[]。
fn string_list(value: Option<&serde_json::Value>) -> Vec<&str> {
    match value {
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => vec![],
    }
}

/// 这条规则是不是「复用上游出口」的专用规则。分享是给外部用户的快照，必须剔除：
/// 规则本身不会命中分享用户，但出站里带着住宅代理的 SOCKS/HTTP 凭据——把整个
/// 主配置原样复制过去，等于把凭据交给分享实例。宁可分享实例没有住宅线路，也不能泄漏。
fn rule_targets_upstream(rule: &serde_json::Value) -> bool {
    rule.get("outboundTag")
        .and_then(|v| v.as_str())
        .is_some_and(is_upstream_outbound_tag)
        || rule
            .get("outbound")
            .and_then(|v| v.as_str())
            .is_some_and(is_upstream_outbound_tag)
        || string_list(rule.get("user"))
            .iter()
            .any(|u| u.starts_with("up-"))
        || string_list(rule.get("auth_user"))
            .iter()
            .any(|u| u.starts_with("up-"))
}

fn clone_xray(
    main: Option<&str>,
    upstream: Option<&str>,
    specs: &[&ShareNodeSpec],
) -> Result<String, String> {
    let main_root = main
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .map_err(|e| format!("Xray 主配置解析失败: {e}"))?;
    let upstream_root = upstream
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .map_err(|e| format!("上游出口配置解析失败: {e}"))?;
    let main_inbounds = main_root
        .as_ref()
        .and_then(|r| r.get("inbounds"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let upstream_inbounds = upstream_root
        .as_ref()
        .and_then(|r| r.get("inbounds"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut inbounds = Vec::new();
    let mut inbound_tags = HashSet::new();
    let mut upstream_tags = Vec::new();
    for s in specs {
        let (source_inbounds, label) = match s.source {
            ShareSource::Main => (
                &main_inbounds,
                format!("主配置里找不到 {}:{} 入站", s.protocol, s.original_port),
            ),
            ShareSource::Upstream => (
                &upstream_inbounds,
                format!("上游配置里找不到 {}:{} 入站", s.protocol, s.original_port),
            ),
        };
        let orig = source_inbounds
            .iter()
            .find(|ib| {
                ib.get("protocol").and_then(|v| v.as_str()) == Some(s.protocol.as_str())
                    && ib.get("port").and_then(|v| v.as_u64()) == Some(s.original_port as u64)
            })
            .ok_or(label)?;
        if s.source == ShareSource::Upstream {
            let tag = orig.get("tag").and_then(|v| v.as_str()).ok_or_else(|| {
                format!(
                    "上游配置里 {}:{} 入站缺 tag，无法配对出站",
                    s.protocol, s.original_port
                )
            })?;
            if !inbound_tags.insert(tag.to_string()) {
                return Err(format!("入站 tag 重复: {tag}"));
            }
            upstream_tags.push(tag.to_string());
        } else if let Some(tag) = orig.get("tag").and_then(|v| v.as_str()) {
            if !inbound_tags.insert(tag.to_string()) {
                return Err(format!("入站 tag 重复: {tag}"));
            }
        }

        let mut ib = orig.clone();
        ib["listen"] = serde_json::json!("127.0.0.1");
        ib["port"] = serde_json::json!(s.internal_port);
        match s.protocol.as_str() {
            "vless" => {
                let flow = orig
                    .pointer("/settings/clients/0/flow")
                    .cloned()
                    .unwrap_or(serde_json::json!("xtls-rprx-vision"));
                ib["settings"]["clients"] =
                    serde_json::json!([{ "id": s.new_credential, "flow": flow }]);
            }
            "trojan" => {
                ib["settings"]["clients"] = serde_json::json!([{ "password": s.new_credential }]);
            }
            _ => {
                let alter = orig
                    .pointer("/settings/clients/0/alterId")
                    .cloned()
                    .unwrap_or(serde_json::json!(0));
                ib["settings"]["clients"] =
                    serde_json::json!([{ "id": s.new_credential, "alterId": alter }]);
            }
        }
        inbounds.push(ib);
    }

    // 主配置的出站/分流原样保留；上游只复制**被选中出口**对应的出站与路由，
    // 绝不把同机其它出口的 SOCKS/HTTP 凭据带进分享实例。
    let mut outbounds = Vec::new();
    let mut outbound_tags = HashSet::new();
    let mut main_rules = Vec::new();
    let mut upstream_rules = Vec::new();
    let mut domain_strategy = None;

    if let Some(root) = &main_root {
        if let Some(list) = root.get("outbounds").and_then(|v| v.as_array()) {
            for ob in list {
                if let Some(tag) = ob.get("tag").and_then(|v| v.as_str()) {
                    // 住宅代理出站不进分享：它的 settings 里是明文 SOCKS/HTTP 凭据。
                    if is_upstream_outbound_tag(tag) {
                        continue;
                    }
                    if !outbound_tags.insert(tag.to_string()) {
                        return Err(format!("出站 tag 重复: {tag}"));
                    }
                }
                outbounds.push(ob.clone());
            }
        }
        if let Some(routing) = root.get("routing") {
            if domain_strategy.is_none() {
                domain_strategy = routing.get("domainStrategy").cloned();
            }
            if let Some(rules) = routing.get("rules").and_then(|v| v.as_array()) {
                main_rules.extend(rules.iter().filter(|r| !rule_targets_upstream(r)).cloned());
            }
        }
    }

    if let Some(root) = &upstream_root {
        let up_outbounds = root
            .get("outbounds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let up_rules = root
            .pointer("/routing/rules")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for tag in &upstream_tags {
            let mut matched = false;
            for rule in &up_rules {
                let hits = rule
                    .get("inboundTag")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|v| v.as_str() == Some(tag.as_str())))
                    .unwrap_or(false);
                if !hits {
                    continue;
                }
                matched = true;
                let out_tag = rule
                    .get("outboundTag")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("上游配置里入站 {tag} 的路由规则缺 outboundTag"))?;
                let ob = up_outbounds
                    .iter()
                    .find(|o| o.get("tag").and_then(|v| v.as_str()) == Some(out_tag))
                    .ok_or_else(|| {
                        format!("上游配置里找不到路由指向的出站 {out_tag}（入站 {tag}）")
                    })?;
                if !outbound_tags.insert(out_tag.to_string()) {
                    return Err(format!("出站 tag 重复: {out_tag}"));
                }
                outbounds.push(ob.clone());
                upstream_rules.push(rule.clone());
            }
            if !matched {
                return Err(format!("上游配置里找不到入站 {tag} 的路由规则"));
            }
        }
        if domain_strategy.is_none() {
            domain_strategy = root.pointer("/routing/domainStrategy").cloned();
        }
    }

    // 上游规则放在主配置规则之前：Xray 首条匹配生效，主配置若带 catch-all，
    // 放后面会让上游入站被主配置的直连规则抢走，住宅出口形同虚设。
    let routing_rules: Vec<_> = upstream_rules.into_iter().chain(main_rules).collect();

    let mut out = serde_json::json!({ "inbounds": inbounds });
    if !outbounds.is_empty() {
        out["outbounds"] = serde_json::json!(outbounds);
    }
    let mut routing = serde_json::json!({});
    if let Some(ds) = domain_strategy {
        routing["domainStrategy"] = ds;
    }
    if !routing_rules.is_empty() {
        routing["rules"] = serde_json::json!(routing_rules);
    }
    if routing.as_object().is_some_and(|m| !m.is_empty()) {
        out["routing"] = routing;
    }
    Ok(out.to_string())
}

fn clone_singbox(main: &str, specs: &[&ShareNodeSpec], share_id: &str) -> Result<String, String> {
    let root: serde_json::Value = serde_json::from_str(main).map_err(|e| e.to_string())?;
    let main_inbounds = root
        .get("inbounds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut inbounds = Vec::new();
    for s in specs {
        let orig = main_inbounds
            .iter()
            .find(|ib| {
                ib.get("type").and_then(|v| v.as_str()) == Some(s.protocol.as_str())
                    && ib.get("listen_port").and_then(|v| v.as_u64())
                        == Some(s.original_port as u64)
            })
            .ok_or_else(|| format!("主配置里找不到 {}:{} 入站", s.protocol, s.original_port))?;
        let mut ib = orig.clone();
        ib["listen"] = serde_json::json!("127.0.0.1");
        ib["listen_port"] = serde_json::json!(s.internal_port);
        let tls = ib
            .get_mut("tls")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| format!("sing-box {} 入站缺 tls 块", s.protocol))?;
        tls.insert(
            "certificate_path".into(),
            serde_json::json!(format!("{}/singbox.crt", share_dir(share_id))),
        );
        tls.insert(
            "key_path".into(),
            serde_json::json!(format!("{}/singbox.key", share_dir(share_id))),
        );
        match s.protocol.as_str() {
            "hysteria2" => {
                ib["users"] = serde_json::json!([{ "password": s.new_credential }]);
            }
            "tuic" => {
                ib["users"] = serde_json::json!([{
                    "uuid": s.new_credential,
                    "password": s.new_password.clone().ok_or("tuic 缺新密码")?,
                }]);
            }
            other => return Err(format!("sing-box 不支持分享的协议: {other}")),
        }
        inbounds.push(ib);
    }
    let mut out = serde_json::json!({ "inbounds": inbounds });
    // 与 Xray 同一条纪律：剔除住宅出站与其路由规则，既不把凭据带进分享实例，
    // 也不让分享用户有机会命中它们。
    if let Some(list) = root.get("outbounds").and_then(|v| v.as_array()) {
        let filtered: Vec<_> = list
            .iter()
            .filter(|o| {
                !o.get("tag")
                    .and_then(|t| t.as_str())
                    .is_some_and(is_upstream_outbound_tag)
            })
            .cloned()
            .collect();
        out["outbounds"] = serde_json::json!(filtered);
    }
    if let Some(route) = root.get("route") {
        let mut route = route.clone();
        if let Some(rules) = route.get("rules").and_then(|v| v.as_array()) {
            let filtered: Vec<_> = rules
                .iter()
                .filter(|r| !rule_targets_upstream(r))
                .cloned()
                .collect();
            route["rules"] = serde_json::json!(filtered);
        }
        out["route"] = route;
    }
    if let Some(dns) = root.get("dns") {
        out["dns"] = dns.clone();
    }
    Ok(out.to_string())
}

fn clone_ss(main: &str, specs: &[&ShareNodeSpec]) -> Result<String, String> {
    let s = specs.first().ok_or("SS 分享缺节点")?;
    if specs.len() != 1 {
        return Err("SS 分享一次只允许一个节点".into());
    }
    let root: serde_json::Value = serde_json::from_str(main).map_err(|e| e.to_string())?;
    let ipv6_first = root
        .get("ipv6_first")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(serde_json::json!({
        "servers": [{
            "server": "127.0.0.1",
            "server_port": s.internal_port,
            "method": "2022-blake3-aes-256-gcm",
            "password": s.new_credential,
            "mode": "tcp_and_udp"
        }],
        "locals": [],
        "ipv6_first": ipv6_first
    })
    .to_string())
}
