# 服务器健康监控（应用内）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. 执行中回写「执行状态」段。

**Goal Contract**

- **Original Goal:** 用户从功能盘点中选择「服务器健康监控」，范围「只做应用内」，指标「资源+服务+趋势 (Recommended)」；确认设计后说「可以，写吧」，随后说「先做吧！」。
- **Hard Constraints:**
  - 只在桌面端应用打开期间采集；应用关闭即停止；不做 24×7、无界面常驻、Windows 计划任务。
  - 不做外部通知（Toast/邮件/Webhook/Telegram），不做服务器写操作，不新增监听端口，不上传数据。
  - 不引入任何新的 npm / cargo 依赖；趋势图必须是纯 SVG。
  - 迁移必须是 v17；样本保留 7 天、已恢复告警保留 90 天；配置键为 `server_monitor_config`。
  - 默认值逐字保留：间隔 60s、并发 2、CPU 90%、内存 90%、磁盘 85%/95%、负载每核 2.0、`breach_samples` 3、`clear_samples` 2、`offline_samples` 3、`TRANSITION_SAMPLES` 2。
  - SSH 连接超时 10s、命令超时 10s；每轮每台一次 SSH 会话；远端脚本只读、不 sudo。
  - 单文件 ≤1500 行、UI 组件 ≤500 行；`db/repos.rs`（1446 行）与 `commands/mod.rs`（1475 行）不得新增业务逻辑；`commands/mod.rs` 只加 `mod monitor; pub use monitor::*;`。
  - 监控的 DB 代码放 `server_monitor/store.rs`；命令业务放 `commands/monitor.rs`。
  - 测试源码放 `tests/`（不随 git 上传），只在 `--features local-tests` 下编译；提交时只加源码，不加测试文件。
  - UI 文案全中文；在线/告警状态不能只靠颜色表达。
  - 凭据、私钥、密码不得进入样本、告警、日志或事件；错误文本单行且 ≤200 字符。
  - 应用退出时调度器 ≤2 秒停止；退出后不得再产生样本。
  - ≥2 台被探测服务器全部连接失败时，本轮不写样本、不改告警（本机网络疑似异常守卫）。
  - 服务与 WireGuard 固定连续 `TRANSITION_SAMPLES=2`，不随数值阈值配置改变。
  - 同一台服务器同一类型最多一条活动告警；部分唯一索引兜底。
  - 不做服务器页状态点、告警静默时段、证书/域名到期、延迟/丢包趋势。

- **Acceptance Criteria:**
  - A1: 迁移后 `PRAGMA user_version=17`；重复打开不报错；旧表数据保留；删除服务器级联删除样本与告警。
  - A2: 监控页对每台服务器展示在线状态、SSH 连接耗时、CPU、内存、磁盘、负载、运行时长、核数、BBR、三个服务状态、WG 接口、最近样本时间与错误；真机读数与对应 SSH 命令一致。
  - A3: 自动轮次按配置间隔运行，单轮并发不超过配置值且轮次不重叠；手动采集可用，并与自动轮次互斥。
  - A4: 趋势支持 1 小时/24 小时/7 天，返回 ≤360 点；离线空档断线；五种指标均可查看。
  - A5: 七类告警按规格工作：打开/恢复/级别调整/确认；同类活动告警唯一；自动化测试覆盖。
  - A6: 全不可达守卫：≥2 台全部连接失败时不写样本、不产生离线告警，UI 有明确提示。
  - A7: 暂停/恢复、单台停用、配置持久化、坏配置回退、非法保存拒绝均符合规格。
  - A8: 应用退出时调度器 ≤2 秒停止，退出后不再产生样本；现有启动/退出/网络收尾不受影响。
  - A9: 无新增 npm/cargo 依赖；`cargo fmt --check`、`cargo clippy`、`npm run test:typecheck`、`npm test`、`npm run build`、`check:org`、`check:size`、`check:sensitive` 通过。
  - A10: 日志、事件、数据库和 UI 中不出现凭据、私钥或密码；错误文本单行且 ≤200 字符。

**Architecture:** 后端新增 `server_monitor` 模块：调度器按间隔并发建立只读 SSH 会话，解析资源/服务/WG 输出，一次短事务写入样本并驱动纯函数告警状态机；前端由单一事件 store 驱动导航角标、监控页和仪表盘卡片。采集只在应用运行时发生，不引入常驻进程或新依赖。

**Tech Stack:** Tauri 2 / Rust 2021 / rusqlite（bundled）/ tokio / chrono / serde / async-trait / russh；React 19 / TypeScript / Tailwind 4 / Vitest / Testing Library；纯 SVG 图表。

## 模块地图（ASCII）

**层级（包含关系）**

```
App（应用根组件）
├─ Nav（左侧导航 + 监控未确认角标）
├─ Dashboard（仪表盘）
│  └─ ServerHealthCard（服务器健康统计卡）
└─ MonitorView（监控页）
   ├─ MonitorHeader（页头：上次采集/立即采集/设置）
   ├─ AlertBanner（活动告警条）
   ├─ ServerCard（服务器健康卡片）
   │  ├─ MetricBar（CPU/内存/磁盘资源条）
   │  ├─ ServiceBadges（xray/ssserver/singbox 徽章）
   │  └─ TrendPanel（趋势面板）
   │     └─ LineChart（纯 SVG 折线图）
   ├─ AlertHistory（告警历史表）
   └─ MonitorSettingsDrawer（监控设置抽屉）
```

**调用（运行时触发）**

```
App ──▶ installMonitorListener（安装一次 monitor:// 事件监听）
App ──▶ refreshMonitor（启动拉一次摘要）
MonitorView ──▶ monitorCollect（手动采集一轮）
MonitorView ──▶ monitorAck（确认活动告警）
TrendPanel ──▶ monitorHistory（按服务器/指标/范围取趋势）
AlertHistory ──▶ monitorAlerts（取告警历史）
MonitorSettingsDrawer ──▶ monitorSetConfig（校验并保存配置）
Scheduler ──▶ monitor://tick / monitor://collecting ──▶ RegistryStore ──▶ App / Dashboard / MonitorView
```

**依赖（数据 / 契约来源）**

```
RegistryStore ⇠── api/types.ts（MonitorSummary / MonitorSample / MonitorAlert）
ServerCard ⇠── MonitorSummary.servers（ServerHealth）
TrendPanel ⇠── MonitorPoint[] + LineChart
MonitorSettingsDrawer ⇠── MonitorConfig / MonitorThresholds
Scheduler ⇠── MonitorProbe（probe.rs）+ parse.rs + alerts.rs + store.rs
AlertMachine ⇠── MonitorSample 序列 + 活动告警 + EffectiveThresholds
Store ⇠── migration v17（server_metric_samples / server_alerts）
```

## 执行状态（进度指针）

- 当前：T3（pending）
- 下一动作：实现采集脚本输出解析器
- 阻塞：无
- 已完成：T1、T2
- 最后更新：2026-10-19

---

## T1: 监控类型与配置解析

- **Depends on:** 无

**Files:**
- Create: `src-tauri/src/server_monitor/mod.rs`
- Create: `src-tauri/src/server_monitor/config.rs`
- Modify: `src-tauri/src/main.rs`（只加 `mod server_monitor;`）
- Test: `tests/rust/server_monitor/config_tests.rs`

**Interfaces:**
- Consumes: `crate::settings::SettingsService`（`get`/`set`）。
- Produces:
  - `MonitorSample`、`ServerHealth`、`MonitorSummary`、`MonitorAlert`、`MonitorPoint`、`MonitorMetric`、`sanitize_text`（`mod.rs`）
  - `MonitorConfig`、`Thresholds`、`PartialThresholds`、`ServerMonitorOverride`
  - `config::load(&SettingsService) -> (MonitorConfig, Option<String>)`
  - `config::save(&SettingsService, &MonitorConfig) -> Result<MonitorConfig, String>`
  - `config::effective_thresholds(&MonitorConfig, i64) -> Thresholds`
  - `config::server_enabled(&MonitorConfig, i64) -> bool`
  - 常量 `CONFIG_KEY`、`SAMPLE_RETENTION_DAYS`、`ALERT_RETENTION_DAYS`、`TRANSITION_SAMPLES`

- **验收:** `cargo test --features local-tests server_monitor::config -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；`cargo check` 通过。

- **状态:** completed ｜ **证据:** commit `c0777e2`；config 测试 5/5、server_monitor 模块 7/7 通过，`cargo check` 通过；任务审查 Spec ✅ / Task quality Approved（5 条 Minor 已登记 SDD ledger）

- [ ] **Step 1: 写失败的配置测试**

创建 `tests/rust/server_monitor/config_tests.rs`：

```rust
use super::*;
use crate::db::Db;
use crate::settings::SettingsService;
use std::sync::Arc;
use tempfile::tempdir;

fn settings() -> (tempfile::TempDir, SettingsService) {
    let dir = tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("test.db")).unwrap());
    (dir, SettingsService::new(db).unwrap())
}

#[test]
fn missing_config_uses_built_in_defaults() {
    let (_dir, s) = settings();
    let (cfg, err) = load(&s);
    assert!(err.is_none());
    assert_eq!(cfg, MonitorConfig::default());
    assert_eq!(cfg.interval_secs, 60);
    assert_eq!(cfg.concurrency, 2);
    assert_eq!(cfg.defaults.cpu_pct, 90.0);
    assert_eq!(cfg.defaults.disk_critical_pct, 95.0);
    assert_eq!(cfg.defaults.offline_samples, 3);
}

#[test]
fn partial_override_inherits_global_defaults() {
    let (_dir, s) = settings();
    s.set(
        CONFIG_KEY,
        r#"{"defaults":{"cpu_pct":80.0},"servers":{"7":{"enabled":false,"thresholds":{"mem_pct":70.0}}}}"#,
    )
    .unwrap();
    let (cfg, err) = load(&s);
    assert!(err.is_none(), "{err:?}");
    assert_eq!(cfg.defaults.cpu_pct, 80.0);
    assert_eq!(cfg.defaults.mem_pct, 90.0);
    let t = effective_thresholds(&cfg, 7);
    assert_eq!(t.cpu_pct, 80.0);
    assert_eq!(t.mem_pct, 70.0);
    assert_eq!(t.disk_pct, 85.0);
    assert!(!server_enabled(&cfg, 7));
    assert!(server_enabled(&cfg, 8));
}

#[test]
fn invalid_json_and_invalid_values_fall_back_with_error() {
    let (_dir, s) = settings();
    s.set(CONFIG_KEY, "not json").unwrap();
    let (cfg, err) = load(&s);
    assert_eq!(cfg, MonitorConfig::default());
    assert!(err.unwrap().contains("无法解析"));

    s.set(CONFIG_KEY, r#"{"interval_secs":5}"#).unwrap();
    let (cfg, err) = load(&s);
    assert_eq!(cfg, MonitorConfig::default());
    assert!(err.unwrap().contains("30–3600"));
}

#[test]
fn save_rejects_bad_thresholds_and_normalizes_default_entries() {
    let (_dir, s) = settings();
    let mut cfg = MonitorConfig::default();
    cfg.defaults.disk_critical_pct = 80.0; // <= warning 85
    assert!(save(&s, &cfg).unwrap_err().contains("严重阈值"));

    cfg.defaults.disk_critical_pct = 95.0;
    cfg.servers.insert("1".into(), ServerMonitorOverride::default());
    cfg.servers.insert(
        "2".into(),
        ServerMonitorOverride { enabled: Some(false), thresholds: None },
    );
    let saved = save(&s, &cfg).unwrap();
    assert!(!saved.servers.contains_key("1"), "全默认条目应被丢弃");
    assert_eq!(saved.servers["2"].enabled, Some(false));
    let raw = s.get(CONFIG_KEY).unwrap().unwrap();
    let round: MonitorConfig = serde_json::from_str(&raw).unwrap();
    assert_eq!(round, saved);
}

#[test]
fn unknown_server_ids_are_ignored_by_effective_lookup() {
    let cfg = MonitorConfig::default();
    assert_eq!(effective_thresholds(&cfg, 999), cfg.defaults);
    assert!(server_enabled(&cfg, 999));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::config -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，`server_monitor` 模块不存在。

- [ ] **Step 3: 写 `server_monitor/mod.rs` 的公共类型**

```rust
//! 服务器健康监控：应用内调度、只读 SSH 采集、样本/告警存储与 IPC 类型。
//! 子模块随任务逐个加入（T1 只声明 config）；每新增一个文件都要在这里补一行，
//! 否则后面的任务会在模块文件尚未创建时编译失败。
pub mod config;

pub use config::{MonitorConfig, PartialThresholds, ServerMonitorOverride, Thresholds};

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
```

`tests/rust/server_monitor/mods_tests.rs`：

```rust
use super::*;

#[test]
fn sanitize_text_collapses_controls_and_truncates() {
    let raw = format!("a\n\tb   c{}", "x".repeat(300));
    let out = sanitize_text(&raw);
    assert!(!out.contains('\n'));
    assert!(out.starts_with("a b c"));
    assert_eq!(out.chars().count(), 200);
}

#[test]
fn metric_parse_accepts_only_known_names() {
    assert_eq!(MonitorMetric::parse("cpu_pct"), Ok(MonitorMetric::CpuPct));
    assert!(MonitorMetric::parse("cpu").is_err());
}
```

- [ ] **Step 4: 写 `server_monitor/config.rs`**

```rust
//! 监控配置：JSON 存取、默认值、逐字段覆盖与校验。
use crate::settings::SettingsService;
use super::CONFIG_KEY;
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
    pub servers: HashMap<String, ServerMonitorOverride>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: DEFAULT_INTERVAL,
            concurrency: DEFAULT_CONCURRENCY,
            defaults: Thresholds::default(),
            servers: HashMap::new(),
        }
    }
}

fn default_enabled() -> bool { true }
fn default_interval() -> u64 { DEFAULT_INTERVAL }
fn default_concurrency() -> usize { DEFAULT_CONCURRENCY }

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

fn d_cpu_pct() -> f64 { 90.0 }
fn d_mem_pct() -> f64 { 90.0 }
fn d_disk_pct() -> f64 { 85.0 }
fn d_disk_critical_pct() -> f64 { 95.0 }
fn d_load_per_core() -> f64 { 2.0 }
fn d_breach_samples() -> u32 { 3 }
fn d_clear_samples() -> u32 { 2 }
fn d_offline_samples() -> u32 { 3 }

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
    for key in cfg.servers.keys() {
        let id: i64 = key
            .parse()
            .map_err(|_| format!("服务器 id 非法: {key}"))?;
        validate_thresholds(&effective_thresholds(cfg, id))?;
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

/// 只保留有意义的覆盖条目，避免每次保存都把默认值写进 JSON。
fn normalize(mut cfg: MonitorConfig) -> MonitorConfig {
    cfg.servers.retain(|_, ov| {
        ov.enabled == Some(false) || ov.thresholds.is_some()
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
```

- [ ] **Step 5: 在 `main.rs` 声明模块并运行测试**

在 `src-tauri/src/main.rs` 的模块声明区加：

```rust
mod server_monitor;
```

Run:
`cargo test --features local-tests server_monitor::config -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 5 个配置测试通过。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/main.rs src-tauri/src/server_monitor/mod.rs src-tauri/src/server_monitor/config.rs
git commit -m "feat(monitor): 增加监控类型与配置解析"
```

---

## T2: 迁移 v17 与样本存储

- **Depends on:** T1

**Files:**
- Modify: `src-tauri/src/db/migrations.rs`
- Modify: `tests/rust/db/tests.rs:1216`（v16 版本断言改为 `>= 16`）
- Create: `src-tauri/src/server_monitor/store.rs`
- Test: `tests/rust/server_monitor/store_tests.rs`

**Interfaces:**
- Consumes: `MonitorSample`、`MonitorMetric`、`MonitorPoint`。
- Produces:
  - `store::insert_sample(&rusqlite::Connection, &MonitorSample) -> Result<(), String>`
  - `store::latest_sample(&Connection, i64) -> Result<Option<MonitorSample>, String>`
  - `store::recent_samples(&Connection, i64, usize) -> Result<Vec<MonitorSample>, String>`（旧→新）
  - `store::last_reachable_sample(&Connection, i64) -> Result<Option<MonitorSample>, String>`
  - `store::last_nonempty_wg_sample(&Connection, i64) -> Result<Option<MonitorSample>, String>`
  - `store::max_ts(&Connection) -> Result<Option<String>, String>`
  - `store::history(&Connection, i64, MonitorMetric, &str, i64) -> Result<Vec<MonitorPoint>, String>`
  - `store::prune(&Connection, &str, &str) -> Result<(), String>`
  - `store::wg_expected_server_ids(&Connection) -> Result<std::collections::HashSet<i64>, String>`

- **验收:** `cargo test --features local-tests server_monitor::store -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；`migrations::latest_version()` 随迁移条数变为 17。

- **状态:** completed ｜ **证据:** commit `325d5a6`；store 测试 6/6、全量 896 通过（net 跳过），`user_version=17`；任务审查 Spec ✅ / Task quality Approved（6 条 Minor 已登记 SDD ledger）

- [ ] **Step 1: 写失败的存储测试**

创建 `tests/rust/server_monitor/store_tests.rs`：

```rust
use super::*;
use crate::db::Db;
use crate::domain::Server;
use crate::server_monitor::store;
use tempfile::tempdir;

fn db() -> (tempfile::TempDir, Db) {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("test.db")).unwrap();
    (dir, db)
}

fn insert_server(db: &Db, id: i64) {
    let s = Server {
        id,
        name: format!("测试-{id}"),
        host: "127.0.0.1".into(),
        host_v6: String::new(),
        addr_family: "v4".into(),
        egress_family: "v4".into(),
        port: 22,
        user: "root".into(),
        auth_kind: "password".into(),
        kind: "landing".into(),
        system: String::new(),
        notes: String::new(),
        created_at: "2026-10-19T00:00:00.000Z".into(),
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
    };
    crate::db::repos::servers::insert(&db.lock(), &s, "dummy").unwrap();
}

fn sample(server_id: i64, ts: &str, cpu: Option<f64>) -> MonitorSample {
    MonitorSample {
        server_id,
        ts: ts.into(),
        reachable: true,
        connect_ms: Some(42),
        cpu_pct: cpu,
        cpu_cores: Some(4),
        mem_used_kb: Some(50),
        mem_total_kb: Some(100),
        disk_used_kb: Some(50),
        disk_total_kb: Some(100),
        load1: Some(0.5),
        ..Default::default()
    }
}

#[test]
fn migration_is_v17_and_samples_roundtrip() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    let v: i64 = db
        .lock()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 17);
    let mut s = sample(1, "2026-10-19T00:00:00.000Z", Some(12.5));
    s.services = Some(std::collections::BTreeMap::from([("xray".into(), "active".into())]));
    s.wg_interfaces = Some(vec!["wg0".into()]);
    store::insert_sample(&db.lock(), &s).unwrap();
    let got = store::latest_sample(&db.lock(), 1).unwrap().unwrap();
    assert_eq!(got.cpu_pct, Some(12.5));
    assert_eq!(got.services.unwrap()["xray"], "active");
    assert_eq!(got.wg_interfaces, Some(vec!["wg0".into()]));
}

#[test]
fn nullable_unknown_services_and_wg_are_not_empty_values() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    let mut s = sample(1, "2026-10-19T00:00:00.000Z", None);
    s.services = None;
    s.wg_interfaces = None;
    store::insert_sample(&db.lock(), &s).unwrap();
    let got = store::latest_sample(&db.lock(), 1).unwrap().unwrap();
    assert!(got.services.is_none());
    assert!(got.wg_interfaces.is_none());
}

#[test]
fn history_buckets_and_computes_percentages() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    for (i, cpu) in [10.0, 20.0, 30.0].into_iter().enumerate() {
        let ts = format!("2026-10-19T00:0{i}:00.000Z");
        store::insert_sample(&db.lock(), &sample(1, &ts, Some(cpu))).unwrap();
    }
    let points = store::history(
        &db.lock(),
        1,
        MonitorMetric::CpuPct,
        "2026-10-18T00:00:00.000Z",
        60,
    )
    .unwrap();
    assert_eq!(points.len(), 3);
    assert!((points[0].value - 10.0).abs() < 0.001);

    let mem = store::history(
        &db.lock(),
        1,
        MonitorMetric::MemPct,
        "2026-10-18T00:00:00.000Z",
        60,
    )
    .unwrap();
    assert!((mem[0].value - 50.0).abs() < 0.001, "used/total = 50%");
}

#[test]
fn prune_removes_old_samples_but_keeps_active_alerts() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    store::insert_sample(&db.lock(), &sample(1, "2020-01-01T00:00:00.000Z", Some(1.0))).unwrap();
    store::insert_sample(&db.lock(), &sample(1, "2026-10-19T00:00:00.000Z", Some(2.0))).unwrap();
    {
        let conn = db.lock();
        // 老但仍在活动的告警：resolved_at IS NULL，必须保留。
        conn.execute(
            "INSERT INTO server_alerts
             (server_id,kind,severity,opened_at,last_at,message)
             VALUES (1,'cpu','warning','2020-01-01T00:00:00.000Z','2020-01-01T00:00:00.000Z','active')",
            [],
        )
        .unwrap();
        // 老且已恢复的告警：超过保留期，应删除。
        conn.execute(
            "INSERT INTO server_alerts
             (server_id,kind,severity,opened_at,resolved_at,last_at,message)
             VALUES (1,'disk','warning','2020-01-01T00:00:00.000Z',
                     '2020-01-01T00:00:00.000Z','2020-01-01T00:00:00.000Z','resolved')",
            [],
        )
        .unwrap();
    }
    store::prune(
        &db.lock(),
        "2026-01-01T00:00:00.000Z",
        "2021-01-01T00:00:00.000Z",
    )
    .unwrap();
    assert_eq!(
        store::latest_sample(&db.lock(), 1)
            .unwrap()
            .unwrap()
            .cpu_pct,
        Some(2.0)
    );
    let conn = db.lock();
    let active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM server_alerts WHERE resolved_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let resolved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM server_alerts WHERE resolved_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 1);
    assert_eq!(resolved, 0);
}

#[test]
fn wg_expected_comes_only_from_built_or_imported_rules() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    insert_server(&db, 2);
    insert_server(&db, 3);
    {
        let conn = db.lock();
        conn.execute(
            "INSERT INTO relay_rules
             (name,relay_server_id,landing_server_id,client_mode,wg_port,landing_port,
              subnet,relay_wg_ip,landing_wg_ip,converged,status)
             VALUES ('r1',1,2,'forward',51820,8388,'10.66.66.0/30','10.66.66.1','10.66.66.2',1,'built')",
            [],
        )
        .unwrap();
    }
    let ids = store::wg_expected_server_ids(&db.lock()).unwrap();
    assert!(ids.contains(&1) && ids.contains(&2));
    assert!(!ids.contains(&3));
}

#[test]
fn deleting_server_cascades_samples() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    store::insert_sample(&db.lock(), &sample(1, "2026-10-19T00:00:00.000Z", Some(1.0))).unwrap();
    db.lock().execute("DELETE FROM servers WHERE id=1", []).unwrap();
    assert!(store::latest_sample(&db.lock(), 1).unwrap().is_none());
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::store -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败或表不存在。

- [ ] **Step 3: 在 `MIGRATIONS` 末尾追加 v17**

在 `src-tauri/src/db/migrations.rs` 的 `MIGRATIONS` 数组末尾（v16 那条之后）加：

```rust
    r#"
-- v17：服务器健康监控。
-- 样本按轮写入；ts 是客户端 UTC RFC3339（毫秒精度，固定宽度，便于字典序比较）。
CREATE TABLE server_metric_samples (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  ts TEXT NOT NULL,
  reachable INTEGER NOT NULL,
  connect_ms INTEGER,
  uptime_secs INTEGER,
  load1 REAL,
  load5 REAL,
  load15 REAL,
  cpu_pct REAL,
  cpu_cores INTEGER,
  mem_used_kb INTEGER,
  mem_total_kb INTEGER,
  disk_used_kb INTEGER,
  disk_total_kb INTEGER,
  bbr_algo TEXT,
  services_json TEXT,
  wg_json TEXT,
  error TEXT
);
CREATE INDEX idx_server_metric_samples_server_ts
  ON server_metric_samples(server_id, ts);
CREATE INDEX idx_server_metric_samples_ts ON server_metric_samples(ts);

CREATE TABLE server_alerts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  severity TEXT NOT NULL CHECK (severity IN ('warning','critical')),
  opened_at TEXT NOT NULL,
  resolved_at TEXT,
  acked_at TEXT,
  last_value TEXT,
  last_at TEXT NOT NULL,
  message TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_server_alerts_active
  ON server_alerts(server_id, kind) WHERE resolved_at IS NULL;
CREATE INDEX idx_server_alerts_opened_at ON server_alerts(opened_at);
"#,
```

同时把 `tests/rust/db/tests.rs:1216`（`migration_v16_adds_egress_family_defaulting_to_v4`）里的：

```rust
    assert_eq!(migrations::latest_version(), 16, "最新的迁移应为 v16");
```

改成：

```rust
    assert!(
        migrations::latest_version() >= 16,
        "迁移至少应包含 v16"
    );
```

v17 的精确版本由本任务 store 测试里的 `user_version == 17` 断言。

- [ ] **Step 4: 写 `server_monitor/store.rs` 的样本部分**

先在 `server_monitor/mod.rs` 的模块声明处加一行 `pub mod store;`，再创建文件：

```rust
//! 监控 SQLite 读写：样本、告警、历史降采样与保留期清理。
//! 所有函数返回 Result<_, String>，命令层用 AppError::Message 统一收口。
use super::{MonitorMetric, MonitorPoint, MonitorSample};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;

const SAMPLE_COLS: &str = "server_id,ts,reachable,connect_ms,uptime_secs,load1,load5,load15,\
cpu_pct,cpu_cores,mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,bbr_algo,services_json,wg_json,error";

fn row_to_sample(r: &rusqlite::Row<'_>) -> rusqlite::Result<MonitorSample> {
    let services_json: Option<String> = r.get(15)?;
    let wg_json: Option<String> = r.get(16)?;
    Ok(MonitorSample {
        server_id: r.get(0)?,
        ts: r.get(1)?,
        reachable: r.get::<_, i64>(2)? != 0,
        connect_ms: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
        uptime_secs: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
        load1: r.get(5)?,
        load5: r.get(6)?,
        load15: r.get(7)?,
        cpu_pct: r.get(8)?,
        cpu_cores: r.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        mem_used_kb: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        mem_total_kb: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        disk_used_kb: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        disk_total_kb: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        bbr_algo: r.get(14)?,
        services: services_json.and_then(|s| serde_json::from_str(&s).ok()),
        wg_interfaces: wg_json.and_then(|s| serde_json::from_str(&s).ok()),
        error: r.get(17)?,
    })
}

pub fn insert_sample(conn: &Connection, s: &MonitorSample) -> Result<(), String> {
    let services_json = match &s.services {
        Some(m) => Some(serde_json::to_string(m).map_err(|e| e.to_string())?),
        None => None,
    };
    let wg_json = match &s.wg_interfaces {
        Some(v) => Some(serde_json::to_string(v).map_err(|e| e.to_string())?),
        None => None,
    };
    conn.execute(
        "INSERT INTO server_metric_samples
         (server_id,ts,reachable,connect_ms,uptime_secs,load1,load5,load15,cpu_pct,cpu_cores,
          mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,bbr_algo,services_json,wg_json,error)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![
            s.server_id,
            s.ts,
            s.reachable as i64,
            s.connect_ms.map(|v| v as i64),
            s.uptime_secs.map(|v| v as i64),
            s.load1,
            s.load5,
            s.load15,
            s.cpu_pct,
            s.cpu_cores.map(|v| v as i64),
            s.mem_used_kb.map(|v| v as i64),
            s.mem_total_kb.map(|v| v as i64),
            s.disk_used_kb.map(|v| v as i64),
            s.disk_total_kb.map(|v| v as i64),
            s.bbr_algo,
            services_json,
            wg_json,
            s.error,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn latest_sample(conn: &Connection, server_id: i64) -> Result<Option<MonitorSample>, String> {
    conn.query_row(
        &format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 ORDER BY ts DESC LIMIT 1"
        ),
        params![server_id],
        row_to_sample,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn recent_samples(
    conn: &Connection,
    server_id: i64,
    limit: usize,
) -> Result<Vec<MonitorSample>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 ORDER BY ts DESC LIMIT ?2"
        ))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id, limit as i64], row_to_sample)
        .map_err(|e| e.to_string())?;
    let mut out: Vec<MonitorSample> = rows
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())?;
    out.reverse();
    Ok(out)
}

pub fn last_reachable_sample(
    conn: &Connection,
    server_id: i64,
) -> Result<Option<MonitorSample>, String> {
    conn.query_row(
        &format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 AND reachable=1 ORDER BY ts DESC LIMIT 1"
        ),
        params![server_id],
        row_to_sample,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn last_nonempty_wg_sample(
    conn: &Connection,
    server_id: i64,
) -> Result<Option<MonitorSample>, String> {
    conn.query_row(
        &format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 AND reachable=1 AND wg_json IS NOT NULL AND wg_json != '[]'
             ORDER BY ts DESC LIMIT 1"
        ),
        params![server_id],
        row_to_sample,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn max_ts(conn: &Connection) -> Result<Option<String>, String> {
    conn.query_row("SELECT MAX(ts) FROM server_metric_samples", [], |r| {
        r.get::<_, Option<String>>(0)
    })
    .map_err(|e| e.to_string())
}

pub fn history(
    conn: &Connection,
    server_id: i64,
    metric: MonitorMetric,
    cutoff: &str,
    bucket_secs: i64,
) -> Result<Vec<MonitorPoint>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT ts,cpu_pct,mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,load1,connect_ms
             FROM server_metric_samples
             WHERE server_id=?1 AND ts>=?2 AND reachable=1
             ORDER BY ts",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id, cutoff], |r| {
            let ts: String = r.get(0)?;
            let value = match metric {
                MonitorMetric::CpuPct => r.get::<_, Option<f64>>(1)?,
                MonitorMetric::MemPct => {
                    let used: Option<i64> = r.get(2)?;
                    let total: Option<i64> = r.get(3)?;
                    match (used, total) {
                        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                        _ => None,
                    }
                }
                MonitorMetric::DiskPct => {
                    let used: Option<i64> = r.get(4)?;
                    let total: Option<i64> = r.get(5)?;
                    match (used, total) {
                        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                        _ => None,
                    }
                }
                MonitorMetric::Load1 => r.get::<_, Option<f64>>(6)?,
                MonitorMetric::ConnectMs => r
                    .get::<_, Option<i64>>(7)?
                    .map(|v| v as f64),
            };
            Ok((ts, value))
        })
        .map_err(|e| e.to_string())?;

    let bucket = bucket_secs.max(1);
    let mut buckets: std::collections::BTreeMap<i64, (String, f64, usize)> =
        std::collections::BTreeMap::new();
    for row in rows {
        let (ts, value) = row.map_err(|e| e.to_string())?;
        let Some(value) = value else { continue };
        let epoch = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let entry = buckets
            .entry(epoch / bucket)
            .or_insert_with(|| (ts.clone(), 0.0, 0));
        entry.1 += value;
        entry.2 += 1;
    }
    Ok(buckets
        .into_values()
        .map(|(ts, sum, n)| MonitorPoint {
            ts,
            value: sum / n as f64,
        })
        .collect())
}

pub fn prune(conn: &Connection, sample_cutoff: &str, alert_cutoff: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM server_metric_samples WHERE ts < ?1",
        params![sample_cutoff],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM server_alerts WHERE resolved_at IS NOT NULL AND resolved_at < ?1",
        params![alert_cutoff],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn wg_expected_server_ids(conn: &Connection) -> Result<HashSet<i64>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT relay_server_id, landing_server_id FROM relay_rules
             WHERE status IN ('built','imported')",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut set = HashSet::new();
    for row in rows {
        let (a, b) = row.map_err(|e| e.to_string())?;
        set.insert(a);
        set.insert(b);
    }
    Ok(set)
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/store_tests.rs"
    ));
}
```

- [ ] **Step 5: 运行存储测试**

Run: `cargo test --features local-tests server_monitor::store -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 6 个存储测试通过。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/db/migrations.rs src-tauri/src/server_monitor/store.rs
git commit -m "feat(monitor): 增加 v17 迁移与样本存储"
```

---

## T3: 采集脚本解析器

- **Depends on:** T1

**Files:**
- Create: `src-tauri/src/server_monitor/parse.rs`
- Test: `tests/rust/server_monitor/parse_tests.rs`

**Interfaces:**
- Produces: `ParsedProbe`、`parse_probe_output(&str) -> ParsedProbe`。
- `ParsedProbe` 字段：`uptime_secs`、`load1/5/15`、`cpu_pct`、`cpu_cores`、`mem_used_kb`、`mem_total_kb`、`disk_used_kb`、`disk_total_kb`、`bbr_algo`、`services: Option<BTreeMap<String,String>>`、`wg_interfaces: Option<Vec<String>>`、`warning: Option<String>`。

- **验收:** `cargo test --features local-tests server_monitor::parse -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的解析测试**

创建 `tests/rust/server_monitor/parse_tests.rs`：

```rust
use super::*;

const FULL: &str = "\
##uptime
123456.78 234567.89
##load
0.42 0.31 0.22 1/234 5678
##cpu1
cpu  100 0 50 1000 10 0 0 0 0 0
##cpu2
cpu  110 0 55 1100 10 0 0 0 0 0
##mem
MemTotal:        2048000 kB
MemAvailable:     512000 kB
##disk
Filesystem 1024-blocks Used Available Capacity Mounted on
/dev/vda1      10240000 5120000 5120000      50% /
##cores
4
##bbr
bbr
##svc
active
inactive
failed
##wg
wg0 wg1
##wg_rc
0
##done
";

#[test]
fn parses_all_sections_and_computes_cpu() {
    let p = parse_probe_output(FULL);
    assert_eq!(p.uptime_secs, Some(123456));
    assert_eq!(p.load1, Some(0.42));
    assert_eq!(p.load5, Some(0.31));
    assert_eq!(p.load15, Some(0.22));
    assert!((p.cpu_pct.unwrap() - 13.043).abs() < 0.01, "15/115");
    assert_eq!(p.cpu_cores, Some(4));
    assert_eq!(p.mem_used_kb, Some(1536000));
    assert_eq!(p.mem_total_kb, Some(2048000));
    assert_eq!(p.disk_used_kb, Some(5120000));
    assert_eq!(p.disk_total_kb, Some(10240000));
    assert_eq!(p.bbr_algo.as_deref(), Some("bbr"));
    let svc = p.services.unwrap();
    assert_eq!(svc["xray"], "active");
    assert_eq!(svc["ssserver"], "inactive");
    assert_eq!(svc["singbox"], "failed");
    assert_eq!(p.wg_interfaces, Some(vec!["wg0".into(), "wg1".into()]));
    assert!(p.warning.is_none());
}

#[test]
fn zero_cpu_delta_is_unknown_not_zero() {
    let out = "\
##cpu1
cpu  100 0 50 1000 10 0 0 0 0 0
##cpu2
cpu  100 0 50 1000 10 0 0 0 0 0
##done
";
    assert_eq!(parse_probe_output(out).cpu_pct, None);
}

#[test]
fn missing_mem_available_is_unknown() {
    let out = "\
##mem
MemTotal: 2048000 kB
##done
";
    let p = parse_probe_output(out);
    assert_eq!(p.mem_used_kb, None);
    assert_eq!(p.mem_total_kb, Some(2048000));
}

#[test]
fn wg_nonzero_rc_is_unknown_and_empty_rc_zero_is_empty() {
    let unknown = "\
##wg

##wg_rc
1
##done
";
    assert_eq!(parse_probe_output(unknown).wg_interfaces, None);
    let empty = "\
##wg

##wg_rc
0
##done
";
    assert_eq!(parse_probe_output(empty).wg_interfaces, Some(vec![]));
}

#[test]
fn short_service_output_marks_missing_unknown() {
    let out = "\
##svc
active
##done
";
    let p = parse_probe_output(out);
    let svc = p.services.unwrap();
    assert_eq!(svc["xray"], "active");
    assert_eq!(svc["ssserver"], "unknown");
    assert_eq!(svc["singbox"], "unknown");
}

#[test]
fn missing_done_sets_warning_but_keeps_parsed_values() {
    let out = "\
##load
1.0 0.5 0.2 1/10 1
";
    let p = parse_probe_output(out);
    assert_eq!(p.load1, Some(1.0));
    assert_eq!(p.warning.as_deref(), Some("采集输出不完整"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::parse -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，`parse_probe_output` 不存在。

- [ ] **Step 3: 写 `server_monitor/parse.rs`**

先在 `server_monitor/mod.rs` 加 `pub mod parse;`，再创建文件：

```rust
//! 采集脚本输出解析：纯函数、无 IO；所有缺失/异常都落到 Option，不把空值当 0。
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedProbe {
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
    pub services: Option<BTreeMap<String, String>>,
    pub wg_interfaces: Option<Vec<String>>,
    pub warning: Option<String>,
}

pub fn parse_probe_output(out: &str) -> ParsedProbe {
    let sections = split_sections(out);
    let mut p = ParsedProbe::default();

    p.uptime_secs = sections
        .get("uptime")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.max(0.0) as u64);

    if let Some(load) = sections.get("load") {
        let vals: Vec<f64> = load
            .split_whitespace()
            .take(3)
            .filter_map(|v| v.parse().ok())
            .collect();
        if vals.len() == 3 {
            p.load1 = Some(vals[0]);
            p.load5 = Some(vals[1]);
            p.load15 = Some(vals[2]);
        }
    }

    p.cpu_pct = cpu_pct(sections.get("cpu1"), sections.get("cpu2"));

    if let Some(mem) = sections.get("mem") {
        p.mem_total_kb = mem_kb(mem, "MemTotal:");
        p.mem_used_kb = match (p.mem_total_kb, mem_kb(mem, "MemAvailable:")) {
            (Some(total), Some(avail)) => Some(total.saturating_sub(avail)),
            _ => None,
        };
    }

    if let Some(disk) = sections.get("disk") {
        if let Some(line) = disk
            .lines()
            .find(|l| l.split_whitespace().last() == Some("/"))
        {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 6 {
                p.disk_total_kb = f[1].parse().ok();
                p.disk_used_kb = f[2].parse().ok();
            }
        }
    }

    p.cpu_cores = sections
        .get("cores")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0);

    p.bbr_algo = sections
        .get("bbr")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if let Some(svc) = sections.get("svc") {
        let lines: Vec<&str> = svc
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if !lines.is_empty() {
            let mut map = BTreeMap::new();
            for (i, unit) in ["xray", "ssserver", "singbox"].iter().enumerate() {
                let state = lines.get(i).copied().unwrap_or("unknown");
                map.insert((*unit).to_string(), normalize_service(state));
            }
            p.services = Some(map);
        }
    }

    let rc = sections
        .get("wg_rc")
        .and_then(|s| s.trim().parse::<i32>().ok());
    p.wg_interfaces = match (rc, sections.get("wg")) {
        (Some(0), Some(s)) => {
            let mut ifaces: Vec<String> = s
                .split_whitespace()
                .map(str::to_string)
                .collect();
            ifaces.sort();
            ifaces.dedup();
            Some(ifaces)
        }
        _ => None,
    };

    if !sections.contains_key("done") {
        p.warning = Some("采集输出不完整".into());
    }
    p
}

fn split_sections(out: &str) -> HashMap<&str, String> {
    let mut map: HashMap<&str, String> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("##") {
            let name = name.trim();
            map.entry(name).or_default();
            current = Some(name);
        } else if let Some(name) = current {
            let entry = map.entry(name).or_default();
            entry.push_str(line);
            entry.push('\n');
        }
    }
    map
}

fn cpu_pct(s1: Option<&String>, s2: Option<&String>) -> Option<f64> {
    let a = parse_cpu_line(s1?)?;
    let b = parse_cpu_line(s2?)?;
    let total_a: u64 = a.iter().sum();
    let total_b: u64 = b.iter().sum();
    let idle_a = a[3] + a[4];
    let idle_b = b[3] + b[4];
    let dt = total_b.checked_sub(total_a)?;
    let di = idle_b.checked_sub(idle_a)?;
    if dt == 0 {
        return None;
    }
    Some((100.0 * (dt.saturating_sub(di)) as f64 / dt as f64).clamp(0.0, 100.0))
}

/// 8 个字段：user nice system idle iowait irq softirq steal；guest 已计入 user/nice，排除。
fn parse_cpu_line(s: &str) -> Option<[u64; 8]> {
    let vals: Vec<u64> = s
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() != 8 {
        return None;
    }
    Some([vals[0], vals[1], vals[2], vals[3], vals[4], vals[5], vals[6], vals[7]])
}

fn mem_kb(mem: &str, key: &str) -> Option<u64> {
    mem.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn normalize_service(s: &str) -> String {
    match s.trim() {
        "active" => "active",
        "inactive" => "inactive",
        "failed" => "failed",
        "activating" => "activating",
        "deactivating" => "deactivating",
        "reloading" => "reloading",
        _ => "unknown",
    }
    .to_string()
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/parse_tests.rs"
    ));
}
```

- [ ] **Step 4: 运行解析测试**

Run: `cargo test --features local-tests server_monitor::parse -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 6 个解析测试通过。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/server_monitor/parse.rs
git commit -m "feat(monitor): 增加采集脚本输出解析"
```

---

## T4: 告警状态机

- **Depends on:** T1

**Files:**
- Create: `src-tauri/src/server_monitor/alerts.rs`
- Test: `tests/rust/server_monitor/alert_tests.rs`

**Interfaces:**
- Produces:
  - `AlertKind`（`Offline/Cpu/Memory/Disk/Load/Service/Wireguard`，`as_str()`）
  - `Severity`（`Warning/Critical`，`as_str()`）
  - `AlertTransition::{Open{kind,severity,message,value}, Update{id,severity,message,value,clear_ack}, Resolve{id}}`
  - `AlertInput` 与 `evaluate(&AlertInput) -> Vec<AlertTransition>`

- **验收:** `cargo test --features local-tests server_monitor::alert -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的告警测试**

创建 `tests/rust/server_monitor/alert_tests.rs`：

```rust
use super::*;
use crate::server_monitor::{MonitorAlert, MonitorSample, Thresholds};

fn sample(ts: &str, reachable: bool) -> MonitorSample {
    MonitorSample {
        server_id: 1,
        ts: ts.into(),
        reachable,
        cpu_pct: Some(10.0),
        cpu_cores: Some(4),
        load1: Some(0.1),
        mem_used_kb: Some(10),
        mem_total_kb: Some(100),
        disk_used_kb: Some(10),
        disk_total_kb: Some(100),
        services: Some(std::collections::BTreeMap::from([
            ("xray".into(), "active".into()),
            ("ssserver".into(), "active".into()),
            ("singbox".into(), "active".into()),
        ])),
        wg_interfaces: Some(vec!["wg0".into()]),
        ..Default::default()
    }
}

fn input<'a>(
    current: &'a MonitorSample,
    recent: &'a [MonitorSample],
    active: &'a [MonitorAlert],
    t: &'a Thresholds,
) -> AlertInput<'a> {
    AlertInput {
        server_name: "测试机",
        current,
        recent,
        last_reachable: None,
        wg_baseline: None,
        active,
        thresholds: t,
        expected_services: &["xray", "ssserver", "singbox"],
        deployment_known: true,
        wg_expected: false,
        all_unreachable_guard: false,
    }
}

#[test]
fn cpu_opens_after_three_breaches_and_resolves_after_two_healthy() {
    let t = Thresholds::default();
    let healthy = sample("t1", true);
    let mut a = sample("t2", true);
    let mut b = sample("t3", true);
    a.cpu_pct = Some(95.0);
    b.cpu_pct = Some(96.0);
    // recent 旧→新：healthy, a, b；current 是第 3 次越界
    let mut cur = sample("t4", true);
    cur.cpu_pct = Some(97.0);
    let tr = evaluate(&input(
        &cur,
        &[healthy, a.clone(), b.clone()],
        &[],
        &t,
    ));
    assert!(tr.iter().any(|x| matches!(x, AlertTransition::Open { kind: AlertKind::Cpu, severity: Severity::Warning, .. })));

    // active alert; two healthy samples clear
    let active = MonitorAlert {
        id: 9, server_id: 1, server_name: "测试机".into(), kind: "cpu".into(),
        severity: "warning".into(), opened_at: "t4".into(), resolved_at: None,
        acked_at: None, last_value: None, last_at: "t4".into(), message: "m".into(),
    };
    let mut h1 = sample("t5", true); h1.cpu_pct = Some(80.0);
    let mut h2 = sample("t6", true); h2.cpu_pct = Some(70.0);
    let tr = evaluate(&input(&h2, &[h1], &[active], &t));
    assert!(tr.iter().any(|x| matches!(x, AlertTransition::Resolve { id: 9 })));
}

#[test]
fn unknown_metric_samples_freeze_the_counter() {
    let t = Thresholds::default();
    let mut a = sample("t1", true);
    a.cpu_pct = Some(95.0);
    let mut unknown = sample("t2", true);
    unknown.cpu_pct = None;
    let mut b = sample("t3", true);
    b.cpu_pct = Some(96.0);
    let mut cur = sample("t4", true);
    cur.cpu_pct = Some(97.0);
    // 未知样本既不计数也不清零：95 / null / 96 + current 97 = 3 次
    let tr = evaluate(&input(&cur, &[a, unknown, b], &[], &t));
    assert!(tr.iter().any(|x| matches!(
        x,
        AlertTransition::Open { kind: AlertKind::Cpu, .. }
    )));
}

#[test]
fn disk_opens_immediately_with_critical_severity() {
    let t = Thresholds::default();
    let mut cur = sample("t1", true);
    cur.disk_used_kb = Some(96);
    cur.disk_total_kb = Some(100);
    let tr = evaluate(&input(&cur, &[], &[], &t));
    assert!(tr.iter().any(|x| matches!(
        x,
        AlertTransition::Open { kind: AlertKind::Disk, severity: Severity::Critical, .. }
    )));
}

#[test]
fn offline_opens_after_three_unreachable_and_guard_blocks_it() {
    let t = Thresholds::default();
    let u1 = sample("t1", false);
    let u2 = sample("t2", false);
    let cur = sample("t3", false);
    let tr = evaluate(&input(&cur, &[u1.clone(), u2.clone()], &[], &t));
    assert!(tr.iter().any(|x| matches!(x, AlertTransition::Open { kind: AlertKind::Offline, severity: Severity::Critical, .. })));

    let mut guarded = input(&cur, &[u1, u2], &[], &t);
    guarded.all_unreachable_guard = true;
    assert!(evaluate(&guarded).is_empty());
}

#[test]
fn service_failed_on_first_sample_opens_immediately_but_inactive_does_not() {
    let t = Thresholds::default();
    let mut cur = sample("t1", true);
    cur.services.as_mut().unwrap().insert("xray".into(), "failed".into());
    let tr = evaluate(&input(&cur, &[], &[], &t));
    assert!(tr.iter().any(|x| matches!(x, AlertTransition::Open { kind: AlertKind::Service, .. })));

    let mut cur2 = sample("t1", true);
    cur2.services.as_mut().unwrap().insert("xray".into(), "inactive".into());
    assert!(evaluate(&input(&cur2, &[], &[], &t)).is_empty());
}

#[test]
fn service_transition_from_active_opens_after_two_samples() {
    let t = Thresholds::default();
    let base = sample("t1", true);
    let mut d1 = sample("t2", true);
    d1.services.as_mut().unwrap().insert("xray".into(), "inactive".into());
    let mut d2 = sample("t3", true);
    d2.services.as_mut().unwrap().insert("xray".into(), "inactive".into());
    let recent = [base.clone(), d1];
    let mut i = input(&d2, &recent, &[], &t);
    i.last_reachable = Some(&base);
    let tr = evaluate(&i);
    assert!(tr.iter().any(|x| matches!(x, AlertTransition::Open { kind: AlertKind::Service, .. })));
}

#[test]
fn wireguard_disappearance_opens_after_two_empty_samples_and_needs_baseline() {
    let t = Thresholds::default();
    let base = vec!["wg0".to_string()];
    let mut e1 = sample("t2", true);
    e1.wg_interfaces = Some(vec![]);
    let mut e2 = sample("t3", true);
    e2.wg_interfaces = Some(vec![]);
    let mut i = input(&e2, &[e1], &[], &t);
    i.wg_expected = true;
    i.wg_baseline = Some(&base);
    let tr = evaluate(&i);
    assert_eq!(
        tr.iter().filter(|x| matches!(x, AlertTransition::Open { kind: AlertKind::Wireguard, .. })).count(),
        1
    );

    let mut no_base = input(&e2, &[], &[], &t);
    no_base.wg_expected = true;
    no_base.wg_baseline = None;
    assert!(evaluate(&no_base).is_empty());
}

#[test]
fn severity_escalation_clears_ack() {
    let t = Thresholds::default();
    let active = MonitorAlert {
        id: 3, server_id: 1, server_name: "测试机".into(), kind: "disk".into(),
        severity: "warning".into(), opened_at: "t1".into(), resolved_at: None,
        acked_at: Some("t2".into()), last_value: Some("86%".into()),
        last_at: "t1".into(), message: "m".into(),
    };
    let mut cur = sample("t3", true);
    cur.disk_used_kb = Some(96);
    cur.disk_total_kb = Some(100);
    let tr = evaluate(&input(&cur, &[], &[active], &t));
    assert!(tr.iter().any(|x| matches!(
        x,
        AlertTransition::Update { clear_ack: true, severity: Severity::Critical, .. }
    )));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::alert -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败。

- [ ] **Step 3: 写 `server_monitor/alerts.rs`**

先在 `server_monitor/mod.rs` 加 `pub mod alerts;`，再创建文件：

```rust
//! 告警状态机：纯函数，不碰 DB/SSH。输入样本序列与活动告警，输出 Open/Update/Resolve。
use super::{sanitize_text, MonitorAlert, MonitorSample, Thresholds, TRANSITION_SAMPLES};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Offline,
    Cpu,
    Memory,
    Disk,
    Load,
    Service,
    Wireguard,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Load => "load",
            Self::Service => "service",
            Self::Wireguard => "wireguard",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlertTransition {
    Open {
        kind: AlertKind,
        severity: Severity,
        message: String,
        value: Option<String>,
    },
    Update {
        id: i64,
        severity: Severity,
        message: String,
        value: Option<String>,
        clear_ack: bool,
    },
    Resolve { id: i64 },
}

pub struct AlertInput<'a> {
    pub server_name: &'a str,
    pub current: &'a MonitorSample,
    pub recent: &'a [MonitorSample],
    pub last_reachable: Option<&'a MonitorSample>,
    pub wg_baseline: Option<&'a [String]>,
    pub active: &'a [MonitorAlert],
    pub thresholds: &'a Thresholds,
    pub expected_services: &'a [String],
    pub deployment_known: bool,
    pub wg_expected: bool,
    pub all_unreachable_guard: bool,
}

pub fn evaluate(input: &AlertInput<'_>) -> Vec<AlertTransition> {
    let mut out = Vec::new();
    if !input.all_unreachable_guard {
        eval_offline(input, &mut out);
    }
    eval_cpu(input, &mut out);
    eval_memory(input, &mut out);
    eval_disk(input, &mut out);
    eval_load(input, &mut out);
    eval_service(input, &mut out);
    eval_wireguard(input, &mut out);
    out
}

#[derive(Clone, Copy)]
enum Compare {
    /// 违反条件 `v >= t`；恢复 `v < t`（CPU、内存）。
    Ge,
    /// 违反条件 `v > t`；恢复 `v <= t`（负载）。
    Gt,
}

impl Compare {
    fn breach(self, v: f64, t: f64) -> bool {
        match self {
            Self::Ge => v >= t,
            Self::Gt => v > t,
        }
    }

    fn clear(self, v: f64, t: f64) -> bool {
        match self {
            Self::Ge => v < t,
            Self::Gt => v <= t,
        }
    }
}

fn active_for<'a>(input: &'a AlertInput<'_>, kind: AlertKind) -> Option<&'a MonitorAlert> {
    input
        .active
        .iter()
        .find(|a| a.kind == kind.as_str() && a.resolved_at.is_none())
}

fn eval_offline(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let kind = AlertKind::Offline;
    let active = active_for(input, kind);
    if !input.current.reachable {
        let streak = count_back(true, input.recent, |s| Some(!s.reachable));
        let value = input.current.error.as_deref().map(sanitize_text);
        let message = format!(
            "SSH 连续 {} 轮不可达：{}",
            streak,
            value.as_deref().unwrap_or("连接失败")
        );
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Critical,
                message,
                value,
                clear_ack: false,
            }),
            None if streak >= input.thresholds.offline_samples => out.push(AlertTransition::Open {
                kind,
                severity: Severity::Critical,
                message,
                value,
            }),
            None => {}
        }
    } else if let Some(a) = active {
        let streak = count_back(true, input.recent, |s| Some(s.reachable));
        if streak >= input.thresholds.clear_samples {
            out.push(AlertTransition::Resolve { id: a.id });
        }
    }
}

fn eval_numeric(
    input: &AlertInput<'_>,
    kind: AlertKind,
    value: Option<f64>,
    threshold: f64,
    compare: Compare,
    out: &mut Vec<AlertTransition>,
) {
    let Some(v) = value else { return };
    let active = active_for(input, kind);
    let label = match kind {
        AlertKind::Cpu => "CPU",
        AlertKind::Memory => "内存",
        AlertKind::Load => "负载",
        _ => "指标",
    };
    let value_text = |v: f64| -> String {
        if kind == AlertKind::Load {
            format!("{v:.2}")
        } else {
            format!("{v:.1}%")
        }
    };

    if compare.breach(v, threshold) {
        let streak = count_back(true, input.recent, |s| {
            metric_of(s, kind).map(|x| compare.breach(x, threshold))
        });
        let message = if kind == AlertKind::Load {
            format!(
                "1 分钟负载 {v:.2} 超过 {} 核 × {:.1}（阈值 {threshold:.1}），连续 {streak} 轮",
                input.current.cpu_cores.unwrap_or(0),
                input.thresholds.load_per_core,
            )
        } else {
            format!("{label}连续 {streak} 轮高于 {threshold:.0}%（最近 {v:.1}%）")
        };
        let value_text = Some(value_text(v));
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Warning,
                message,
                value: value_text,
                clear_ack: false,
            }),
            None if streak >= input.thresholds.breach_samples => out.push(AlertTransition::Open {
                kind,
                severity: Severity::Warning,
                message,
                value: value_text,
            }),
            None => {}
        }
    } else if let Some(a) = active {
        let clear = count_back(true, input.recent, |s| {
            metric_of(s, kind).map(|x| compare.clear(x, threshold))
        });
        if clear >= input.thresholds.clear_samples {
            out.push(AlertTransition::Resolve { id: a.id });
        } else {
            let message = if kind == AlertKind::Load {
                format!("1 分钟负载 {v:.2}，不高于阈值 {threshold:.1}")
            } else {
                format!("{label} {v:.1}%，低于阈值 {threshold:.0}%")
            };
            out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Warning,
                message,
                value: Some(value_text(v)),
                clear_ack: false,
            });
        }
    }
}

fn eval_cpu(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    eval_numeric(
        input,
        AlertKind::Cpu,
        input.current.cpu_pct,
        input.thresholds.cpu_pct,
        Compare::Ge,
        out,
    );
}

fn metric_of(s: &MonitorSample, kind: AlertKind) -> Option<f64> {
    match kind {
        AlertKind::Cpu => s.cpu_pct,
        AlertKind::Memory => match (s.mem_used_kb, s.mem_total_kb) {
            (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
            _ => None,
        },
        AlertKind::Load => s.load1,
        _ => None,
    }
}

fn eval_memory(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let value = match (input.current.mem_used_kb, input.current.mem_total_kb) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
        _ => None,
    };
    eval_numeric(
        input,
        AlertKind::Memory,
        value,
        input.thresholds.mem_pct,
        Compare::Ge,
        out,
    );
}

fn eval_disk(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let Some(v) = metric_of_percent(input.current.disk_used_kb, input.current.disk_total_kb) else {
        return;
    };
    let active = active_for(input, AlertKind::Disk);
    let warning = input.thresholds.disk_pct;
    let critical = input.thresholds.disk_critical_pct;
    if v >= warning {
        let severity = if v >= critical { Severity::Critical } else { Severity::Warning };
        let message = if severity == Severity::Critical {
            format!("磁盘使用率 {v:.1}% 超过严重阈值 {critical:.0}%（警告阈值 {warning:.0}%）")
        } else {
            format!("磁盘使用率 {v:.1}% 超过警告阈值 {warning:.0}%")
        };
        let value = Some(format!("{v:.1}%"));
        match active {
            Some(a) => {
                let was_critical = a.severity == "critical";
                out.push(AlertTransition::Update {
                    id: a.id,
                    severity,
                    message,
                    value,
                    clear_ack: severity == Severity::Critical && !was_critical,
                });
            }
            None => out.push(AlertTransition::Open {
                kind: AlertKind::Disk,
                severity,
                message,
                value,
            }),
        }
    } else if let Some(a) = active {
        let clear = count_back(true, input.recent, |s| {
            metric_of_percent(s.disk_used_kb, s.disk_total_kb).map(|x| x < warning)
        });
        if clear >= input.thresholds.clear_samples {
            out.push(AlertTransition::Resolve { id: a.id });
        } else {
            out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Warning,
                message: format!("磁盘使用率 {v:.1}%，低于警告阈值 {warning:.0}%"),
                value: Some(format!("{v:.1}%")),
                clear_ack: false,
            });
        }
    }
}

fn metric_of_percent(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    match (used, total) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
        _ => None,
    }
}

fn eval_load(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let (Some(load), Some(cores)) = (input.current.load1, input.current.cpu_cores) else {
        return;
    };
    let threshold = cores as f64 * input.thresholds.load_per_core;
    eval_numeric(
        input,
        AlertKind::Load,
        Some(load),
        threshold,
        Compare::Gt,
        out,
    );
}

fn eval_service(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    if !input.deployment_known || input.expected_services.is_empty() {
        return;
    }
    let Some(cur) = &input.current.services else { return };
    let active = active_for(input, AlertKind::Service);
    let down = input
        .expected_services
        .iter()
        .any(|u| matches!(cur.get(u).map(String::as_str), Some("inactive" | "failed")));
    let all_active = input
        .expected_services
        .iter()
        .all(|u| cur.get(u).map(String::as_str) == Some("active"));

    if down {
        let message = service_message(input.expected_services, cur);
        let value = Some(down_value(input.expected_services, cur));
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Critical,
                message,
                value,
                clear_ack: false,
            }),
            None => {
                let first_failed = input.last_reachable.is_none()
                    && input
                        .expected_services
                        .iter()
                        .any(|u| cur.get(u).map(String::as_str) == Some("failed"));
                if first_failed {
                    out.push(AlertTransition::Open {
                        kind: AlertKind::Service,
                        severity: Severity::Critical,
                        message,
                        value,
                    });
                    return;
                }
                let baseline_active = input.last_reachable.is_some_and(|s| {
                    s.services.as_ref().is_some_and(|m| {
                        input
                            .expected_services
                            .iter()
                            .all(|u| m.get(u).map(String::as_str) == Some("active"))
                    })
                });
                if baseline_active && service_down_streak(input) >= TRANSITION_SAMPLES {
                    out.push(AlertTransition::Open {
                        kind: AlertKind::Service,
                        severity: Severity::Critical,
                        message,
                        value,
                    });
                }
            }
        }
    } else if all_active {
        if let Some(a) = active {
            if service_healthy_streak(input) >= TRANSITION_SAMPLES {
                out.push(AlertTransition::Resolve { id: a.id });
            }
        }
    }
}

fn service_message(expected: &[String], states: &BTreeMap<String, String>) -> String {
    let parts: Vec<String> = expected
        .iter()
        .map(|u| format!("{u} {}", states.get(u).map(String::as_str).unwrap_or("unknown")))
        .collect();
    format!("服务异常：{}", parts.join("；"))
}

fn down_value(expected: &[String], states: &BTreeMap<String, String>) -> String {
    expected
        .iter()
        .filter(|u| matches!(states.get(*u).map(String::as_str), Some("inactive" | "failed")))
        .map(|u| format!("{u}={}", states.get(u).map(String::as_str).unwrap_or("unknown")))
        .collect::<Vec<_>>()
        .join(",")
}

fn service_down_streak(input: &AlertInput<'_>) -> u32 {
    let mut n = 0;
    for s in input.recent.iter().rev() {
        if !s.reachable {
            continue;
        }
        let Some(m) = &s.services else { continue };
        if input
            .expected_services
            .iter()
            .any(|u| matches!(m.get(u).map(String::as_str), Some("inactive" | "failed")))
        {
            n += 1;
        } else if input
            .expected_services
            .iter()
            .all(|u| m.get(u).map(String::as_str) == Some("active"))
        {
            break;
        }
    }
    n + 1 // current is down
}

fn service_healthy_streak(input: &AlertInput<'_>) -> u32 {
    let mut n = 0;
    for s in input.recent.iter().rev() {
        if !s.reachable {
            continue;
        }
        let Some(m) = &s.services else { continue };
        if input
            .expected_services
            .iter()
            .all(|u| m.get(u).map(String::as_str) == Some("active"))
        {
            n += 1;
        } else if input
            .expected_services
            .iter()
            .any(|u| matches!(m.get(u).map(String::as_str), Some("inactive" | "failed")))
        {
            break;
        }
    }
    n + 1 // current is all active
}

fn eval_wireguard(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    if !input.wg_expected {
        return;
    }
    let Some(cur) = &input.current.wg_interfaces else { return };
    let active = active_for(input, AlertKind::Wireguard);
    if cur.is_empty() {
        let Some(base) = input.wg_baseline.filter(|b| !b.is_empty()) else {
            return;
        };
        let message = format!("WireGuard 接口全部消失（基线：{}）", base.join(", "));
        let value = Some(format!("无接口（基线 {}）", base.join(", ")));
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Critical,
                message,
                value,
                clear_ack: false,
            }),
            None => {
                let streak = count_back(true, input.recent, |s| {
                    s.wg_interfaces.as_ref().map(|v| v.is_empty())
                });
                if streak >= TRANSITION_SAMPLES {
                    out.push(AlertTransition::Open {
                        kind: AlertKind::Wireguard,
                        severity: Severity::Critical,
                        message,
                        value,
                    });
                }
            }
        }
    } else if let Some(a) = active {
        let streak = count_back(true, input.recent, |s| {
            s.wg_interfaces.as_ref().map(|v| !v.is_empty())
        });
        if streak >= TRANSITION_SAMPLES {
            out.push(AlertTransition::Resolve { id: a.id });
        }
    }
}

/// 从 recent 末尾（最新）向前数连续样本，返回计数（含 current）。
/// `pred` 返回 `None` 表示该样本指标未知：既不计数也不清零（规格 6.1 的冻结规则）。
fn count_back(
    current_matches: bool,
    recent: &[MonitorSample],
    pred: impl Fn(&MonitorSample) -> Option<bool>,
) -> u32 {
    if !current_matches {
        return 0;
    }
    let mut n = 0;
    for s in recent.iter().rev() {
        match pred(s) {
            Some(true) => n += 1,
            Some(false) => break,
            None => continue,
        }
    }
    n + 1
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/alert_tests.rs"
    ));
}
```

> 计数语义（规格 6.1）：`count_back` 始终把 current 计入；`None` 表示指标未知，跳过但不打断连续段。CPU/内存用 `>=` / `<`，负载用 `>` / `<=`（`Compare`）。

- [ ] **Step 4: 运行告警测试**

Run: `cargo test --features local-tests server_monitor::alert -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 7 个告警测试通过。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/server_monitor/alerts.rs
git commit -m "feat(monitor): 增加告警状态机"
```

---

## T5: 告警存储与摘要查询

- **Depends on:** T2、T4

**Files:**
- Modify: `src-tauri/src/server_monitor/store.rs`
- Create: `src-tauri/src/server_monitor/summary.rs`
- Test: `tests/rust/server_monitor/summary_tests.rs`

**Interfaces:**
- Consumes: `AlertTransition`、`MonitorAlert`、`MonitorSummary`、`ServerHealth`、`config::load`、`config::server_enabled`、`deployed_protocols`。
- Produces:
  - `store::active_alerts(&Connection) -> Result<Vec<MonitorAlert>, String>`
  - `store::alerts(&Connection, bool, i64) -> Result<Vec<MonitorAlert>, String>`
  - `store::active_alerts_for_server(&Connection, i64) -> Result<Vec<MonitorAlert>, String>`
  - `store::apply_transitions(&Connection, i64, &str, &[AlertTransition]) -> Result<(), String>`
  - `store::ack(&Connection, i64, &str) -> Result<bool, String>`
  - `summary::expected_units(&[String]) -> Vec<String>`
  - `summary::build_summary(&Arc<AppState>, Option<&MonitorShared>) -> Result<MonitorSummary, String>`

- **验收:** `cargo test --features local-tests server_monitor::summary -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的摘要测试**

创建 `tests/rust/server_monitor/summary_tests.rs`：

```rust
use super::*;
use crate::crypto::MemoryStore;
use crate::domain::Server;
use crate::server_monitor::alerts::{AlertKind, AlertTransition, Severity};
use crate::server_monitor::{store, MonitorSample};
use crate::state::AppState;
use std::sync::Arc;
use tempfile::tempdir;

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempdir().unwrap();
    let state = AppState::new_with_store(
        &dir.path().join("test.db"),
        Arc::new(MemoryStore::default()),
    )
    .unwrap();
    (dir, state)
}

fn insert_server(state: &AppState, id: i64) {
    let s = Server {
        id,
        name: format!("测试-{id}"),
        host: "127.0.0.1".into(),
        host_v6: String::new(),
        addr_family: "v4".into(),
        egress_family: "v4".into(),
        port: 22,
        user: "root".into(),
        auth_kind: "password".into(),
        kind: "landing".into(),
        system: String::new(),
        notes: String::new(),
        created_at: "2026-10-19T00:00:00.000Z".into(),
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
    };
    crate::db::repos::servers::insert(&state.db.lock(), &s, "dummy").unwrap();
}

fn sample(server_id: i64, ts: &str) -> MonitorSample {
    MonitorSample {
        server_id,
        ts: ts.into(),
        reachable: true,
        cpu_pct: Some(12.0),
        cpu_cores: Some(4),
        load1: Some(0.2),
        mem_used_kb: Some(50),
        mem_total_kb: Some(100),
        disk_used_kb: Some(50),
        disk_total_kb: Some(100),
        ..Default::default()
    }
}

#[test]
fn expected_units_maps_deployed_protocols() {
    assert_eq!(
        expected_units(&["vless".into(), "ss".into(), "tuic".into()]),
        vec!["xray".to_string(), "ssserver".to_string(), "singbox".to_string()]
    );
    assert!(expected_units(&[]).is_empty());
}

#[test]
fn summary_contains_latest_sample_and_counts() {
    let (_dir, state) = state();
    insert_server(&state, 1);
    {
        let conn = state.db.lock();
        store::insert_sample(&conn, &sample(1, "2026-10-19T00:00:00.000Z")).unwrap();
        store::insert_sample(&conn, &sample(1, "2026-10-19T00:01:00.000Z")).unwrap();
        conn.execute(
            "INSERT INTO server_alerts
             (server_id,kind,severity,opened_at,last_at,message)
             VALUES (1,'cpu','warning','2026-10-19T00:00:00.000Z','2026-10-19T00:00:00.000Z','m')",
            [],
        )
        .unwrap();
    }
    let sum = build_summary(&state, None).unwrap();
    assert_eq!(sum.servers.len(), 1);
    assert_eq!(sum.servers[0].online, Some(true));
    assert_eq!(sum.servers[0].sample.as_ref().unwrap().cpu_pct, Some(12.0));
    assert_eq!(sum.active_alerts.len(), 1);
    assert_eq!(sum.last_round_at.as_deref(), Some("2026-10-19T00:01:00.000Z"));
    assert_eq!(sum.servers[0].active_alerts, 1);
    assert_eq!(sum.servers[0].unacked_alerts, 1);
}

#[test]
fn ack_only_marks_active_alert() {
    let (_dir, state) = state();
    insert_server(&state, 1);
    let id = {
        let conn = state.db.lock();
        conn.execute(
            "INSERT INTO server_alerts
             (server_id,kind,severity,opened_at,last_at,message)
             VALUES (1,'disk','warning','t','t','m')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    };
    assert!(store::ack(&state.db.lock(), id, "t2").unwrap());
    let alert = store::active_alerts(&state.db.lock()).unwrap().remove(0);
    assert_eq!(alert.acked_at.as_deref(), Some("t2"));
    assert!(!store::ack(&state.db.lock(), 999, "t3").unwrap());
}

#[test]
fn apply_transitions_open_update_resolve() {
    let (_dir, state) = state();
    insert_server(&state, 1);
    {
        let conn = state.db.lock();
        store::apply_transitions(
            &conn,
            1,
            "t1",
            &[AlertTransition::Open {
                kind: AlertKind::Cpu,
                severity: Severity::Warning,
                message: "CPU 高".into(),
                value: Some("95%".into()),
            }],
        )
        .unwrap();
    }
    let active = store::active_alerts(&state.db.lock()).unwrap();
    assert_eq!(active.len(), 1);
    let id = active[0].id;
    {
        let conn = state.db.lock();
        store::apply_transitions(
            &conn,
            1,
            "t2",
            &[AlertTransition::Update {
                id,
                severity: Severity::Critical,
                message: "更严重".into(),
                value: Some("99%".into()),
                clear_ack: true,
            }],
        )
        .unwrap();
    }
    let active = store::active_alerts(&state.db.lock()).unwrap();
    assert_eq!(active[0].severity, "critical");
    assert!(active[0].acked_at.is_none());
    {
        let conn = state.db.lock();
        store::apply_transitions(&conn, 1, "t3", &[AlertTransition::Resolve { id }]).unwrap();
    }
    assert!(store::active_alerts(&state.db.lock()).unwrap().is_empty());
    let all = store::alerts(&state.db.lock(), true, 10).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].resolved_at.as_deref(), Some("t3"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::summary -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，store 告警函数与 summary 不存在。

- [ ] **Step 3: 在 `store.rs` 末尾追加告警读写**

```rust
const ALERT_COLS: &str = "a.id,a.server_id,s.name,a.kind,a.severity,a.opened_at,\
a.resolved_at,a.acked_at,a.last_value,a.last_at,a.message";

fn row_to_alert(r: &rusqlite::Row<'_>) -> rusqlite::Result<MonitorAlert> {
    Ok(MonitorAlert {
        id: r.get(0)?,
        server_id: r.get(1)?,
        server_name: r.get(2)?,
        kind: r.get(3)?,
        severity: r.get(4)?,
        opened_at: r.get(5)?,
        resolved_at: r.get(6)?,
        acked_at: r.get(7)?,
        last_value: r.get(8)?,
        last_at: r.get(9)?,
        message: r.get(10)?,
    })
}

pub fn active_alerts(conn: &Connection) -> Result<Vec<MonitorAlert>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ALERT_COLS} FROM server_alerts a
             JOIN servers s ON s.id=a.server_id
             WHERE a.resolved_at IS NULL
             ORDER BY CASE a.severity WHEN 'critical' THEN 0 ELSE 1 END, a.opened_at"
        ))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], row_to_alert)
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

pub fn alerts(
    conn: &Connection,
    include_resolved: bool,
    limit: i64,
) -> Result<Vec<MonitorAlert>, String> {
    let sql = if include_resolved {
        format!(
            "SELECT {ALERT_COLS} FROM server_alerts a
             JOIN servers s ON s.id=a.server_id
             ORDER BY a.opened_at DESC LIMIT ?1"
        )
    } else {
        format!(
            "SELECT {ALERT_COLS} FROM server_alerts a
             JOIN servers s ON s.id=a.server_id
             WHERE a.resolved_at IS NULL
             ORDER BY a.opened_at DESC LIMIT ?1"
        )
    };
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![limit], row_to_alert)
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

pub fn active_alerts_for_server(
    conn: &Connection,
    server_id: i64,
) -> Result<Vec<MonitorAlert>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ALERT_COLS} FROM server_alerts a
             JOIN servers s ON s.id=a.server_id
             WHERE a.server_id=?1 AND a.resolved_at IS NULL"
        ))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id], row_to_alert)
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

pub fn apply_transitions(
    conn: &Connection,
    server_id: i64,
    ts: &str,
    transitions: &[AlertTransition],
) -> Result<(), String> {
    for t in transitions {
        match t {
            AlertTransition::Open { kind, severity, message, value } => {
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM server_alerts
                         WHERE server_id=?1 AND kind=?2 AND resolved_at IS NULL",
                        params![server_id, kind.as_str()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some(id) = existing {
                    update_alert(conn, id, *severity, message, value.as_deref(), ts, false)?;
                } else {
                    conn.execute(
                        "INSERT INTO server_alerts
                         (server_id,kind,severity,opened_at,last_value,last_at,message)
                         VALUES (?1,?2,?3,?4,?5,?4,?6)",
                        params![
                            server_id,
                            kind.as_str(),
                            severity.as_str(),
                            ts,
                            value,
                            message
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            AlertTransition::Update { id, severity, message, value, clear_ack } => {
                update_alert(conn, *id, *severity, message, value.as_deref(), ts, *clear_ack)?;
            }
            AlertTransition::Resolve { id } => {
                conn.execute(
                    "UPDATE server_alerts SET resolved_at=?2 WHERE id=?1 AND resolved_at IS NULL",
                    params![id, ts],
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn update_alert(
    conn: &Connection,
    id: i64,
    severity: Severity,
    message: &str,
    value: Option<&str>,
    ts: &str,
    clear_ack: bool,
) -> Result<(), String> {
    if clear_ack {
        conn.execute(
            "UPDATE server_alerts
             SET severity=?2,message=?3,last_value=?4,last_at=?5,acked_at=NULL
             WHERE id=?1",
            params![id, severity.as_str(), message, value, ts],
        )
    } else {
        conn.execute(
            "UPDATE server_alerts
             SET severity=?2,message=?3,last_value=?4,last_at=?5
             WHERE id=?1",
            params![id, severity.as_str(), message, value, ts],
        )
    }
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 返回 false = 没有这条活动告警（已恢复/不存在）。
pub fn ack(conn: &Connection, alert_id: i64, ts: &str) -> Result<bool, String> {
    let n = conn
        .execute(
            "UPDATE server_alerts SET acked_at=?2
             WHERE id=?1 AND resolved_at IS NULL",
            params![alert_id, ts],
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}
```

把 `store.rs` 顶部的 import 改为：

```rust
use super::alerts::{AlertTransition, Severity};
use super::{MonitorAlert, MonitorMetric, MonitorPoint, MonitorSample};
```

- [ ] **Step 4: 写 `server_monitor/summary.rs`**

先在 `server_monitor/mod.rs` 加 `pub mod summary;`。摘要只依赖 T1 的 `MonitorStatus`，不依赖调度器：

```rust
//! 摘要装配：配置、服务器健康、活动告警、采集状态。
use super::config;
use super::store;
use super::{MonitorStatus, MonitorSummary, ServerHealth};
use crate::db::repos;
use crate::state::AppState;
use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;

pub fn expected_units(protocols: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if protocols
        .iter()
        .any(|p| matches!(p.as_str(), "vless" | "trojan" | "vmess"))
    {
        out.push("xray".to_string());
    }
    if protocols.iter().any(|p| p == "ss") {
        out.push("ssserver".to_string());
    }
    if protocols
        .iter()
        .any(|p| matches!(p.as_str(), "hysteria2" | "tuic"))
    {
        out.push("singbox".to_string());
    }
    out
}

pub fn build_summary(
    state: &Arc<AppState>,
    status: Option<&MonitorStatus>,
) -> Result<MonitorSummary, String> {
    let (config, config_error) = config::load(&state.settings);
    let (servers, active_alerts, latest, max_ts, wg_ids) = {
        let conn = state.db.lock();
        let servers = repos::servers::list(&conn).map_err(|e| e.to_string())?;
        let active = store::active_alerts(&conn)?;
        let mut latest = HashMap::new();
        for s in &servers {
            latest.insert(s.id, store::latest_sample(&conn, s.id)?);
        }
        let max_ts = store::max_ts(&conn)?;
        let wg_ids = store::wg_expected_server_ids(&conn)?;
        (servers, active, latest, max_ts, wg_ids)
    };

    let mut counts: HashMap<i64, (usize, usize)> = HashMap::new();
    for a in &active_alerts {
        let entry = counts.entry(a.server_id).or_insert((0, 0));
        entry.0 += 1;
        if a.acked_at.is_none() {
            entry.1 += 1;
        }
    }

    let now = Utc::now();
    let stale_after = Duration::seconds((config.interval_secs.saturating_mul(2) + 30) as i64);
    let local_errors = status
        .map(|s| {
            s.local_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();

    let mut health = Vec::with_capacity(servers.len());
    for s in servers {
        let (expected_services, deployment_known) =
            match crate::servers::deploy::deployed_protocols(state, s.id) {
                Ok(p) => (expected_units(&p), true),
                Err(_) => (Vec::new(), false),
            };
        let sample = latest.remove(&s.id).flatten();
        let (active_count, unacked_count) = counts.get(&s.id).copied().unwrap_or((0, 0));
        let stale = sample
            .as_ref()
            .and_then(|x| chrono::DateTime::parse_from_rfc3339(&x.ts).ok())
            .map(|t| now.signed_duration_since(t.with_timezone(&Utc)) > stale_after)
            .unwrap_or(true);
        health.push(ServerHealth {
            server_id: s.id,
            name: s.name,
            host: s.host,
            monitored: config::server_enabled(&config, s.id),
            online: sample.as_ref().map(|x| x.reachable),
            stale,
            last_sample_at: sample.as_ref().map(|x| x.ts.clone()),
            expected_services,
            deployment_known,
            wg_expected: wg_ids.contains(&s.id),
            sample,
            local_error: local_errors.get(&s.id).cloned(),
            active_alerts: active_count,
            unacked_alerts: unacked_count,
        });
    }

    Ok(MonitorSummary {
        config,
        config_error,
        last_round_at: max_ts,
        last_round_error: status.and_then(|s| {
            s.last_round_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }),
        collecting: status
            .map(|s| s.collecting.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false),
        servers: health,
        active_alerts,
    })
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/summary_tests.rs"
    ));
}
```

- [ ] **Step 5: 运行摘要测试**

Run: `cargo test --features local-tests server_monitor::summary -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 4 个摘要测试通过。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/server_monitor/store.rs src-tauri/src/server_monitor/summary.rs
git commit -m "feat(monitor): 增加告警存储与摘要查询"
```

---

## T6: SSH 探针与脚本

- **Depends on:** T1

**Files:**
- Create: `src-tauri/src/server_monitor/probe.rs`
- Test: `tests/rust/server_monitor/probe_tests.rs`

**Interfaces:**
- Consumes: `crate::ssh::{AuthMaterial, SshSession}`、`crate::servers::load_auth_parts`、`domain::effective_host`。
- Produces:
  - `PROBE_SCRIPT: &str`
  - `MonitorTarget { server: Server, expected_services: Vec<String>, deployment_known: bool, wg_expected: bool }`
  - `ProbeOutcome::{Connected{connect_ms, stdout, command_error}, Unreachable{error}, LocalError{error}}`
  - `MonitorProbe` trait（`async_trait`）与 `RealProbe { db: Arc<Db>, creds: Arc<CredentialService> }`

- **验收:** `cargo test --features local-tests server_monitor::probe -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；脚本字符串包含全部段标记且不含 `sudo`。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的探针测试**

创建 `tests/rust/server_monitor/probe_tests.rs`：

```rust
use super::*;

#[test]
fn probe_script_has_every_marker_and_is_read_only() {
    for marker in [
        "##uptime", "##load", "##cpu1", "##cpu2", "##mem", "##disk",
        "##cores", "##bbr", "##svc", "##wg", "##wg_rc", "##done",
    ] {
        assert!(PROBE_SCRIPT.contains(marker), "缺少 {marker}");
    }
    assert!(!PROBE_SCRIPT.contains("sudo"));
    assert!(!PROBE_SCRIPT.contains("systemctl restart"));
    assert!(PROBE_SCRIPT.contains("systemctl is-active"));
    assert!(PROBE_SCRIPT.contains("sleep 1"));
}

#[test]
fn probe_script_captures_wg_exit_code_before_echoing_marker() {
    let wg = PROBE_SCRIPT.find("wg show interfaces").unwrap();
    let rc = PROBE_SCRIPT.find("rc=$?").unwrap();
    assert!(wg < rc, "rc 必须在 wg 之后立即捕获");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::probe -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败。

- [ ] **Step 3: 写 `server_monitor/probe.rs`**

先在 `server_monitor/mod.rs` 加 `pub mod probe;`，再创建文件：

```rust
//! 只读 SSH 探针：连接、执行采集脚本、返回原始输出或结构化失败。
use crate::crypto::CredentialService;
use crate::db::Db;
use crate::domain::{effective_host, Server};
use crate::ssh::SshSession;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub const PROBE_SCRIPT: &str = r#"echo '##uptime'; cat /proc/uptime 2>/dev/null
echo '##load'; cat /proc/loadavg 2>/dev/null
echo '##cpu1'; grep '^cpu ' /proc/stat 2>/dev/null
sleep 1
echo '##cpu2'; grep '^cpu ' /proc/stat 2>/dev/null
echo '##mem'; cat /proc/meminfo 2>/dev/null
echo '##disk'; df -P -k / 2>/dev/null
echo '##cores'; nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo 2>/dev/null
echo '##bbr'; sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null
echo '##svc'; systemctl is-active xray ssserver singbox 2>/dev/null
echo '##wg'; wg show interfaces 2>/dev/null
rc=$?
echo '##wg_rc'; echo "$rc"
echo '##done'
"#;

#[derive(Debug, Clone)]
pub struct MonitorTarget {
    pub server: Server,
    pub expected_services: Vec<String>,
    pub deployment_known: bool,
    pub wg_expected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProbeOutcome {
    Connected {
        connect_ms: u64,
        stdout: String,
        command_error: Option<String>,
    },
    Unreachable { error: String },
    LocalError { error: String },
}

#[async_trait]
pub trait MonitorProbe: Send + Sync {
    async fn probe(&self, target: &MonitorTarget) -> ProbeOutcome;
}

pub struct RealProbe {
    pub db: Arc<Db>,
    pub creds: Arc<CredentialService>,
}

#[async_trait]
impl MonitorProbe for RealProbe {
    async fn probe(&self, target: &MonitorTarget) -> ProbeOutcome {
        let (auth, _) =
            match crate::servers::load_auth_parts(&self.db, &self.creds, target.server.id) {
                Ok(v) => v,
                Err(e) => {
                    return ProbeOutcome::LocalError {
                        error: format!("读取服务器凭据失败: {e}"),
                    }
                }
            };
        let started = Instant::now();
        let mut session = match SshSession::connect(
            effective_host(&target.server),
            target.server.port as u16,
            &target.server.user,
            &auth,
            CONNECT_TIMEOUT,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return ProbeOutcome::Unreachable {
                    error: e.to_string(),
                }
            }
        };
        let connect_ms = started.elapsed().as_millis() as u64;
        match session
            .run("服务器健康采集", PROBE_SCRIPT, COMMAND_TIMEOUT)
            .await
        {
            Ok(out) => ProbeOutcome::Connected {
                connect_ms,
                stdout: out.stdout,
                command_error: if out.exit_code == 0 {
                    None
                } else {
                    Some(format!("采集脚本退出码 {}", out.exit_code))
                },
            },
            Err(e) => ProbeOutcome::Connected {
                connect_ms,
                stdout: String::new(),
                command_error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/probe_tests.rs"
    ));
}
```

- [ ] **Step 4: 运行探针测试**

Run: `cargo test --features local-tests server_monitor::probe -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 2 个探针测试通过。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/server_monitor/probe.rs
git commit -m "feat(monitor): 增加只读 SSH 探针"
```

---

## T7: 调度器、轮次编排与生命周期

- **Depends on:** T2、T4、T5、T6

**Files:**
- Create: `src-tauri/src/server_monitor/scheduler.rs`
- Modify: `src-tauri/src/state.rs`（加 `monitor` 槽位）
- Modify: `src-tauri/src/main.rs`（start / stop / module already declared）
- Test: `tests/rust/server_monitor/scheduler_tests.rs`

**Interfaces:**
- Consumes: `config::{load, effective_thresholds, server_enabled}`、`probe::{MonitorProbe, RealProbe, ProbeOutcome}`、`parse::parse_probe_output`、`alerts::{evaluate, AlertInput}`、`store`、`summary::build_summary`。
- Produces:
  - `MonitorEmit = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>`
  - `MonitorShared`、`MonitorHandle`
  - `start(&Arc<AppState>, MonitorEmit) -> Result<(), String>`
  - `stop(&Arc<AppState>)`（async）
  - `collect_now(&Arc<AppState>) -> Result<MonitorSummary, String>`（async）
  - `run_round(&Arc<AppState>, &Arc<MonitorShared>, Arc<dyn MonitorProbe>) -> Result<(), String>`（`pub(crate)`，供测试注入 fake）

- **验收:** `cargo test --features local-tests server_monitor::scheduler -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；`cargo check` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的调度测试**

创建 `tests/rust/server_monitor/scheduler_tests.rs`：

```rust
use super::*;
use crate::crypto::MemoryStore;
use crate::domain::Server;
use crate::server_monitor::probe::{MonitorProbe, MonitorTarget, ProbeOutcome};
use crate::server_monitor::store;
use crate::state::AppState;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[derive(Default)]
struct FakeProbe {
    outcomes: Mutex<HashMap<i64, ProbeOutcome>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    delay_ms: u64,
}

#[async_trait]
impl MonitorProbe for FakeProbe {
    async fn probe(&self, target: &MonitorTarget) -> ProbeOutcome {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now, Ordering::SeqCst);
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        let out = self
            .outcomes
            .lock()
            .unwrap()
            .get(&target.server.id)
            .cloned()
            .unwrap_or(ProbeOutcome::Unreachable {
                error: "missing fake".into(),
            });
        self.active.fetch_sub(1, Ordering::SeqCst);
        out
    }
}

fn noop_emit() -> MonitorEmit {
    Arc::new(|_, _| {})
}

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempdir().unwrap();
    let state = AppState::new_with_store(
        &dir.path().join("test.db"),
        Arc::new(MemoryStore::default()),
    )
    .unwrap();
    (dir, state)
}

fn insert_server(state: &AppState, id: i64) {
    let s = Server {
        id,
        name: format!("测试-{id}"),
        host: "127.0.0.1".into(),
        host_v6: String::new(),
        addr_family: "v4".into(),
        egress_family: "v4".into(),
        port: 22,
        user: "root".into(),
        auth_kind: "password".into(),
        kind: "landing".into(),
        system: String::new(),
        notes: String::new(),
        created_at: "2026-10-19T00:00:00.000Z".into(),
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
    };
    crate::db::repos::servers::insert(&state.db.lock(), &s, "dummy").unwrap();
}

fn connected(stdout: &str) -> ProbeOutcome {
    ProbeOutcome::Connected {
        connect_ms: 5,
        stdout: stdout.into(),
        command_error: None,
    }
}

fn shared() -> Arc<MonitorShared> {
    Arc::new(MonitorShared::new(noop_emit()))
}

#[tokio::test]
async fn round_isolates_local_and_unreachable_failures() {
    let (_dir, state) = state();
    insert_server(&state, 1);
    insert_server(&state, 2);
    insert_server(&state, 3);
    let probe = Arc::new(FakeProbe::default());
    {
        let mut m = probe.outcomes.lock().unwrap();
        m.insert(1, connected("##done\n"));
        m.insert(2, ProbeOutcome::Unreachable { error: "refused".into() });
        m.insert(3, ProbeOutcome::LocalError { error: "no creds".into() });
    }
    let sh = shared();
    run_round(&state, &sh, probe.clone()).await.unwrap();
    let conn = state.db.lock();
    assert!(store::latest_sample(&conn, 1).unwrap().unwrap().reachable);
    assert!(!store::latest_sample(&conn, 2).unwrap().unwrap().reachable);
    assert!(store::latest_sample(&conn, 3).unwrap().is_none(), "本地错误不写样本");
    drop(conn);
    let errors = sh.status.local_errors.lock().unwrap().clone();
    assert!(errors.get(&3).is_some(), "本地错误要出现在摘要里");
}

#[tokio::test]
async fn all_unreachable_guard_writes_no_samples() {
    let (_dir, state) = state();
    insert_server(&state, 1);
    insert_server(&state, 2);
    let probe = Arc::new(FakeProbe::default());
    {
        let mut m = probe.outcomes.lock().unwrap();
        m.insert(1, ProbeOutcome::Unreachable { error: "refused".into() });
        m.insert(2, ProbeOutcome::Unreachable { error: "refused".into() });
    }
    let sh = shared();
    run_round(&state, &sh, probe).await.unwrap();
    let conn = state.db.lock();
    assert!(store::latest_sample(&conn, 1).unwrap().is_none());
    assert!(store::latest_sample(&conn, 2).unwrap().is_none());
    drop(conn);
    let msg = sh.status.last_round_error.lock().unwrap().clone();
    assert!(msg.unwrap().contains("所有服务器均不可达"));
}

#[tokio::test]
async fn concurrency_never_exceeds_config_value() {
    let (_dir, state) = state();
    for id in 1..=4 {
        insert_server(&state, id);
    }
    let probe = Arc::new(FakeProbe { delay_ms: 10, ..Default::default() });
    {
        let mut m = probe.outcomes.lock().unwrap();
        for id in 1..=4 {
            m.insert(id, connected("##done\n"));
        }
    }
    run_round(&state, &shared(), probe.clone()).await.unwrap();
    assert!(probe.max_active.load(Ordering::SeqCst) <= 2);
}

#[tokio::test]
async fn stop_aborts_a_sleeping_scheduler_within_three_seconds() {
    let (_dir, state) = state();
    let shared = shared();
    let join = tauri::async_runtime::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    *state.monitor.lock().unwrap() = Some(MonitorHandle { shared, join });
    let started = std::time::Instant::now();
    stop(&state).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::scheduler -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，`MonitorShared` 不存在。

- [ ] **Step 3: 声明调度模块并给 `state.rs` 增加槽位**

先在 `server_monitor/mod.rs` 加：

```rust
pub mod scheduler;
pub use scheduler::{collect_now, start, stop, MonitorEmit};
```


在 `AppState` 字段末尾（`data_dir` 之前或之后）加：

```rust
    /// 服务器健康监控调度器；应用退出时 stop。None = 尚未启动。
    pub monitor: std::sync::Mutex<Option<crate::server_monitor::scheduler::MonitorHandle>>,
```

在 `new_with_store` 的构造里加：

```rust
            monitor: std::sync::Mutex::new(None),
```

- [ ] **Step 4: 写 `server_monitor/scheduler.rs`**

```rust
//! 调度器：启动/停止、定时轮次、并发采集、全不可达守卫、事件发射。
use super::alerts::{self, AlertInput};
use super::config::{self, MonitorConfig};
use super::parse::parse_probe_output;
use super::probe::{MonitorProbe, MonitorTarget, ProbeOutcome, RealProbe};
use super::store;
use super::summary;
use super::{sanitize_text, MonitorSample, MonitorStatus, MonitorSummary};
use crate::state::AppState;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tauri::async_runtime::JoinHandle;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinSet;

pub type MonitorEmit = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

const FIRST_ROUND_DELAY: Duration = Duration::from_secs(3);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const RECENT_SAMPLES: usize = 10;
const PRUNE_INTERVAL: chrono::Duration = chrono::Duration::hours(1);

pub struct MonitorShared {
    pub collect: tokio::sync::Mutex<()>,
    /// 摘要/命令层共享的运行态（收集状态、上一轮错误、本地凭据错误）。
    pub status: Arc<MonitorStatus>,
    pub wake: Notify,
    pub shutdown: watch::Sender<bool>,
    pub emit: MonitorEmit,
    pub last_prune: std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>,
}

impl MonitorShared {
    pub fn new(emit: MonitorEmit) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            collect: tokio::sync::Mutex::new(()),
            status: Arc::new(MonitorStatus::default()),
            wake: Notify::new(),
            shutdown,
            emit,
            last_prune: std::sync::Mutex::new(None),
        }
    }
}

pub struct MonitorHandle {
    pub shared: Arc<MonitorShared>,
    pub join: JoinHandle<()>,
}

pub fn start(state: &Arc<AppState>, emit: MonitorEmit) -> Result<(), String> {
    let mut slot = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Err("监控调度器已在运行".into());
    }
    let shared = Arc::new(MonitorShared::new(emit));
    let weak: Weak<AppState> = Arc::downgrade(state);
    let loop_shared = shared.clone();
    let join = tauri::async_runtime::spawn(scheduler_loop(weak, loop_shared));
    *slot = Some(MonitorHandle { shared, join });
    Ok(())
}

pub async fn stop(state: &Arc<AppState>) {
    let handle = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(handle) = handle else { return };
    let _ = handle.shared.shutdown.send_replace(true);
    handle.shared.wake.notify_waiters();
    let abort = handle.join.inner().abort_handle();
    if tokio::time::timeout(STOP_TIMEOUT, handle.join).await.is_err() {
        abort.abort();
    }
}

pub async fn collect_now(state: &Arc<AppState>) -> Result<MonitorSummary, String> {
    let shared = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.shared.clone())
        .ok_or_else(|| "监控调度器尚未启动".to_string())?;
    let _permit = shared
        .collect
        .try_lock()
        .map_err(|_| "正在采集，请稍候".to_string())?;
    shared
        .status
        .local_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let probe: Arc<dyn MonitorProbe> = Arc::new(RealProbe {
        db: state.db.clone(),
        creds: state.creds.clone(),
    });
    let result = run_round(state, &shared, probe).await;
    emit_tick(state, &shared);
    result?;
    summary::build_summary(state, Some(&shared.status))
}

async fn scheduler_loop(state: Weak<AppState>, shared: Arc<MonitorShared>) {
    let mut shutdown = shared.shutdown.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(FIRST_ROUND_DELAY) => {}
        _ = shared.wake.notified() => {}
        _ = shutdown.changed() => return,
    }
    let mut last_round_end: Option<Instant> = None;
    let mut force_round = false;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let Some(state) = state.upgrade() else { break };
        let (cfg, _) = config::load(&state.settings);
        if !cfg.enabled {
            tokio::select! {
                _ = shared.wake.notified() => { force_round = true; continue; }
                _ = shutdown.changed() => break,
            }
        }
        if let Some(last) = last_round_end {
            if !force_round {
                let due = last + Duration::from_secs(cfg.interval_secs);
                let now = Instant::now();
                if due > now {
                    tokio::select! {
                        _ = tokio::time::sleep(due - now) => {}
                        _ = shared.wake.notified() => continue,
                        _ = shutdown.changed() => break,
                    }
                }
            }
        }
        let (cfg_now, _) = config::load(&state.settings);
        if !cfg_now.enabled {
            continue;
        }
        force_round = false;
        let probe: Arc<dyn MonitorProbe> = Arc::new(RealProbe {
            db: state.db.clone(),
            creds: state.creds.clone(),
        });
        if let Ok(_permit) = shared.collect.try_lock() {
            if let Err(e) = run_round(&state, &shared, probe).await {
                *shared
                    .status
                    .last_round_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(e);
            }
            emit_tick(&state, &shared);
        }
        last_round_end = Some(Instant::now());
    }
}

/// 调用方必须已持有 `shared.collect` 锁；本函数不重复加锁。
pub(crate) async fn run_round(
    state: &Arc<AppState>,
    shared: &Arc<MonitorShared>,
    probe: Arc<dyn MonitorProbe>,
) -> Result<(), String> {
    let result = run_round_inner(state, shared, probe).await;
    let message = match &result {
        Ok(warning) => warning.clone(),
        Err(e) => Some(e.clone()),
    };
    *shared
        .status
        .last_round_error
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = message;
    result.map(|_| ())
}

async fn run_round_inner(
    state: &Arc<AppState>,
    shared: &Arc<MonitorShared>,
    probe: Arc<dyn MonitorProbe>,
) -> Result<Option<String>, String> {
    let (cfg, _) = config::load(&state.settings);
    let targets = collect_targets(state, &cfg)?;
    if targets.is_empty() {
        return Ok(Some("没有启用监控的服务器".into()));
    }
    shared
        .status
        .local_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let _guard = CollectingGuard::new(shared);

    let sem = Arc::new(Semaphore::new(cfg.concurrency));
    let mut set = JoinSet::new();
    for target in targets.iter().cloned() {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| format!("采集并发控制失败: {e}"))?;
        let probe = probe.clone();
        set.spawn(async move {
            let _permit = permit;
            (target.server.id, probe.probe(&target).await)
        });
    }
    let mut outcomes: HashMap<i64, ProbeOutcome> = HashMap::new();
    while let Some(joined) = set.join_next().await {
        let (id, outcome) = joined.map_err(|e| format!("采集任务失败: {e}"))?;
        outcomes.insert(id, outcome);
    }

    // 本地凭据/DB 错误先收集再判守卫：守卫轮也要让 UI 看到“这台是本地错误”。
    let mut local_errors = HashMap::new();
    for (id, outcome) in &outcomes {
        if let ProbeOutcome::LocalError { error } = outcome {
            local_errors.insert(*id, sanitize_text(error));
        }
    }
    *shared
        .status
        .local_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = local_errors;

    let probed = outcomes
        .values()
        .filter(|o| !matches!(o, ProbeOutcome::LocalError { .. }))
        .count();
    let unreachable = outcomes
        .values()
        .filter(|o| matches!(o, ProbeOutcome::Unreachable { .. }))
        .count();
    if probed >= 2 && unreachable == probed {
        return Ok(Some("所有服务器均不可达，疑似本机网络异常".into()));
    }

    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut samples = Vec::new();
    for target in &targets {
        let Some(outcome) = outcomes.remove(&target.server.id) else {
            continue;
        };
        match outcome {
            ProbeOutcome::LocalError { .. } => {}
            ProbeOutcome::Unreachable { error } => samples.push(MonitorSample {
                server_id: target.server.id,
                ts: ts.clone(),
                reachable: false,
                error: Some(sanitize_text(&error)),
                ..Default::default()
            }),
            ProbeOutcome::Connected { connect_ms, stdout, command_error } => {
                let parsed = parse_probe_output(&stdout);
                let error = command_error
                    .map(|e| sanitize_text(&e))
                    .or(parsed.warning.clone());
                samples.push(MonitorSample {
                    server_id: target.server.id,
                    ts: ts.clone(),
                    reachable: true,
                    connect_ms: Some(connect_ms),
                    uptime_secs: parsed.uptime_secs,
                    load1: parsed.load1,
                    load5: parsed.load5,
                    load15: parsed.load15,
                    cpu_pct: parsed.cpu_pct,
                    cpu_cores: parsed.cpu_cores,
                    mem_used_kb: parsed.mem_used_kb,
                    mem_total_kb: parsed.mem_total_kb,
                    disk_used_kb: parsed.disk_used_kb,
                    disk_total_kb: parsed.disk_total_kb,
                    bbr_algo: parsed.bbr_algo,
                    services: parsed.services,
                    wg_interfaces: parsed.wg_interfaces,
                    error,
                });
            }
        }
    }
    let target_map: HashMap<i64, &MonitorTarget> =
        targets.iter().map(|t| (t.server.id, t)).collect();
    let mut conn = state.db.lock();
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for sample in &samples {
        let target = target_map
            .get(&sample.server_id)
            .ok_or_else(|| format!("缺少服务器 {} 的采集目标", sample.server_id))?;
        let thresholds = config::effective_thresholds(&cfg, sample.server_id);
        let recent = store::recent_samples(&tx, sample.server_id, RECENT_SAMPLES)?;
        let last_reachable = store::last_reachable_sample(&tx, sample.server_id)?;
        let wg_baseline = store::last_nonempty_wg_sample(&tx, sample.server_id)?
            .and_then(|s| s.wg_interfaces);
        let active = store::active_alerts_for_server(&tx, sample.server_id)?;
        let transitions = alerts::evaluate(&AlertInput {
            server_name: &target.server.name,
            current: sample,
            recent: &recent,
            last_reachable: last_reachable.as_ref(),
            wg_baseline: wg_baseline.as_deref(),
            active: &active,
            thresholds: &thresholds,
            expected_services: &target.expected_services,
            deployment_known: target.deployment_known,
            wg_expected: target.wg_expected,
            all_unreachable_guard: false,
        });
        store::insert_sample(&tx, sample)?;
        store::apply_transitions(&tx, sample.server_id, &sample.ts, &transitions)?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    // 必须先放掉 DB 锁：maybe_prune 会再次 lock，std::sync::Mutex 不可重入。
    drop(conn);

    maybe_prune(&state, shared)?;
    Ok(None)
}

fn collect_targets(
    state: &Arc<AppState>,
    cfg: &MonitorConfig,
) -> Result<Vec<MonitorTarget>, String> {
    let servers = {
        let conn = state.db.lock();
        crate::db::repos::servers::list(&conn).map_err(|e| e.to_string())?
    };
    let wg_ids = {
        let conn = state.db.lock();
        store::wg_expected_server_ids(&conn)?
    };
    let mut out = Vec::new();
    for server in servers {
        if !config::server_enabled(cfg, server.id) {
            continue;
        }
        let (expected_services, deployment_known) =
            match crate::servers::deploy::deployed_protocols(state, server.id) {
                Ok(p) => (summary::expected_units(&p), true),
                Err(_) => (Vec::new(), false),
            };
        out.push(MonitorTarget {
            wg_expected: wg_ids.contains(&server.id),
            deployment_known,
            expected_services,
            server,
        });
    }
    Ok(out)
}

fn maybe_prune(state: &Arc<AppState>, shared: &Arc<MonitorShared>) -> Result<(), String> {
    let now = chrono::Utc::now();
    let mut last = shared
        .last_prune
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| now - t < PRUNE_INTERVAL) {
        return Ok(());
    }
    let sample_cutoff = (now - chrono::Duration::days(super::SAMPLE_RETENTION_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let alert_cutoff = (now - chrono::Duration::days(super::ALERT_RETENTION_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    store::prune(&state.db.lock(), &sample_cutoff, &alert_cutoff)?;
    *last = Some(now);
    Ok(())
}

fn emit_tick(state: &Arc<AppState>, shared: &Arc<MonitorShared>) {
    match summary::build_summary(state, Some(&shared.status)) {
        Ok(s) => {
            if let Ok(v) = serde_json::to_value(s) {
                (shared.emit)("monitor://tick", v);
            }
        }
        Err(e) => eprintln!("[vpn-ctl] 生成监控摘要失败: {e}"),
    }
}

struct CollectingGuard<'a> {
    shared: &'a MonitorShared,
}

impl<'a> CollectingGuard<'a> {
    fn new(shared: &'a MonitorShared) -> Self {
        shared.status.collecting.store(true, Ordering::SeqCst);
        (shared.emit)("monitor://collecting", json!({ "collecting": true }));
        Self { shared }
    }
}

impl Drop for CollectingGuard<'_> {
    fn drop(&mut self) {
        self.shared.status.collecting.store(false, Ordering::SeqCst);
        (self.shared.emit)("monitor://collecting", json!({ "collecting": false }));
    }
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/scheduler_tests.rs"
    ));
}
```

> `MonitorTarget` 与 `ProbeOutcome` 的 `Clone` 已在 T6 派生；`run_round` 的 `pub(crate)` 可见性供测试直接调用。

- [ ] **Step 5: 在 `main.rs` 启动与退出**

把 setup 末尾的 `app.manage(state); Ok(())` 替换为：

```rust
            let monitor_state = state.clone();
            app.manage(state);
            let monitor_handle = app.handle().clone();
            let monitor_emit: crate::server_monitor::MonitorEmit =
                Arc::new(move |event, payload| {
                    let _ = monitor_handle.emit(event, payload);
                });
            if let Err(e) = crate::server_monitor::start(&monitor_state, monitor_emit) {
                eprintln!("[vpn-ctl] 启动服务器监控失败: {e}");
            }
            Ok(())
```

（`state` 原来是直接 `app.manage(state); Ok(())`，改成上面的克隆 + manage。）

在 `RunEvent::ExitRequested` 里、`speed::stop_all` 之前加：

```rust
                tauri::async_runtime::block_on(crate::server_monitor::stop(&state));
```

- [ ] **Step 6: 运行调度测试**

Run: `cargo test --features local-tests server_monitor::scheduler -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 4 个测试通过（其中 stop 测试约 2 秒）。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/state.rs src-tauri/src/main.rs src-tauri/src/server_monitor/scheduler.rs
git commit -m "feat(monitor): 增加调度器与轮次编排"
```

---

## T8: 命令层与 IPC 注册

- **Depends on:** T7、T5

**Files:**
- Create: `src-tauri/src/commands/monitor.rs`
- Modify: `src-tauri/src/commands/mod.rs`（只加 `mod monitor; pub use monitor::*;`）
- Modify: `src-tauri/src/commands/ipc.rs`
- Modify: `src-tauri/src/main.rs`（`invoke_handler!`）
- Test: `tests/rust/server_monitor/commands_tests.rs`

**Interfaces:**
- Consumes: `server_monitor::{collect_now, config, store, summary, MonitorAlert, MonitorConfig, MonitorMetric, MonitorPoint, MonitorSummary}`。
- Produces（IPC 命令）：
  - `server_monitor_summary_cmd` → `MonitorSummary`
  - `server_monitor_history_cmd(serverId, metric, rangeSecs)` → `MonitorPoint[]`
  - `server_monitor_alerts_cmd(includeResolved, limit)` → `MonitorAlert[]`
  - `server_monitor_collect_cmd` → `MonitorSummary`
  - `server_monitor_config_cmd` → `MonitorConfig`
  - `server_monitor_set_config_cmd(config)` → `MonitorConfig`
  - `server_monitor_ack_cmd(alertId)` → `()`

- **验收:** `cargo test --features local-tests server_monitor::commands -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；`cargo check` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的命令测试**

创建 `tests/rust/server_monitor/commands_tests.rs`：

```rust
use super::*;
use crate::crypto::MemoryStore;
use crate::state::AppState;
use std::sync::Arc;
use tempfile::tempdir;

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempdir().unwrap();
    let state = AppState::new_with_store(
        &dir.path().join("test.db"),
        Arc::new(MemoryStore::default()),
    )
    .unwrap();
    (dir, state)
}

#[test]
fn history_rejects_unknown_metric_and_range() {
    let (_dir, state) = state();
    assert!(monitor_history(state.clone(), 1, "cpu".into(), 3600).is_err());
    assert!(monitor_history(state, 1, "cpu_pct".into(), 120).is_err());
}

#[test]
fn alerts_reject_bad_limit() {
    let (_dir, state) = state();
    assert!(monitor_alerts(state.clone(), false, 0).is_err());
    assert!(monitor_alerts(state, false, 501).is_err());
}

#[test]
fn set_config_rejects_invalid_and_drops_unknown_servers() {
    let (_dir, state) = state();
    let mut cfg = MonitorConfig::default();
    cfg.defaults.disk_critical_pct = 80.0;
    assert!(monitor_set_config(state.clone(), cfg).is_err());

    let mut cfg = MonitorConfig::default();
    cfg.servers.insert(
        "999".into(),
        crate::server_monitor::ServerMonitorOverride {
            enabled: Some(false),
            thresholds: None,
        },
    );
    let saved = monitor_set_config(state, cfg).unwrap();
    assert!(saved.servers.is_empty());
}

#[test]
fn ack_missing_alert_is_not_found() {
    let (_dir, state) = state();
    assert!(matches!(
        monitor_ack(state, 12345),
        Err(crate::commands::AppError::NotFound(_))
    ));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --features local-tests server_monitor::commands -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败。

- [ ] **Step 3: 写 `commands/monitor.rs`**

```rust
//! 服务器监控命令层：参数校验 + 领域调用，Tauri 壳在 ipc.rs。
use super::{err, AppError, R};
pub use crate::server_monitor::{MonitorAlert, MonitorConfig, MonitorPoint, MonitorSummary};
use crate::server_monitor::{config, store, summary, MonitorMetric};
use crate::state::AppState;
use std::collections::HashSet;
use std::sync::Arc;

pub fn monitor_summary(state: Arc<AppState>) -> R<MonitorSummary> {
    let status = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.shared.status.clone());
    summary::build_summary(&state, status.as_deref()).map_err(AppError::Message)
}

pub async fn monitor_collect(state: Arc<AppState>) -> R<MonitorSummary> {
    crate::server_monitor::collect_now(&state)
        .await
        .map_err(AppError::Message)
}

pub fn monitor_history(
    state: Arc<AppState>,
    server_id: i64,
    metric: String,
    range_secs: i64,
) -> R<Vec<MonitorPoint>> {
    let metric = MonitorMetric::parse(&metric).map_err(AppError::Validation)?;
    if !matches!(range_secs, 3600 | 86400 | 604800) {
        return Err(AppError::Validation(
            "时间范围必须是 1 小时、24 小时或 7 天".into(),
        ));
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(range_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let bucket = (range_secs / 360).max(1);
    let conn = state.db.lock();
    store::history(&conn, server_id, metric, &cutoff, bucket).map_err(AppError::Message)
}

pub fn monitor_alerts(
    state: Arc<AppState>,
    include_resolved: bool,
    limit: u32,
) -> R<Vec<MonitorAlert>> {
    if !(1..=500).contains(&limit) {
        return Err(AppError::Validation("limit 必须在 1–500 之间".into()));
    }
    let conn = state.db.lock();
    store::alerts(&conn, include_resolved, limit as i64).map_err(AppError::Message)
}

pub fn monitor_config(state: Arc<AppState>) -> R<MonitorConfig> {
    Ok(config::load(&state.settings).0)
}

pub fn monitor_set_config(state: Arc<AppState>, mut cfg: MonitorConfig) -> R<MonitorConfig> {
    let ids: HashSet<i64> = {
        let conn = state.db.lock();
        crate::db::repos::servers::list(&conn)
            .map_err(err)?
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    cfg.servers
        .retain(|k, _| k.parse::<i64>().map(|id| ids.contains(&id)).unwrap_or(false));
    let saved = config::save(&state.settings, &cfg).map_err(AppError::Validation)?;
    let shared = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.shared.clone());
    if let Some(shared) = shared {
        shared.wake.notify_one();
    }
    Ok(saved)
}

pub fn monitor_ack(state: Arc<AppState>, alert_id: i64) -> R<()> {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let conn = state.db.lock();
    let updated = store::ack(&conn, alert_id, &ts).map_err(AppError::Message)?;
    if !updated {
        return Err(AppError::NotFound("告警不存在或已恢复".into()));
    }
    Ok(())
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/commands_tests.rs"
    ));
}
```

- [ ] **Step 4: 注册模块与 Tauri 壳**

`src-tauri/src/commands/mod.rs` 末尾加：

```rust
mod monitor;
pub use monitor::*;
```

`src-tauri/src/commands/ipc.rs` 末尾加：

```rust
#[tauri::command]
pub async fn server_monitor_summary_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<MonitorSummary> {
    monitor_summary(state.inner().clone())
}

#[tauri::command]
pub async fn server_monitor_history_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    metric: String,
    range_secs: i64,
) -> R<Vec<MonitorPoint>> {
    monitor_history(state.inner().clone(), server_id, metric, range_secs)
}

#[tauri::command]
pub async fn server_monitor_alerts_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    include_resolved: bool,
    limit: u32,
) -> R<Vec<MonitorAlert>> {
    monitor_alerts(state.inner().clone(), include_resolved, limit)
}

#[tauri::command]
pub async fn server_monitor_collect_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<MonitorSummary> {
    monitor_collect(state.inner().clone()).await
}

#[tauri::command]
pub async fn server_monitor_config_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<MonitorConfig> {
    monitor_config(state.inner().clone())
}

#[tauri::command]
pub async fn server_monitor_set_config_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    config: MonitorConfig,
) -> R<MonitorConfig> {
    monitor_set_config(state.inner().clone(), config)
}

#[tauri::command]
pub async fn server_monitor_ack_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    alert_id: i64,
) -> R<()> {
    monitor_ack(state.inner().clone(), alert_id)
}
```

- [ ] **Step 5: 在 `main.rs` 的 `invoke_handler!` 注册**

在 `sysproxy::get_system_proxy_cmd` 之前加：

```rust
            commands::server_monitor_summary_cmd,
            commands::server_monitor_history_cmd,
            commands::server_monitor_alerts_cmd,
            commands::server_monitor_collect_cmd,
            commands::server_monitor_config_cmd,
            commands::server_monitor_set_config_cmd,
            commands::server_monitor_ack_cmd,
```

- [ ] **Step 6: 运行命令测试**

Run: `cargo test --features local-tests server_monitor::commands -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 4 个测试通过。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/commands/mod.rs src-tauri/src/commands/ipc.rs src-tauri/src/commands/monitor.rs src-tauri/src/main.rs
git commit -m "feat(monitor): 增加 IPC 命令与注册"
```

---

## T9: 前端类型、API 与事件 store

- **Depends on:** T8

**Files:**
- Modify: `src/api/types.ts`
- Modify: `src/api/index.ts`
- Create: `src/monitor/registry.ts`
- Modify: `src/App.tsx`（只装监听与拉一次摘要；导航留给 T14）
- Test: `tests/frontend/monitor/registry.test.ts`

**Interfaces:**
- Consumes: Tauri `invoke`、`listen`、`MonitorSummary` 等类型。
- Produces:
  - `MonitorThresholds`、`PartialMonitorThresholds`、`MonitorServerOverride`、`MonitorConfig`、`MonitorSample`、`ServerHealth`、`MonitorAlert`、`MonitorSummary`、`MonitorPoint`、`MonitorMetric`
  - `monitorSummary`、`monitorHistory`、`monitorAlerts`、`monitorCollect`、`monitorConfig`、`monitorSetConfig`、`monitorAck`
  - `installMonitorListener`、`refreshMonitor`、`applySummary`、`useMonitorSummary`、`setMonitorError`

- **验收:** `npm run test:typecheck` 通过；`npx vitest run tests/frontend/monitor/registry.test.ts` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 追加 API 类型**

在 `src/api/types.ts` 末尾加：

```ts
export interface MonitorThresholds {
  cpu_pct: number
  mem_pct: number
  disk_pct: number
  disk_critical_pct: number
  load_per_core: number
  breach_samples: number
  clear_samples: number
  offline_samples: number
}

export type PartialMonitorThresholds = Partial<MonitorThresholds>

export interface MonitorServerOverride {
  enabled?: boolean | null
  thresholds?: PartialMonitorThresholds | null
}

export interface MonitorConfig {
  enabled: boolean
  interval_secs: number
  concurrency: number
  defaults: MonitorThresholds
  servers: Record<string, MonitorServerOverride>
}

export interface MonitorSample {
  server_id: number
  ts: string
  reachable: boolean
  connect_ms: number | null
  uptime_secs: number | null
  load1: number | null
  load5: number | null
  load15: number | null
  cpu_pct: number | null
  cpu_cores: number | null
  mem_used_kb: number | null
  mem_total_kb: number | null
  disk_used_kb: number | null
  disk_total_kb: number | null
  bbr_algo: string | null
  services: Record<string, string> | null
  wg_interfaces: string[] | null
  error: string | null
}

export interface ServerHealth {
  server_id: number
  name: string
  host: string
  monitored: boolean
  online: boolean | null
  stale: boolean
  last_sample_at: string | null
  expected_services: string[]
  deployment_known: boolean
  wg_expected: boolean
  sample: MonitorSample | null
  local_error: string | null
  active_alerts: number
  unacked_alerts: number
}

export type MonitorAlertKind =
  | 'offline'
  | 'cpu'
  | 'memory'
  | 'disk'
  | 'load'
  | 'service'
  | 'wireguard'

export interface MonitorAlert {
  id: number
  server_id: number
  server_name: string
  kind: MonitorAlertKind
  severity: 'warning' | 'critical'
  opened_at: string
  resolved_at: string | null
  acked_at: string | null
  last_value: string | null
  last_at: string
  message: string
}

export interface MonitorSummary {
  config: MonitorConfig
  config_error: string | null
  last_round_at: string | null
  last_round_error: string | null
  collecting: boolean
  servers: ServerHealth[]
  active_alerts: MonitorAlert[]
}

export interface MonitorPoint {
  ts: string
  value: number
}

export type MonitorMetric =
  | 'cpu_pct'
  | 'mem_pct'
  | 'disk_pct'
  | 'load1'
  | 'connect_ms'
```

- [ ] **Step 2: 追加 API 函数**

在 `src/api/index.ts` 的 import type 列表中加入：
`MonitorAlert`、`MonitorConfig`、`MonitorMetric`、`MonitorPoint`、`MonitorSummary`。
并在文件末尾加：

```ts
export function monitorSummary(): Promise<MonitorSummary> {
  return invoke('server_monitor_summary_cmd')
}

export function monitorHistory(
  serverId: number,
  metric: MonitorMetric,
  rangeSecs: number,
): Promise<MonitorPoint[]> {
  return invoke('server_monitor_history_cmd', { serverId, metric, rangeSecs })
}

export function monitorAlerts(
  includeResolved: boolean,
  limit = 100,
): Promise<MonitorAlert[]> {
  return invoke('server_monitor_alerts_cmd', { includeResolved, limit })
}

export function monitorCollect(): Promise<MonitorSummary> {
  return invoke('server_monitor_collect_cmd')
}

export function monitorConfig(): Promise<MonitorConfig> {
  return invoke('server_monitor_config_cmd')
}

export function monitorSetConfig(config: MonitorConfig): Promise<MonitorConfig> {
  return invoke('server_monitor_set_config_cmd', { config })
}

export function monitorAck(alertId: number): Promise<void> {
  return invoke('server_monitor_ack_cmd', { alertId })
}
```

- [ ] **Step 3: 写失败的前端 store 测试**

创建 `tests/frontend/monitor/registry.test.ts`：

```ts
import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { MonitorSummary } from '../../../src/api/types'
import {
  __resetMonitorStore,
  applySummary,
  getMonitorSnapshot,
  installMonitorListener,
  setCollecting,
  useMonitorSummary,
} from '../../../src/monitor/registry'

const summary = (collecting = false): MonitorSummary => ({
  config: {
    enabled: true,
    interval_secs: 60,
    concurrency: 2,
    defaults: {
      cpu_pct: 90, mem_pct: 90, disk_pct: 85, disk_critical_pct: 95,
      load_per_core: 2, breach_samples: 3, clear_samples: 2, offline_samples: 3,
    },
    servers: {},
  },
  config_error: null,
  last_round_at: '2026-10-19T00:00:00.000Z',
  last_round_error: null,
  collecting,
  servers: [],
  active_alerts: [],
})

let handlers: Array<(e: { payload: unknown }) => void> = []
let listenCalls = 0

vi.mock('@tauri-apps/api/event', () => ({
  listen: (_name: string, cb: (e: { payload: unknown }) => void) => {
    listenCalls += 1
    handlers.push(cb)
    return Promise.resolve(() => {})
  },
}))

vi.mock('../../../src/api', () => ({
  monitorSummary: vi.fn(async () => summary()),
}))

beforeEach(() => {
  __resetMonitorStore()
  handlers = []
  listenCalls = 0
})

afterEach(() => vi.restoreAllMocks())

describe('监控 store', () => {
  it('安装一次监听并只在首次安装', async () => {
    const { listen } = await import('@tauri-apps/api/event')
    await installMonitorListener(listen)
    await installMonitorListener(listen)
    expect(listenCalls).toBe(2) // tick + collecting 各一次，重复安装不再增加
  })

  it('tick 更新摘要，collecting 事件切换状态', async () => {
    const { listen } = await import('@tauri-apps/api/event')
    await installMonitorListener(listen)
    act(() => {
      handlers[0]({ payload: summary() })
    })
    expect(getMonitorSnapshot().summary?.last_round_at).toBe('2026-10-19T00:00:00.000Z')
    act(() => {
      handlers[1]({ payload: { collecting: true } })
    })
    expect(getMonitorSnapshot().collecting).toBe(true)
    act(() => setCollecting(false))
    expect(getMonitorSnapshot().collecting).toBe(false)
  })

  it('useMonitorSummary 订阅同一份快照', () => {
    const { result } = renderHook(() => useMonitorSummary())
    act(() => applySummary(summary(true)))
    expect(result.current.summary?.collecting).toBe(true)
    expect(result.current.collecting).toBe(true)
  })
})
```

- [ ] **Step 4: 写 `src/monitor/registry.ts`**

```ts
// 监控全局 store：事件流是主路径，命令返回用于手动采集/设置保存后的即时更新。
import { useSyncExternalStore } from 'react'
import type { listen } from '@tauri-apps/api/event'
import { monitorSummary } from '../api'
import type { MonitorSummary } from '../api/types'

export interface MonitorStore {
  summary: MonitorSummary | null
  collecting: boolean
  error: string | null
}

let store: MonitorStore = { summary: null, collecting: false, error: null }
const listeners = new Set<() => void>()

function emit(): void {
  for (const fn of listeners) fn()
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

function getSnapshot(): MonitorStore {
  return store
}

export function getMonitorSnapshot(): MonitorStore {
  return store
}

export function useMonitorSummary(): MonitorStore {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}

export function applySummary(s: MonitorSummary): void {
  store = { ...store, summary: s, collecting: s.collecting, error: null }
  emit()
}

export function setCollecting(v: boolean): void {
  store = { ...store, collecting: v }
  emit()
}

export function setMonitorError(e: string): void {
  store = { ...store, error: e }
  emit()
}

export async function refreshMonitor(): Promise<void> {
  try {
    applySummary(await monitorSummary())
  } catch (e) {
    setMonitorError(String(e))
  }
}

let installed = false

/**
 * 全进程只安装一次。生产入口在 App 的 useEffect；StrictMode 下 setup1 发起后
 * cleanup 先跑、setup2 再调用——用模块级布尔保证不会装第二份；监听随进程存活。
 */
export async function installMonitorListener(
  listenFn: typeof listen,
): Promise<void> {
  if (installed) return
  installed = true
  try {
    await listenFn('monitor://tick', (e) => {
      applySummary(e.payload as MonitorSummary)
    })
    await listenFn('monitor://collecting', (e) => {
      const p = e.payload as { collecting?: boolean }
      setCollecting(Boolean(p.collecting))
    })
  } catch (e) {
    // 安装失败时允许下次重试，否则监控页永远收不到事件。
    installed = false
    setMonitorError(`安装监控事件监听失败: ${String(e)}`)
  }
}

/** 测试专用：清空 store 与安装标记。 */
export function __resetMonitorStore(): void {
  store = { summary: null, collecting: false, error: null }
  installed = false
  emit()
}
```

- [ ] **Step 5: 在 `App.tsx` 安装监听并拉一次摘要**

在 import 区加：

```ts
import { installMonitorListener, refreshMonitor } from './monitor/registry'
```

在现有的 job listener `useEffect` 之后加：

```tsx
  useEffect(() => {
    void installMonitorListener(listen)
    void refreshMonitor()
  }, [])
```

- [ ] **Step 6: 运行前端 store 测试与类型检查**

Run: `npx vitest run tests/frontend/monitor/registry.test.ts && npm run test:typecheck`
Expected: 3 个测试通过，类型检查通过。

- [ ] **Step 7: 提交**

```bash
git add src/api/types.ts src/api/index.ts src/monitor/registry.ts src/App.tsx
git commit -m "feat(monitor): 增加前端类型、API 与事件 store"
```

---

## T10: 纯 SVG 趋势图

- **Depends on:** T9

**Files:**
- Create: `src/components/charts/LineChart.tsx`
- Test: `tests/frontend/components/LineChart.test.tsx`

**Interfaces:**
- Consumes: `MonitorPoint`。
- Produces: `LineChart({ points, intervalSecs, label, color?, formatValue? })`。

- **验收:** `npx vitest run tests/frontend/components/LineChart.test.tsx` 通过；组件 ≤500 行。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的图表测试**

创建 `tests/frontend/components/LineChart.test.tsx`：

```tsx
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import LineChart from '../../../src/components/charts/LineChart'

const pts = (values: number[]) =>
  values.map((value, i) => ({
    ts: new Date(Date.UTC(2026, 9, 19, 0, i)).toISOString(),
    value,
  }))

describe('LineChart', () => {
  it('空数据展示空态', () => {
    render(<LineChart points={[]} intervalSecs={60} label="CPU" />)
    expect(screen.getByText(/暂无数据/)).toBeInTheDocument()
  })

  it('正常数据渲染一条折线', () => {
    const { container } = render(
      <LineChart points={pts([10, 20, 30])} intervalSecs={60} label="CPU" />,
    )
    expect(container.querySelectorAll('path').length).toBe(1)
    expect(screen.getByRole('img', { name: /CPU/ })).toBeInTheDocument()
  })

  it('时间间隔超过 2×interval 时断线', () => {
    const points = [
      ...pts([10, 20]),
      { ts: new Date(Date.UTC(2026, 9, 19, 1, 0)).toISOString(), value: 30 },
    ]
    const { container } = render(
      <LineChart points={points} intervalSecs={60} label="CPU" />,
    )
    expect(container.querySelectorAll('path').length).toBe(2)
  })

  it('全等值时仍能渲染且不出现 NaN', () => {
    const { container } = render(
      <LineChart points={pts([5, 5, 5])} intervalSecs={60} label="CPU" />,
    )
    expect(container.innerHTML).not.toContain('NaN')
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npx vitest run tests/frontend/components/LineChart.test.tsx`
Expected: 模块不存在。

- [ ] **Step 3: 写 `LineChart.tsx`**

```tsx
import type { MonitorPoint } from '../../api/types'

const W = 360
const H = 120
const PAD = 8

export default function LineChart({
  points,
  intervalSecs,
  label,
  color = '#34d399',
  formatValue = (v) => v.toFixed(1),
}: {
  points: MonitorPoint[]
  intervalSecs: number
  label: string
  color?: string
  formatValue?: (v: number) => string
}) {
  if (points.length === 0) {
    return <p className="py-6 text-center text-sm text-neutral-500">暂无数据（应用运行时才会采集）</p>
  }

  const times = points.map((p) => new Date(p.ts).getTime())
  const values = points.map((p) => p.value)
  const start = Math.min(...times)
  const end = Math.max(...times)
  const span = Math.max(end - start, 1)
  const min = Math.min(...values)
  const max = Math.max(...values)
  const pad = max - min === 0 ? 1 : (max - min) * 0.1
  const lo = min - pad
  const hi = max + pad

  const x = (t: number) => PAD + ((t - start) / span) * (W - PAD * 2)
  const y = (v: number) => H - PAD - ((v - lo) / (hi - lo)) * (H - PAD * 2)

  const segments: string[] = []
  let d = ''
  for (let i = 0; i < points.length; i++) {
    const gap =
      i > 0 && times[i] - times[i - 1] > Math.max(intervalSecs * 2, 1) * 1000
    if (gap) {
      if (d) segments.push(d)
      d = ''
    }
    d += `${d ? 'L' : 'M'}${x(times[i]).toFixed(1)},${y(values[i]).toFixed(1)}`
  }
  if (d) segments.push(d)

  const last = points[points.length - 1]
  const fmtTime = (t: number) =>
    span > 2 * 3600_000
      ? new Date(t).toLocaleString([], {
          month: '2-digit',
          day: '2-digit',
          hour: '2-digit',
          minute: '2-digit',
        })
      : new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })

  return (
    <div>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        className="w-full"
        role="img"
        aria-label={`${label} 趋势，${fmtTime(start)} 至 ${fmtTime(end)}`}
      >
        <title>{`${label} 趋势`}</title>
        {[0, 0.5, 1].map((f) => {
          const gy = PAD + f * (H - PAD * 2)
          return (
            <line
              key={f}
              x1={PAD}
              x2={W - PAD}
              y1={gy}
              y2={gy}
              stroke="#262626"
              strokeWidth="1"
              vectorEffect="non-scaling-stroke"
            />
          )
        })}
        {segments.map((seg, i) => (
          <path
            key={i}
            d={seg}
            fill="none"
            stroke={color}
            strokeWidth="1.5"
            vectorEffect="non-scaling-stroke"
          />
        ))}
      </svg>
      <div className="flex justify-between text-xs text-neutral-500">
        <span>{fmtTime(start)}</span>
        <span>{fmtTime(end)}</span>
      </div>
      <p className="mt-1 text-xs text-neutral-400">
        最新 {formatValue(last.value)} · 最小 {formatValue(min)} · 最大 {formatValue(max)}
      </p>
    </div>
  )
}
```

- [ ] **Step 4: 运行图表测试**

Run: `npx vitest run tests/frontend/components/LineChart.test.tsx`
Expected: 4 个测试通过。

- [ ] **Step 5: 提交**

```bash
git add src/components/charts/LineChart.tsx
git commit -m "feat(monitor): 增加纯 SVG 趋势图"
```

---

## T11: 趋势面板

- **Depends on:** T9、T10

**Files:**
- Create: `src/views/monitor/TrendPanel.tsx`
- Test: `tests/frontend/views/TrendPanel.test.tsx`

**Interfaces:**
- Consumes: `monitorHistory`、`MonitorMetric`、`MonitorPoint`、`LineChart`。
- Produces: `TrendPanel({ serverId, intervalSecs })`。

- **验收:** `npx vitest run tests/frontend/views/TrendPanel.test.tsx` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的趋势测试**

创建 `tests/frontend/views/TrendPanel.test.tsx`：

```tsx
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import TrendPanel from '../../../src/views/monitor/TrendPanel'

const history = vi.fn(async () => [
  { ts: '2026-10-19T00:00:00.000Z', value: 12.5 },
  { ts: '2026-10-19T00:01:00.000Z', value: 20 },
])

vi.mock('../../../src/api', () => ({
  monitorHistory: (serverId: number, metric: string, range: number) => history(),
}))

beforeEach(() => history.mockClear())

describe('TrendPanel', () => {
  it('默认加载 CPU 1 小时并渲染图', async () => {
    render(<TrendPanel serverId={1} intervalSecs={60} />)
    await waitFor(() => expect(history).toHaveBeenCalledWith(1, 'cpu_pct', 3600))
    expect(await screen.findByRole('img', { name: /CPU 趋势/ })).toBeInTheDocument()
  })

  it('切换指标与范围会重新取数', async () => {
    const user = userEvent.setup()
    render(<TrendPanel serverId={1} intervalSecs={60} />)
    await user.click(screen.getByRole('button', { name: '内存' }))
    await user.click(screen.getByRole('button', { name: '24 小时' }))
    await waitFor(() => expect(history).toHaveBeenCalledWith(1, 'mem_pct', 86400))
  })

  it('取数失败展示错误而不是崩溃', async () => {
    history.mockRejectedValueOnce(new Error('db down'))
    render(<TrendPanel serverId={1} intervalSecs={60} />)
    expect(await screen.findByText(/趋势加载失败/)).toBeInTheDocument()
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npx vitest run tests/frontend/views/TrendPanel.test.tsx`
Expected: 模块不存在。

- [ ] **Step 3: 写 `TrendPanel.tsx`**

```tsx
import { useEffect, useState } from 'react'
import { monitorHistory } from '../../api'
import type { MonitorMetric, MonitorPoint } from '../../api/types'
import LineChart from '../../components/charts/LineChart'

const METRICS: { key: MonitorMetric; label: string; format: (v: number) => string }[] = [
  { key: 'cpu_pct', label: 'CPU', format: (v) => `${v.toFixed(1)}%` },
  { key: 'mem_pct', label: '内存', format: (v) => `${v.toFixed(1)}%` },
  { key: 'disk_pct', label: '磁盘', format: (v) => `${v.toFixed(1)}%` },
  { key: 'load1', label: '负载', format: (v) => v.toFixed(2) },
  { key: 'connect_ms', label: '连接耗时', format: (v) => `${Math.round(v)} ms` },
]

const RANGES = [
  { secs: 3600, label: '1 小时' },
  { secs: 86400, label: '24 小时' },
  { secs: 604800, label: '7 天' },
]

export default function TrendPanel({
  serverId,
  intervalSecs,
}: {
  serverId: number
  intervalSecs: number
}) {
  const [metric, setMetric] = useState<MonitorMetric>('cpu_pct')
  const [range, setRange] = useState(3600)
  const [points, setPoints] = useState<MonitorPoint[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    setLoading(true)
    setError(null)
    monitorHistory(serverId, metric, range)
      .then((p) => {
        if (alive) setPoints(p)
      })
      .catch((e) => {
        if (alive) setError(String(e))
      })
      .finally(() => {
        if (alive) setLoading(false)
      })
    return () => {
      alive = false
    }
  }, [serverId, metric, range])

  const meta = METRICS.find((m) => m.key === metric)!

  return (
    <div className="mt-3 border-t border-neutral-800 pt-3">
      <div className="mb-2 flex flex-wrap gap-1">
        {METRICS.map((m) => (
          <button
            key={m.key}
            type="button"
            onClick={() => setMetric(m.key)}
            className={`rounded px-2 py-1 text-xs ${
              metric === m.key
                ? 'bg-neutral-700 text-white'
                : 'text-neutral-400 hover:bg-neutral-800'
            }`}
          >
            {m.label}
          </button>
        ))}
        <span className="mx-1 w-px bg-neutral-800" />
        {RANGES.map((r) => (
          <button
            key={r.secs}
            type="button"
            onClick={() => setRange(r.secs)}
            className={`rounded px-2 py-1 text-xs ${
              range === r.secs
                ? 'bg-neutral-700 text-white'
                : 'text-neutral-400 hover:bg-neutral-800'
            }`}
          >
            {r.label}
          </button>
        ))}
      </div>
      {error ? (
        <p className="text-xs text-red-300">趋势加载失败：{error}</p>
      ) : loading ? (
        <p className="py-6 text-center text-xs text-neutral-500">加载中…</p>
      ) : (
        <LineChart
          points={points}
          intervalSecs={intervalSecs}
          label={meta.label}
          formatValue={meta.format}
        />
      )}
    </div>
  )
}
```

- [ ] **Step 4: 运行趋势测试**

Run: `npx vitest run tests/frontend/views/TrendPanel.test.tsx`
Expected: 3 个测试通过。

- [ ] **Step 5: 提交**

```bash
git add src/views/monitor/TrendPanel.tsx
git commit -m "feat(monitor): 增加趋势面板"
```

---

## T12: 服务器卡片与监控页骨架

- **Depends on:** T9、T11

**Files:**
- Create: `src/views/monitor/ServerCard.tsx`
- Create: `src/views/Monitor.tsx`
- Test: `tests/frontend/views/Monitor.test.tsx`

**Interfaces:**
- Consumes: `useMonitorSummary`、`refreshMonitor`、`monitorCollect`、`ServerHealth`、`MonitorSample`、`TrendPanel`。
- Produces: `Monitor({ onNavigate })`；`ServerCard({ health, config, expanded, onToggle })`。

- **验收:** `npx vitest run tests/frontend/views/Monitor.test.tsx` 通过；`Monitor.tsx` 与 `ServerCard.tsx` 各 ≤500 行。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的监控页测试**

创建 `tests/frontend/views/Monitor.test.tsx`：

```tsx
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { MonitorSummary } from '../../../src/api/types'

const collect = vi.fn(async () => summary)
const ack = vi.fn(async () => {})

const summary: MonitorSummary = {
  config: {
    enabled: true,
    interval_secs: 60,
    concurrency: 2,
    defaults: {
      cpu_pct: 90, mem_pct: 90, disk_pct: 85, disk_critical_pct: 95,
      load_per_core: 2, breach_samples: 3, clear_samples: 2, offline_samples: 3,
    },
    servers: {},
  },
  config_error: null,
  last_round_at: '2026-10-19T00:00:00.000Z',
  last_round_error: null,
  collecting: false,
  servers: [
    {
      server_id: 1,
      name: '香港落地',
      host: '1.2.3.4',
      monitored: true,
      online: true,
      stale: false,
      last_sample_at: '2026-10-19T00:00:00.000Z',
      expected_services: ['xray', 'ssserver'],
      deployment_known: true,
      wg_expected: false,
      sample: {
        server_id: 1,
        ts: '2026-10-19T00:00:00.000Z',
        reachable: true,
        connect_ms: 42,
        uptime_secs: 86400,
        load1: 0.5,
        load5: 0.4,
        load15: 0.3,
        cpu_pct: 12.5,
        cpu_cores: 4,
        mem_used_kb: 50,
        mem_total_kb: 100,
        disk_used_kb: 50,
        disk_total_kb: 100,
        bbr_algo: 'bbr',
        services: { xray: 'active', ssserver: 'active', singbox: 'inactive' },
        wg_interfaces: null,
        error: null,
      },
      local_error: null,
      active_alerts: 0,
      unacked_alerts: 0,
    },
  ],
  active_alerts: [
    {
      id: 7,
      server_id: 1,
      server_name: '香港落地',
      kind: 'cpu',
      severity: 'warning',
      opened_at: '2026-10-19T00:00:00.000Z',
      resolved_at: null,
      acked_at: null,
      last_value: '95.0%',
      last_at: '2026-10-19T00:00:00.000Z',
      message: 'CPU 连续 3 轮高于 90%（最近 95.0%）',
    },
  ],
}

vi.mock('../../../src/api', () => ({
  monitorSummary: vi.fn(async () => summary),
  monitorCollect: () => collect(),
  monitorAck: (id: number) => ack(),
  monitorHistory: vi.fn(async () => []),
  monitorAlerts: vi.fn(async () => []),
  monitorSetConfig: vi.fn(async (c: unknown) => c),
}))

vi.mock('../../../src/monitor/registry', async () => {
  const actual = await vi.importActual<typeof import('../../../src/monitor/registry')>(
    '../../../src/monitor/registry',
  )
  return {
    ...actual,
    useMonitorSummary: () => ({
      summary,
      collecting: false,
      error: null,
    }),
  }
})

import Monitor from '../../../src/views/Monitor'

beforeEach(() => {
  collect.mockClear()
  ack.mockClear()
})

describe('Monitor 页面', () => {
  it('渲染服务器卡片与资源/服务', () => {
    render(<Monitor onNavigate={() => {}} />)
    expect(screen.getByText('香港落地')).toBeInTheDocument()
    expect(screen.getByText('1.2.3.4')).toBeInTheDocument()
    expect(screen.getByText(/12.5%/)).toBeInTheDocument()
    expect(screen.getByText(/xray/)).toBeInTheDocument()
  })

  it('立即采集调用后端并更新 store', async () => {
    const user = userEvent.setup()
    render(<Monitor onNavigate={() => {}} />)
    await user.click(screen.getByRole('button', { name: '立即采集' }))
    expect(collect).toHaveBeenCalledTimes(1)
  })

  it('活动告警可确认', async () => {
    const user = userEvent.setup()
    render(<Monitor onNavigate={() => {}} />)
    expect(screen.getByText(/活动告警 1 条/)).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: '确认' }))
    await waitFor(() => expect(ack).toHaveBeenCalledWith(7))
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npx vitest run tests/frontend/views/Monitor.test.tsx`
Expected: 模块不存在。

- [ ] **Step 3: 写 `ServerCard.tsx`**

```tsx
import type { MonitorConfig, ServerHealth } from '../../api/types'
import TrendPanel from './TrendPanel'

function pct(used: number | null, total: number | null): number | null {
  if (used == null || total == null || total <= 0) return null
  return (used / total) * 100
}

function barColor(v: number | null, warning = 85, critical = 95): string {
  if (v == null) return 'bg-neutral-700'
  if (v >= critical) return 'bg-red-500'
  if (v >= warning) return 'bg-amber-500'
  return 'bg-emerald-500'
}

function fmtUptime(secs: number | null): string {
  if (secs == null) return '—'
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  return d > 0 ? `${d} 天 ${h} 小时` : `${h} 小时`
}

function relative(ts: string | null): string {
  if (!ts) return '暂无样本'
  const diff = Date.now() - new Date(ts).getTime()
  const min = Math.floor(diff / 60000)
  if (min < 1) return '刚刚'
  if (min < 60) return `${min} 分钟前`
  return `${Math.floor(min / 60)} 小时前`
}

function MetricBar({
  label,
  value,
  warning,
  critical,
}: {
  label: string
  value: number | null
  warning?: number
  critical?: number
}) {
  return (
    <div>
      <div className="flex justify-between text-xs text-neutral-400">
        <span>{label}</span>
        <span>{value == null ? '未知' : `${value.toFixed(1)}%`}</span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded bg-neutral-800">
        <div
          className={`h-full ${barColor(value, warning, critical)}`}
          style={{ width: `${Math.min(Math.max(value ?? 0, 0), 100)}%` }}
        />
      </div>
    </div>
  )
}

export default function ServerCard({
  health,
  config,
  expanded,
  onToggle,
}: {
  health: ServerHealth
  config: MonitorConfig
  expanded: boolean
  onToggle: () => void
}) {
  const s = health.sample
  const cpu = s?.cpu_pct ?? null
  const mem = pct(s?.mem_used_kb ?? null, s?.mem_total_kb ?? null)
  const disk = pct(s?.disk_used_kb ?? null, s?.disk_total_kb ?? null)
  const loadThreshold = (s?.cpu_cores ?? 0) * config.defaults.load_per_core
  const paused = !health.monitored

  const stateText = paused
    ? '已暂停监控'
    : health.online === true
      ? '在线'
      : health.online === false
        ? '不可达'
        : '暂无数据'

  return (
    <div className="rounded border border-neutral-800 bg-neutral-900 p-4">
      <button type="button" onClick={onToggle} className="w-full text-left">
        <div className="flex items-start justify-between gap-2">
          <div>
            <p className="font-medium">{health.name}</p>
            <p className="text-xs text-neutral-500">{health.host}</p>
          </div>
          <div className="text-right text-xs">
            <p
              className={
                paused
                  ? 'text-neutral-500'
                  : health.online === true
                    ? 'text-emerald-400'
                    : health.online === false
                      ? 'text-red-400'
                      : 'text-neutral-500'
              }
            >
              {stateText}
            </p>
            <p className="text-neutral-500">
              {health.stale ? '数据已过期 · ' : ''}
              {relative(health.last_sample_at)}
            </p>
          </div>
        </div>

        <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-3">
          <MetricBar label="CPU（约 1 秒）" value={cpu} />
          <MetricBar label="内存" value={mem} />
          <MetricBar label="磁盘 /" value={disk} warning={config.defaults.disk_pct} critical={config.defaults.disk_critical_pct} />
        </div>

        <div className="mt-3 grid grid-cols-2 gap-1 text-xs text-neutral-400 sm:grid-cols-3">
          <span>负载 {s?.load1?.toFixed(2) ?? '—'} / {s?.cpu_cores ?? '—'} 核</span>
          <span>负载阈值 {loadThreshold.toFixed(1)}</span>
          <span>运行 {fmtUptime(s?.uptime_secs ?? null)}</span>
          <span>连接 {s?.connect_ms != null ? `${s.connect_ms} ms` : '—'}</span>
          <span>BBR {s?.bbr_algo ?? '—'}</span>
          <span>核数 {s?.cpu_cores ?? '—'}</span>
        </div>

        <div className="mt-3 flex flex-wrap gap-1 text-xs">
          {['xray', 'ssserver', 'singbox'].map((unit) => {
            const expected = health.expected_services.includes(unit)
            const state = s?.services?.[unit] ?? 'unknown'
            const cls = !expected
              ? health.deployment_known
                ? 'bg-neutral-800 text-neutral-500'
                : 'bg-neutral-800 text-neutral-400'
              : state === 'active'
                ? 'bg-emerald-950 text-emerald-300'
                : state === 'inactive' || state === 'failed'
                  ? 'bg-red-950 text-red-300'
                  : 'bg-neutral-800 text-neutral-400'
            return (
              <span key={unit} className={`rounded px-2 py-0.5 ${cls}`}>
                {unit} {health.deployment_known ? (expected ? state : '未部署') : state}
              </span>
            )
          })}
          {health.wg_expected && (
            <span className="rounded bg-neutral-800 px-2 py-0.5 text-neutral-300">
              WG {s?.wg_interfaces == null ? '未知' : s.wg_interfaces.length ? s.wg_interfaces.join(', ') : '未发现接口'}
            </span>
          )}
        </div>

        {(s?.error || health.local_error) && (
          <p className="mt-2 truncate text-xs text-red-300" title={s?.error ?? health.local_error ?? ''}>
            {s?.error ?? health.local_error}
          </p>
        )}
      </button>
      {expanded && (
        <TrendPanel serverId={health.server_id} intervalSecs={config.interval_secs} />
      )}
    </div>
  )
}
```

- [ ] **Step 4: 写 `Monitor.tsx` 骨架**

```tsx
import { useEffect, useState } from 'react'
import { monitorAck, monitorCollect } from '../api'
import type { ViewKey } from '../App'
import {
  applySummary,
  refreshMonitor,
  useMonitorSummary,
} from '../monitor/registry'
import ServerCard from './monitor/ServerCard'

function relative(ts: string | null): string {
  if (!ts) return '尚未采集'
  const min = Math.floor((Date.now() - new Date(ts).getTime()) / 60000)
  if (min < 1) return '刚刚'
  if (min < 60) return `${min} 分钟前`
  return `${Math.floor(min / 60)} 小时前`
}

export default function Monitor({ onNavigate }: { onNavigate: (v: ViewKey) => void }) {
  const { summary, collecting, error } = useMonitorSummary()
  const [expanded, setExpanded] = useState<number | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)

  useEffect(() => {
    void refreshMonitor()
  }, [])

  const collect = async () => {
    setActionError(null)
    try {
      applySummary(await monitorCollect())
    } catch (e) {
      setActionError(String(e))
    }
  }

  const ack = async (id: number) => {
    setActionError(null)
    try {
      await monitorAck(id)
      await refreshMonitor()
    } catch (e) {
      setActionError(String(e))
    }
  }

  if (!summary) {
    return (
      <div className="space-y-3">
        <h2 className="text-xl font-semibold">服务器监控</h2>
        <p className="text-sm text-neutral-400">{error ?? '正在加载监控摘要…'}</p>
      </div>
    )
  }

  const unacked = summary.active_alerts.filter((a) => a.acked_at == null).length

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-semibold">服务器监控</h2>
          <p className="text-xs text-neutral-500">
            上次成功采集：{relative(summary.last_round_at)} · 仅在应用打开时采集
          </p>
        </div>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => void collect()}
            disabled={collecting}
            className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white disabled:opacity-50"
          >
            {collecting ? '采集中…' : '立即采集'}
          </button>
        </div>
      </div>

      {(actionError || error) && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {actionError ?? error}
        </p>
      )}
      {summary.config_error && (
        <p className="rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200">
          监控配置无法解析，已使用默认值：{summary.config_error}
        </p>
      )}
      {!summary.config.enabled && (
        <p className="rounded border border-neutral-700 bg-neutral-900 px-3 py-2 text-sm text-neutral-300">
          自动监控已暂停，仍可手动采集。
        </p>
      )}
      {summary.last_round_error && (
        <p className="rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200">
          本轮采集失败：{summary.last_round_error}
        </p>
      )}
      {summary.active_alerts.length > 0 && (
        <section className="rounded border border-red-900 bg-red-950/40 p-3">
          <h3 className="text-sm font-medium text-red-200">
            活动告警 {summary.active_alerts.length} 条（未确认 {unacked} 条）
          </h3>
          <ul className="mt-2 space-y-1">
            {summary.active_alerts.map((a) => (
              <li key={a.id} className="flex items-start justify-between gap-2 text-xs">
                <span className={a.acked_at ? 'text-neutral-400' : 'text-red-200'}>
                  [{a.severity === 'critical' ? '严重' : '警告'}] {a.server_name}：{a.message}
                </span>
                {a.acked_at ? (
                  <span className="shrink-0 text-neutral-500">已确认</span>
                ) : (
                  <button
                    type="button"
                    onClick={() => void ack(a.id)}
                    className="shrink-0 rounded border border-red-700 px-2 py-0.5 text-red-200"
                  >
                    确认
                  </button>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}

      {summary.servers.length === 0 ? (
        <div className="rounded border border-neutral-800 bg-neutral-900 p-6 text-center text-sm text-neutral-400">
          还没有服务器，先去服务器页添加。
          <button
            type="button"
            onClick={() => onNavigate('servers')}
            className="ml-2 text-emerald-400 underline"
          >
            去服务器页
          </button>
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
          {summary.servers.map((h) => (
            <ServerCard
              key={h.server_id}
              health={h}
              config={summary.config}
              expanded={expanded === h.server_id}
              onToggle={() =>
                setExpanded((cur) => (cur === h.server_id ? null : h.server_id))
              }
            />
          ))}
        </div>
      )}
    </div>
  )
}
```

> T13 会把告警历史与设置抽屉接进本文件；Monkey 页骨架先只保留卡片与活动告警条。

- [ ] **Step 5: 运行监控页测试**

Run: `npx vitest run tests/frontend/views/Monitor.test.tsx`
Expected: 3 个测试通过（第三个用例在 T13 补成真正的确认调用断言）。

- [ ] **Step 6: 提交**

```bash
git add src/views/Monitor.tsx src/views/monitor/ServerCard.tsx
git commit -m "feat(monitor): 增加服务器卡片与监控页骨架"
```

---

## T13: 告警历史与设置抽屉

- **Depends on:** T12

**Files:**
- Create: `src/views/monitor/AlertHistory.tsx`
- Create: `src/views/monitor/MonitorSettingsDrawer.tsx`
- Modify: `src/views/Monitor.tsx`（接入两个组件）
- Test: `tests/frontend/views/MonitorSettings.test.tsx`

**Interfaces:**
- Consumes: `monitorAlerts`、`monitorAck`、`monitorSetConfig`、`MonitorConfig`、`ServerHealth`。
- Produces:
  - `AlertHistory({ refreshKey, onAck })`
  - `MonitorSettingsDrawer({ config, servers, onClose, onSaved })`

- **验收:** `npx vitest run tests/frontend/views/MonitorSettings.test.tsx` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的设置测试**

创建 `tests/frontend/views/MonitorSettings.test.tsx`：

```tsx
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { MonitorConfig, ServerHealth } from '../../../src/api/types'
import MonitorSettingsDrawer from '../../../src/views/monitor/MonitorSettingsDrawer'

const save = vi.fn(async (c: MonitorConfig) => c)
vi.mock('../../../src/api', () => ({
  monitorSetConfig: (c: MonitorConfig) => save(c),
}))

const config: MonitorConfig = {
  enabled: true,
  interval_secs: 60,
  concurrency: 2,
  defaults: {
    cpu_pct: 90, mem_pct: 90, disk_pct: 85, disk_critical_pct: 95,
    load_per_core: 2, breach_samples: 3, clear_samples: 2, offline_samples: 3,
  },
  servers: { '1': { enabled: false } },
}

const servers: ServerHealth[] = [
  {
    server_id: 1, name: '一号', host: '1.1.1.1', monitored: false,
    online: null, stale: true, last_sample_at: null, expected_services: [],
    deployment_known: true, wg_expected: false, sample: null,
    local_error: null, active_alerts: 0, unacked_alerts: 0,
  },
]

beforeEach(() => save.mockClear())

describe('MonitorSettingsDrawer', () => {
  it('保存时保留每台阈值并写入启用状态', async () => {
    const user = userEvent.setup()
    const onSaved = vi.fn()
    render(
      <MonitorSettingsDrawer
        config={{ ...config, servers: { '1': { enabled: false, thresholds: { cpu_pct: 80 } } } }}
        servers={servers}
        onClose={() => {}}
        onSaved={onSaved}
      />,
    )
    await user.click(screen.getByRole('checkbox', { name: /一号/ }))
    await user.click(screen.getByRole('button', { name: '保存' }))
    await waitFor(() => expect(save).toHaveBeenCalled())
    const sent = save.mock.calls[0][0]
    expect(sent.servers['1'].enabled).toBe(true)
    expect(sent.servers['1'].thresholds).toEqual({ cpu_pct: 80 })
    expect(onSaved).toHaveBeenCalled()
  })

  it('磁盘严重阈值不高于警告阈值时就地报错且不保存', async () => {
    const user = userEvent.setup()
    render(
      <MonitorSettingsDrawer
        config={config}
        servers={servers}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    )
    const critical = screen.getByLabelText('磁盘严重阈值')
    await user.clear(critical)
    await user.type(critical, '80')
    await user.click(screen.getByRole('button', { name: '保存' }))
    expect(await screen.findByText(/必须高于警告阈值/)).toBeInTheDocument()
    expect(save).not.toHaveBeenCalled()
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npx vitest run tests/frontend/views/MonitorSettings.test.tsx`
Expected: 模块不存在。

- [ ] **Step 3: 写 `AlertHistory.tsx`**

```tsx
import { useCallback, useEffect, useState } from 'react'
import { monitorAlerts } from '../../api'
import type { MonitorAlert } from '../../api/types'

export default function AlertHistory({
  refreshKey,
  onAck,
}: {
  refreshKey: number
  onAck: (id: number) => void
}) {
  const [includeResolved, setIncludeResolved] = useState(false)
  const [alerts, setAlerts] = useState<MonitorAlert[]>([])
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(() => {
    monitorAlerts(includeResolved, 100)
      .then(setAlerts)
      .catch((e) => setError(String(e)))
  }, [includeResolved])

  useEffect(() => {
    load()
  }, [load, refreshKey])

  return (
    <section className="rounded border border-neutral-800 bg-neutral-900 p-4">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium">告警历史</h3>
        <label className="flex items-center gap-2 text-xs text-neutral-400">
          <input
            type="checkbox"
            checked={includeResolved}
            onChange={(e) => setIncludeResolved(e.target.checked)}
          />
          包含已恢复
        </label>
      </div>
      {error && <p className="mt-2 text-xs text-red-300">{error}</p>}
      {alerts.length === 0 ? (
        <p className="mt-3 text-xs text-neutral-500">暂无告警</p>
      ) : (
        <div className="mt-3 overflow-x-auto">
          <table className="w-full text-left text-xs">
            <thead className="text-neutral-500">
              <tr>
                <th className="py-1 pr-3">级别</th>
                <th className="py-1 pr-3">服务器</th>
                <th className="py-1 pr-3">说明</th>
                <th className="py-1 pr-3">最近值</th>
                <th className="py-1 pr-3">时间</th>
                <th className="py-1">操作</th>
              </tr>
            </thead>
            <tbody>
              {alerts.map((a) => (
                <tr key={a.id} className="border-t border-neutral-800">
                  <td className={a.severity === 'critical' ? 'text-red-300' : 'text-amber-300'}>
                    {a.severity === 'critical' ? '严重' : '警告'}
                  </td>
                  <td className="py-1 pr-3">{a.server_name}</td>
                  <td className="py-1 pr-3">{a.message}</td>
                  <td className="py-1 pr-3 text-neutral-400">{a.last_value ?? '—'}</td>
                  <td className="py-1 pr-3 text-neutral-500">
                    {a.resolved_at ? `已恢复 ${new Date(a.resolved_at).toLocaleString()}` : new Date(a.opened_at).toLocaleString()}
                  </td>
                  <td className="py-1">
                    {a.resolved_at == null ? (
                      a.acked_at ? (
                        <span className="text-neutral-500">已确认</span>
                      ) : (
                        <button type="button" onClick={() => onAck(a.id)} className="text-emerald-400">
                          确认
                        </button>
                      )
                    ) : (
                      <span className="text-neutral-600">—</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}
```

- [ ] **Step 4: 写 `MonitorSettingsDrawer.tsx`**

```tsx
import { useState, type ChangeEvent } from 'react'
import { monitorSetConfig } from '../../api'
import type { MonitorConfig, MonitorThresholds, ServerHealth } from '../../api/types'

const RESET: MonitorThresholds = {
  cpu_pct: 90, mem_pct: 90, disk_pct: 85, disk_critical_pct: 95,
  load_per_core: 2, breach_samples: 3, clear_samples: 2, offline_samples: 3,
}

export default function MonitorSettingsDrawer({
  config,
  servers,
  onClose,
  onSaved,
}: {
  config: MonitorConfig
  servers: ServerHealth[]
  onClose: () => void
  onSaved: (c: MonitorConfig) => void
}) {
  const [enabled, setEnabled] = useState(config.enabled)
  const [interval, setIntervalSecs] = useState(config.interval_secs)
  const [t, setT] = useState<MonitorThresholds>({ ...config.defaults })
  const [serverOn, setServerOn] = useState<Record<number, boolean>>(() =>
    Object.fromEntries(
      servers.map((s) => [s.server_id, config.servers[String(s.server_id)]?.enabled !== false]),
    ),
  )
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const num = (key: keyof MonitorThresholds) => ({
    value: t[key],
    onChange: (e: ChangeEvent<HTMLInputElement>) =>
      setT((prev) => ({ ...prev, [key]: e.target.valueAsNumber })),
  })

  const validate = (): string | null => {
    if (!(interval >= 30 && interval <= 3600)) return '采集间隔必须在 30–3600 秒之间'
    for (const k of ['cpu_pct', 'mem_pct', 'disk_pct', 'disk_critical_pct'] as const) {
      if (!(t[k] >= 1 && t[k] <= 100)) return '百分比阈值必须在 1–100 之间'
    }
    if (!(t.disk_critical_pct > t.disk_pct)) return '磁盘严重阈值必须高于警告阈值'
    if (!(t.load_per_core >= 0.1 && t.load_per_core <= 100)) return '负载阈值必须在 0.1–100 之间'
    for (const k of ['breach_samples', 'clear_samples', 'offline_samples'] as const) {
      if (!(t[k] >= 1 && t[k] <= 10)) return '连续次数必须在 1–10 之间'
    }
    return null
  }

  const save = async () => {
    const msg = validate()
    if (msg) {
      setError(msg)
      return
    }
    setBusy(true)
    setError(null)
    try {
      const next: MonitorConfig = {
        ...config,
        enabled,
        interval_secs: interval,
        defaults: { ...t },
        servers: Object.fromEntries(
          servers.map((s) => {
            const prev = config.servers[String(s.server_id)] ?? {}
            return [String(s.server_id), { ...prev, enabled: serverOn[s.server_id] }]
          }),
        ),
      }
      onSaved(await monitorSetConfig(next))
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const field = (label: string, key: keyof MonitorThresholds, step = '1') => (
    <label className="block text-xs text-neutral-400">
      {label}
      <input
        type="number"
        step={step}
        {...num(key)}
        aria-label={label}
        className="mt-1 w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm text-neutral-100"
      />
    </label>
  )

  return (
    <div className="fixed inset-y-0 right-0 z-20 w-96 overflow-y-auto border-l border-neutral-800 bg-neutral-950 p-4 shadow-xl">
      <div className="flex items-center justify-between">
        <h3 className="font-medium">监控设置</h3>
        <button type="button" onClick={onClose} className="text-neutral-400">
          关闭
        </button>
      </div>
      <div className="mt-4 space-y-3">
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
          自动监控（应用打开时）
        </label>
        <label className="block text-xs text-neutral-400">
          采集间隔（秒）
          <input
            type="number"
            min={30}
            max={3600}
            value={interval}
            onChange={(e) => setIntervalSecs(e.target.valueAsNumber)}
            aria-label="采集间隔"
            className="mt-1 w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
        <div className="grid grid-cols-2 gap-2">
          {field('CPU 阈值', 'cpu_pct')}
          {field('内存阈值', 'mem_pct')}
          {field('磁盘警告阈值', 'disk_pct')}
          {field('磁盘严重阈值', 'disk_critical_pct')}
          {field('负载（每核）', 'load_per_core', '0.1')}
          {field('连续越界次数', 'breach_samples')}
          {field('连续恢复次数', 'clear_samples')}
          {field('离线判定次数', 'offline_samples')}
        </div>
        <div>
          <p className="text-xs text-neutral-400">服务器</p>
          {servers.map((s) => (
            <label key={s.server_id} className="mt-1 flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={serverOn[s.server_id] ?? true}
                onChange={(e) =>
                  setServerOn((prev) => ({ ...prev, [s.server_id]: e.target.checked }))
                }
              />
              {s.name}
            </label>
          ))}
        </div>
        {error && <p className="text-xs text-red-300">{error}</p>}
        <div className="flex justify-between">
          <button
            type="button"
            onClick={() => setT({ ...RESET })}
            className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300"
          >
            恢复默认
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void save()}
            className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white disabled:opacity-50"
          >
            保存
          </button>
        </div>
      </div>
    </div>
  )
}
```

- [ ] **Step 5: 把组件接进 `Monitor.tsx`**

在 import 区加：

```ts
import AlertHistory from './monitor/AlertHistory'
import MonitorSettingsDrawer from './monitor/MonitorSettingsDrawer'
```

在组件 state 区加：

```ts
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [historyKey, setHistoryKey] = useState(0)
```

在「立即采集」按钮旁加「设置」按钮：

```tsx
          <button
            type="button"
            onClick={() => setSettingsOpen(true)}
            className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300"
          >
            设置
          </button>
```

在 `ack` 函数末尾加：

```ts
      setHistoryKey((k) => k + 1)
```

在卡片网格之后、组件返回结束前加：

```tsx
      <AlertHistory refreshKey={historyKey} onAck={(id) => void ack(id)} />
      {settingsOpen && (
        <MonitorSettingsDrawer
          config={summary.config}
          servers={summary.servers}
          onClose={() => setSettingsOpen(false)}
          onSaved={(cfg) => {
            setSettingsOpen(false)
            void refreshMonitor()
          }}
        />
      )}
```

- [ ] **Step 6: 运行设置与监控页测试**

Run: `npx vitest run tests/frontend/views/MonitorSettings.test.tsx tests/frontend/views/Monitor.test.tsx`
Expected: 设置 2 个、监控页 3 个测试通过。

- [ ] **Step 7: 提交**

```bash
git add src/views/Monitor.tsx src/views/monitor/AlertHistory.tsx src/views/monitor/MonitorSettingsDrawer.tsx
git commit -m "feat(monitor): 增加告警历史与设置抽屉"
```

---

## T14: 导航角标与仪表盘卡片

- **Depends on:** T12

**Files:**
- Create: `src/components/NavItem.tsx`
- Create: `src/views/dashboard/ServerHealthCard.tsx`
- Modify: `src/App.tsx`
- Modify: `src/views/Dashboard.tsx`
- Test: `tests/frontend/components/NavItem.test.tsx`
- Test: `tests/frontend/views/ServerHealthCard.test.tsx`
- Test: `tests/frontend/navigation.test.ts`

**Interfaces:**
- Consumes: `useMonitorSummary`、`Monitor`、`MonitorSummary`、`ViewKey`。
- Produces:
  - `NavItem({ label, active, badge, onClick })`
  - `ServerHealthCard({ summary, onClick })`
  - `NAV_ITEMS`（从 `App.tsx` 导出，供导航顺序断言）

- **验收:** `npx vitest run tests/frontend/components/NavItem.test.tsx tests/frontend/views/ServerHealthCard.test.tsx tests/frontend/navigation.test.ts` 通过；`npm run test:typecheck` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败的组件测试**

`tests/frontend/components/NavItem.test.tsx`：

```tsx
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import NavItem from '../../../src/components/NavItem'

describe('NavItem', () => {
  it('渲染标签、角标并响应点击', async () => {
    const user = userEvent.setup()
    const onClick = vi.fn()
    render(<NavItem label="监控" active={false} badge={3} onClick={onClick} />)
    expect(screen.getByText('监控')).toBeInTheDocument()
    expect(screen.getByLabelText('3 条未确认告警')).toHaveTextContent('3')
    await user.click(screen.getByRole('button', { name: /监控/ }))
    expect(onClick).toHaveBeenCalledTimes(1)
  })

  it('角标为 0 时不渲染', () => {
    render(<NavItem label="监控" active={false} onClick={() => {}} />)
    expect(screen.queryByLabelText(/未确认告警/)).toBeNull()
  })
})
```

`tests/frontend/views/ServerHealthCard.test.tsx`：

```tsx
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import type { MonitorSummary } from '../../../src/api/types'
import ServerHealthCard from '../../../src/views/dashboard/ServerHealthCard'

// 卡片只读这几个字段；用 partial fixture 保持测试聚焦。
const summary = {
  config_error: null,
  servers: [
    { monitored: true, online: true },
    { monitored: true, online: false },
    { monitored: false, online: null },
  ],
  active_alerts: [{ acked_at: null }, { acked_at: 't' }],
} as unknown as MonitorSummary

describe('ServerHealthCard', () => {
  it('展示在线数/监控数与未确认告警', async () => {
    const user = userEvent.setup()
    const onClick = vi.fn()
    render(<ServerHealthCard summary={summary} onClick={onClick} />)
    expect(screen.getByText('1/2')).toBeInTheDocument()
    expect(screen.getByText('2 条活动告警（1 条未确认）')).toBeInTheDocument()
    await user.click(screen.getByRole('button'))
    expect(onClick).toHaveBeenCalledTimes(1)
  })

  it('无摘要时显示占位，不显示 0/0', () => {
    render(<ServerHealthCard summary={null} onClick={() => {}} />)
    expect(screen.getByText('—')).toBeInTheDocument()
    expect(screen.getByText('尚未采集')).toBeInTheDocument()
  })

  it('配置错误显示异常文案', () => {
    render(
      <ServerHealthCard
        summary={{ ...summary, config_error: 'bad json' } as MonitorSummary}
        onClick={() => {}}
      />,
    )
    expect(screen.getByText('监控配置异常')).toBeInTheDocument()
  })
})
```

`tests/frontend/navigation.test.ts`：

```ts
import { describe, expect, it } from 'vitest'
import { NAV_ITEMS } from '../../src/App'

describe('导航', () => {
  it('监控项紧跟在服务器之后', () => {
    const keys = NAV_ITEMS.map((i) => i.key)
    expect(keys).toContain('monitor')
    expect(keys.indexOf('monitor')).toBe(keys.indexOf('servers') + 1)
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npx vitest run tests/frontend/components/NavItem.test.tsx tests/frontend/views/ServerHealthCard.test.tsx tests/frontend/navigation.test.ts`
Expected: 模块不存在 / `NAV_ITEMS` 未导出。

- [ ] **Step 3: 写 `NavItem.tsx` 与 `ServerHealthCard.tsx`**

`src/components/NavItem.tsx`：

```tsx
export default function NavItem({
  label,
  active,
  badge = 0,
  onClick,
}: {
  label: string
  active: boolean
  badge?: number
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`rounded px-3 py-2 text-left transition-colors ${
        active
          ? 'bg-neutral-800 text-white'
          : 'text-neutral-400 hover:bg-neutral-900 hover:text-neutral-200'
      }`}
    >
      <span className="flex items-center justify-between gap-2">
        <span>{label}</span>
        {badge > 0 && (
          <span
            aria-label={`${badge} 条未确认告警`}
            className="rounded-full bg-red-600 px-1.5 text-xs text-white"
          >
            {badge > 99 ? '99+' : badge}
          </span>
        )}
      </span>
    </button>
  )
}
```

`src/views/dashboard/ServerHealthCard.tsx`：

```tsx
import type { MonitorSummary } from '../../api/types'

export default function ServerHealthCard({
  summary,
  onClick,
}: {
  summary: MonitorSummary | null
  onClick: () => void
}) {
  const monitored = summary?.servers.filter((s) => s.monitored) ?? []
  const online = monitored.filter((s) => s.online === true).length
  const unacked =
    summary?.active_alerts.filter((a) => a.acked_at == null).length ?? 0
  const alert = Boolean(
    summary && (summary.config_error || summary.active_alerts.length > 0),
  )
  const sub =
    summary == null
      ? '尚未采集'
      : summary.config_error
        ? '监控配置异常'
        : summary.active_alerts.length > 0
          ? `${summary.active_alerts.length} 条活动告警（${unacked} 条未确认）`
          : '无活动告警'

  return (
    <button
      type="button"
      onClick={onClick}
      className={`rounded border p-4 text-left transition-colors hover:border-neutral-600 ${
        alert ? 'border-red-900 bg-red-950' : 'border-neutral-800 bg-neutral-900'
      }`}
    >
      <p className="text-xs text-neutral-400">服务器健康</p>
      <p className="mt-1 text-2xl font-semibold">
        {summary ? `${online}/${monitored.length}` : '—'}
      </p>
      <p className="mt-1 text-xs text-neutral-500">{sub}</p>
    </button>
  )
}
```

- [ ] **Step 4: 接入 `App.tsx` 与 `Dashboard.tsx`**

`src/App.tsx`：

1. import 区加：

```ts
import NavItem from './components/NavItem'
import Monitor from './views/Monitor'
import { useMonitorSummary } from './monitor/registry'
```

2. `ViewKey` 加 `'monitor'`；`NAV_ITEMS` 改为 `export const NAV_ITEMS`，并在 `servers` 之后插入：

```ts
  { key: 'monitor', label: '监控' },
```

3. 组件内、`view` state 之后加：

```ts
  const { summary: monitorSummary } = useMonitorSummary()
  const unackedMonitor =
    monitorSummary?.active_alerts.filter((a) => a.acked_at == null).length ?? 0
```

4. 把 `NAV_ITEMS.map` 里的 `<button>…</button>`（`src/App.tsx:57-70`）整体替换为：

```tsx
        {NAV_ITEMS.map((item) => (
          <NavItem
            key={item.key}
            label={item.label}
            active={view === item.key}
            badge={item.key === 'monitor' ? unackedMonitor : 0}
            onClick={() => setView(item.key)}
          />
        ))}
```

5. 条件渲染加：

```tsx
        {view === 'monitor' && <Monitor onNavigate={setView} />}
```

`src/views/Dashboard.tsx`：

1. import 区加：

```ts
import ServerHealthCard from './dashboard/ServerHealthCard'
import { useMonitorSummary } from '../monitor/registry'
```

2. `Dashboard` 组件体内第一行加：

```ts
  const { summary: monitor } = useMonitorSummary()
```

3. 在「订阅」那张 `StatCard` 之后、状态卡网格的 `</div>` 之前加：

```tsx
        <ServerHealthCard
          summary={monitor}
          onClick={() => onNavigate('monitor')}
        />
```

- [ ] **Step 5: 运行导航与仪表盘测试**

Run: `npx vitest run tests/frontend/components/NavItem.test.tsx tests/frontend/views/ServerHealthCard.test.tsx tests/frontend/navigation.test.ts && npm run test:typecheck`
Expected: NavItem 2 个、ServerHealthCard 3 个、导航 1 个测试通过；类型检查通过。

- [ ] **Step 6: 提交**

```bash
git add src/components/NavItem.tsx src/views/dashboard/ServerHealthCard.tsx src/App.tsx src/views/Dashboard.tsx
git commit -m "feat(monitor): 接入导航角标与仪表盘卡片"
```

---

## T15: 收尾、文档与全量验收

- **Depends on:** T1–T14

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `docs/design/ipc-interface-reference.md`
- Test: 全量命令（见下）

**Interfaces:**
- Consumes: 全部已完成任务。
- Produces: 可交付的功能与文档。

- **验收:** 下列全部命令通过，并完成真机读数核对。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 更新 CHANGELOG**

在 `CHANGELOG.md` 的 `## [未发布]` 下、现有最新一节之前加：

```markdown
### 服务器健康监控（应用内）（2026-10-19）

- **新增**：导航「监控」页与仪表盘「服务器健康」卡；每台服务器展示在线状态、
  SSH 连接耗时、CPU/内存/磁盘/负载/运行时长/核数/BBR、xray/ssserver/singbox
  服务状态与 WG 接口。
- **采集边界**：只在桌面端应用打开时按间隔采集，应用退出即停；不做 24×7、
  无界面常驻、计划任务或外部通知；不写服务器配置、不新增监听端口。
- **趋势**：1 小时 / 24 小时 / 7 天，五种指标，纯 SVG 折线；离线空档断线。
- **告警**：离线、CPU、内存、磁盘、负载、服务、WireGuard 七类；连续次数可配；
  应用内确认；全不可达守卫避免本机断网时误报一屏离线。
- **数据**：迁移 v17；样本保留 7 天、已恢复告警保留 90 天；无新增依赖。
- **测试**：解析、告警状态机、存储/迁移、调度器、命令校验与前端组件测试；
  真机读数核对见实施计划 T15。
```

- [ ] **Step 2: 更新 IPC 文档**

在 `docs/design/ipc-interface-reference.md` 增加 7 个命令与 2 个事件：

```markdown
## 服务器健康监控

| 命令 | 入参 | 返回 |
| --- | --- | --- |
| `server_monitor_summary_cmd` | — | `MonitorSummary` |
| `server_monitor_history_cmd` | `serverId`,`metric`,`rangeSecs` | `MonitorPoint[]` |
| `server_monitor_alerts_cmd` | `includeResolved`,`limit` | `MonitorAlert[]` |
| `server_monitor_collect_cmd` | — | `MonitorSummary` |
| `server_monitor_config_cmd` | — | `MonitorConfig` |
| `server_monitor_set_config_cmd` | `config` | `MonitorConfig` |
| `server_monitor_ack_cmd` | `alertId` | `()` |

| 事件 | 载荷 |
| --- | --- |
| `monitor://tick` | `MonitorSummary` |
| `monitor://collecting` | `{ collecting: boolean }` |
```

- [ ] **Step 3: 后端全量检查**

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests
```

Expected: fmt 无 diff；clippy 无 warning/error；全部测试通过（含既有测试）。

- [ ] **Step 4: 前端全量检查**

```bash
npm run test:typecheck
npm test
npm run build
npm run check:org
npm run check:size
npm run check:sensitive
```

Expected: 全部通过；`check:*` 输出「通过：N 个已跟踪文件」。

- [ ] **Step 5: 真机读数核对**

启动桌面端（`cargo run` 或现有运行方式），打开「监控」页：

1. 对每台服务器用 SSH 执行：
   `uptime; cat /proc/loadavg; free -k; df -P -k /; systemctl is-active xray ssserver singbox; wg show interfaces`
   与卡片值对比（CPU 允许约 1 秒窗口的合理波动）。
2. 点击「立即采集」：按钮禁用，完成后数据与「上次成功采集」更新。
3. 暂停自动监控，等待 2 个间隔，确认无新样本；恢复后立即开始一轮。
4. 关闭应用：2 秒内退出；重开后配置与历史仍在，应用关闭期间无样本。
5. 检查日志/告警/UI 中无凭据、私钥或密码。

- [ ] **Step 6: 提交文档**

```bash
git add CHANGELOG.md docs/design/ipc-interface-reference.md
git commit -m "docs(monitor): 补充服务器健康监控说明与 IPC 接口"
```

- [ ] **Step 7: 最终提交计划状态**

把本文件的「执行状态」回写为：

```markdown
## 执行状态（进度指针）

- 当前：T15（completed）
- 下一动作：无
- 阻塞：无
- 已完成：T1–T15
- 最后更新：YYYY-MM-DD
```

然后提交计划更新（若计划文件已随仓库管理）：

```bash
git add docs/superpowers/plans/2026-10-19-server-health-monitor.md
git commit -m "docs(monitor): 完成实施计划执行记录"
```

## Self-Review

**Spec coverage:**

| 规格章节 | 对应任务 |
| --- | --- |
| 1 边界/可达语义 | T1/T3/T6/T7 |
| 2 架构/锁 | T7 |
| 3.1 v17 迁移 | T2 |
| 3.2 配置 | T1 |
| 3.3 阈值 | T1/T4 |
| 4.1 脚本 | T6 |
| 4.2–4.5 指标/解析/错误 | T3/T6/T7 |
| 5 调度 | T7 |
| 6 告警 | T4/T5 |
| 7 IPC/类型/历史 | T5/T8/T9 |
| 8 前端 | T10–T14 |
| 9 错误处理与安全 | T3/T6/T7/T8 |
| 10 测试 | 每个任务的测试步骤 + T15 |
| 11 验收 | T15 |
| 12 实施顺序 | 本计划任务顺序 |
| 13 风险 | 设计文档已记录；实现不改变边界 |

**Placeholder scan:** 无 TBD/TODO/「类似 Task N」；每个测试与实现块都是可直接落盘的内容。子模块声明按任务逐个添加，避免 T1 引用尚未创建的文件。

**Type consistency:** `MonitorSample`/`MonitorAlert`/`MonitorSummary`/`ServerHealth` 在 T1 定义，T2–T9 的字段名一致；`MonitorStatus` 在 T1 定义，T5 的摘要与 T7 的调度器共用，T8 通过 `handle.shared.status` 读取；`MonitorShared`/`MonitorHandle` 在 T7 定义；`PROBE_SCRIPT`/`ProbeOutcome` 在 T6 定义，T7 使用；`AlertTransition` 在 T4 定义，T5 使用；`MonitorMetric` 在 T1 定义，T2/T8 使用；`NAV_ITEMS`/`NavItem`/`ServerHealthCard` 在 T14 定义并测试。

**Goal Contract 完整性:** Original Goal 引用用户原话；Hard Constraints 逐条；A1–A10 均可由命令或真机操作证明。

**模块地图三关系:** 层级/调用/依赖三块均在，节点带中文功能名。

**Task 卡字段:** T1–T15 均有稳定 ID、`Depends on`、验收、状态；依赖只引用已定义 ID。
