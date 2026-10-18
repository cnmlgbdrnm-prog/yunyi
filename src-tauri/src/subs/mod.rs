//! 订阅域模块：拉取、格式识别、解析、过滤、入库与合并配置刷新。

pub mod parse;
pub mod service;

use serde::Serialize;
use thiserror::Error;

/// 订阅域错误。
#[derive(Debug, Error)]
pub enum SubError {
    #[error("不支持的订阅格式")]
    UnsupportedFormat,
    #[error("解析失败: {0}")]
    ParseFailed(String),
    #[error("网络请求失败: {0}")]
    FetchFailed(String),
}

/// 订阅同步结果。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SyncOutcome {
    pub upserted: usize,
    pub removed: usize,
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/subs/mods.rs"
));
