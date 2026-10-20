//! 服务器端运行时脚本（Python/Shell）作为 crate 内嵌资源，避免发布额外文件。

pub const SHARE_SERVER_PY: &str = include_str!("scripts/share_server.py");
pub const SHARE_GATEWAY_PY: &str = include_str!("scripts/share_gateway.py");
pub const SHARE_SUMMARY_PY: &str = include_str!("scripts/share_summary.py");
pub const FIREWALL_SH: &str = include_str!("scripts/vpnshare-firewall.sh");

pub fn share_server_script() -> String {
    SHARE_SERVER_PY.to_string()
}
pub fn share_gateway_script() -> String {
    SHARE_GATEWAY_PY.to_string()
}
pub fn share_summary_script() -> String {
    SHARE_SUMMARY_PY.to_string()
}
pub fn firewall_script() -> String {
    FIREWALL_SH.to_string()
}
