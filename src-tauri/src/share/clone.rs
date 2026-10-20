//! 从主服务配置克隆出分享专属入站：只改端口、监听地址与凭据，其余流设置/出站
//! 保持与主服务一致。分享是快照，克隆完成后主服务再改不会回写这里。

use crate::domain::{Node, Privacy};
use crate::share::naming::share_dir;

pub struct ShareNodeSpec {
    pub name: String,
    pub protocol: String,
    pub original_port: u16,
    pub public_port: u16,
    pub internal_port: u16,
    pub transport: &'static str, // tcp | udp | tcp_and_udp
    pub original_params: serde_json::Value,
    pub new_credential: String,       // vless/vmess: uuid; trojan/hy2/ss: password; tuic: uuid
    pub new_password: Option<String>, // 仅 tuic 需要
}

pub struct ClonedConfigs {
    pub xray_config: Option<String>,
    pub singbox_config: Option<String>,
    pub ss_config: Option<String>,
    pub share_nodes: Vec<Node>,
    pub yaml: String,
    pub singbox_json: String,
}

pub fn clone_configs(
    main_xray: Option<&str>,
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
        Some(clone_xray(
            main_xray.ok_or("缺少 Xray 主配置")?,
            &xray_specs,
        )?)
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
        Some(clone_ss(main_ss.ok_or("缺少 Shadowsocks 主配置")?, &ss_specs)?)
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
                params["password"] = serde_json::Value::String(
                    s.new_password.clone().ok_or("tuic 缺新密码")?,
                );
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
    let (singbox_json, _) = crate::config::singbox::build_singbox_with_privacy(&share_nodes, privacies)
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

fn clone_xray(main: &str, specs: &[&ShareNodeSpec]) -> Result<String, String> {
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
                ib.get("protocol").and_then(|v| v.as_str()) == Some(s.protocol.as_str())
                    && ib.get("port").and_then(|v| v.as_u64()) == Some(s.original_port as u64)
            })
            .ok_or_else(|| {
                format!("主配置里找不到 {}:{} 入站", s.protocol, s.original_port)
            })?;
        let mut ib = orig.clone();
        ib["listen"] = serde_json::json!("127.0.0.1");
        ib["port"] = serde_json::json!(s.internal_port);
        match s.protocol.as_str() {
            "vless" => {
                let flow = orig
                    .pointer("/settings/clients/0/flow")
                    .cloned()
                    .unwrap_or(serde_json::json!("xtls-rprx-vision"));
                ib["settings"]["clients"] = serde_json::json!([{ "id": s.new_credential, "flow": flow }]);
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
    let mut out = serde_json::json!({ "inbounds": inbounds });
    if let Some(v) = root.get("outbounds") {
        out["outbounds"] = v.clone();
    }
    if let Some(v) = root.get("routing") {
        out["routing"] = v.clone();
    }
    Ok(out.to_string())
}

fn clone_singbox(
    main: &str,
    specs: &[&ShareNodeSpec],
    share_id: &str,
) -> Result<String, String> {
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
            .ok_or_else(|| {
                format!("主配置里找不到 {}:{} 入站", s.protocol, s.original_port)
            })?;
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
    for key in ["outbounds", "route", "dns"] {
        if let Some(v) = root.get(key) {
            out[key] = v.clone();
        }
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
