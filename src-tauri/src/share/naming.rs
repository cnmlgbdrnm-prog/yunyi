//! 分享的命名规则：内部 id、URL token、服务器路径与 systemd 单元名。
//!
//! 内部 id 与 URL token **分离**：id 只出现在路径、单元名和本地接口里（16 位十六
//! 进制），token 只出现在订阅 URL 里（新分享 32 位）。这样列目录/看单元名不会直接
//! 拿到可用的订阅凭证；旧分享的 16 位 token 仍按 id=token 兼容。

pub const SHARE_ROOT: &str = "/srv/vpnshare";
pub const UNIT_PREFIX: &str = "vpnshare";

/// 订阅服务端口段（HTTPS）。
pub const PORT_RANGE_START: u16 = 18080;
pub const PORT_RANGE_END: u16 = 18180;
/// 数据面公网端口段（网关监听）。
pub const DATA_PORT_START: u16 = 18200;
pub const DATA_PORT_END: u16 = 18499;
/// 回环内部端口段（分享实例监听）。
pub const INTERNAL_PORT_START: u16 = 18500;
pub const INTERNAL_PORT_END: u16 = 18799;

/// 16 位小写十六进制：内部 id，也是旧分享的 token。
pub fn is_valid_id(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// 16 位（旧）或 32 位（新）小写十六进制。
pub fn is_valid_token(s: &str) -> bool {
    (s.len() == 16 || s.len() == 32)
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn share_dir(id: &str) -> String {
    format!("{SHARE_ROOT}/{id}")
}
pub fn manifest_path(id: &str) -> String {
    format!("{SHARE_ROOT}/{id}/share.json")
}
pub fn gateway_config_path(id: &str) -> String {
    format!("{SHARE_ROOT}/{id}/gateway.json")
}
pub fn usage_path(id: &str) -> String {
    format!("{SHARE_ROOT}/{id}/usage.json")
}
pub fn yaml_path(id: &str, token: &str) -> String {
    format!("{SHARE_ROOT}/{id}/sub/{token}")
}
pub fn singbox_path(id: &str, token: &str) -> String {
    format!("{SHARE_ROOT}/{id}/sub/{token}.json")
}
pub fn instance_config_path(id: &str, kind: &str) -> String {
    format!("{SHARE_ROOT}/{id}/{kind}.json")
}
pub fn instance_unit_name(id: &str, kind: &str) -> String {
    format!("{UNIT_PREFIX}-{id}-{kind}")
}
pub fn gateway_unit_name(id: &str) -> String {
    format!("{UNIT_PREFIX}-{id}-gateway")
}
pub fn unit_name(id: &str) -> String {
    format!("{UNIT_PREFIX}-{id}")
}
pub fn unit_path(unit: &str) -> String {
    format!("/etc/systemd/system/{unit}.service")
}
pub fn tls_dir() -> String {
    format!("{SHARE_ROOT}/tls")
}
pub fn share_server_script_path() -> String {
    format!("{SHARE_ROOT}/share_server.py")
}
pub fn share_gateway_script_path() -> String {
    format!("{SHARE_ROOT}/share_gateway.py")
}
pub fn share_summary_script_path() -> String {
    format!("{SHARE_ROOT}/share_summary.py")
}
pub fn firewall_script_path() -> String {
    format!("{SHARE_ROOT}/vpnshare-firewall.sh")
}

pub fn url_yaml(domain: &str, port: u16, token: &str) -> String {
    format!("https://{domain}:{port}/sub/{token}")
}
pub fn url_singbox(domain: &str, port: u16, token: &str) -> String {
    format!("https://{domain}:{port}/singbox/{token}")
}

/// 旧分享的明文 HTTP URL；IPv6 字面量必须加方括号。
pub fn legacy_url(host: &str, port: u16, token: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("http://{host}:{port}/sub/{token}")
}
