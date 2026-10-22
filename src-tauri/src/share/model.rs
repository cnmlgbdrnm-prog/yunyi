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
    /// 旧口径合计配额（仅旧分享使用；新分享的合计值也会写在这里便于展示）。
    pub quota_bytes: u64,
    pub used_bytes: u64,
    /// 上行/下行各自独立配额；0 = 该方向不限。
    pub quota_up_bytes: u64,
    pub quota_down_bytes: u64,
    pub used_up_bytes: u64,
    pub used_down_bytes: u64,
    /// true = 新口径（上下行分别判停）；false = 旧分享的合计口径。
    pub split_quota: bool,
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

#[allow(dead_code)] // 生产路径改用 ShareSnapshot；测试仍按列表形态断言
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

/// 后端定时采集的分享全量快照：服务器上的分享列表 + 各服务器证书状态。
///
/// 分享状态只存在服务器上，列举要逐台 SSH（实测一次 ~14 秒）。前端切到分享页
/// 只读这份缓存，不再在挂载/切页时触发远端查询；后端轮询完成后经
/// `share://status` 推送整份快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ShareSnapshot {
    pub shares: Vec<ShareInfo>,
    pub unreachable: Vec<String>,
    /// 每台可达服务器的证书状态，key = server id。证书读取与分享摘要共用
    /// 同一次 SSH 会话，不额外开连接。
    #[serde(default)]
    pub tls: std::collections::HashMap<i64, ShareTlsStatus>,
    /// 本轮采集完成时间（RFC3339）；None = 后端尚未完成第一轮。
    #[serde(default)]
    pub checked_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShareOptions {
    /// 上行配额；0 = 不限。
    pub quota_up_bytes: u64,
    /// 下行配额；0 = 不限。
    pub quota_down_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_in_days: u32,
}

/// 1 GB = 1024³（与 UI 文案一致）。
pub fn quota_gb_to_bytes(gb: u64) -> u64 {
    gb.saturating_mul(1024 * 1024 * 1024)
}

pub fn validate_options(o: &ShareOptions) -> Result<(), String> {
    // 每个方向 0 = 不限；两个方向都填 0（完全不限）也允许，分享仍受到期时间约束。
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
    /// 旧口径合计配额（旧分享必填；新分享写成上下行之和，仅供参考）。
    pub quota_bytes: u64,
    /// 新口径：上下行各自独立配额。两个字段都缺失 = 旧分享，仍按 quota_bytes 合计判停。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_up_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_down_bytes: Option<u64>,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at_unix: i64,
    pub http_port: u16,
    pub domain: String,
    pub nodes: Vec<ShareNode>,
}

impl ShareManifest {
    /// 是否使用上下行独立配额。任一字段存在即视为新口径；缺失的方向按 0（不限）处理。
    pub fn uses_split_quota(&self) -> bool {
        self.quota_up_bytes.is_some() || self.quota_down_bytes.is_some()
    }

    pub fn quota_up(&self) -> u64 {
        self.quota_up_bytes.unwrap_or(0)
    }

    pub fn quota_down(&self) -> u64 {
        self.quota_down_bytes.unwrap_or(0)
    }
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
    /// 旧口径合计配额；新口径下仍写上下行之和，仅供旧网关脚本兜底。
    pub quota_bytes: u64,
    /// 新口径：任一字段存在即启用上下行独立配额。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_up_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_down_bytes: Option<u64>,
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
    /// 新口径的分向用量。旧 usage 文件没有这两个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_up_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_down_bytes: Option<u64>,
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub blocked_at: Option<String>,
    /// 配额停用时是哪个方向超了："up" / "down"；旧口径或其它停用原因为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_direction: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

impl UsageState {
    /// 取出分向用量。旧 usage（无分向字段）遇到新口径时，把历史合计全部计入下行，
    /// 与网关的迁移规则保持一致，避免升级后凭空多出额度。
    pub fn direction_counts(&self, split_quota: bool) -> (u64, u64) {
        if self.used_up_bytes.is_some() || self.used_down_bytes.is_some() {
            (
                self.used_up_bytes.unwrap_or(0),
                self.used_down_bytes.unwrap_or(0),
            )
        } else if split_quota {
            (0, self.used_bytes)
        } else {
            (0, 0)
        }
    }
}
