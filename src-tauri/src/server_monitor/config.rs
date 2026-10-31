//! 监控配置：JSON 存取、默认值、逐字段覆盖与校验。
use super::CONFIG_KEY;
use crate::settings::SettingsService;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const DEFAULT_INTERVAL: u64 = 60;
const DEFAULT_CONCURRENCY: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonitorConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub defaults: Thresholds,
    #[serde(default)]
    pub carrier: CarrierConfig,
    #[serde(default)]
    pub servers: HashMap<String, ServerMonitorOverride>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: DEFAULT_INTERVAL,
            concurrency: DEFAULT_CONCURRENCY,
            defaults: Thresholds::default(),
            carrier: CarrierConfig::default(),
            servers: HashMap::new(),
        }
    }
}

/// 三网面板的统计窗口、格数与分级阈值。阈值不用 Komari 原值（60/100/160/200ms）：
/// 那是国内探国内的量级；境外服务器到国内实测 230–310ms，原值会整片最高级。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CarrierConfig {
    #[serde(default = "d_carrier_window")]
    pub window_secs: u64,
    #[serde(default = "d_carrier_bars")]
    pub bars: usize,
    #[serde(default = "d_carrier_latency_levels")]
    pub latency_levels_ms: [f64; 4],
    #[serde(default = "d_carrier_loss_levels")]
    pub loss_levels_pct: [f64; 4],
}

impl Default for CarrierConfig {
    fn default() -> Self {
        Self {
            window_secs: 3600,
            bars: 20,
            latency_levels_ms: [120.0, 200.0, 300.0, 400.0],
            loss_levels_pct: [1.0, 3.0, 6.0, 9.0],
        }
    }
}

fn d_carrier_window() -> u64 {
    3600
}
fn d_carrier_bars() -> usize {
    20
}
fn d_carrier_latency_levels() -> [f64; 4] {
    [120.0, 200.0, 300.0, 400.0]
}
fn d_carrier_loss_levels() -> [f64; 4] {
    [1.0, 3.0, 6.0, 9.0]
}

fn default_enabled() -> bool {
    true
}
fn default_interval() -> u64 {
    DEFAULT_INTERVAL
}
fn default_concurrency() -> usize {
    DEFAULT_CONCURRENCY
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Thresholds {
    #[serde(default = "d_cpu_pct")]
    pub cpu_pct: f64,
    #[serde(default = "d_mem_pct")]
    pub mem_pct: f64,
    #[serde(default = "d_disk_pct")]
    pub disk_pct: f64,
    #[serde(default = "d_disk_critical_pct")]
    pub disk_critical_pct: f64,
    #[serde(default = "d_load_per_core")]
    pub load_per_core: f64,
    #[serde(default = "d_breach_samples")]
    pub breach_samples: u32,
    #[serde(default = "d_clear_samples")]
    pub clear_samples: u32,
    #[serde(default = "d_offline_samples")]
    pub offline_samples: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            cpu_pct: 90.0,
            mem_pct: 90.0,
            disk_pct: 85.0,
            disk_critical_pct: 95.0,
            load_per_core: 2.0,
            breach_samples: 3,
            clear_samples: 2,
            offline_samples: 3,
        }
    }
}

fn d_cpu_pct() -> f64 {
    90.0
}
fn d_mem_pct() -> f64 {
    90.0
}
fn d_disk_pct() -> f64 {
    85.0
}
fn d_disk_critical_pct() -> f64 {
    95.0
}
fn d_load_per_core() -> f64 {
    2.0
}
fn d_breach_samples() -> u32 {
    3
}
fn d_clear_samples() -> u32 {
    2
}
fn d_offline_samples() -> u32 {
    3
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PartialThresholds {
    pub cpu_pct: Option<f64>,
    pub mem_pct: Option<f64>,
    pub disk_pct: Option<f64>,
    pub disk_critical_pct: Option<f64>,
    pub load_per_core: Option<f64>,
    pub breach_samples: Option<u32>,
    pub clear_samples: Option<u32>,
    pub offline_samples: Option<u32>,
}

impl PartialThresholds {
    pub fn overlay(&self, base: &Thresholds) -> Thresholds {
        Thresholds {
            cpu_pct: self.cpu_pct.unwrap_or(base.cpu_pct),
            mem_pct: self.mem_pct.unwrap_or(base.mem_pct),
            disk_pct: self.disk_pct.unwrap_or(base.disk_pct),
            disk_critical_pct: self.disk_critical_pct.unwrap_or(base.disk_critical_pct),
            load_per_core: self.load_per_core.unwrap_or(base.load_per_core),
            breach_samples: self.breach_samples.unwrap_or(base.breach_samples),
            clear_samples: self.clear_samples.unwrap_or(base.clear_samples),
            offline_samples: self.offline_samples.unwrap_or(base.offline_samples),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerMonitorOverride {
    pub enabled: Option<bool>,
    pub thresholds: Option<PartialThresholds>,
    #[serde(default)]
    pub carrier_region: Option<String>,
}

pub fn load(settings: &SettingsService) -> (MonitorConfig, Option<String>) {
    let raw = match settings.get(CONFIG_KEY) {
        Ok(v) => v,
        Err(e) => {
            return (
                MonitorConfig::default(),
                Some(format!("读取监控配置失败，已使用默认值: {e}")),
            )
        }
    };
    let Some(raw) = raw else {
        return (MonitorConfig::default(), None);
    };
    match serde_json::from_str::<MonitorConfig>(&raw) {
        Ok(cfg) => match validate(&cfg) {
            Ok(()) => (normalize(cfg), None),
            Err(e) => (
                MonitorConfig::default(),
                Some(format!("监控配置无效，已使用默认值: {e}")),
            ),
        },
        Err(e) => (
            MonitorConfig::default(),
            Some(format!("监控配置无法解析，已使用默认值: {e}")),
        ),
    }
}

pub fn save(settings: &SettingsService, cfg: &MonitorConfig) -> Result<MonitorConfig, String> {
    validate(cfg)?;
    let cfg = normalize(cfg.clone());
    let raw = serde_json::to_string(&cfg).map_err(|e| format!("序列化监控配置失败: {e}"))?;
    settings.set(CONFIG_KEY, &raw)?;
    Ok(cfg)
}

pub fn validate(cfg: &MonitorConfig) -> Result<(), String> {
    if !(30..=3600).contains(&cfg.interval_secs) {
        return Err("采集间隔必须在 30–3600 秒之间".into());
    }
    if !(1..=4).contains(&cfg.concurrency) {
        return Err("并发数必须在 1–4 之间".into());
    }
    validate_thresholds(&cfg.defaults)?;
    if !(600..=86400).contains(&cfg.carrier.window_secs) {
        return Err("三网统计窗口必须在 600–86400 秒之间".into());
    }
    if !(10..=60).contains(&cfg.carrier.bars) {
        return Err("三网格子数必须在 10–60 之间".into());
    }
    validate_levels(&cfg.carrier.latency_levels_ms, "三网延迟分级", 10_000.0)?;
    validate_levels(&cfg.carrier.loss_levels_pct, "三网失败率分级", 100.0)?;
    for (key, ov) in &cfg.servers {
        let id: i64 = key.parse().map_err(|_| format!("服务器 id 非法: {key}"))?;
        validate_thresholds(&effective_thresholds(cfg, id))?;
        if let Some(region) = &ov.carrier_region {
            if super::carrier::region_name(region).is_none() {
                return Err(format!("服务器 {key} 的三网地区无效: {region}"));
            }
        }
    }
    Ok(())
}

fn validate_levels(v: &[f64; 4], name: &str, max: f64) -> Result<(), String> {
    if v.iter().any(|x| !x.is_finite() || *x < 0.0 || *x > max) {
        return Err(format!("{name}必须是 0–{max} 之间的数"));
    }
    if !(v[0] < v[1] && v[1] < v[2] && v[2] < v[3]) {
        return Err(format!("{name}必须严格递增"));
    }
    Ok(())
}

fn validate_thresholds(t: &Thresholds) -> Result<(), String> {
    for (name, v) in [
        ("CPU", t.cpu_pct),
        ("内存", t.mem_pct),
        ("磁盘警告", t.disk_pct),
        ("磁盘严重", t.disk_critical_pct),
    ] {
        if !(1.0..=100.0).contains(&v) {
            return Err(format!("{name}阈值必须在 1–100 之间"));
        }
    }
    if t.disk_critical_pct <= t.disk_pct {
        return Err("磁盘严重阈值必须高于警告阈值".into());
    }
    if !(0.1..=100.0).contains(&t.load_per_core) {
        return Err("负载阈值必须在 0.1–100 之间".into());
    }
    for (name, v) in [
        ("连续越界次数", t.breach_samples),
        ("连续恢复次数", t.clear_samples),
        ("离线判定次数", t.offline_samples),
    ] {
        if !(1..=10).contains(&v) {
            return Err(format!("{name}必须在 1–10 之间"));
        }
    }
    Ok(())
}

pub fn effective_thresholds(cfg: &MonitorConfig, server_id: i64) -> Thresholds {
    match cfg.servers.get(&server_id.to_string()) {
        Some(ov) => match &ov.thresholds {
            Some(p) => p.overlay(&cfg.defaults),
            None => cfg.defaults.clone(),
        },
        None => cfg.defaults.clone(),
    }
}

pub fn server_enabled(cfg: &MonitorConfig, server_id: i64) -> bool {
    cfg.servers
        .get(&server_id.to_string())
        .and_then(|o| o.enabled)
        .unwrap_or(true)
}

/// 该台服务器配置的三网地区 code；None = 不测试。
pub fn carrier_region(cfg: &MonitorConfig, server_id: i64) -> Option<String> {
    cfg.servers
        .get(&server_id.to_string())
        .and_then(|o| o.carrier_region.clone())
}

/// 只保留有意义的覆盖条目，避免每次保存都把默认值写进 JSON。
fn normalize(mut cfg: MonitorConfig) -> MonitorConfig {
    cfg.servers.retain(|_, ov| {
        ov.enabled == Some(false) || ov.thresholds.is_some() || ov.carrier_region.is_some()
    });
    cfg
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/config_tests.rs"
    ));
}
