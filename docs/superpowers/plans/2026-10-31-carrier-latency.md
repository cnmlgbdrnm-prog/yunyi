# 三网延迟测试 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. 执行中回写「执行状态」段。

**Goal Contract**

- **Original Goal:** 「我看到有的开源项目可以做三网延迟测试？你去找找，然后把这个功能加到服务器监控里面去……然后常态就直接展开把！然后服务器首先展示延迟，还有现在的cpu内存什么的百分比都没有，我说的是图表」；「对了记住，前端只负责渲染」。已确认：测量方向 = 服务器 → 国内三网；每台服务器单独选一个地区，选定后只采样该地区三网 3 个目标；跟随现有采集轮次（默认 60 秒）；一个格子 = 一次探测；探测协议 = TCP 建连；参考 Komari 的形态。
- **Hard Constraints:**
  - 采集、聚合、阈值、调度、下采样全部在 Rust 后端；前端只做字段映射与渲染，不做统计/分级/轮询。
  - 目标固定为 31 个省级行政区 × 电信/联通/移动（`<code>-{ct,cu,cm}-v4.ip.zstaticcdn.com:80`）；台湾/香港/澳门无目标，不提供。
  - 每轮每网 1 次探测，复用现有 SSH 会话与 60 秒轮次；三网失败不影响资源采集与其他服务器。
  - 失败口径 = **TCP 建连失败率**，不是 ICMP 丢包；不引入 ICMP。
  - 复用 `server_metric_samples`、现有 7 天保留期、现有告警语义（三网不告警）。
  - 不新增 npm/cargo 依赖；不新增端口/agent/外部服务。
  - 文档、代码、夹具、IPC 载荷不出现真实 IP、凭据、私钥；示例只用文档地址段（203.0.113.0/24 等）。
  - 迁移号接续现有 v21 → v22；旧二进制遇到新库必须继续报「数据目录比本程序新」。
  - 工作区有大量与本次无关的未提交改动：每个提交只 `git add` 本任务列出的文件，禁止 `git add -A`。
  - 本地测试源码在 `tests/`（被 `.gitignore` 忽略，不随 git 提交）；提交步骤不包含 `tests/`。
  - 现有运行方式：`cargo test --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests`；`cargo fmt --check`、`cargo clippy -- -D warnings`、`npx tsc`、`npm test`。
- **Acceptance Criteria:**
  - A1: v21→v22 迁移可重复执行；三网七列（region + 3×(ok,ms)）NULL/失败/成功三种状态可正确往返，旧样本保留。
  - A2: 每台服务器可配置 31 省之一或「不测试」；非法 code 保存被拒绝；normalize 不丢 `carrier_region`。
  - A3: 配置地区时探针脚本包含 3 条并发 curl 与 `##ct/##cu/##cm` 段；未配置时脚本与旧版逐字一致（无 curl、无三网段）。
  - A4: 解析区分「成功 / 探了失败 / 没探到」；窗口统计只看当前地区与真正探过的样本；均值/失败率/分级/20 格符合设计 §4.4。
  - A5: `ServerHealth.carrier` 由后端给出全部数字与格子；`monitor_carrier_regions_cmd` 返回 31 省；`monitor_resources_cmd` 一次返回三指标且 ≤60 点。
  - A6: 监控页卡片默认全部展开；三网延迟/丢包面板在资源图之前；资源图为 CPU/内存/磁盘迷你折线；未配置/等待采集/全失败三种空态正确。
  - A7: 旧 `TrendPanel`、`monitor_history` 命令/封装、`MonitorMetric`、`store::history` 全部退役，无死代码；IPC 参考文档更新。
  - A8: fmt/clippy/tsc/前端测试/后端测试通过；真机配置一个地区跑一轮，卡片与数据库写入一致。

## 模块地图（ASCII）

**层级（包含关系）**

```
MonitorView（服务器监控页）
├─ ServerCard（服务器卡片，默认展开）
│  ├─ CarrierPanel（三网延迟/丢包面板）
│  ├─ ResourceCharts（资源迷你图容器）
│  │  └─ Sparkline（迷你折线）
│  └─ Details / Badges（现有负载、服务、WG 明细）
└─ MonitorSettingsDrawer（监控设置抽屉）
   └─ 地区下拉（每台服务器一个）
```

**调用（运行时触发）**

```
MonitorView ──▶ ServerCard（渲染全部服务器，不再有展开开关）
ServerCard ──▶ CarrierPanel（carrier 为空 → 未配置空态）
ServerCard ──▶ ResourceCharts ──▶ monitor_resources_cmd（切范围重新取数）
MonitorSettingsDrawer ──▶ monitor_carrier_regions_cmd（加载 31 省）
MonitorSettingsDrawer ──▶ monitor_set_config_cmd（保存 carrier_region）
scheduler（调度器）──▶ monitor://tick（MonitorSummary 含 carrier）──▶ MonitorView
```

**依赖（数据 / 契约来源）**

```
CarrierPanel ⇠── CarrierHealth（ServerHealth.carrier，后端算好数字/等级/格子）
ResourceCharts ⇠── ServerResources（monitor_resources_cmd，后端下采样）
MonitorSettingsDrawer ⇠── CarrierRegion[]（monitor_carrier_regions_cmd）
server_monitor/carrier.rs ⇠── config::CarrierConfig（窗口/格数/分级）
server_monitor/summary.rs ⇠── store::samples_since + carrier::summarise
server_monitor/probe.rs ⇠── carrier::target_hosts / probe_setup / probe_tail
```

## 执行状态（进度指针）

- 当前：T9（completed）
- 下一动作：无
- 阻塞：无
- 已完成：T1–T9（迁移 v22 / carrier 模块 / 配置 / 探针 / 摘要 / IPC / 前端 / 设置抽屉与退役旧趋势 / 文档与真机）
- 最后更新：2026-10-31
- 真机证据：迁移 user_version=22；上海三网在 4 台服务器上均写入 `ct/cu/cm ok=1` 与
  毫秒值；期间发现并修复「curl -w 无换行导致 `##cu/##cm` 标记粘行」的真机缺陷
  （提交 d57fc51）。全量测试 1129 通过、2 个并发计时 flake 单跑通过；前端
  Monitor/MonitorSettings 用例通过，DeployWizard 的 `localStorage` 缺失为本地
  jsdom 环境既有问题。

---

### T1: 迁移 v22 与三网类型/存储

- **Depends on:** 无

**Files:**
- Modify: `src-tauri/src/db/migrations.rs`（在 v21 条目后追加 v22）
- Modify: `src-tauri/src/server_monitor/mod.rs`（三网类型、`MonitorSample.carrier`、`ServerHealth.carrier`、`ServerResources`、`CarrierRegion`）
- Modify: `src-tauri/src/server_monitor/store.rs`（`SAMPLE_COLS`、`row_to_sample`、`insert_sample`）
- Test: `tests/rust/server_monitor/store_tests.rs`（本地，不入库）

**Interfaces:**
- Produces:
  - `CarrierProbe { ok: bool, ms: Option<f64> }`
  - `CarrierSample { region: String, ct/cu/cm: Option<CarrierProbe> }`
  - `MonitorSample.carrier: Option<CarrierSample>`
  - `CarrierBar { ts: String, ok: bool, latency_level: Option<u8> }`
  - `CarrierStats { key, avg_ms, avg_level, loss_pct, loss_level, samples, bars }`
  - `CarrierHealth { region, region_name, bar_count, carriers }`
  - `CarrierRegion { code, name }`
  - `ServerResources { cpu_pct, mem_pct, disk_pct }`
  - DB 列顺序：`carrier_region, ct_ok, ct_ms, cu_ok, cu_ms, cm_ok, cm_ms`

- **验收:** `cargo test --features local-tests server_monitor::store:: -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过，且新增三网往返用例通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试（本地测试文件）**

在 `tests/rust/server_monitor/store_tests.rs` 追加：

```rust
#[test]
fn carrier_columns_roundtrip_three_states() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    let mut s = sample(1, "2026-10-31T00:00:00.000Z", Some(12.5));
    s.carrier = Some(crate::server_monitor::CarrierSample {
        region: "gd".into(),
        ct: Some(crate::server_monitor::CarrierProbe { ok: true, ms: Some(244.5) }),
        cu: Some(crate::server_monitor::CarrierProbe { ok: false, ms: None }),
        cm: None,
    });
    store::insert_sample(&db.lock(), &s).unwrap();
    let got = store::latest_sample(&db.lock(), 1).unwrap().unwrap();
    assert_eq!(got.carrier, s.carrier);
}

#[test]
fn no_carrier_is_null_not_zero() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    store::insert_sample(&db.lock(), &sample(1, "2026-10-31T00:00:00.000Z", Some(1.0))).unwrap();
    assert_eq!(store::latest_sample(&db.lock(), 1).unwrap().unwrap().carrier, None);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --features local-tests carrier_columns_roundtrip -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，提示 `CarrierSample` 不存在 / `MonitorSample` 无 `carrier` 字段。

- [ ] **Step 3: 加迁移 v22**

在 `src-tauri/src/db/migrations.rs` 的 `MIGRATIONS` 末尾（v21 条目之后）追加：

```rust
    r#"
-- v22：三网延迟（服务器 → 国内三网 TCP 建连）。
-- 每轮每网一次探测。carrier_region 记录该样本探测的省份 code：
-- NULL = 未配置地区或本轮未探测；切换地区后旧地区样本不能混进统计。
-- ct/cu/cm 三组列：ok NULL = 本轮没探到（SSH/脚本失败），ok=0 = 探了但建连失败；
-- ms 仅在 ok=1 时非空。
ALTER TABLE server_metric_samples ADD COLUMN carrier_region TEXT;
ALTER TABLE server_metric_samples ADD COLUMN ct_ok INTEGER;
ALTER TABLE server_metric_samples ADD COLUMN ct_ms REAL;
ALTER TABLE server_metric_samples ADD COLUMN cu_ok INTEGER;
ALTER TABLE server_metric_samples ADD COLUMN cu_ms REAL;
ALTER TABLE server_metric_samples ADD COLUMN cm_ok INTEGER;
ALTER TABLE server_metric_samples ADD COLUMN cm_ms REAL;
"#,
```

- [ ] **Step 4: 加类型**

`src-tauri/src/server_monitor/mod.rs` 在 `MonitorSample` 前加：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarrierProbe {
    pub ok: bool,
    pub ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarrierSample {
    pub region: String,
    pub ct: Option<CarrierProbe>,
    pub cu: Option<CarrierProbe>,
    pub cm: Option<CarrierProbe>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarrierBar {
    pub ts: String,
    pub ok: bool,
    pub latency_level: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarrierStats {
    pub key: String,
    pub avg_ms: Option<f64>,
    pub avg_level: Option<u8>,
    pub loss_pct: Option<f64>,
    pub loss_level: Option<u8>,
    pub samples: u32,
    pub bars: Vec<CarrierBar>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarrierHealth {
    pub region: String,
    pub region_name: String,
    pub bar_count: usize,
    pub carriers: Vec<CarrierStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarrierRegion {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerResources {
    pub cpu_pct: Vec<MonitorPoint>,
    pub mem_pct: Vec<MonitorPoint>,
    pub disk_pct: Vec<MonitorPoint>,
}
```

`MonitorSample` 末尾加：

```rust
    #[serde(default)]
    pub carrier: Option<CarrierSample>,
```

`ServerHealth` 末尾加：

```rust
    #[serde(default)]
    pub carrier: Option<CarrierHealth>,
```

- [ ] **Step 5: 改 store 读写**

`SAMPLE_COLS` 末尾追加：

```rust
const SAMPLE_COLS: &str = "server_id,ts,reachable,connect_ms,uptime_secs,load1,load5,load15,\
cpu_pct,cpu_cores,mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,bbr_algo,services_json,wg_json,error,\
carrier_region,ct_ok,ct_ms,cu_ok,cu_ms,cm_ok,cm_ms";
```

`row_to_sample` 末尾（`Ok(MonitorSample { ... })` 内加 `carrier`）：

```rust
    let region: Option<String> = r.get(18)?;
    let ct_ok: Option<i64> = r.get(19)?;
    let ct_ms: Option<f64> = r.get(20)?;
    let cu_ok: Option<i64> = r.get(21)?;
    let cu_ms: Option<f64> = r.get(22)?;
    let cm_ok: Option<i64> = r.get(23)?;
    let cm_ms: Option<f64> = r.get(24)?;
    let carrier = region.map(|region| super::CarrierSample {
        region,
        ct: probe_from(ct_ok, ct_ms),
        cu: probe_from(cu_ok, cu_ms),
        cm: probe_from(cm_ok, cm_ms),
    });
```

同文件加：

```rust
fn probe_from(ok: Option<i64>, ms: Option<f64>) -> Option<super::CarrierProbe> {
    ok.map(|v| super::CarrierProbe {
        ok: v != 0,
        ms: if v != 0 { ms } else { None },
    })
}
```

`insert_sample` 的 SQL 列与 VALUES 追加 7 列，参数追加：

```rust
            carrier_region,
            ct_ok,
            ct_ms,
            cu_ok,
            cu_ms,
            cm_ok,
            cm_ms,
```

在函数开头计算：

```rust
    let (carrier_region, ct_ok, ct_ms, cu_ok, cu_ms, cm_ok, cm_ms) = match &s.carrier {
        Some(c) => (
            Some(c.region.clone()),
            c.ct.as_ref().map(|p| p.ok as i64),
            c.ct.as_ref().and_then(|p| p.ms),
            c.cu.as_ref().map(|p| p.ok as i64),
            c.cu.as_ref().and_then(|p| p.ms),
            c.cm.as_ref().map(|p| p.ok as i64),
            c.cm.as_ref().and_then(|p| p.ms),
        ),
        None => (None, None, None, None, None, None, None),
    };
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test --features local-tests server_monitor::store:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS（含既有迁移/历史用例）。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/db/migrations.rs src-tauri/src/server_monitor/mod.rs src-tauri/src/server_monitor/store.rs
git commit -m "feat(monitor): 增加三网延迟样本存储（v22）"
```

---

### T2: carrier 基础模块（地区/目标/脚本/解析/分级）

- **Depends on:** T1

**Files:**
- Create: `src-tauri/src/server_monitor/carrier.rs`
- Modify: `src-tauri/src/server_monitor/mod.rs`（`pub mod carrier;`）
- Modify: `src-tauri/src/server_monitor/parse.rs`（`split_sections` 提升为 `pub(crate)`）
- Test: `tests/rust/server_monitor/carrier_tests.rs`

**Interfaces:**
- Produces:
  - `carrier::REGIONS: &[Region]`（31 省）
  - `carrier::regions() -> Vec<CarrierRegion>`
  - `carrier::region_name(&str) -> Option<&'static str>`
  - `carrier::target_hosts(&str) -> Result<[String; 3], String>`
  - `carrier::probe_setup(&[String; 3]) -> String`
  - `carrier::probe_tail() -> &'static str`
  - `carrier::parse(&str, &str) -> CarrierSample`
  - `carrier::latency_level(f64, &[f64; 4]) -> u8`
  - `carrier::loss_level(f64, &[f64; 4]) -> u8`

- **验收:** `cargo test --features local-tests carrier:: -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试**

新建 `tests/rust/server_monitor/carrier_tests.rs`：

```rust
use super::*;

#[test]
fn all_31_regions_have_three_targets() {
    assert_eq!(REGIONS.len(), 31);
    assert_eq!(region_name("gd"), Some("广东"));
    assert_eq!(region_name("tw"), None);
    let hosts = target_hosts("gd").unwrap();
    assert_eq!(hosts[0], "gd-ct-v4.ip.zstaticcdn.com");
    assert_eq!(hosts[1], "gd-cu-v4.ip.zstaticcdn.com");
    assert_eq!(hosts[2], "gd-cm-v4.ip.zstaticcdn.com");
    assert!(target_hosts("xx").is_err());
}

#[test]
fn probe_fragment_runs_three_curls_in_parallel_before_wait() {
    let hosts = target_hosts("gd").unwrap();
    let setup = probe_setup(&hosts);
    assert_eq!(setup.matches("_c '").count(), 3);
    assert!(setup.contains("--connect-timeout 3"));
    assert!(setup.contains("--max-time 4"));
    assert!(setup.contains("&\n"));
    let tail = probe_tail();
    assert!(tail.starts_with("wait\n"));
    assert!(tail.contains("echo '##ct'"));
    assert!(tail.contains("rm -rf \"$d\""));
}

#[test]
fn parse_distinguishes_success_failure_and_missing() {
    let out = "##ct\n0.229489\n##cu\n0.000000\n##done\n";
    let c = parse(out, "gd");
    assert_eq!(c.region, "gd");
    assert_eq!(c.ct, Some(CarrierProbe { ok: true, ms: Some(229.5) }));
    assert_eq!(c.cu, Some(CarrierProbe { ok: false, ms: None }));
    assert_eq!(c.cm, None);
}

#[test]
fn parse_empty_section_is_failure_but_absent_is_unknown() {
    assert_eq!(parse("##ct\n\n##done\n", "gd").ct, Some(CarrierProbe { ok: false, ms: None }));
    assert_eq!(parse("##done\n", "gd").ct, None);
}

#[test]
fn levels_use_threshold_boundaries() {
    let latency = [120.0, 200.0, 300.0, 400.0];
    assert_eq!(latency_level(120.0, &latency), 1);
    assert_eq!(latency_level(120.1, &latency), 2);
    assert_eq!(latency_level(400.0, &latency), 4);
    assert_eq!(latency_level(400.1, &latency), 5);
    let loss = [1.0, 3.0, 6.0, 9.0];
    assert_eq!(loss_level(0.0, &loss), 1);
    assert_eq!(loss_level(3.0, &loss), 2);
    assert_eq!(loss_level(9.1, &loss), 5);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --features local-tests carrier:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，`carrier` 模块不存在。

- [ ] **Step 3: 实现 carrier.rs**

新建文件（省份顺序按行政区划）：

```rust
//! 三网延迟：地区表、目标、脚本片段、解析与分级。纯函数，无 IO。
use super::{CarrierProbe, CarrierRegion, CarrierSample};

pub struct Region {
    pub code: &'static str,
    pub name: &'static str,
}

pub const REGIONS: &[Region] = &[
    Region { code: "bj", name: "北京" },
    Region { code: "tj", name: "天津" },
    Region { code: "he", name: "河北" },
    Region { code: "sx", name: "山西" },
    Region { code: "nm", name: "内蒙古" },
    Region { code: "ln", name: "辽宁" },
    Region { code: "jl", name: "吉林" },
    Region { code: "hl", name: "黑龙江" },
    Region { code: "sh", name: "上海" },
    Region { code: "js", name: "江苏" },
    Region { code: "zj", name: "浙江" },
    Region { code: "ah", name: "安徽" },
    Region { code: "fj", name: "福建" },
    Region { code: "jx", name: "江西" },
    Region { code: "sd", name: "山东" },
    Region { code: "ha", name: "河南" },
    Region { code: "hb", name: "湖北" },
    Region { code: "hn", name: "湖南" },
    Region { code: "gd", name: "广东" },
    Region { code: "gx", name: "广西" },
    Region { code: "hi", name: "海南" },
    Region { code: "cq", name: "重庆" },
    Region { code: "sc", name: "四川" },
    Region { code: "gz", name: "贵州" },
    Region { code: "yn", name: "云南" },
    Region { code: "xz", name: "西藏" },
    Region { code: "sn", name: "陕西" },
    Region { code: "gs", name: "甘肃" },
    Region { code: "qh", name: "青海" },
    Region { code: "nx", name: "宁夏" },
    Region { code: "xj", name: "新疆" },
];

pub fn regions() -> Vec<CarrierRegion> {
    REGIONS
        .iter()
        .map(|r| CarrierRegion { code: r.code.into(), name: r.name.into() })
        .collect()
}

pub fn region_name(code: &str) -> Option<&'static str> {
    REGIONS.iter().find(|r| r.code == code).map(|r| r.name)
}

pub fn target_hosts(region: &str) -> Result<[String; 3], String> {
    if region_name(region).is_none() {
        return Err(format!("未知的三网地区: {region}"));
    }
    Ok(["ct", "cu", "cm"].map(|c| format!("{region}-{c}-v4.ip.zstaticcdn.com")))
}

pub fn probe_setup(hosts: &[String; 3]) -> String {
    format!(
        "d=$(mktemp -d 2>/dev/null) || d=/tmp/.vpnctl-probe-$$\n\
         mkdir -p \"$d\"\n\
         _c(){{ curl -o /dev/null -s -w '%{{time_connect}}' --connect-timeout 3 --max-time 4 \"http://$1/\" 2>/dev/null; }}\n\
         _c '{}' > \"$d/ct\" 2>/dev/null &\n\
         _c '{}' > \"$d/cu\" 2>/dev/null &\n\
         _c '{}' > \"$d/cm\" 2>/dev/null &\n",
        hosts[0], hosts[1], hosts[2]
    )
}

pub fn probe_tail() -> &'static str {
    "wait\necho '##ct'; cat \"$d/ct\" 2>/dev/null\n\
     echo '##cu'; cat \"$d/cu\" 2>/dev/null\n\
     echo '##cm'; cat \"$d/cm\" 2>/dev/null\n\
     rm -rf \"$d\"\n"
}

pub fn parse(out: &str, region: &str) -> CarrierSample {
    let sections = super::parse::split_sections(out);
    CarrierSample {
        region: region.into(),
        ct: section_probe(sections.get("ct")),
        cu: section_probe(sections.get("cu")),
        cm: section_probe(sections.get("cm")),
    }
}

fn section_probe(section: Option<&String>) -> Option<CarrierProbe> {
    let Some(section) = section else { return None };
    let secs = section.trim().parse::<f64>().ok().filter(|v| *v > 0.0);
    Some(match secs {
        Some(secs) => CarrierProbe {
            ok: true,
            ms: Some((secs * 10_000.0).round() / 10.0),
        },
        None => CarrierProbe { ok: false, ms: None },
    })
}

pub fn latency_level(ms: f64, levels: &[f64; 4]) -> u8 {
    match levels.iter().position(|t| ms <= *t) {
        Some(i) => i as u8 + 1,
        None => 5,
    }
}

pub fn loss_level(pct: f64, levels: &[f64; 4]) -> u8 {
    match levels.iter().position(|t| pct <= *t) {
        Some(i) => i as u8 + 1,
        None => 5,
    }
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/carrier_tests.rs"
    ));
}
```

- [ ] **Step 4: 接模块与 split_sections**

`src-tauri/src/server_monitor/mod.rs` 子模块列表加 `pub mod carrier;`（保持字母序）。
`src-tauri/src/server_monitor/parse.rs`：`fn split_sections` 改为 `pub(crate) fn split_sections`。

- [ ] **Step 5: 跑测试**

Run: `cargo test --features local-tests carrier:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/server_monitor/carrier.rs src-tauri/src/server_monitor/mod.rs src-tauri/src/server_monitor/parse.rs
git commit -m "feat(monitor): 增加三网目标/脚本/解析模块"
```

---

### T3: 配置（carrier 段 + 每台地区）

- **Depends on:** T2

**Files:**
- Modify: `src-tauri/src/server_monitor/config.rs`
- Modify: `src-tauri/src/server_monitor/mod.rs`（`pub use config::{CarrierConfig, MonitorConfig, Thresholds};`）
- Test: `tests/rust/server_monitor/config_tests.rs`、`tests/rust/server_monitor/commands_tests.rs`（本地）

**Interfaces:**
- Produces:
  - `CarrierConfig { window_secs: u64, bars: usize, latency_levels_ms: [f64; 4], loss_levels_pct: [f64; 4] }`
  - `MonitorConfig.carrier: CarrierConfig`
  - `ServerMonitorOverride.carrier_region: Option<String>`
  - `config::carrier_region(&MonitorConfig, i64) -> Option<String>`

- **验收:** `cargo test --features local-tests config:: -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过，且合法/非法地区与分级校验用例通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试**

`tests/rust/server_monitor/config_tests.rs` 追加：

```rust
#[test]
fn carrier_defaults_and_region_roundtrip() {
    let cfg = MonitorConfig::default();
    assert_eq!(cfg.carrier.window_secs, 3600);
    assert_eq!(cfg.carrier.bars, 20);
    assert_eq!(cfg.carrier.latency_levels_ms, [120.0, 200.0, 300.0, 400.0]);
    let mut cfg = cfg;
    cfg.servers.insert(
        "1".into(),
        ServerMonitorOverride { carrier_region: Some("gd".into()), ..Default::default() },
    );
    assert_eq!(carrier_region(&cfg, 1).as_deref(), Some("gd"));
}

#[test]
fn invalid_region_and_levels_are_rejected() {
    let mut cfg = MonitorConfig::default();
    cfg.servers.insert(
        "1".into(),
        ServerMonitorOverride { carrier_region: Some("tw".into()), ..Default::default() },
    );
    assert!(validate(&cfg).is_err());

    let mut cfg = MonitorConfig::default();
    cfg.carrier.latency_levels_ms = [120.0, 100.0, 300.0, 400.0];
    assert!(validate(&cfg).is_err());

    let mut cfg = MonitorConfig::default();
    cfg.carrier.loss_levels_pct = [1.0, 3.0, 6.0, 101.0];
    assert!(validate(&cfg).is_err());
}

#[test]
fn normalize_keeps_region_only_entries() {
    let mut cfg = MonitorConfig::default();
    cfg.servers.insert(
        "1".into(),
        ServerMonitorOverride { carrier_region: Some("gd".into()), ..Default::default() },
    );
    let saved = save(&SettingsService::memory_for_test(), &cfg).unwrap();
    assert_eq!(saved.servers["1"].carrier_region.as_deref(), Some("gd"));
}
```

若 `SettingsService::memory_for_test()` 不存在，改用现有 `commands_tests` 的 `AppState`/`SettingsService` 构造方式（实现时以已有测试为准）；核心断言是 normalize 保留条目。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --features local-tests carrier_defaults -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，缺 `carrier` 字段/函数。

- [ ] **Step 3: 实现**

`config.rs` 加：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

fn d_carrier_window() -> u64 { 3600 }
fn d_carrier_bars() -> usize { 20 }
fn d_carrier_latency_levels() -> [f64; 4] { [120.0, 200.0, 300.0, 400.0] }
fn d_carrier_loss_levels() -> [f64; 4] { [1.0, 3.0, 6.0, 9.0] }
```

`MonitorConfig` 增加字段并在 `Default` 中赋值：

```rust
    #[serde(default)]
    pub carrier: CarrierConfig,
```

```rust
            carrier: CarrierConfig::default(),
```

`ServerMonitorOverride` 增加：

```rust
    #[serde(default)]
    pub carrier_region: Option<String>,
```

`validate` 末尾加：

```rust
    if !(600..=86400).contains(&cfg.carrier.window_secs) {
        return Err("三网统计窗口必须在 600–86400 秒之间".into());
    }
    if !(10..=60).contains(&cfg.carrier.bars) {
        return Err("三网格子数必须在 10–60 之间".into());
    }
    validate_levels(&cfg.carrier.latency_levels_ms, "三网延迟分级", 10_000.0)?;
    validate_levels(&cfg.carrier.loss_levels_pct, "三网失败率分级", 100.0)?;
    for (key, ov) in &cfg.servers {
        if let Some(region) = &ov.carrier_region {
            if super::carrier::region_name(region).is_none() {
                return Err(format!("服务器 {key} 的三网地区无效: {region}"));
            }
        }
    }
```

加辅助函数：

```rust
fn validate_levels(v: &[f64; 4], name: &str, max: f64) -> Result<(), String> {
    if v.iter().any(|x| !x.is_finite() || *x < 0.0 || *x > max) {
        return Err(format!("{name}必须是 0–{max} 之间的数"));
    }
    if !(v[0] < v[1] && v[1] < v[2] && v[2] < v[3]) {
        return Err(format!("{name}必须严格递增"));
    }
    Ok(())
}
```

加：

```rust
pub fn carrier_region(cfg: &MonitorConfig, server_id: i64) -> Option<String> {
    cfg.servers
        .get(&server_id.to_string())
        .and_then(|o| o.carrier_region.clone())
}
```

`normalize` 的 `retain` 条件改为：

```rust
    cfg.servers.retain(|_, ov| {
        ov.enabled == Some(false) || ov.thresholds.is_some() || ov.carrier_region.is_some()
    });
```

`mod.rs` 的 `pub use config::{MonitorConfig, Thresholds};` 改为
`pub use config::{CarrierConfig, MonitorConfig, Thresholds};`。

- [ ] **Step 4: 修既有结构体字面量**

`tests/rust/server_monitor/commands_tests.rs` 里的 `ServerMonitorOverride { enabled: Some(false), thresholds: None }` 改成
`ServerMonitorOverride { enabled: Some(false), thresholds: None, carrier_region: None }`。
其他出现 `ServerMonitorOverride {` 的本地测试同样补字段。

- [ ] **Step 5: 跑测试**

Run: `cargo test --features local-tests config:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/server_monitor/config.rs src-tauri/src/server_monitor/mod.rs
git commit -m "feat(monitor): 增加三网地区与分级配置"
```

---

### T4: 探针脚本与调度接入

- **Depends on:** T1、T2、T3

**Files:**
- Modify: `src-tauri/src/server_monitor/probe.rs`
- Modify: `src-tauri/src/server_monitor/scheduler.rs`
- Test: `tests/rust/server_monitor/probe_tests.rs`、`tests/rust/server_monitor/scheduler_tests.rs`

**Interfaces:**
- Consumes: `carrier::target_hosts/probe_setup/probe_tail/parse`、`config::carrier_region`
- Produces:
  - `probe::probe_script(Option<&str>) -> String`（替换 `pub const PROBE_SCRIPT`）
  - `MonitorTarget.carrier_region: Option<String>`
  - `MonitorSample.carrier` 在调度器中填充

- **验收:** `cargo test --features local-tests probe:: -- --skip net::runtime_tests --skip net::runtime_service_tests` 与 `scheduler::` 均通过；无地区时脚本与旧版逐字一致。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试**

`tests/rust/server_monitor/probe_tests.rs` 改为：

```rust
use super::*;

#[test]
fn no_region_script_is_the_legacy_script() {
    let script = probe_script(None);
    for marker in [
        "##uptime", "##load", "##cpu1", "##cpu2", "##mem", "##disk",
        "##cores", "##bbr", "##svc", "##wg", "##wg_rc", "##done",
    ] {
        assert!(script.contains(marker), "缺少 {marker}");
    }
    assert!(!script.contains("curl"));
    assert!(!script.contains("##ct"));
    assert!(!script.contains("mktemp"));
}

#[test]
fn region_script_has_three_curls_before_body_and_tail_before_done() {
    let script = probe_script(Some("gd"));
    assert_eq!(script.matches("_c '").count(), 3);
    assert!(script.contains("gd-ct-v4.ip.zstaticcdn.com"));
    let setup = script.find("mktemp").unwrap();
    let uptime = script.find("##uptime").unwrap();
    let wg_rc = script.find("##wg_rc").unwrap();
    let wait = script.find("\nwait\n").unwrap();
    let ct = script.find("echo '##ct'").unwrap();
    let done = script.find("##done").unwrap();
    assert!(setup < uptime && uptime < wg_rc && wg_rc < wait && wait < ct && ct < done);
}

#[test]
fn script_is_read_only() {
    let script = probe_script(Some("gd"));
    assert!(!script.contains("sudo"));
    assert!(!script.contains("systemctl restart"));
    assert!(script.contains("systemctl is-active"));
    assert!(script.contains("sleep 1"));
}
```

`tests/rust/server_monitor/scheduler_tests.rs` 追加（复用现有 `state/insert_server/connected/shared`）：

```rust
#[tokio::test]
async fn round_writes_carrier_columns_only_when_region_configured() {
    let (_dir, state) = state();
    insert_server(&state, 1);
    insert_server(&state, 2);
    let mut cfg = MonitorConfig::default();
    cfg.servers.insert(
        "1".into(),
        crate::server_monitor::config::ServerMonitorOverride {
            carrier_region: Some("gd".into()),
            ..Default::default()
        },
    );
    config::save(&state.settings, &cfg).unwrap();
    let probe = Arc::new(FakeProbe::default());
    let out = "##ct\n0.244500\n##cu\n0.000000\n##done\n";
    {
        let mut m = probe.outcomes.lock().unwrap();
        m.insert(1, connected(out));
        m.insert(2, connected(out));
    }
    run_round(&state, &sh(), probe).await.unwrap();
    let conn = state.db.lock();
    let one = store::latest_sample(&conn, 1).unwrap().unwrap();
    let two = store::latest_sample(&conn, 2).unwrap().unwrap();
    assert_eq!(one.carrier.as_ref().unwrap().region, "gd");
    assert_eq!(one.carrier.as_ref().unwrap().ct, Some(CarrierProbe { ok: true, ms: Some(244.5) }));
    assert_eq!(two.carrier, None);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --features local-tests probe:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，`probe_script` 不存在。

- [ ] **Step 3: 改 probe.rs**

把 `pub const PROBE_SCRIPT: &str = r#"..."#;` 改成：

```rust
const PROBE_BODY: &str = r#"echo '##uptime'; cat /proc/uptime 2>/dev/null
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
"#;

pub fn probe_script(region: Option<&str>) -> String {
    let hosts = region.and_then(|r| super::carrier::target_hosts(r).ok());
    let mut out = String::new();
    if let Some(hosts) = &hosts {
        out.push_str(&super::carrier::probe_setup(hosts));
    }
    out.push_str(PROBE_BODY);
    if hosts.is_some() {
        out.push_str(super::carrier::probe_tail());
    }
    out.push_str("echo '##done'\n");
    out
}
```

`MonitorTarget` 增加：

```rust
    pub carrier_region: Option<String>,
```

`RealProbe::probe` 的 `run` 改为：

```rust
        let script = probe_script(target.carrier_region.as_deref());
        match session
            .run("服务器健康采集", &script, COMMAND_TIMEOUT)
            .await
```

- [ ] **Step 4: 改 scheduler.rs**

`collect_targets` 的 `MonitorTarget` 构造加：

```rust
            carrier_region: config::carrier_region(cfg, server.id),
```

`Connected` 分支在 `let parsed = parse_probe_output(&stdout);` 后加：

```rust
                let carrier = target
                    .carrier_region
                    .as_deref()
                    .map(|region| super::carrier::parse(&stdout, region));
```

并给该分支的 `MonitorSample` 加 `carrier,`。

- [ ] **Step 5: 跑测试**

Run: `cargo test --features local-tests server_monitor::probe:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Run: `cargo test --features local-tests server_monitor::scheduler:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/server_monitor/probe.rs src-tauri/src/server_monitor/scheduler.rs
git commit -m "feat(monitor): 探针按地区并发探测三网"
```

---

### T5: 窗口聚合与摘要

- **Depends on:** T1、T2、T3、T4

**Files:**
- Modify: `src-tauri/src/server_monitor/carrier.rs`（`summarise`）
- Modify: `src-tauri/src/server_monitor/store.rs`（`samples_since`）
- Modify: `src-tauri/src/server_monitor/summary.rs`
- Test: `tests/rust/server_monitor/carrier_tests.rs`、`tests/rust/server_monitor/summary_tests.rs`

**Interfaces:**
- Produces:
  - `store::samples_since(&Connection, i64, &str) -> Result<Vec<MonitorSample>, String>`（按 ts 升序）
  - `carrier::summarise(&str, &[MonitorSample], &CarrierConfig) -> CarrierHealth`
  - `ServerHealth.carrier` 在 `build_summary` 中填充

- **验收:** `cargo test --features local-tests carrier:: -- --skip net::runtime_tests --skip net::runtime_service_tests` 与 `summary::` 通过；切地区过滤、SSH 失败不计失败率用例通过。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试**

`tests/rust/server_monitor/carrier_tests.rs` 追加：

```rust
fn cfg() -> CarrierConfig {
    CarrierConfig::default()
}

fn sample(ts: &str, region: &str, ct: Option<CarrierProbe>) -> MonitorSample {
    MonitorSample {
        server_id: 1,
        ts: ts.into(),
        reachable: true,
        carrier: Some(CarrierSample { region: region.into(), ct, cu: None, cm: None }),
        ..Default::default()
    }
}

#[test]
fn summary_uses_only_current_region_and_probed_samples() {
    let samples = vec![
        sample("2026-10-31T00:00:00.000Z", "gd", Some(CarrierProbe { ok: true, ms: Some(100.0) })),
        sample("2026-10-31T00:01:00.000Z", "gd", Some(CarrierProbe { ok: true, ms: Some(200.0) })),
        sample("2026-10-31T00:02:00.000Z", "gd", Some(CarrierProbe { ok: false, ms: None })),
        sample("2026-10-31T00:03:00.000Z", "sh", Some(CarrierProbe { ok: true, ms: Some(999.0) })),
        sample("2026-10-31T00:04:00.000Z", "gd", None),
    ];
    let h = summarise("gd", &samples, &cfg());
    let ct = h.carriers.iter().find(|c| c.key == "ct").unwrap();
    assert_eq!(ct.samples, 3);
    assert_eq!(ct.avg_ms, Some(150.0));
    assert_eq!(ct.loss_pct, Some(100.0 / 3.0));
    assert_eq!(ct.bars.len(), 3);
    assert_eq!(ct.bars.iter().filter(|b| !b.ok).count(), 1);
}

#[test]
fn bars_are_truncated_to_configured_count_and_ordered() {
    let mut samples = Vec::new();
    for i in 0..25 {
        samples.push(sample(
            &format!("2026-10-31T00:{:02}:00.000Z", i),
            "gd",
            Some(CarrierProbe { ok: true, ms: Some(100.0) }),
        ));
    }
    let h = summarise("gd", &samples, &cfg());
    let ct = h.carriers.iter().find(|c| c.key == "ct").unwrap();
    assert_eq!(ct.bars.len(), 20);
    assert_eq!(ct.bars[19].ts, "2026-10-31T00:24:00.000Z");
}

#[test]
fn no_samples_means_all_none() {
    let h = summarise("gd", &[], &cfg());
    for c in &h.carriers {
        assert_eq!(c.samples, 0);
        assert_eq!(c.avg_ms, None);
        assert_eq!(c.loss_pct, None);
        assert!(c.bars.is_empty());
    }
    assert_eq!(h.region_name, "广东");
    assert_eq!(h.bar_count, 20);
}
```

`tests/rust/server_monitor/summary_tests.rs` 追加（沿用现有构造）：

```rust
#[test]
fn configured_region_yields_carrier_health() {
    // 1) 建 state+server，写一条带 carrier 的样本；
    // 2) config 里给该服务器写 carrier_region="gd"；
    // 3) build_summary → health.carrier 为 Some，region_name="广东"；
    // 4) 未配置的另一台为 None。
    // 具体建库/插样本函数复用本文件既有 helper（见 summary_tests 现有用例）。
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --features local-tests summary_uses_only_current_region -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，`summarise` 不存在。

- [ ] **Step 3: 实现 summarise**

`carrier.rs` 加：

```rust
use super::config::CarrierConfig;
use super::{CarrierBar, CarrierHealth, CarrierStats, MonitorSample};

fn probe_of<'a>(c: &'a CarrierSample, key: &str) -> Option<&'a CarrierProbe> {
    match key {
        "ct" => c.ct.as_ref(),
        "cu" => c.cu.as_ref(),
        "cm" => c.cm.as_ref(),
        _ => None,
    }
}

pub fn summarise(region: &str, samples: &[MonitorSample], cfg: &CarrierConfig) -> CarrierHealth {
    CarrierHealth {
        region: region.into(),
        region_name: region_name(region).unwrap_or(region).into(),
        bar_count: cfg.bars,
        carriers: ["ct", "cu", "cm"]
            .iter()
            .map(|key| summarise_one(key, region, samples, cfg))
            .collect(),
    }
}

fn summarise_one(
    key: &str,
    region: &str,
    samples: &[MonitorSample],
    cfg: &CarrierConfig,
) -> CarrierStats {
    let mut ok_count = 0usize;
    let mut fail_count = 0usize;
    let mut sum = 0.0f64;
    let mut bars: Vec<CarrierBar> = Vec::new();
    for s in samples {
        let Some(c) = &s.carrier else { continue };
        if c.region != region {
            continue;
        }
        let Some(p) = probe_of(c, key) else { continue };
        if p.ok {
            let ms = p.ms.unwrap_or(0.0);
            ok_count += 1;
            sum += ms;
            bars.push(CarrierBar {
                ts: s.ts.clone(),
                ok: true,
                latency_level: Some(latency_level(ms, &cfg.latency_levels_ms)),
            });
        } else {
            fail_count += 1;
            bars.push(CarrierBar { ts: s.ts.clone(), ok: false, latency_level: None });
        }
    }
    let total = ok_count + fail_count;
    let avg_ms = (ok_count > 0).then(|| sum / ok_count as f64);
    let loss_pct = (total > 0).then(|| fail_count as f64 / total as f64 * 100.0);
    if bars.len() > cfg.bars {
        bars = bars.split_off(bars.len() - cfg.bars);
    }
    CarrierStats {
        key: key.into(),
        avg_ms,
        avg_level: avg_ms.map(|v| latency_level(v, &cfg.latency_levels_ms)),
        loss_pct,
        loss_level: loss_pct.map(|v| loss_level(v, &cfg.loss_levels_pct)),
        samples: total as u32,
        bars,
    }
}
```

- [ ] **Step 4: 加 samples_since 与摘要装配**

`store.rs` 加：

```rust
pub fn samples_since(
    conn: &Connection,
    server_id: i64,
    cutoff: &str,
) -> Result<Vec<MonitorSample>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 AND ts>=?2 ORDER BY ts"
        ))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id, cutoff], row_to_sample)
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}
```

`summary.rs`：

1. 顶部 `use super::carrier;`。
2. `let now = Utc::now();` 已存在，移到短 DB 锁之前（或用 `let now = Utc::now();` 提前）。
3. 在短锁块内、`max_ts` 之后：

```rust
        let carrier_cutoff = (now - Duration::seconds(config.carrier.window_secs as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut carrier_samples: HashMap<i64, Vec<super::MonitorSample>> = HashMap::new();
        for s in &servers {
            if config::carrier_region(&config, s.id).is_some() {
                carrier_samples.insert(s.id, store::samples_since(&conn, s.id, &carrier_cutoff)?);
            }
        }
```

并把元组返回加上 `carrier_samples`（同步改解构）。
4. 在 `health.push(ServerHealth { ... })` 前：

```rust
        let carrier = config::carrier_region(&config, s.id).map(|region| {
            let samples = carrier_samples
                .get(&s.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            carrier::summarise(&region, samples, &config.carrier)
        });
```

5. `ServerHealth` 构造加 `carrier,`。

- [ ] **Step 5: 跑测试**

Run: `cargo test --features local-tests server_monitor::carrier:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Run: `cargo test --features local-tests server_monitor::summary:: -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/server_monitor/carrier.rs src-tauri/src/server_monitor/store.rs src-tauri/src/server_monitor/summary.rs
git commit -m "feat(monitor): 后端聚合三网窗口统计"
```

---

### T6: 新 IPC（地区列表、资源序列）

- **Depends on:** T1、T5

**Files:**
- Modify: `src-tauri/src/server_monitor/store.rs`（`resource_history`）
- Modify: `src-tauri/src/commands/monitor.rs`
- Modify: `src-tauri/src/commands/ipc.rs`
- Modify: `src-tauri/src/main.rs`
- Test: `tests/rust/server_monitor/store_tests.rs`、`tests/rust/server_monitor/commands_tests.rs`

**Interfaces:**
- Produces:
  - `store::resource_history(&Connection, i64, &str, i64, usize) -> Result<ServerResources, String>`
  - `monitor_carrier_regions() -> R<Vec<CarrierRegion>>`
  - `monitor_resources(state, server_id, range_secs) -> R<ServerResources>`
  - IPC：`server_monitor_carrier_regions_cmd`（无参）、`server_monitor_resources_cmd`（`serverId`, `rangeSecs`）

- **验收:** `cargo test --features local-tests server_monitor:: -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；新命令已注册。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试**

`tests/rust/server_monitor/store_tests.rs` 追加：

```rust
#[test]
fn resource_history_returns_three_series() {
    let (_dir, db) = db();
    insert_server(&db, 1);
    for (i, cpu) in [10.0, 20.0, 30.0].iter().enumerate() {
        let mut s = sample(1, &format!("2026-10-31T00:0{i}:00.000Z"), Some(*cpu));
        s.mem_used_kb = Some(50);
        s.mem_total_kb = Some(100);
        store::insert_sample(&db.lock(), &s).unwrap();
    }
    let out = store::resource_history(
        &db.lock(),
        1,
        "2026-10-31T00:00:00.000Z",
        60,
        60,
    )
    .unwrap();
    assert_eq!(out.cpu_pct.len(), 3);
    assert_eq!(out.cpu_pct[2].value, 30.0);
    assert!(out.mem_pct.iter().all(|p| (p.value - 50.0).abs() < 1e-9));
    assert_eq!(out.disk_pct[0].value, 50.0);
}
```

`tests/rust/server_monitor/commands_tests.rs` 追加：

```rust
#[test]
fn carrier_regions_and_resources_validate_range() {
    assert_eq!(monitor_carrier_regions().unwrap().len(), 31);
    let (_dir, state) = state();
    assert!(monitor_resources(state.clone(), 1, 120).is_err());
    assert!(monitor_resources(state, 1, 3600).is_ok());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --features local-tests resource_history_returns -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 编译失败，函数不存在。

- [ ] **Step 3: 实现 resource_history**

`store.rs` 加：

```rust
pub fn resource_history(
    conn: &Connection,
    server_id: i64,
    cutoff: &str,
    bucket_secs: i64,
    max_points: usize,
) -> Result<super::ServerResources, String> {
    let mut stmt = conn
        .prepare(
            "SELECT ts,cpu_pct,mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb
             FROM server_metric_samples
             WHERE server_id=?1 AND ts>=?2 AND reachable=1
             ORDER BY ts",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id, cutoff], |r| {
            let ts: String = r.get(0)?;
            let cpu: Option<f64> = r.get(1)?;
            let mu: Option<i64> = r.get(2)?;
            let mt: Option<i64> = r.get(3)?;
            let du: Option<i64> = r.get(4)?;
            let dt: Option<i64> = r.get(5)?;
            let mem = match (mu, mt) {
                (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                _ => None,
            };
            let disk = match (du, dt) {
                (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                _ => None,
            };
            Ok((ts, cpu, mem, disk))
        })
        .map_err(|e| e.to_string())?;
    let bucket = bucket_secs.max(1);
    let mut maps: [std::collections::BTreeMap<i64, (String, f64, usize)>; 3] =
        std::array::from_fn(|_| std::collections::BTreeMap::new());
    for row in rows {
        let (ts, cpu, mem, disk) = row.map_err(|e| e.to_string())?;
        let epoch = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        for (i, value) in [cpu, mem, disk].into_iter().enumerate() {
            let Some(value) = value else { continue };
            let entry = maps[i]
                .entry(epoch / bucket)
                .or_insert_with(|| (ts.clone(), 0.0, 0));
            entry.1 += value;
            entry.2 += 1;
        }
    }
    let series = |map: std::collections::BTreeMap<i64, (String, f64, usize)>| {
        let mut points: Vec<super::MonitorPoint> = map
            .into_values()
            .map(|(ts, sum, n)| super::MonitorPoint { ts, value: sum / n as f64 })
            .collect();
        if points.len() > max_points {
            points = points.split_off(points.len() - max_points);
        }
        points
    };
    let mut iter = maps.into_iter();
    let cpu_pct = series(iter.next().unwrap());
    let mem_pct = series(iter.next().unwrap());
    let disk_pct = series(iter.next().unwrap());
    Ok(super::ServerResources { cpu_pct, mem_pct, disk_pct })
}
```

- [ ] **Step 4: 命令层**

`commands/monitor.rs` 顶部加 `pub use crate::server_monitor::{CarrierRegion, ServerResources};` 与 `use crate::server_monitor::carrier;`，并加：

```rust
pub fn monitor_carrier_regions() -> R<Vec<CarrierRegion>> {
    Ok(carrier::regions())
}

pub fn monitor_resources(
    state: Arc<AppState>,
    server_id: i64,
    range_secs: i64,
) -> R<ServerResources> {
    if !matches!(range_secs, 3600 | 86400 | 604800) {
        return Err(AppError::Validation(
            "时间范围必须是 1 小时、24 小时或 7 天".into(),
        ));
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(range_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let bucket = (range_secs / 60).max(1);
    store::resource_history(&state.db.lock(), server_id, &cutoff, bucket, 60)
        .map_err(AppError::Message)
}
```

- [ ] **Step 5: IPC 与注册**

`commands/ipc.rs`：

```rust
#[tauri::command]
pub async fn server_monitor_carrier_regions_cmd() -> R<Vec<CarrierRegion>> {
    monitor_carrier_regions()
}

#[tauri::command]
pub async fn server_monitor_resources_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    range_secs: i64,
) -> R<ServerResources> {
    monitor_resources(state.inner().clone(), server_id, range_secs)
}
```

`main.rs` 的 `invoke_handler!` 在 `server_monitor_history_cmd` 后加这两条。

- [ ] **Step 6: 跑测试**

Run: `cargo test --features local-tests resource_history -- --skip net::runtime_tests --skip net::runtime_service_tests`
Run: `cargo test --features local-tests carrier_regions_and_resources -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/server_monitor/store.rs src-tauri/src/commands/monitor.rs src-tauri/src/commands/ipc.rs src-tauri/src/main.rs
git commit -m "feat(monitor): 增加三网地区与资源序列命令"
```

---

### T7: 前端三网面板与资源迷你图

- **Depends on:** T5、T6

**Files:**
- Modify: `src/api/types.ts`
- Modify: `src/api/index.ts`
- Create: `src/views/monitor/CarrierPanel.tsx`
- Create: `src/views/monitor/ResourceCharts.tsx`
- Create: `src/components/charts/Sparkline.tsx`
- Modify: `src/views/monitor/ServerCard.tsx`
- Modify: `src/views/Monitor.tsx`
- Test: `tests/frontend/views/Monitor.test.tsx`、`tests/frontend/components/LineChart.test.tsx`（本地）

**Interfaces:**
- Consumes: `ServerHealth.carrier`、`monitorResources`、`monitorCarrierRegions`
- Produces:
  - `CarrierPanel({ carrier, onOpenSettings })`
  - `ResourceCharts({ serverId, intervalSecs })`
  - `Sparkline({ label, points, intervalSecs, color })`
  - 卡片不再有 `expanded`/`onToggle`

- **验收:** `npx tsc --noEmit` 通过；`npx vitest run tests/frontend/views/Monitor.test.tsx` 通过；卡片默认展开、面板在资源图上、空态正确。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 加类型与 API**

`src/api/types.ts`：

```ts
export interface CarrierProbe { ok: boolean; ms: number | null }
export interface CarrierSample {
  region: string
  ct: CarrierProbe | null
  cu: CarrierProbe | null
  cm: CarrierProbe | null
}
export interface CarrierBar { ts: string; ok: boolean; latency_level: number | null }
export interface CarrierStats {
  key: string
  avg_ms: number | null
  avg_level: number | null
  loss_pct: number | null
  loss_level: number | null
  samples: number
  bars: CarrierBar[]
}
export interface CarrierHealth {
  region: string
  region_name: string
  bar_count: number
  carriers: CarrierStats[]
}
export interface CarrierRegion { code: string; name: string }
export interface MonitorCarrierConfig {
  window_secs: number
  bars: number
  latency_levels_ms: [number, number, number, number]
  loss_levels_pct: [number, number, number, number]
}
export interface ServerResources {
  cpu_pct: MonitorPoint[]
  mem_pct: MonitorPoint[]
  disk_pct: MonitorPoint[]
}
```

`MonitorServerOverride` 加 `carrier_region?: string | null`；`MonitorConfig` 加 `carrier: MonitorCarrierConfig`；`MonitorSample` 加 `carrier: CarrierSample | null`；`ServerHealth` 加 `carrier: CarrierHealth | null`。

`src/api/index.ts` 加：

```ts
export function monitorCarrierRegions(): Promise<CarrierRegion[]> {
  return invoke('server_monitor_carrier_regions_cmd')
}

export function monitorResources(
  serverId: number,
  rangeSecs: number,
): Promise<ServerResources> {
  return invoke('server_monitor_resources_cmd', { serverId, rangeSecs })
}
```

- [ ] **Step 2: 写 CarrierPanel**

`src/views/monitor/CarrierPanel.tsx`：

```tsx
import type { CarrierHealth, CarrierStats } from '../../api/types'

const DISPLAY = [
  { key: 'cu', name: '联通', dot: 'bg-red-500' },
  { key: 'ct', name: '电信', dot: 'bg-blue-500' },
  { key: 'cm', name: '移动', dot: 'bg-emerald-500' },
]

const LEVEL_BG: Record<number, string> = {
  1: 'bg-emerald-500',
  2: 'bg-lime-500',
  3: 'bg-amber-500',
  4: 'bg-orange-500',
  5: 'bg-red-500',
}

const LEVEL_TEXT: Record<number, string> = {
  1: 'text-emerald-300',
  2: 'text-lime-300',
  3: 'text-amber-300',
  4: 'text-orange-300',
  5: 'text-red-300',
}

function Cell({ ok, level, mode }: { ok: boolean; level: number | null; mode: 'latency' | 'loss' }) {
  const cls =
    mode === 'loss'
      ? ok
        ? 'bg-emerald-500'
        : 'bg-red-700'
      : ok
        ? LEVEL_BG[level ?? 5]
        : 'bg-red-700'
  return <span className={`h-2 flex-1 rounded-sm ${cls}`} />
}

function Row({
  stat,
  name,
  dot,
  barCount,
  mode,
}: {
  stat: CarrierStats
  name: string
  dot: string
  barCount: number
  mode: 'latency' | 'loss'
}) {
  const value =
    mode === 'latency'
      ? stat.avg_ms == null
        ? '—'
        : `${stat.avg_ms.toFixed(1)} ms`
      : stat.loss_pct == null
        ? '—'
        : `${stat.loss_pct.toFixed(1)}%`
  const level = mode === 'latency' ? stat.avg_level : stat.loss_level
  const pad = Math.max(barCount - stat.bars.length, 0)
  return (
    <div className="flex items-center gap-2 text-xs" aria-label={`${name}${mode === 'latency' ? '延迟' : '丢包'} ${value}`}>
      <span className={`h-2 w-2 shrink-0 rounded-full ${dot}`} />
      <span className="w-8 shrink-0 text-neutral-400">{name}</span>
      <span className={`w-16 shrink-0 text-right ${level ? LEVEL_TEXT[level] : 'text-neutral-400'}`}>
        {value}
      </span>
      <span className="ml-1 flex min-w-0 flex-1 gap-0.5">
        {Array.from({ length: pad }, (_, i) => (
          <span key={`p${i}`} className="h-2 flex-1 rounded-sm bg-neutral-800" />
        ))}
        {stat.bars.map((b, i) => (
          <Cell key={b.ts + i} ok={b.ok} level={b.latency_level} mode={mode} />
        ))}
      </span>
    </div>
  )
}

export default function CarrierPanel({
  carrier,
  onOpenSettings,
}: {
  carrier: CarrierHealth | null
  onOpenSettings: () => void
}) {
  if (!carrier) {
    return (
      <section className="mt-3 rounded border border-neutral-800 p-3">
        <p className="text-xs text-neutral-400">未选择三网地区</p>
        <button
          type="button"
          onClick={onOpenSettings}
          className="mt-1 text-xs text-emerald-400 underline"
        >
          去设置
        </button>
      </section>
    )
  }
  const empty = carrier.carriers.every((c) => c.samples === 0)
  const stats = (key: string) =>
    carrier.carriers.find((c) => c.key === key) ?? {
      key,
      avg_ms: null,
      avg_level: null,
      loss_pct: null,
      loss_level: null,
      samples: 0,
      bars: [],
    }
  return (
    <section className="mt-3 rounded border border-neutral-800 p-3">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div>
          <h4 className="mb-1 text-xs font-medium text-neutral-300">延迟</h4>
          <div className="space-y-1">
            {DISPLAY.map((d) => (
              <Row
                key={d.key}
                stat={stats(d.key)}
                name={d.name}
                dot={d.dot}
                barCount={carrier.bar_count}
                mode="latency"
              />
            ))}
          </div>
        </div>
        <div>
          <h4 className="mb-1 text-xs font-medium text-neutral-300">丢包</h4>
          <div className="space-y-1">
            {DISPLAY.map((d) => (
              <Row
                key={d.key}
                stat={stats(d.key)}
                name={d.name}
                dot={d.dot}
                barCount={carrier.bar_count}
                mode="loss"
              />
            ))}
          </div>
        </div>
      </div>
      {empty && <p className="mt-2 text-xs text-neutral-500">等待首次采集</p>}
    </section>
  )
}
```

- [ ] **Step 3: 写 Sparkline 与 ResourceCharts**

`src/components/charts/Sparkline.tsx`：

```tsx
import type { MonitorPoint } from '../../api/types'

const W = 120
const H = 28
const PAD = 2

export default function Sparkline({
  label,
  points,
  intervalSecs,
  color = '#34d399',
  formatValue = (v) => `${v.toFixed(1)}%`,
}: {
  label: string
  points: MonitorPoint[]
  intervalSecs: number
  color?: string
  formatValue?: (v: number) => string
}) {
  const last = points[points.length - 1]
  if (!last) {
    return (
      <div className="rounded border border-neutral-800 p-2 text-xs text-neutral-500">
        <span>{label}</span>
        <span className="ml-2">暂无数据</span>
      </div>
    )
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
    const gap = i > 0 && times[i] - times[i - 1] > Math.max(intervalSecs * 2, 1) * 1000
    if (gap) {
      if (d) segments.push(d)
      d = ''
    }
    d += `${d ? 'L' : 'M'}${x(times[i]).toFixed(1)},${y(values[i]).toFixed(1)}`
  }
  if (d) segments.push(d)
  return (
    <div className="rounded border border-neutral-800 p-2 text-xs">
      <div className="flex justify-between text-neutral-400">
        <span>{label}</span>
        <span className="text-neutral-200">{formatValue(last.value)}</span>
      </div>
      <svg viewBox={`0 0 ${W} ${H}`} className="mt-1 w-full" role="img" aria-label={`${label} 迷你趋势`}>
        <title>{`${label} 迷你趋势`}</title>
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
    </div>
  )
}
```

`src/views/monitor/ResourceCharts.tsx`：

```tsx
import { useEffect, useState } from 'react'
import { monitorResources } from '../../api'
import type { ServerResources } from '../../api/types'
import Sparkline from '../../components/charts/Sparkline'

const RANGES = [
  { secs: 3600, label: '1 小时' },
  { secs: 86400, label: '24 小时' },
  { secs: 604800, label: '7 天' },
]

export default function ResourceCharts({
  serverId,
  intervalSecs,
}: {
  serverId: number
  intervalSecs: number
}) {
  const [range, setRange] = useState(3600)
  const [data, setData] = useState<ServerResources | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    setError(null)
    monitorResources(serverId, range)
      .then((d) => {
        if (alive) setData(d)
      })
      .catch((e) => {
        if (alive) setError(String(e))
      })
    return () => {
      alive = false
    }
  }, [serverId, range])

  const step = Math.max(intervalSecs, Math.ceil(range / 60))
  return (
    <div className="mt-3">
      <div className="flex justify-end gap-1">
        {RANGES.map((r) => (
          <button
            key={r.secs}
            type="button"
            onClick={() => setRange(r.secs)}
            className={`rounded px-2 py-0.5 text-xs ${
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
        <p className="mt-1 text-xs text-red-300">资源趋势加载失败：{error}</p>
      ) : (
        <div className="mt-1 grid grid-cols-1 gap-2 sm:grid-cols-3">
          <Sparkline label="CPU" points={data?.cpu_pct ?? []} intervalSecs={step} color="#34d399" />
          <Sparkline label="内存" points={data?.mem_pct ?? []} intervalSecs={step} color="#60a5fa" />
          <Sparkline label="磁盘" points={data?.disk_pct ?? []} intervalSecs={step} color="#f59e0b" />
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 4: 改 ServerCard 与 Monitor**

`ServerCard.tsx`：

- 删 `import TrendPanel`；
- 删 `expanded`/`onToggle` props；
- 外层改成 `<div ...>`，内层 `<button onClick>` 改成 `<div>`；
- 在状态头之后插入：

```tsx
      <CarrierPanel carrier={health.carrier} onOpenSettings={onOpenSettings} />
```

- 在资源百分比条之后、明细之前插入：

```tsx
      <ResourceCharts serverId={health.server_id} intervalSecs={config.interval_secs} />
```

- 删除末尾的 `{expanded && <TrendPanel ... />}`。

新 props：

```tsx
export default function ServerCard({
  health,
  config,
  onOpenSettings,
}: {
  health: ServerHealth
  config: MonitorConfig
  onOpenSettings: () => void
})
```

`Monitor.tsx`：

- 删 `const [expanded, setExpanded] = useState<number | null>(null)`；
- `<ServerCard key=... health config onOpenSettings={() => setSettingsOpen(true)} />`；
- 删多余 props。

- [ ] **Step 5: 更新前端测试夹具**

`tests/frontend/views/Monitor.test.tsx`：

- `vi.mock('../../../src/api', ...)` 加 `monitorResources: vi.fn(async () => ({ cpu_pct: [], mem_pct: [], disk_pct: [] }))` 与 `monitorCarrierRegions: vi.fn(async () => [])`；
- summary 的 `config` 加 `carrier: { window_secs: 3600, bars: 20, latency_levels_ms: [120,200,300,400], loss_levels_pct: [1,3,6,9] }`；
- 每台 `ServerHealth` 加 `carrier: null`，每个 `MonitorSample` 加 `carrier: null`；
- 断言卡片默认展示趋势面板内容（如 `CPU` 迷你图存在）。

- [ ] **Step 6: 跑检查**

Run: `npx tsc --noEmit`
Run: `npx vitest run tests/frontend/views/Monitor.test.tsx`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
git add src/api/types.ts src/api/index.ts src/views/monitor/CarrierPanel.tsx src/views/monitor/ResourceCharts.tsx src/components/charts/Sparkline.tsx src/views/monitor/ServerCard.tsx src/views/Monitor.tsx
git commit -m "feat(monitor): 卡片默认展开并展示三网与资源迷你图"
```

---

### T8: 设置抽屉地区选择与退役旧趋势

- **Depends on:** T6、T7

**Files:**
- Modify: `src/views/monitor/MonitorSettingsDrawer.tsx`
- Delete: `src/views/monitor/TrendPanel.tsx`
- Modify: `src/api/index.ts`（删 `monitorHistory`）
- Modify: `src/api/types.ts`（删 `MonitorMetric`）
- Modify: `src-tauri/src/commands/monitor.rs`（删 `monitor_history`）
- Modify: `src-tauri/src/commands/ipc.rs`（删 `server_monitor_history_cmd`）
- Modify: `src-tauri/src/main.rs`（删注册）
- Modify: `src-tauri/src/server_monitor/store.rs`（删 `history`）
- Modify: `src-tauri/src/server_monitor/mod.rs`（删 `MonitorMetric`）
- Modify: `docs/design/ipc-interface-reference.md`
- Test: `tests/frontend/views/MonitorSettings.test.tsx`、`tests/rust/server_monitor/{mods,store,commands}_tests.rs`

**Interfaces:**
- Consumes: `monitorCarrierRegions`、`CarrierRegion`
- Produces: 设置抽屉每台服务器一个地区下拉；旧历史链路不存在

- **验收:** `npx tsc --noEmit`、`npm test`、`cargo test --features local-tests -- --skip ...` 均通过；`grep -R "monitor_history\|TrendPanel\|MonitorMetric" src src-tauri/src docs/design` 无结果。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写失败测试**

`tests/frontend/views/MonitorSettings.test.tsx`：

- `vi.mock('../../../src/api', ...)` 加 `monitorCarrierRegions: vi.fn(async () => [{ code: 'gd', name: '广东' }])`；
- `config` 加 `carrier` 默认段；
- 新用例：

```tsx
it('保存三网地区并保留既有阈值', async () => {
  const user = userEvent.setup()
  render(
    <MonitorSettingsDrawer
      config={{ ...config, carrier: CARRIER, servers: { '1': { enabled: true, thresholds: { cpu_pct: 80 } } } }}
      servers={servers}
      onClose={() => {}}
      onSaved={() => {}}
    />,
  )
  await user.selectOptions(screen.getByLabelText('一号 三网地区'), 'gd')
  await user.click(screen.getByRole('button', { name: '保存' }))
  await waitFor(() => expect(save).toHaveBeenCalled())
  const sent = save.mock.calls[0][0] as MonitorConfig
  expect(sent.servers['1'].carrier_region).toBe('gd')
  expect(sent.servers['1'].thresholds).toEqual({ cpu_pct: 80 })
})
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run tests/frontend/views/MonitorSettings.test.tsx`
Expected: 找不到「一号 三网地区」下拉。

- [ ] **Step 3: 改设置抽屉**

`MonitorSettingsDrawer.tsx`：

```tsx
import { useEffect, useState } from 'react'
import { monitorCarrierRegions, monitorSetConfig } from '../../api'
import type { CarrierRegion, MonitorConfig } from '../../api/types'

const [regions, setRegions] = useState<CarrierRegion[]>([])
const [regionFor, setRegionFor] = useState<Record<number, string>>(() =>
  Object.fromEntries(
    servers.map((s) => [s.server_id, config.servers[String(s.server_id)]?.carrier_region ?? '']),
  ),
)

useEffect(() => {
  let alive = true
  monitorCarrierRegions()
    .then((r) => {
      if (alive) setRegions(r)
    })
    .catch(() => {})
  return () => {
    alive = false
  }
}, [])
```

服务器列表每行改成：

```tsx
          {servers.map((s) => (
            <div key={s.server_id} className="mt-2 space-y-1">
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={serverOn[s.server_id] ?? true}
                  onChange={(e) =>
                    setServerOn((prev) => ({ ...prev, [s.server_id]: e.target.checked }))
                  }
                />
                {s.name}
              </label>
              <select
                aria-label={`${s.name} 三网地区`}
                value={regionFor[s.server_id] ?? ''}
                onChange={(e) =>
                  setRegionFor((prev) => ({ ...prev, [s.server_id]: e.target.value }))
                }
                className="w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm text-neutral-100"
              >
                <option value="">不测试</option>
                {regions.map((r) => (
                  <option key={r.code} value={r.code}>
                    {r.name}
                  </option>
                ))}
              </select>
            </div>
          ))}
```

保存时：

```tsx
            return [
              String(s.server_id),
              {
                ...prev,
                enabled: serverOn[s.server_id],
                carrier_region: regionFor[s.server_id] || null,
              },
            ]
```

- [ ] **Step 4: 退役旧趋势**

- 删 `src/views/monitor/TrendPanel.tsx`（`git rm`）。
- `src/api/index.ts` 删 `monitorHistory`；顶部 import 删 `MonitorMetric`。
- `src/api/types.ts` 删 `MonitorMetric` 类型。
- `src-tauri/src/commands/monitor.rs` 删 `monitor_history` 函数，`use crate::server_monitor::{config, store, summary, MonitorMetric};` 改为 `{config, store, summary}`，`pub use` 删 `MonitorPoint`（仅旧命令用；若别处仍用则保留）。
- `src-tauri/src/commands/ipc.rs` 删 `server_monitor_history_cmd` 与 `MonitorPoint` 的 import（若无其他使用）。
- `src-tauri/src/main.rs` 删该命令注册。
- `src-tauri/src/server_monitor/store.rs` 删 `history` 函数与 `MAX_HISTORY_POINTS`，`use super::{..., MonitorMetric, ...}` 去掉 `MonitorMetric`。
- `src-tauri/src/server_monitor/mod.rs` 删 `MonitorMetric` 及 impl。
- 本地测试：删 `tests/frontend/views/TrendPanel.test.tsx`；`store_tests.rs` 里针对 `store::history` 的用例改为 `resource_history`（保留断点/下采样断言）；`mods_tests.rs` 删 `MonitorMetric::parse` 用例；`commands_tests.rs` 删 `history_rejects_unknown_metric_and_range`，用 `carrier_regions_and_resources_validate_range` 覆盖。

- [ ] **Step 5: 更新 IPC 文档**

`docs/design/ipc-interface-reference.md`：删除 `server_monitor_history_cmd` 行，增加：

```markdown
| `server_monitor_carrier_regions_cmd` | `monitorCarrierRegions()` | — | `CarrierRegion[]` |
| `server_monitor_resources_cmd` | `monitorResources(serverId, rangeSecs)` | `{ serverId, rangeSecs }` | `ServerResources` |
```

- [ ] **Step 6: 全量检查**

Run: `npx tsc --noEmit`
Run: `npm test`
Run: `cargo test --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: PASS；`grep -R "monitor_history\|TrendPanel\|MonitorMetric" src src-tauri/src docs/design` 无结果。

- [ ] **Step 7: 提交**

```bash
git add src/views/monitor/MonitorSettingsDrawer.tsx src/views/monitor/TrendPanel.tsx src/api/index.ts src/api/types.ts src-tauri/src/commands/monitor.rs src-tauri/src/commands/ipc.rs src-tauri/src/main.rs src-tauri/src/server_monitor/store.rs src-tauri/src/server_monitor/mod.rs docs/design/ipc-interface-reference.md
git commit -m "feat(monitor): 设置页支持三网地区并退役旧趋势"
```

---

### T9: 文档、全量检查与真机验收

- **Depends on:** T1–T8

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `docs/design/ipc-interface-reference.md`（T8 已完成时核对）
- Test: 全量命令 + 真机

**Interfaces:**
- Consumes: 全部前置任务
- Produces: 可验收的功能与变更记录

- **验收:** A1–A8 全部满足；真机一轮样本写入 `carrier_region` 且卡片显示 3 行数字与格子。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写 CHANGELOG**

在 `## [未发布]` 顶部加一条（沿用现有格式）：

```markdown
### 新增
- **服务器监控三网延迟测试**：每台服务器可选一个省级地区，按现有采集轮次对电信/联通/移动
  各做一次 TCP 建连探测；卡片默认展开，优先展示延迟/丢包（近 1 小时均值与失败率 +
  最近 20 次探测格子），CPU/内存/磁盘改为带百分比的迷你折线图。采集、聚合、分级与
  下采样全部在 Rust 后端完成，前端只渲染。
```

- [ ] **Step 2: 跑全量检查**

Run: `cargo fmt --check`
Run: `cargo clippy --all-targets -- -D warnings`（按仓库 CI 的实际参数）
Run: `cargo test --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests`
Run: `npx tsc --noEmit`
Run: `npm test`
Run: `npm run check:size && npm run check:org && npm run check:sensitive`
Expected: 全部通过。

- [ ] **Step 3: 真机验收**

1. 启动/重编应用（见风险条：运行中的 exe 需按 `service-locks-debug-exe-rename-trick` 处理）。
2. 监控设置里给一台境外服务器选择「广东」，保存。
3. 「立即采集」一轮；确认卡片出现三行数字与格子、资源迷你图有数据。
4. 用 SQLite 核对：`SELECT ts,carrier_region,ct_ok,ct_ms,cu_ok,cu_ms,cm_ok,cm_ms FROM server_metric_samples ORDER BY ts DESC LIMIT 1;`
   期望：`carrier_region='gd'`，三网 ok/ms 与卡片一致。
5. 再跑一轮，确认格子右移；把地区改为「上海」，确认窗口数字只统计上海样本（旧样本仍在库）。
6. 未配置地区的服务器卡片显示「未选择三网地区」。

- [ ] **Step 4: 提交**

```bash
git add CHANGELOG.md docs/design/ipc-interface-reference.md
git commit -m "docs: 记录服务器监控三网延迟测试"
```

---

## 自审记录

- Spec 覆盖：目标/探测（T2/T4）、存储（T1）、配置（T3）、聚合（T5）、IPC（T6/T8）、前端（T7/T8）、文档与真机（T9）。
- 无占位符；所有类型与函数名在首次出现的任务中给出签名。
- 类型一致性：`CarrierSample`/`CarrierProbe`/`CarrierHealth`/`CarrierRegion`/`ServerResources` 全程同名同字段。
- 任务依赖闭合：T2←T1；T3←T2；T4←T1/T2/T3；T5←T1/T2/T3/T4；T6←T1/T5；T7←T5/T6；T8←T6/T7；T9←T1–T8。
