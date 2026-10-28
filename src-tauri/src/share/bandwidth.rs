//! Keep share Hysteria2 bandwidth hints in sync across both subscription formats.
use super::{clone::ClonedConfigs, model::ShareManifest};
use serde_json::Value;

fn params(raw: &str, up: u32, down: u32) -> Result<String, String> {
    let mut value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let object = value.as_object_mut().ok_or("Hysteria2 参数不是对象")?;
    // With an unlimited direction, use adaptive CC; the gateway still enforces
    // the finite direction. Do not substitute an arbitrary 100 Mbps ceiling.
    for (key, rate) in [("up", up), ("down", down)] {
        if up > 0 && down > 0 {
            object.insert(key.into(), rate.into());
        } else {
            object.remove(key);
        }
    }
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

pub fn sync_manifest(manifest: &mut ShareManifest) -> Result<(), String> {
    for node in &mut manifest.nodes {
        if node.protocol == "hysteria2" {
            node.params_json = params(
                &node.params_json,
                manifest.rate_up_mbps,
                manifest.rate_down_mbps,
            )?;
        }
    }
    Ok(())
}

pub fn sync_cloned(cloned: &mut ClonedConfigs, up: u32, down: u32) -> Result<(), String> {
    if !cloned.share_nodes.iter().any(|n| n.protocol == "hysteria2") {
        return Ok(());
    }
    for node in &mut cloned.share_nodes {
        if node.protocol == "hysteria2" {
            node.params_json = params(&node.params_json, up, down)?;
        }
    }
    (cloned.yaml, cloned.singbox_json) =
        sync_documents(&cloned.yaml, &cloned.singbox_json, up, down)?;
    Ok(())
}

pub fn sync_documents(
    yaml: &str,
    singbox: &str,
    up: u32,
    down: u32,
) -> Result<(String, String), String> {
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| format!("订阅 YAML 损坏：{e}"))?;
    let proxies = yaml
        .get_mut("proxies")
        .and_then(|v| v.as_sequence_mut())
        .ok_or("订阅缺少 proxies")?;
    for proxy in proxies {
        if proxy.get("type").and_then(|v| v.as_str()) != Some("hysteria2") {
            continue;
        }
        let map = proxy.as_mapping_mut().ok_or("Hysteria2 节点不是对象")?;
        for (key, rate) in [("up", up), ("down", down)] {
            let key = serde_yaml::Value::String(key.into());
            if up > 0 && down > 0 {
                map.insert(key, serde_yaml::Value::String(format!("{rate} Mbps")));
            } else {
                map.remove(&key);
            }
        }
    }
    let mut singbox: Value =
        serde_json::from_str(singbox).map_err(|e| format!("sing-box 订阅损坏：{e}"))?;
    let outbounds = singbox
        .get_mut("outbounds")
        .and_then(|v| v.as_array_mut())
        .ok_or("订阅缺少 outbounds")?;
    for outbound in outbounds {
        if outbound.get("type").and_then(|v| v.as_str()) != Some("hysteria2") {
            continue;
        }
        let map = outbound.as_object_mut().ok_or("Hysteria2 出站不是对象")?;
        for (key, rate) in [("up_mbps", up), ("down_mbps", down)] {
            if up > 0 && down > 0 {
                map.insert(key.into(), rate.into());
            } else {
                map.remove(key);
            }
        }
    }
    Ok((
        serde_yaml::to_string(&yaml).map_err(|e| e.to_string())?,
        serde_json::to_string_pretty(&singbox).map_err(|e| e.to_string())?,
    ))
}
