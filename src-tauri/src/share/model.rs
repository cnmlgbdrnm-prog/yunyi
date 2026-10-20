//! 分享对外的数据模型：列表/状态、清单、网关配置与用量。
//!
//! 清单与用量是**服务器文件**的映射，不建本地表；本地只做 IPC 序列化。

use crate::domain::Node;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareInfo {
    pub id: String,
    pub server_id: i64,
    pub server_name: String,
    pub host: String,
    pub token: String,
    pub port: u16,
    pub active: bool,
    pub url: String,
    pub url_singbox: String,
    pub node_names: Vec<String>,
    pub legacy: bool,
    pub status: ShareStatus,
    pub status_detail: Option<String>,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at: Option<String>,
    pub last_access_at: Option<String>,
    pub last_access_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ShareStatus {
    Active,
    BlockedQuota,
    BlockedExpired,
    Degraded,
    Legacy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareList {
    pub shares: Vec<ShareInfo>,
    pub unreachable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareCandidate {
    pub node: Node,
    pub shareable: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareTlsStatus {
    pub domain: String,
    pub ready: bool,
    pub cert_expires_at: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShareOptions {
    pub quota_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_in_days: u32,
}

/// 1 GB = 1024³（与 UI 文案一致）。
pub fn quota_gb_to_bytes(gb: u64) -> u64 {
    gb.saturating_mul(1024 * 1024 * 1024)
}

pub fn validate_options(o: &ShareOptions) -> Result<(), String> {
    if o.quota_bytes == 0 {
        return Err("配额必须大于 0".into());
    }
    if o.rate_up_mbps > 100_000 || o.rate_down_mbps > 100_000 {
        return Err("速率上限过大".into());
    }
    if o.expires_in_days == 0 || o.expires_in_days > 3650 {
        return Err("有效期必须在 1-3650 天之间".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareManifest {
    pub version: u32,
    pub id: String,
    pub token: String,
    pub server_id: i64,
    pub created_at: String,
    pub quota_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at_unix: i64,
    pub http_port: u16,
    pub domain: String,
    pub nodes: Vec<ShareNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareNode {
    pub name: String,
    pub protocol: String,
    pub public_port: u16,
    pub internal_port: u16,
    pub transport: String,
    pub params_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayConfig {
    pub quota_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at_unix: i64,
    pub ports: Vec<GatewayPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayPort {
    pub public: u16,
    pub target: u16,
    pub proto: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageState {
    #[serde(default)]
    pub used_bytes: u64,
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub blocked_at: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}
