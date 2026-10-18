//! 设置服务：settings 表读写与默认值。sub_token 首次启动随机生成一次后持久化。
//! 读写失败显式返回错误字符串，由上层（AppState 装配 / 命令层）决定如何处理。

use crate::db::{repos, Db};
use std::sync::Arc;

/// 默认订阅服务端口。
pub const DEFAULT_SUB_PORT: &str = "8731";
/// 导出订阅是否携带智能分流规则（"1" 开，"0" 关）。
pub const DEFAULT_SMART_SPLIT: &str = "1";

/// 设置服务：薄封装 settings 表。
pub struct SettingsService {
    db: Arc<Db>,
}

impl SettingsService {
    /// 构造并补齐默认值：订阅服务端口与一次性生成的 token。
    pub fn new(db: Arc<Db>) -> Result<Self, String> {
        let svc = Self { db };
        // 默认值：订阅服务 token 首次生成后保持稳定
        if svc.get("sub_port")?.is_none() {
            svc.set("sub_port", DEFAULT_SUB_PORT)?;
        }
        if svc.get("sub_token")?.is_none() {
            let token = uuid::Uuid::new_v4().simple().to_string();
            svc.set("sub_token", &token)?;
        }
        if svc.get("export_smart_split")?.is_none() {
            svc.set("export_smart_split", DEFAULT_SMART_SPLIT)?;
        }
        // 泄漏关注名单：缺省即「实测漏过的那些」。用**插入默认值**而不是读侧兜底，
        // 是为了让用户在设置里看得见、改得动这份名单（读侧另有兜底，清空不等于失效）。
        if svc.get(crate::net::connlog::NET_LEAK_WATCH_KEY)?.is_none() {
            svc.set(
                crate::net::connlog::NET_LEAK_WATCH_KEY,
                crate::net::connlog::DEFAULT_LEAK_WATCH,
            )?;
        }
        Ok(svc)
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        repos::settings::get(&self.db.lock(), key).map_err(|e| e.to_string())
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        repos::settings::set(&self.db.lock(), key, value).map_err(|e| e.to_string())
    }
}
