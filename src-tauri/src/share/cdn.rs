//! CDN shares keep the quota gateway in the data path. The HTTPS subscription
//! listener also forwards WebSocket upgrades to that share's gateway.
use super::{clone::ClonedConfigs, model::ShareManifest};
use crate::{domain::Node, state::AppState};
use serde::{Deserialize, Serialize};

pub const HTTPS_PORTS: &[u16] = &[2083, 2053, 2087, 2096, 8443, 443];

/// 自动模式的候选顺序：从 `start` 起回绕整个 HTTPS 端口表。
///
/// 端口仍只能落在 Cloudflare 支持的固定集合里，随机只改变尝试顺序——
/// 这里**不能**扩大端口范围，Cloudflare 只代理这几个 HTTPS 端口。
pub(crate) fn auto_candidates(start: usize) -> Vec<u16> {
    let n = HTTPS_PORTS.len();
    // 先对 n 取模再加 i：`start + i` 在 start 接近 usize::MAX 时会溢出。
    (0..n).map(|i| HTTPS_PORTS[(start % n + i) % n]).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareCdn {
    pub domain: String,
    /// 0 selects a free supported port; manifests record the resolved port.
    pub port: u16,
}

pub fn settings_key(server_id: i64) -> String {
    format!("share_cdn_{server_id}")
}

pub fn load(state: &AppState, server_id: i64) -> Result<Option<ShareCdn>, String> {
    let raw = state
        .settings
        .get(&settings_key(server_id))?
        .unwrap_or_default();
    if raw.trim().is_empty() || raw.trim() == "null" {
        return Ok(None);
    }
    let mut cdn: ShareCdn = serde_json::from_str(&raw).map_err(|e| format!("CDN 配置无效：{e}"))?;
    cdn.domain = crate::servers::normalize_share_domain(&cdn.domain)?;
    if cdn.domain.is_empty() || cdn.domain.parse::<std::net::IpAddr>().is_ok() {
        return Err("CDN 必须配置已代理的域名".into());
    }
    if cdn.port != 0 && !HTTPS_PORTS.contains(&cdn.port) {
        return Err("CDN 端口必须为 443、2053、2083、2087、2096、8443，或自动选择".into());
    }
    Ok(Some(cdn))
}

pub fn validate_nodes(nodes: &[Node]) -> Result<(), String> {
    for n in nodes {
        let p: serde_json::Value =
            serde_json::from_str(&n.params_json).map_err(|e| e.to_string())?;
        let network = p
            .get("network")
            .or_else(|| p.get("type"))
            .and_then(|v| v.as_str());
        if !matches!(n.protocol.as_str(), "vmess" | "vless")
            || network != Some("ws")
            || p.get("security").and_then(|v| v.as_str()) == Some("reality")
            || p.get("reality-opts").is_some()
        {
            return Err(format!(
                "{} 不支持 CDN 分享：请选择 VMess/VLESS WebSocket 节点",
                n.name
            ));
        }
    }
    Ok(())
}

/// Only exported clients change addresses. Internal instances remain loopback,
/// and the HTTP forwarder connects to public_port (the quota gateway), never
/// directly to internal_port (which would bypass rate/quota/expiry controls).
pub fn apply(cloned: &mut ClonedConfigs, cdn: &ShareCdn) -> Result<(), String> {
    validate_nodes(&cloned.share_nodes)?;
    let mut xray: serde_json::Value =
        serde_json::from_str(cloned.xray_config.as_deref().ok_or("CDN 缺少 Xray 配置")?)
            .map_err(|e| e.to_string())?;
    for ib in xray["inbounds"].as_array_mut().ok_or("CDN 缺少入站")? {
        let stream = ib["streamSettings"]
            .as_object_mut()
            .ok_or("CDN 入站缺少传输配置")?;
        if stream.get("network").and_then(|v| v.as_str()) != Some("ws") {
            return Err("CDN 入站必须使用 WebSocket".into());
        }
        stream.insert("security".into(), serde_json::json!("none"));
        stream.remove("tlsSettings");
        stream.remove("realitySettings");
        if let Some(clients) = ib["settings"]["clients"].as_array_mut() {
            for client in clients {
                if let Some(c) = client.as_object_mut() {
                    c.remove("flow");
                }
            }
        }
    }
    cloned.xray_config = Some(serde_json::to_string_pretty(&xray).map_err(|e| e.to_string())?);
    for n in &mut cloned.share_nodes {
        let mut p: serde_json::Value =
            serde_json::from_str(&n.params_json).map_err(|e| e.to_string())?;
        p["network"] = "ws".into();
        p["path"] = format!("/cdn-ws/{}", n.port).into();
        p["host"] = cdn.domain.clone().into();
        p["sni"] = cdn.domain.clone().into();
        p["servername"] = cdn.domain.clone().into();
        p["tls"] = "tls".into();
        p["security"] = "tls".into();
        p["skip-cert-verify"] = false.into();
        p["allowInsecure"] = false.into();
        p["insecure"] = false.into();
        p["flow"] = "".into();
        n.addr = cdn.domain.clone();
        n.port = cdn.port.into();
        n.params_json = p.to_string();
    }
    cloned.yaml = crate::config::build_yaml_with_privacy(&cloned.share_nodes, false, &[], &[], &[])
        .map_err(|e| e.to_string())?
        .0;
    cloned.singbox_json =
        crate::config::singbox::build_singbox_with_privacy(&cloned.share_nodes, &[])
            .map_err(|e| e.to_string())?
            .0;
    Ok(())
}

pub fn certificate_check(domain: &str) -> String {
    // Domain is normalized before this command is constructed.
    format!("openssl x509 -checkend 0 -noout -in /etc/letsencrypt/live/{domain}/fullchain.pem && openssl x509 -checkhost {domain} -noout -in /etc/letsencrypt/live/{domain}/fullchain.pem && test -s /etc/letsencrypt/live/{domain}/privkey.pem")
}

pub fn certificate_install(domain: &str, dir: &str) -> String {
    format!("install -m 600 -o vpnshare -g vpnshare /etc/letsencrypt/live/{domain}/fullchain.pem {dir}/fullchain.pem && install -m 600 -o vpnshare -g vpnshare /etc/letsencrypt/live/{domain}/privkey.pem {dir}/privkey.pem")
}

pub fn http_unit(unit: String, manifest: &ShareManifest) -> String {
    if manifest.cdn.is_none() {
        return unit;
    }
    let dir = super::naming::share_dir(&manifest.id);
    unit.replace("/srv/vpnshare/tls/", &format!("{dir}/"))
        .replace(
            "NoNewPrivileges=yes",
            "AmbientCapabilities=CAP_NET_BIND_SERVICE\nNoNewPrivileges=yes",
        )
}

pub const RENEW_HOOK: &str = r#"#!/usr/bin/env python3
import glob,json,os,pathlib,shutil,subprocess
lineage=os.environ.get('RENEWED_LINEAGE','')
for f in glob.glob('/srv/vpnshare/*/share.json'):
    m=json.loads(pathlib.Path(f).read_text())
    cdn=m.get('cdn')
    if not cdn or lineage != '/etc/letsencrypt/live/'+cdn['domain']: continue
    d=pathlib.Path(f).parent
    for name in ('fullchain.pem','privkey.pem'):
        tmp=d/(name+'.renew')
        shutil.copyfile(pathlib.Path(lineage)/name,tmp)
        os.chmod(tmp,0o600)
        shutil.chown(tmp,user='vpnshare',group='vpnshare')
        os.replace(tmp,d/name)
    subprocess.run(['systemctl','try-restart','vpnshare-'+m['id']+'.service'],check=True)
"#;

#[cfg(all(test, feature = "local-tests"))]
#[path = "../../../tests/rust/share/cdn_tests.rs"]
mod tests;

// 外部测试模块（tests/rust/share/cdn_tests.rs）已占用 `tests` 名字，
// 内联用例改用独立模块名，避免 `--features local-tests` 下重名。
#[cfg(test)]
mod auto_candidates_tests {
    use super::*;

    #[test]
    fn auto_candidates_rotate_without_duplicates() {
        let v = auto_candidates(4);
        assert_eq!(v.len(), HTTPS_PORTS.len());
        assert_eq!(v[0], HTTPS_PORTS[4]);
        assert_eq!(v[1], HTTPS_PORTS[5]);
        assert_eq!(v[2], HTTPS_PORTS[0], "表尾必须回绕");
        let set: std::collections::HashSet<u16> = v.iter().copied().collect();
        assert_eq!(set.len(), HTTPS_PORTS.len(), "候选不得重复");
        assert!(v.iter().all(|p| HTTPS_PORTS.contains(p)));
    }
}
