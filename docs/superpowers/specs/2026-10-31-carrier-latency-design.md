# 服务器监控：三网延迟测试（应用内）设计

- 日期：2026-10-31
- 状态：设计已确认，待评审后实施
- 依赖：[`2026-10-19-server-health-monitor-design.md`](2026-10-19-server-health-monitor-design.md)（服务器健康监控）

**目标：** 在「服务器监控」里增加「服务器 → 国内三网（电信/联通/移动）」的 TCP 建连
质量测试：每台服务器选一个省份，按现有采集轮次每网探测一次，卡片默认展开并优先展示
延迟/丢包面板与 CPU/内存/磁盘百分比图表。

**范围：** 后端目标生成、探测、存储、窗口聚合、分级；前端只渲染结构化结果
（延迟/丢包双栏 20 格、资源迷你折线、空态）。沿用现有 SSH 采集通道与 7 天保留期。

**不做：**

- 不做 ICMP / UDP 探测；口径是 **TCP 建连质量**。
- 不做一轮内多次并发探测、不做多地同时探测；每台服务器只测一个省份的三网。
- 不做 IPv6 目标、不做自定义目标主机（首版固定 31 省 × 3 网）。
- 不做三网延迟告警（不接现有 `server_alerts`）；本期只展示。
- 不新增外部服务、agent、端口或依赖；不在前端做统计/分级/下采样。

## 1. 边界与语义

### 1.1 口径：TCP 建连失败率

每个目标每轮只做一次 **TCP 建连**（`curl` 的 `%{time_connect}`），成功后立刻关闭，
不读取响应体、不要求 HTTP 2xx。因此：

- 「延迟」= 从被监控服务器到目标 IP 的 TCP 三次握手完成时间（1 个 RTT）。
- 「丢包」= **TCP 建连失败率**（DNS 失败/连接超时/被拒绝/缺少 curl 都算失败），
  不是 ICMP 丢包，不能与 ping 的丢包率直接对比。
- 目标返回 403/412/501 等 HTTP 状态不影响判定：TCP 已建连即成功。

### 1.2 测量方向与目标解析

探测命令在被监控服务器上执行，目标域名也在该服务器上解析：实测的是
「这台服务器 → 该省该运营商的 CDN 接入点」。服务器自身 DNS 异常、出站 80 被封、
或没有 IPv4 出站时，三网会整体失败——这是真实链路状态，UI 照实显示。

### 1.3 跟随采集轮次

- 复用现有调度器与 SSH 会话：每台服务器每轮一条探针脚本，三网探测与资源采集同轮完成。
- 每网每轮 **1 次**探测；默认间隔 60 秒。格子 = 一次探测，不在一轮内并发多次。
- 默认间隔 60 秒；`interval_secs` 可调（30–3600），格子的实际时间跨度随间隔变化。

### 1.4 每台服务器选一个地区

- 设置抽屉里每台服务器一个「三网地区」下拉：31 个省级行政区选 1，或「不测试」。
- 选定后只测该省的三网 3 个目标；未选择时不产生任何三网探测与样本字段。
- 台湾/香港/澳门没有目标数据源，不出现在列表里。

### 1.5 与现有监控的关系

- 三网结果写进同一张 `server_metric_samples` 表、同一条样本、同一个 7 天保留期。
- SSH 连接失败（`reachable=false`）的样本没有三网结果；三网统计只算「真正探过」的样本，
  不把「没采集到」伪装成「建连失败」。
- 不改现有告警状态机；三网数据首版不参与告警。

## 2. 目标

### 2.1 目标生成

省份 code 取 NetQuality `province.json` 的小写形式（`bj`、`sh`、`gd`…，共 31 个）：

```
<code>-ct-v4.ip.zstaticcdn.com   # 电信
<code>-cu-v4.ip.zstaticcdn.com   # 联通
<code>-cm-v4.ip.zstaticcdn.com   # 移动
```

端口固定 80。使用域名（而不是写死 IP）是为了让目标服务器就近解析到该省该运营商的
CDN 接入点，且不需要人工维护 IP 快照。

**验证依据（2026-10-31）：** 31 省 × 3 网共 93 个域名全部可解析；从境外服务器实测
TCP 建连稳定在数百毫秒量级，HTTP 返回 403/412/501/405 均有，不影响建连计时。
台湾/香港/澳门三个 code 的 9 个域名均无解析，因此不提供。

### 2.2 地区列表

后端内置 `(code, 名称)` 常量表并通过命令暴露给前端；前端不写死省份与域名。
首版 31 个省，顺序按行政区划编码。

## 3. 架构

```
调度器每轮
  └─ Probe（每台一次 SSH）
        ├─ 资源采集段（既有）
        └─ 三网段（仅配置了地区时）
              ├─ 3 条 curl 后台并发，与 CPU 的 sleep 1 重叠
              └─ wait 后输出 ##ct/##cu/##cm
        │
        ▼
  parse：资源 + CarrierSample { region, ct, cu, cm }
        │
        ▼
  store：一条样本写入 server_metric_samples（含 carrier_* 列）
        │
        ▼
  summary：按「当前地区 + 窗口」聚合 → ServerHealth.carrier
        │
        ├─ emit monitor://tick
        └─ 前端卡片只渲染（CarrierPanel / ResourceSparkline）
```

### 3.1 模块边界

| 位置 | 职责 |
| --- | --- |
| `src-tauri/src/server_monitor/carrier.rs` | 省份常量表、目标生成、脚本片段、三网段解析、窗口统计/分级纯函数 |
| `server_monitor/probe.rs` | 把三网片段拼进探针脚本；脚本变为按地区生成 |
| `server_monitor/parse.rs` | 调用三网段解析，产出 `CarrierSample` |
| `server_monitor/store.rs` | 新列写入/读取、按窗口取样本、历史序列（含资源三指标） |
| `server_monitor/config.rs` | `carrier_region` 覆盖、`carrier` 全局配置与校验 |
| `server_monitor/summary.rs` | 组装 `ServerHealth.carrier`（全部计算在后端） |
| `src-tauri/src/commands/monitor.rs` / `commands/ipc.rs` | 新命令：地区列表、资源序列；退役单指标历史命令（若无其他使用方） |
| `src/views/monitor/CarrierPanel.tsx` | 延迟/丢包双栏面板（纯渲染） |
| `src/views/monitor/ResourceSparkline.tsx` | 迷你折线 + 当前百分比（纯渲染） |
| `src/views/monitor/ServerCard.tsx` | 卡片默认展开；三网面板置顶、资源图表其次、明细最后 |

**文件规模约束：** 前端组件 ≤1000 行、单文件 ≤3000 行（仓库现行上限）；新逻辑优先进
`carrier.rs` 与独立组件，不把 `parse.rs` / `ServerCard.tsx` 撑大。

### 3.2 并发与锁

- 三网探测是 SSH 脚本内部的后台进程，不新增应用内并发；应用侧仍受现有
  `concurrency` 与「同一时刻最多一轮」约束。
- 聚合所需的窗口样本在写库后的同一读路径中查询，不跨 await 持锁。
- 三网失败绝不让整轮失败：与资源指标一样，缺失即缺失。

## 4. 数据模型

### 4.1 迁移 v22

当前 `MIGRATIONS` 最新为 v21（封禁名单游标），新增第 22 条：

```sql
-- v22：三网延迟（服务器 → 国内三网 TCP 建连）。
-- 每轮每网一次探测。carrier_region 记录该样本探测的省份 code：
-- NULL = 未配置地区或本轮未探测；切换地区后，旧地区的样本不能混进统计。
-- ct/ cu/ cm 三组列：ok NULL = 本轮没探到（SSH/脚本失败），
-- ok=0 = 探了但建连失败；ms 仅在 ok=1 时非空。
ALTER TABLE server_metric_samples ADD COLUMN carrier_region TEXT;
ALTER TABLE server_metric_samples ADD COLUMN ct_ms REAL;
ALTER TABLE server_metric_samples ADD COLUMN ct_ok INTEGER;
ALTER TABLE server_metric_samples ADD COLUMN cu_ms REAL;
ALTER TABLE server_metric_samples ADD COLUMN cu_ok INTEGER;
ALTER TABLE server_metric_samples ADD COLUMN cm_ms REAL;
ALTER TABLE server_metric_samples ADD COLUMN cm_ok INTEGER;
```

- 旧数据新列为 NULL，行为与「未配置地区」一致。
- 沿用现有 7 天清理，不新增表与索引；窗口查询走既有 `(server_id, ts)` 索引。
- 降级护栏沿用：旧二进制遇到 v22 库会明确报「数据目录比本程序新」。

### 4.2 配置

`MonitorConfig` 增加全局 `carrier` 段；`ServerMonitorOverride` 增加 `carrier_region`：

```json
{
  "enabled": true,
  "interval_secs": 60,
  "concurrency": 2,
  "defaults": { "...": "既有阈值" },
  "carrier": {
    "window_secs": 3600,
    "bars": 20,
    "latency_levels_ms": [120.0, 200.0, 300.0, 400.0],
    "loss_levels_pct": [1.0, 3.0, 6.0, 9.0]
  },
  "servers": {
    "5": { "enabled": true, "carrier_region": "gd" }
  }
}
```

| 字段 | 默认 | 校验 | 说明 |
| --- | --- | --- | --- |
| `carrier.window_secs` | `3600` | 600–86400 | 数字（均值/失败率）的统计窗口 |
| `carrier.bars` | `20` | 10–60 | 每行格子数 = 最近 N 次探测 |
| `carrier.latency_levels_ms` | `[120,200,300,400]` | 4 个严格递增正数 | 延迟分级上界，> 最后一项为 5 级 |
| `carrier.loss_levels_pct` | `[1,3,6,9]` | 4 个严格递增、0–100 | 失败率分级上界，> 最后一项为 5 级 |

- `carrier_region` 必须是 31 个 code 之一；空/未知值在保存时拒绝。
- `normalize` 保留条件扩展为 `enabled==false || thresholds.is_some() || carrier_region.is_some()`，
  避免保存时把地区配置抹掉。
- 阈值不用 Komari 原值（60/100/160/200ms）：那是「国内探国内」的量级；境外服务器到
  国内实测 230–310ms，用原值会整片最高级。默认值按跨境放宽，且可通过配置调整。

### 4.3 类型

```rust
pub struct CarrierProbe {        // 单网单次探测
    pub ok: bool,
    pub ms: Option<f64>,         // ok=false 时为 None
}

pub struct CarrierSample {       // 一条样本上的三网结果
    pub region: String,          // 省 code
    pub ct: Option<CarrierProbe>,// None = 本轮没探到（SSH/脚本失败）
    pub cu: Option<CarrierProbe>,
    pub cm: Option<CarrierProbe>,
}

pub struct CarrierBar {          // 一个格子 = 一次探测
    pub ts: String,
    pub ok: bool,
    pub latency_level: Option<u8>, // 1–5；ok=false 时为 None（前端渲染为失败格）
}

pub struct CarrierStats {        // 单个运营商
    pub key: String,             // "ct" | "cu" | "cm"
    pub avg_ms: Option<f64>,     // 窗口内成功探测的均值
    pub avg_level: Option<u8>,   // 1–5（后端按阈值算）
    pub loss_pct: Option<f64>,   // 窗口内 ok=false / 已探测总数
    pub loss_level: Option<u8>,  // 1–5
    pub samples: u32,            // 窗口内真正探过的次数（ok+failed）
    pub bars: Vec<CarrierBar>,   // 最近 N 次，旧→新
}

pub struct CarrierHealth {
    pub region: String,          // "gd"
    pub region_name: String,     // "广东"
    pub carriers: Vec<CarrierStats>, // 固定顺序 ct, cu, cm
}
```

`MonitorSample` 增加 `pub carrier: Option<CarrierSample>`（与 `carrier_region` 及
`ct/cu/cm` 六列一一对应）；`ServerHealth` 增加 `pub carrier: Option<CarrierHealth>`：
未配置地区为 `None`。

### 4.4 窗口统计与分级

- 取该服务器 `ts >= now - window_secs` 且 `carrier_region = 当前地区` 的样本，按 ts 升序。
- 每个运营商只统计 `ok IS NOT NULL` 的样本：
  - `samples = ok 1/0 的条数`；
  - `loss_pct = ok=0 的条数 / samples × 100`；
  - `avg_ms = ok=1 的 ms 平均`（无成功样本时为 `None`）。
- `avg_level` / `loss_level`：`≤阈值[0]`→1，`≤阈值[1]`→2，`≤阈值[2]`→3，
  `≤阈值[3]`→4，否则 5；无值为 `None`。
- `bars`：取窗口内 `ok IS NOT NULL` 的最近 `bars` 条（旧→新），每条一个格子；
  `latency_level` 用该次 `ms` 分级；`ok=false` 时 `latency_level=None`。
- 丢包格：成功 = 0% → 前端渲染为 1 级，失败 = 失败格（红色）；这是样式映射，
  不涉及阈值计算。
- 窗口内没有可统计样本：`avg_ms/avg_level/loss_pct/loss_level = None`，`bars` 为空。

## 5. 采集协议

### 5.1 脚本片段

仅当该台服务器配置了地区时，`probe_script(region)` 在既有脚本中插入：

```sh
d=$(mktemp -d 2>/dev/null) || d=/tmp/.vpnctl-probe-$$
mkdir -p "$d"
_c(){ curl -o /dev/null -s -w '%{time_connect}' --connect-timeout 3 --max-time 4 "http://$1/" 2>/dev/null; }
_c <code>-ct-v4.ip.zstaticcdn.com > "$d/ct" 2>/dev/null &
_c <code>-cu-v4.ip.zstaticcdn.com > "$d/cu" 2>/dev/null &
_c <code>-cm-v4.ip.zstaticcdn.com > "$d/cm" 2>/dev/null &
# …既有资源采集段（含 sleep 1）…
wait
echo '##ct'; cat "$d/ct" 2>/dev/null; echo
echo '##cu'; cat "$d/cu" 2>/dev/null; echo
echo '##cm'; cat "$d/cm" 2>/dev/null; echo
rm -rf "$d"
echo '##done'
```

- `curl -w '%{time_connect}'` **不输出换行**；`cat` 后必须补 `echo`，否则
  `0.310546##cu` 会粘成一行，解析器只认行首的 `##` 标记，cu/cm 整段丢失
  （2026-10-31 真机验收发现并修复）。

- 3 条 curl 后台并发，和 CPU 采样的 `sleep 1` 重叠；最坏等待 `--max-time 4`，
  现有命令超时 10 秒仍够用。
- `curl` 建连失败时 `time_connect` 输出 `0.000000` 或空，退出码非零；解析只看输出值，
  不依赖退出码（后台任务拿不到可靠退出码）。
- 服务器缺少 curl 时输出为空 → 三网全失败。测速域已有「服务器缺少 curl」的明确错误，
  可在排查时佐证；本期不额外探测 curl 是否存在。
- 未配置地区：脚本中不出现 curl、临时目录与 `##ct/##cu/##cm` 段，输出与旧版一致。

### 5.2 解析

- `##ct/##cu/##cm` 段存在且可解析为正的浮点秒数 → `Some(CarrierProbe { ok: true, ms })`，
  ms 保留 1 位小数（`round(s × 1000 × 10) / 10`）。
- 段存在但为空/`0.000000`/不可解析 → `Some(CarrierProbe { ok: false, ms: None })`。
- 段缺失（脚本超时、`##done` 缺失、旧夹具）→ `None`（本轮没探到）。
- 三网段全部缺失且配置了地区：样本 `carrier_region=Some(region)`、三网全 `None`；
  窗口统计不会把它计为失败。

### 5.3 探针脚本的生成方式

`PROBE_SCRIPT` 常量改为 `probe_script(region: Option<&str>) -> String`：

- `None` → 与当前脚本逐字一致（减少既有测试/夹具的改动面）。
- `Some(code)` → 在 `##uptime` 前插入临时目录与后台 curl，在 `##wg_rc` 之后插入
  `wait` 与三网段。
- 三网段放在 `##done` 之前，保证 `##done` 仍表示输出完整。

## 6. IPC

| 命令 | 入参（JS） | 返回 | 说明 |
| --- | --- | --- | --- |
| `server_monitor_carrier_regions_cmd` | — | `CarrierRegion[]` | 31 省 `{ code, name }`，按行政区划顺序 |
| `server_monitor_resources_cmd` | `serverId`, `rangeSecs` | `ServerResources` | 单台服务器 CPU/内存/磁盘三条序列，后端下采样 ≤60 点/指标 |

```rust
pub struct CarrierRegion { pub code: String, pub name: String }

pub struct ServerResources {
    pub cpu_pct: Vec<MonitorPoint>,
    pub mem_pct: Vec<MonitorPoint>,
    pub disk_pct: Vec<MonitorPoint>,
}
```

- 摘要（`monitor://tick` 与 `server_monitor_summary_cmd`）里的 `ServerHealth.carrier`
  已包含面板所需的全部数字与格子，前端不再额外查询三网历史。
- `server_monitor_history_cmd` 只被旧趋势面板使用；移除趋势面板后一并退役该命令与
  前端封装（实现时先确认无其他调用方）。
- 资源序列与三网面板相互独立：范围切换只影响资源迷你图。
- 新命令仍走 `commands/monitor.rs` 业务函数 + `commands/ipc.rs` 壳 + `invoke_handler!` 注册。

## 7. 前端

### 7.1 卡片布局（默认全部展开）

```
┌ 示例服务器  203.0.113.10  ● 在线  1 分钟前         [设置] ┐
│ 延迟                                     丢包            │
│ ● 联通  268 ms  ███████████████▓░░░░    0.0%  ░░░░░░░░░░  │
│ ● 电信  244 ms  █████████████▓░░░░░░    3.3%  ██░░░░░░░░  │
│ ● 移动  281 ms  ████████████████░░░░    0.0%  ░░░░░░░░░░  │
│                                                           │
│ CPU 23% ╱╲╱─╲╱  内存 41% ─╲╱──╲  磁盘 68% ╲──╱╲──         │
│ 负载 0.42 · 连接耗时 312ms · xray ✓ ssserver ✓           │
└───────────────────────────────────────────────────────────┘
```

- 去掉「同一时间只展开一台」的交互：所有卡片默认展示全部内容。
- 顺序：三网面板 → 资源迷你图（当前百分比 + 折线）→ 明细/服务徽章/错误。
- 行顺序与颜色对齐参考截图：联通红、电信蓝、移动绿；延迟列显示 `avg_ms`（1 位小数），
  丢包列显示 `loss_pct`（1 位小数）。
- 两个面板的格子共用同一组 `bars`：延迟格用 `latency_level` 上色，失败格与丢包格
  用失败样式；成功格的丢包面板用 1 级颜色。

### 7.2 状态

- 未配置地区：面板显示「未选择三网地区」+「去设置」按钮（打开设置抽屉）。
- 已配置但窗口内无样本：两个面板显示「等待首次采集」。
- 全失败：数字显示 `—`，格子全部为失败样式。
- 服务器离线：沿用最近一条样本的面板；下次采样成功后更新。
- 颜色只作辅助：每格与数值都有文字/`title`/`aria-label`，等级 1–5 映射到
  现有主题色（绿 → 黄 → 橙 → 红 → 深红）。

### 7.3 资源迷你图

- `ResourceSparkline`：纯 SVG，宽约 100–140、高约 28，点由后端给出，前端只画路径。
- 每张图显示指标名、当前百分比与最新点；空态「暂无数据」。
- 三张图共用一个卡片级范围选择器（1 小时 / 24 小时 / 7 天，默认 1 小时）；
  切换时调用 `server_monitor_resources_cmd(serverId, rangeSecs)` 重新取数。
- 旧 `TrendPanel` 移除；`load1`、`connect_ms` 作为文字明细保留。
- 不引入图表库；沿用现有 `LineChart` 的坐标/断点做法（相邻点间隔超过
  `2 × interval` 断线），但尺寸更小。

## 8. 错误处理与安全

- **单台隔离**：三网失败/缺 curl/DNS 失败只影响该台该轮，不影响资源采集与其他服务器。
- **不伪造失败**：SSH 未连上或脚本超时 → 三网 `None`，不计入失败率分母。
- **切换地区**：样本带 `carrier_region`，窗口统计只取当前地区；旧地区格子不混入。
- **配置回退**：`carrier` 字段缺失用默认值；非法值保存时拒绝；坏 JSON 整份回退默认并
  在 `config_error` 展示（沿用现有行为）。
- **安全边界**：探针仍只读；不新增端口/agent/外部服务；不记录凭据；错误文本单行
  ≤200 字符；文档/代码/夹具不出现真实 IP（目标域名是公开 CDN 名称）。
- **时间**：样本 ts 仍是客户端 UTC；三网时间戳取该样本的 ts，不单独记录服务器时钟。

## 9. 测试

### 9.1 后端（`tests/rust/`，`--features local-tests`）

1. **目标与地区**：31 个 code 生成正确的 3 个域名；未知 code 拒绝；地区列表数量与顺序。
2. **脚本生成**：无地区时脚本与旧版逐字一致、不含 `##ct`；有地区时含 3 条 curl、
   临时目录、`wait` 在输出段之前、`##done` 最后；三网段在 `##wg_rc` 之后。
3. **解析**：成功浮点（含 `0.229489`）→ ms；`0.000000`/空/非数字 → ok=false；
   段缺失 → None；`##done` 缺失时仍保留三网解析结果与既有错误。
4. **窗口统计与分级**：均值只算成功；失败率分母只算已探测；阈值边界（等于上界）；
   无样本 → 全 None；`bars` 旧→新、最多 N 条；`latency_level` 与 `avg_level` 一致性。
5. **切地区过滤**：窗口内混入旧地区样本时不计入；样本 `carrier_region` 为 NULL 不计入。
6. **存储**：v21→v22 迁移保留旧数据；新列读写；三网字段 NULL/0/成功三种往返。
7. **摘要**：配置地区的服务器 `carrier=Some` 且名称正确；未配置为 `None`；
   SSH 失败样本不拉低失败率。
8. **配置**：`carrier` 缺省/部分字段；非法分级（非递增、负数）拒绝；
   `carrier_region` 往返与 normalize 保留。
9. **资源序列**：三指标一次查询、下采样 ≤60 点、空档省略。

### 9.2 前端（`tests/frontend/`）

- `CarrierPanel`：数字与格数（20）、等级 class、失败格、全空/未配置/等待采集文案。
- `ResourceSparkline`：正常折线、空数组、单点、全等值。
- `ServerCard`：默认展开、面板在资源图上方、范围切换触发资源命令。
- 清理旧 `TrendPanel` 测试与 `monitor_history` 封装引用。

### 9.3 真机验收

- 给一台境外服务器配置「广东」，立即采集一轮：卡片出现三行数据，数字与格子与
  服务器上手工 `curl -w '%{time_connect}'` 结果同量级；数据库 `carrier_region='gd'`。
- 未配置地区的服务器：面板为空态，脚本输出不含三网段。
- 切换地区后：窗口数字只统计新地区样本（旧样本仍在库里，但不计入）。
- 重复采集多轮：格子向右推进，失败率随失败次数变化。
- 应用重启：迁移到 v22，历史与配置保留；重启后首批样本继续写入三网列。

运行：`cargo test --features local-tests -- --skip net::runtime_tests
--skip net::runtime_service_tests`；前端 `npx tsc` 与现有 vitest 命令。

## 10. 验收标准

1. v21→v22 迁移可重复执行，旧样本保留；三网列 NULL 与 0/成功三种状态可正确往返。
2. 每台服务器可单独配置 31 省之一；未配置不产生探测；保存/加载/归一化不丢地区。
3. 每轮每网最多 1 次 TCP 建连；三网探测与资源采集同轮、同一 SSH 会话；
   三网失败不影响该轮资源指标与其他服务器。
4. 卡片默认展开；三网延迟/丢包面板在资源图表之前；数字为窗口均值/失败率，
   格子为最近 N 次探测，分级与颜色由后端结果驱动。
5. CPU/内存/磁盘百分比以迷你折线展示，支持 1 小时/24 小时/7 天；数据由后端下采样。
6. 切换地区后，窗口统计只包含当前地区样本；SSH 失败样本不计入失败率。
7. 无新增 npm/cargo 依赖；`cargo fmt --check`、`cargo clippy`、`tsc`、规模/组织/
   敏感信息检查通过。
8. 文档、代码、夹具与 IPC 载荷中不出现真实 IP、凭据或私钥。

## 11. 实施顺序

1. **迁移与类型**：v22、`MonitorSample`/`CarrierSample`、store 读写与迁移测试。
2. **目标/脚本/解析**：`carrier.rs`、`probe_script(region)`、解析与脚本单测。
3. **配置**：`carrier` 段、`carrier_region` 覆盖、校验/归一化与测试。
4. **聚合与摘要**：窗口统计、分级、`ServerHealth.carrier`、摘要测试。
5. **IPC**：地区列表、资源序列；退役旧单指标历史命令（确认无调用方）。
6. **前端**：`CarrierPanel`、`ResourceSparkline`、`ServerCard` 重排、`Monitor.tsx`
   去掉单卡展开；类型与 API 封装；前端测试。
7. **收尾**：CHANGELOG、IPC 参考文档、全量检查、真机验收。

## 12. 风险与已知限制

- **第三方目标**：依赖 `zstaticcdn.com` 的 93 个省级域名；目标下线/变更会让对应
  三网整体失败。域名清单集中在 `carrier.rs`，改一处即可更新。
- **TCP ≠ 应用可用性**：建连成功不代表 HTTP 可用；口径已明确为 TCP 建连质量。
- **端口 80**：服务器出站封 80 时三网整体失败，与链路质量无关；本期不做自动换端口。
- **仅 IPv4**：目标只有 v4 域名；服务器没有 IPv4 出站时不可测。
- **DNS 依赖**：域名在被监控服务器上解析，服务器 DNS 异常会表现为三网失败。
- **数字与格子的窗口不同**：数字默认近 1 小时，格子是最近 20 次（默认约 20 分钟）；
  两者一致需要更长历史，首版按 Komari 的「数字=窗口统计、格子=最近网格」呈现。
- **阈值是全局的**：同一组阈值下，亚洲服务器的绝对延迟会普遍低于美洲服务器；
  需要按机器调整时要改全局配置，首版不做每台阈值。
- **应用关闭即无数据**：沿袭现有监控边界，不补采。
- **数据量**：每台每小时新增约 60 行、每行 7 个小列，7 天保留期内可忽略。
