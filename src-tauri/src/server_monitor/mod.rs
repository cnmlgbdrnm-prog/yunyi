//! 服务器健康监控：应用内调度、只读 SSH 采集、样本/告警存储与 IPC 类型。
//! 子模块随任务逐个加入（T1 只声明 config）；每新增一个文件都要在这里补一行，
//! 否则后面的任务会在模块文件尚未创建时编译失败。
pub mod alerts;
pub mod config;
pub mod parse;
pub mod probe;
pub mod scheduler;
pub mod store;
pub mod summary;

pub use scheduler::{collect_now, start, stop, MonitorEmit};

pub use config::{MonitorConfig, Thresholds};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 调度器与摘要共享的运行态：摘要只依赖这个类型，不依赖 scheduler，
/// 因此 T5 可以先于调度器落地并独立测试。
#[derive(Default)]
pub struct MonitorStatus {
    pub collecting: std::sync::atomic::AtomicBool,
    pub last_round_error: std::sync::Mutex<Option<String>>,
    pub local_errors: std::sync::Mutex<std::collections::HashMap<i64, String>>,
}

pub const CONFIG_KEY: &str = "server_monitor_config";
pub const SAMPLE_RETENTION_DAYS: i64 = 7;
pub const ALERT_RETENTION_DAYS: i64 = 90;
/// 服务与 WireGuard 是状态迁移，不跟 CPU/内存的连续次数配置走。
pub const TRANSITION_SAMPLES: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MonitorSample {
    pub server_id: i64,
    pub ts: String,
    pub reachable: bool,
    pub connect_ms: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub cpu_pct: Option<f64>,
    pub cpu_cores: Option<u32>,
    pub mem_used_kb: Option<u64>,
    pub mem_total_kb: Option<u64>,
    pub disk_used_kb: Option<u64>,
    pub disk_total_kb: Option<u64>,
    pub bbr_algo: Option<String>,
    /// None = 本轮无法得知；空 map = 确认三个 unit 都不是 active。
    pub services: Option<BTreeMap<String, String>>,
    /// None = 无法得知；Some([]) = 确认没有接口。
    pub wg_interfaces: Option<Vec<String>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerHealth {
    pub server_id: i64,
    pub name: String,
    pub host: String,
    pub monitored: bool,
    pub online: Option<bool>,
    pub stale: bool,
    pub last_sample_at: Option<String>,
    pub expected_services: Vec<String>,
    pub deployment_known: bool,
    pub wg_expected: bool,
    pub sample: Option<MonitorSample>,
    pub local_error: Option<String>,
    pub active_alerts: usize,
    pub unacked_alerts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonitorAlert {
    pub id: i64,
    pub server_id: i64,
    pub server_name: String,
    pub kind: String,
    pub severity: String,
    pub opened_at: String,
    pub resolved_at: Option<String>,
    pub acked_at: Option<String>,
    pub last_value: Option<String>,
    pub last_at: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonitorSummary {
    pub config: MonitorConfig,
    pub config_error: Option<String>,
    pub last_round_at: Option<String>,
    pub last_round_error: Option<String>,
    pub collecting: bool,
    pub servers: Vec<ServerHealth>,
    pub active_alerts: Vec<MonitorAlert>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonitorPoint {
    pub ts: String,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorMetric {
    CpuPct,
    MemPct,
    DiskPct,
    Load1,
    ConnectMs,
}

impl MonitorMetric {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "cpu_pct" => Ok(Self::CpuPct),
            "mem_pct" => Ok(Self::MemPct),
            "disk_pct" => Ok(Self::DiskPct),
            "load1" => Ok(Self::Load1),
            "connect_ms" => Ok(Self::ConnectMs),
            other => Err(format!("未知的监控指标: {other}")),
        }
    }
}

/// 远端文本只用于展示与排查：去控制字符、压空白、截断 200 字符。
pub fn sanitize_text(s: &str) -> String {
    let one_line: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let squashed = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
    squashed.chars().take(200).collect()
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/mods_tests.rs"
    ));
}
