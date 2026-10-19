# 服务器健康监控（应用内）设计

- 日期：2026-10-19
- 状态：设计已确认，待评审后实施
- 依赖：[`2026-08-13-vpn-ctl-p2-design.md`](2026-08-13-vpn-ctl-p2-design.md)（服务器管理与部署）、
  [`2026-10-18-server-egress-family-design.md`](2026-10-18-server-egress-family-design.md)（服务器模型）

**目标：** 在「云驿」桌面端内持续采集已登记服务器的资源、服务与连通性指标，
展示当前状态与 1 小时 / 24 小时 / 7 天趋势，并在持续越界或状态迁移时产生可确认的
应用内告警。

**范围：** 应用打开期间的后台采集（可暂停、可手动触发）、SQLite 样本与告警存储、
服务器卡片与趋势图、告警确认与历史、仪表盘入口。全部数据留在本机，不新增外部服务。

**不做：**

- 不做 24×7 / 无界面常驻 / Windows 计划任务 / 开机自启监控；应用关闭即停止采集。
- 不做 Windows Toast、邮件、Webhook、Telegram 等外部通知。
- 不做证书 / 域名到期、端口扫描、网络延迟与丢包的持续探测（现有测速是手动的一次性任务，
  本期不并入趋势）。
- 不做自定义脚本、日志关键字、自定义告警规则、告警静默时段。
- 不做超过 7 天的样本保留或长期报表。
- 不对服务器做任何写操作（不自动重启服务、不自动修复链路）。
- 不在「服务器」页增加状态点；本期入口只有「监控」页与仪表盘卡片。
- 不引入图表库或任何新的前后端依赖。

## 1. 边界与语义

### 1.1 只在应用运行时采集

监控是**桌面端的一部分**：调度器随应用启动、随应用退出。应用关闭期间没有采集、
没有告警、也没有补采；重新打开后从当前时刻继续，趋势图上表现为一段空档。
UI 必须明确写出这一点，避免用户把它当成服务端 24 小时监控。

### 1.2 采集通道与被监控对象

- 每轮对每台启用监控的服务器建立**一次 SSH 会话**，执行一段只读 POSIX sh 脚本。
- SSH 与服务器管理、部署、测速走同一条 `SshSession` 通道（经 `net::local_proxy`：
  开启网络时走 mihomo，未开启时直连），复用 known_hosts TOFU 与凭据解密。
- 入口地址用 `domain::effective_host`，尊重服务器的 `addr_family`（v4/v6）选择，
  与服务器管理/测速完全一致。
- 监控对象是 `servers` 表里的全部服务器，包括中转机与落地机；每台可在配置里单独停用。
- 不要求服务器上有任何 agent、额外端口或常驻进程。

### 1.3 「可达」的判定

`reachable` 只表示 **SSH 连接与认证成功**，不表示采集脚本成功：

| 情况 | reachable | 样本 | 是否推进离线告警 |
| --- | --- | --- | --- |
| SSH 连接/认证失败 | `false` | 写入，`error` 为失败原因 | 是 |
| SSH 成功，脚本超时/失败 | `true` | 写入，`error` 为脚本错误，指标为空 | 否 |
| SSH 成功，输出部分缺失 | `true` | 写入，`error` 说明不完整，能解析的指标保留 | 否 |
| 本地凭据/DB 读取失败 | 不写样本 | 摘要里 `local_error`，不影响告警计数 | 否 |

这样「脚本报错」不会被误判成「机器离线」，而「机器离线」也不会被脚本错误掩盖。

## 2. 架构

```
应用启动
  └─ setup: 创建 AppState、注册 emitter、启动调度任务（首轮延迟 3s）
        │
        ▼
  scheduler ──每轮──► 读配置 + 服务器列表（短 DB 锁，随后释放）
        │                  │
        │                  ├─ expected_services（由已部署协议推导）
        │                  └─ wg_expected（由 built/imported 中转规则推导）
        │
        ├─ 并发 Probe（默认 2）：SSH 连接 + 执行脚本 → 解析成 RawSample
        │
        ├─ 全部连接失败且 ≥2 台 → 本机网络疑似异常：本轮不写样本、不改告警
        │
        └─ 一次短 DB 事务：写样本 + 评估并写告警
              │
              └─ emit monitor://tick（MonitorSummary）
                    │
                    ▼
              前端单例 store（App / 仪表盘 / 监控页共用）
```

### 2.1 模块边界

| 位置 | 职责 |
| --- | --- |
| `src-tauri/src/server_monitor/mod.rs` | 公共类型、配置读写、启动/停止、`collect_now` 入口 |
| `server_monitor/probe.rs` | `MonitorProbe` 抽象、真实 SSH 采集、脚本常量 |
| `server_monitor/parse.rs` | 脚本输出 → `ParsedProbe`（纯函数，无 IO） |
| `server_monitor/alerts.rs` | 告警状态机（纯函数：输入样本序列，输出 Open/Update/Resolve） |
| `server_monitor/store.rs` | SQLite 读写：样本、告警、历史降采样、保留期清理 |
| `server_monitor/scheduler.rs` | 定时循环、并发限制、唤醒/退出、事件发射 |
| `src-tauri/src/commands/monitor.rs` | 命令层业务函数（不直接写进已接近 1500 行的 `commands/mod.rs`） |
| `src-tauri/src/commands/ipc.rs` | 新增 `#[tauri::command]` 壳，只做 State 提取 |
| `src/views/Monitor.tsx` | 监控页容器（卡片、告警条、抽屉的组合） |
| `src/views/monitor/*.tsx` | 服务器卡片、趋势面板、告警历史、设置抽屉 |
| `src/components/charts/LineChart.tsx` | 纯 SVG 折线图（无新依赖） |
| `src/monitor/registry.ts` | 前端单例 store + Tauri 事件监听（仿 `src/jobs/registry.ts`） |

**文件规模约束：** `db/repos.rs` 已有 1446 行、`commands/mod.rs` 已有 1475 行，
监控的 DB 代码放 `server_monitor/store.rs`，命令业务放 `commands/monitor.rs`；
`commands/mod.rs` 只增加 `mod monitor; pub use monitor::*;` 两行。前端每个组件 ≤500 行。

### 2.2 锁与并发不变量

1. **SSH 期间绝不持有 DB 锁**：目标列表、部署信息、凭据都在采集前读好；采集完成后
   才进入一次短事务。任何「持锁等网络」的写法都不可接受。
2. **同一时刻最多一轮采集**：调度器与手动采集共用一个 `tokio::sync::Mutex<()>`；
   拿不到锁的调度轮次直接跳过（手动轮次等价），手动命令返回「正在采集，请稍候」。
3. **配置在每轮开始时取快照**：轮内配置变化只影响下一轮；保存配置会唤醒调度器。
4. **每台服务器互相隔离**：一台 SSH 超时/凭据失败不中止其他服务器；错误写进该台样本
   或 `local_error`，不升级为整轮失败。
5. **任务不持有 `Arc<AppState>`**：调度任务持 `Weak<AppState>`，每轮 `upgrade()`，
   避免 `AppState → 任务句柄 → AppState` 的循环引用；升级失败即退出。

## 3. 数据模型

### 3.1 迁移 v17

当前 `MIGRATIONS` 已有 16 条（最新 v16 `egress_family`），新增第 17 条：

```sql
-- v17：服务器健康监控。
-- 样本按「轮」写入，ts 是客户端 UTC RFC3339（毫秒精度，固定宽度，便于字典序比较）；
-- 原始样本保留 7 天，由调度器每小时清理一次。
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
  -- 三行服务状态：{"xray":"active","ssserver":"inactive","singbox":"failed"}
  -- NULL = 本轮无法得知（systemctl 缺失/输出不完整），必须与「全部 inactive」区分。
  services_json TEXT,
  -- [] = 确认没有接口；["wg0","wg1"] = 有接口；NULL = 无法得知（wg 缺失/无权限）。
  wg_json TEXT,
  error TEXT
);
CREATE INDEX idx_server_metric_samples_server_ts
  ON server_metric_samples(server_id, ts);
-- 保留期清理按 ts 全表扫描会随行数变慢；单独索引让每小时一次的 DELETE 走索引。
CREATE INDEX idx_server_metric_samples_ts ON server_metric_samples(ts);

-- 一条记录 = 一次告警事件。活动告警 = resolved_at IS NULL；
-- 同一台服务器同一类型最多只有一条活动告警（部分唯一索引兜底）。
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
```

- `Db::open` 已设置 `PRAGMA foreign_keys=ON`，删除服务器会级联删除样本与告警。
- `ts` 统一用 `chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)`
  （例如 `2026-10-19T12:34:56.789Z`）。前端解析后按本地时区显示。
- 保留期：样本 7 天、已恢复告警 90 天；活动告警永不自动删除。清理用
  `DELETE ... WHERE ts < ?`（固定宽度 UTC 字符串可直接字典序比较），
  每轮结束后若距上次清理 ≥1 小时则执行一次。

### 3.2 配置

配置存 `settings` 表的键 `server_monitor_config`（字符串 JSON）。读取失败、JSON 非法
或校验不通过时**整份回退内置默认值**，并在 `MonitorSummary.config_error` 里说明；
用户保存合法配置后错误清空。运行期任何一处都不应因坏配置 panic。

```json
{
  "enabled": true,
  "interval_secs": 60,
  "concurrency": 2,
  "defaults": {
    "cpu_pct": 90.0,
    "mem_pct": 90.0,
    "disk_pct": 85.0,
    "disk_critical_pct": 95.0,
    "load_per_core": 2.0,
    "breach_samples": 3,
    "clear_samples": 2,
    "offline_samples": 3
  },
  "servers": {
    "3": { "enabled": false },
    "5": { "enabled": true, "thresholds": { "cpu_pct": 80.0 } }
  }
}
```

| 字段 | 默认 | 校验 | 说明 |
| --- | --- | --- | --- |
| `enabled` | `true` | — | 只控制自动调度；手动采集不受影响 |
| `interval_secs` | `60` | 30–3600 | UI 提供 30/60/120/300；自定义值原样显示 |
| `concurrency` | `2` | 1–4 | 后端字段，首版 UI 不暴露 |
| `defaults.cpu_pct` | `90` | 1–100 | 达到即越界（`>=`） |
| `defaults.mem_pct` | `90` | 1–100 | 达到即越界 |
| `defaults.disk_pct` | `85` | 1–100 | 磁盘警告阈值 |
| `defaults.disk_critical_pct` | `95` | 1–100 且 > `disk_pct` | 磁盘严重阈值 |
| `defaults.load_per_core` | `2.0` | 0.1–100 | `load1 > cores × 该值` |
| `defaults.breach_samples` | `3` | 1–10 | CPU / 内存 / 负载的连续越界次数 |
| `defaults.clear_samples` | `2` | 1–10 | 所有类型的连续恢复次数 |
| `defaults.offline_samples` | `3` | 1–10 | 连续 SSH 连接失败次数 |

- **缺字段**：全局 `defaults` 与顶层字段缺省时用内置默认补齐；每台服务器的
  `thresholds` 是**逐字段覆盖**——缺的字段继承全局 `defaults`，存在的字段用覆盖值。
- **每台服务器覆盖**：`servers` 的键是 `server_id` 的字符串形式。未知 id 在加载时忽略、
  保存时丢弃；不会导致整份配置失效。
- **规范化保存**：只保留 `enabled=false` 或有 `thresholds` 的条目；保存时写回完整的
  规范化 JSON。UI 首版只编辑 `enabled`，但必须原样保留已有的 `thresholds`，不能因为
  切换启用状态把高级配置抹掉。
- 首版 UI 不暴露每台服务器阈值与 `concurrency`，但后端支持，便于测试与后续扩展。

### 3.3 运行时阈值

```rust
struct EffectiveThresholds {
    cpu_pct: f64,
    mem_pct: f64,
    disk_pct: f64,
    disk_critical_pct: f64,
    load_per_core: f64,
    breach_samples: u32,
    clear_samples: u32,
    offline_samples: u32,
}
```

每轮对每台服务器计算一次：内置默认 → 全局 `defaults` → 该服务器 `thresholds`。
服务与 WireGuard 是状态迁移，不跟数值阈值走，固定连续 2 轮（常量 `TRANSITION_SAMPLES`）。

## 4. 采集协议

### 4.1 SSH 脚本

每轮每台服务器执行一次，最大输出约几十行；`sleep 1` 用于取两次 `/proc/stat` 计算
CPU 占用。脚本只读，不需要 sudo（`wg show` 若权限不足会明确返回非零，见 4.3）。

```sh
echo '##uptime'; cat /proc/uptime 2>/dev/null
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
```

- 超时：连接/认证 10 秒，命令 10 秒（含 sleep 1）。命令超时由 `SshSession::run`
  统一中断，本轮该台标记为「已连接但采集失败」。
- 不要求远端有 bash；脚本只用 POSIX sh 内建与 coreutils。
- 输出按 `##name` 分段解析；段缺失 = 该指标未知，不把空值当 0。
- `##done` 缺失说明输出可能被截断：保留已解析段，`error` 记「采集输出不完整」。
- `##wg_rc` 必须紧跟 `wg show interfaces` 后捕获；`rc != 0` 表示 `wg` 缺失或无权限，
  WG 状态为未知（`None`），**不是**「没有接口」。

### 4.2 指标定义

| 指标 | 来源 | 计算 / 语义 | 单位 |
| --- | --- | --- | --- |
| 连接耗时 `connect_ms` | SSH 建连计时 | `Instant` 包住 `SshSession::connect`（含 DNS/代理/认证）；失败为 `null` | ms |
| 在线 `reachable` | SSH 连接结果 | 连接+认证成功即 `true`，与脚本成败无关 | bool |
| 运行时长 `uptime_secs` | `/proc/uptime` 第 1 个字段 | 向下取整 | 秒 |
| 负载 `load1/5/15` | `/proc/loadavg` 前 3 个字段 | 原值 | — |
| CPU 核数 `cpu_cores` | `nproc`，失败回退 `grep -c '^processor'` | 至少 1 | 核 |
| CPU 占用 `cpu_pct` | `/proc/stat` 的 `cpu` 行两次采样 | 见 4.3 | % |
| 内存 `mem_used/total` | `/proc/meminfo` 的 `MemTotal`、`MemAvailable` | `used = total - available`；缺 `MemAvailable` 则未知 | kB |
| 磁盘 `disk_used/total` | `df -P -k /` 的 `/` 行 | `used/total`；不使用 `Use%` 字段（它已取整） | kB |
| 拥塞控制 `bbr_algo` | `sysctl -n net.ipv4.tcp_congestion_control` | 原字符串（`bbr`/`cubic`/…） | — |
| 服务状态 | `systemctl is-active xray ssserver singbox` | 三行按顺序映射；见 4.4 | — |
| WG 接口 | `wg show interfaces` | 空白分隔的接口名；见 4.3 | — |

### 4.3 CPU 与 WG 的解析规则

**CPU：**

- `/proc/stat` 的 `cpu` 行为
  `user nice system idle iowait irq softirq steal guest guest_nice`。
- 只累加前 8 个字段：`total = user+nice+system+idle+iowait+irq+softirq+steal`；
  **排除 `guest`/`guest_nice`**（它们已计入 user/nice，重复相加会虚高）。
- `idle_all = idle + iowait`。两次采样的增量：
  `cpu_pct = 100 × (Δtotal − Δidle_all) / Δtotal`，结果钳制到 `[0, 100]`。
- `Δtotal <= 0`（计数器回绕/重启/输出异常）→ `cpu_pct = null`，不是 0。
- 单次采样只反映约 1 秒窗口，不是长期均值；UI 标注「约 1 秒采样」。

**WG：**

- 先看 `##wg_rc`：非 0 → `wg_interfaces = None`（未知）；UI 显示「WG 状态未知
  （无权限或未安装）」，不参与告警。
- `rc == 0` 且输出为空 → `Some(vec![])`（确认没有接口）。
- `rc == 0` 且有输出 → 按空白拆分，去重排序后存 `Vec<String>`。
- 告警只处理「整个接口集合从非空变成空」；集合里少一个但仍有接口不告警（可能正在重建链路），
  仅在卡片上展示差异。

### 4.4 服务状态

`systemctl is-active xray ssserver singbox` 每个 unit 输出一行，顺序固定。解析：

- 第 0/1/2 行分别对应 `xray` / `ssserver` / `singbox`；缺失的行记为 `unknown`。
- 原样接受 systemd 的状态词：`active`、`inactive`、`failed`、`activating`、
  `deactivating`、`reloading`、`unknown`；未知词也归为 `unknown`。
- 退出码**不作判据**：有任一 unit 未激活时 `systemctl is-active` 本身返回非零，
  但三行输出仍然有效。
- 「应监控的 unit」由该服务器已部署协议推导：

| 已部署协议 | 对应 unit |
| --- | --- |
| `vless` / `trojan` / `vmess` | `xray` |
| `ss` | `ssserver` |
| `hysteria2` / `tuic` | `singbox` |

- 实际状态仍展示全部三个 unit；未部署的 unit 在 UI 上灰显「未部署」，不参与告警。
- 读不到部署信息（DB 错误）时，该轮不评估服务告警，只展示实际状态；
  `ServerHealth.deployment_known=false`，UI 标注「部署信息未知」，不推断哪些 unit
  未部署。

### 4.5 错误文本

- 所有写进 `error` / `local_error` / 告警 `message` 的远端文本：去掉换行与控制字符、
  压缩连续空白、截断到 200 个字符。
- 不记录完整命令输出、不记录凭据、不把私钥/密码放进任何展示字段。
- 错误文本只用于展示与排查，不参与程序分支判断（分支只看结构化的 `reachable` / 状态值）。

## 5. 调度

### 5.1 生命周期

1. `main` 的 setup 在 `app.manage(state)` 之后启动监控：克隆 `Arc<AppState>`、
   用 `app.handle()` 构造 emitter，`tokio::spawn` 调度任务；句柄存入
   `AppState.monitor`。
2. 首轮延迟 **3 秒**，避开启动自愈、配置重建与 sidecar 初始化的资源竞争。
3. 每轮结束后按 `interval_secs` 休眠；**间隔从本轮结束算起**，不固定墙钟。
4. 保存配置时 `wake.notify_one()`：调度器按「上一轮结束时间 + 新间隔」重算下一次到期时间。
   暂停→启用立即到期；缩短间隔可能立即触发；其他改动只调整等待时长，不额外采集。
5. `ExitRequested` 中先于网络收尾调用 `server_monitor::stop`：发退出信号、唤醒、
   等待任务 ≤2 秒，超时 `abort()`。退出后不再产生样本。

### 5.2 每轮流程

1. 读配置；`enabled=false` 则等待唤醒/退出，不做任何 SSH。
2. 读服务器列表、每台服务器的启用状态、`deployed_protocols`、是否有
   `built/imported` 中转规则；全部在短锁内完成，随后释放。
3. 组装目标列表（排除该台被停用的服务器）。为空则记「没有启用监控的服务器」，休眠。
4. 置 `collecting=true`，发 `monitor://collecting`。
5. 用 `JoinSet` + `Semaphore(concurrency)` 并发采集；每台最多
   `connect 10s + command 10s`。单台失败只影响该台。
6. **本机网络疑似异常守卫**：若「实际探测过的服务器 ≥2 台」且「全部连接失败」，
   判定为疑似本机网络问题：本轮**不写任何样本、不改任何告警**，
   `last_round_error` 写明「所有服务器均不可达，疑似本机网络异常」；跳到最后一步。
   （本地凭据失败不算「探测过」；只有一台服务器时不启用该守卫，按真实离线处理。）
7. 一次短 DB 事务：
   - 为每台读最近 10 条样本（数值/离线计数用）、最近一条可达样本（服务基线）、
     最近一条 `wg_json` 非空的样本（WG 基线）、当前活动告警；
   - 调纯函数告警状态机得到 Open/Update/Resolve；
   - 插入样本，应用告警变更。
   事务内没有 SSH/await；失败则整轮不写，`last_round_error` 记录原因。
8. 每小时最多清理一次过期样本与已恢复告警。
9. 置 `collecting=false`，按本轮结果重建每台 `local_error`；**正常完成**（无守卫、
   无 DB 错误）时清空 `last_round_error`，守卫或失败保留本轮原因。读取最新摘要并
   `emit monitor://tick` 与 `monitor://collecting(false)`。**无论成功、失败还是被
   守卫拦下，都必须执行这一步**（用 Drop guard 或显式 finally 语义），否则 UI 会
   永远停在「采集中」。

### 5.3 手动采集

- 命令 `server_monitor_collect_cmd` 走同一套 `run_round`，共享采集互斥锁。
- 全局 `enabled=false` 时手动采集**仍然可用**（一次性检查）；每台服务器的停用仍然生效。
- 正在采集时再次点击：返回「正在采集，请稍候」；UI 在 `collecting=true` 时禁用按钮。
- 命令在整轮结束后返回 `MonitorSummary`，同时后端仍会发 `monitor://tick`。

### 5.4 配置快照与调度唤醒

- 调度器每轮重新读配置与服务器列表；运行中改配置不需要重启应用。
- 改变 `interval_secs` 不会打断正在跑的一轮；调度器唤醒后按「上一轮结束 + 新间隔」
  重算到期时间，可能立即开始下一轮，也可能延长等待。
- 全局暂停时，若有正在跑的一轮，允许它跑完再停；不 mid-round 杀 SSH（退出除外）。

## 6. 告警

### 6.1 类型、阈值与恢复

| kind | 违规条件 | 打开所需 | 级别 | 恢复条件 | 恢复所需 |
| --- | --- | --- | --- | --- | --- |
| `offline` | `reachable=false`（连接失败） | 连续 `offline_samples`（默认 3） | critical | `reachable=true` | 连续 `clear_samples`（默认 2） |
| `cpu` | `cpu_pct >= cpu_pct` 阈值 | 连续 `breach_samples`（默认 3） | warning | `<` 阈值 | 连续 `clear_samples` |
| `memory` | `mem_used/mem_total >= mem_pct` | 连续 `breach_samples`（默认 3） | warning | `<` 阈值 | 连续 `clear_samples` |
| `disk` | `disk_used/disk_total >= disk_pct` | **1 次立即** | warning；`>= disk_critical_pct` 时 critical | `< disk_pct` | 连续 `clear_samples` |
| `load` | `load1 > cpu_cores × load_per_core` | 连续 `breach_samples`（默认 3） | warning | `<=` 阈值 | 连续 `clear_samples` |
| `service` | 任一应监控 unit 为 `inactive`/`failed` | 见 6.2 | critical | 全部应监控 unit 为 `active` | 连续 `TRANSITION_SAMPLES`（2） |
| `wireguard` | 应监控且基线接口集合非空，当前集合为空 | 连续 `TRANSITION_SAMPLES`（2） | critical | 当前集合非空 | 连续 `TRANSITION_SAMPLES`（2） |

> `service` / `wireguard` 的 2 次是常量 `TRANSITION_SAMPLES`，不随 `breach_samples` /
> `clear_samples` 改变——它们判定的是状态迁移，不是连续数值。

- 计数器按**连续样本**从新到旧数，遇到不满足条件的样本即清零；指标为 `null`/`unknown`
  的样本**既不推进也不清零**（冻结），避免一次采集抖动把已有计数抹掉。
- 已打开的告警必须满足恢复条件才关闭；单次回到阈值内不会立即消失（这是有意的回差）。
- 磁盘告警级别随当前值实时调整：85–95% 为 warning，≥95% 为 critical。
- 负载阈值依赖 `cpu_cores`；核数未知时该轮不评估负载告警，只展示 `load1`。

### 6.2 服务与 WireGuard 的状态迁移

**服务：**

- 以「最近一条可达样本」为基线判断迁移，而不是要求历史里曾经出现过任意状态。
- 状态分类：`active` = 健康；`inactive` / `failed` = 故障；`activating` /
  `deactivating` / `reloading` / `unknown` / 缺失 = 未知（冻结计数，不打开也不恢复）。
- 首次采样：
  - 任一应监控 unit 为 `failed` → **立即**打开（`failed` 是明确的失败态）；
  - 只有 `inactive` → 不打开（可能本来就没启动，避免升级后立刻刷一屏告警）。
- 后续：基线为 `active`，当前变为 `inactive`/`failed`，连续 `TRANSITION_SAMPLES=2`
  次仍非 `active` → 打开；当前为 `unknown` → 冻结计数。
- 一台服务器只产生一条 `service` 告警，消息列出所有异常 unit；全部恢复 `active`
  连续 2 次才关闭。若告警已打开、xray 恢复但 ss 又掉线，告警保持打开并更新消息。
- 服务器离线期间服务状态未知：冻结服务计数；恢复采集后与离线前最后一条可达样本比较。
  离线本身由 `offline` 告警覆盖，不把「未知」当成服务掉线。

**WireGuard：**

- 只对「至少有一条 `relay_rules.status IN ('built','imported')` 关联」的服务器评估
  （中转机与落地机都算关联）；其他服务器只展示接口列表，不告警。
- 基线 = 最近一次**非空**的接口集合。当前集合为空连续 2 次 → 打开；
  非空连续 2 次 → 关闭。基线在告警打开期间保持不变。
- 基线由 store 单独查询「最近一条 `wg_json` 非空」的样本得到，不受 5.2 中
  「最近 10 条」的限制（长时间离线后仍能找回离线前的接口集合）。
- 首次采样就是空集合：只展示「未发现 WG 接口」，不告警（可能是链路尚未收敛）。
- `wg_interfaces = None`（无权限/未安装）不参与告警。

### 6.3 打开、更新、恢复与确认

- **打开**：插入 `server_alerts`，`opened_at = last_at = 本轮 ts`，`acked_at = NULL`，
  `message` 用本轮值生成，`last_value` 记录简短值（≤200 字符）。
- **更新**：活动告警每轮更新 `last_value`、`last_at`、`message`；severity 按当前值调整。
  若 severity **升级**（warning → critical），清空 `acked_at`，让用户重新看到未确认告警。
- **恢复**：写 `resolved_at = 本轮 ts`；不删除记录，历史里显示「已恢复」。
  恢复不清空 `acked_at`（如果曾经确认过）。
- **确认**：`acked_at = now`，告警仍处于活动状态，只是不再计入导航角标与仪表盘
  「未确认」数；已确认的活动告警在告警条里以弱化样式保留。
- 同一 `(server_id, kind)` 在活动期间只有一条记录；部分唯一索引兜底，状态机不产生重复。
- 告警消息模板（全部中文、单行、≤200 字符）：

| kind | 模板示例 |
| --- | --- |
| `offline` | `SSH 连续 3 轮不可达：connection refused` |
| `cpu` | `CPU 连续 3 轮高于 90%（最近 94.2%）` |
| `memory` | `内存连续 3 轮高于 90%（最近 93.1%）` |
| `disk` | `磁盘使用率 96.3% 超过严重阈值 95%（警告阈值 85%）` |
| `load` | `1 分钟负载 9.20 超过 4 核 × 2.0，连续 3 轮` |
| `service` | `服务异常：xray failed；ssserver active；singbox active` |
| `wireguard` | `WireGuard 接口全部消失（基线：wg0）` |

### 6.4 与「本机网络疑似异常」的关系

- 守卫触发时本轮不写样本、不改告警，活动告警保持原状态（不因缺少数据被恢复）。
- 下一轮正常采集后，计数器按新样本继续；被跳过的轮次不参与「连续」计数。
- UI 在摘要 `last_round_error` 有该提示时显示黄色横幅：
  「本轮所有服务器均不可达，已跳过离线告警（疑似本机网络异常）」。
- 只有一台被监控服务器时不使用守卫：单台不可达就是该台的真实离线。

### 6.5 暂停监控时的告警

- 全局暂停或单台停用**不会**自动恢复活动告警：暂停不是故障恢复，伪造 `resolved_at`
  会污染历史。活动告警保留，UI 标注「监控已暂停，状态停留在暂停前」。
- 用户可正常确认这些告警；重新启用后，状态机按新样本继续判断，满足恢复条件才关闭。
- 删除服务器时级联删除其样本与告警。

## 7. IPC

### 7.1 命令

所有返回体字段保持 Rust serde 的 `snake_case`（与 `Server`/`Job` 一致）；
前端 `invoke` 的入参按 Tauri 规则用 camelCase。

| 命令 | 入参（JS） | 返回 | 说明 |
| --- | --- | --- | --- |
| `server_monitor_summary_cmd` | — | `MonitorSummary` | 当前配置、服务器健康、活动告警、采集状态 |
| `server_monitor_history_cmd` | `serverId`, `metric`, `rangeSecs` | `MonitorPoint[]` | 趋势；metric ∈ `cpu_pct`/`mem_pct`/`disk_pct`/`load1`/`connect_ms`；range ∈ 3600/86400/604800 |
| `server_monitor_alerts_cmd` | `includeResolved`, `limit` | `MonitorAlert[]` | 告警历史；limit 1–500 |
| `server_monitor_collect_cmd` | — | `MonitorSummary` | 立即采集一轮；正在采集时报错 |
| `server_monitor_config_cmd` | — | `MonitorConfig` | 读当前规范化配置 |
| `server_monitor_set_config_cmd` | `config` | `MonitorConfig` | 校验、保存、唤醒调度器；返回规范化配置 |
| `server_monitor_ack_cmd` | `alertId` | `()` | 确认活动告警；幂等 |

- 摘要、历史、告警、配置读取命令只读 DB，不发起 SSH；只有 `collect` 与后台调度
  会连接服务器。
- 命令壳放 `commands/ipc.rs`，业务函数放 `commands/monitor.rs`；在 `main.rs` 的
  `invoke_handler!` 注册。
- 校验失败返回 `AppError::Validation` 的中文文案；不存在返回 `NotFound`；
  「正在采集」用 `Message`。前端统一按字符串展示。

### 7.2 事件

| 事件 | 载荷 | 时机 |
| --- | --- | --- |
| `monitor://tick` | `MonitorSummary` | 每轮成功/失败收尾后；手动采集结束后 |
| `monitor://collecting` | `{ "collecting": bool }` | 轮次开始与收尾 |

- 事件与命令的 `MonitorSummary` 完全同构；前端只维护一个 store，不按来源区分。
- 事件不持久化；应用启动时前端主动调一次 `summary` 兜底。

### 7.3 类型

```rust
pub struct MonitorSummary {
    pub config: MonitorConfig,
    pub config_error: Option<String>,
    /// 最近一条成功写入样本的时间（`MAX(ts)`）；失败/守卫轮次不推进。
    pub last_round_at: Option<String>,
    /// 最近一次尝试失败的原因；成功完成后清空。
    pub last_round_error: Option<String>,
    pub collecting: bool,
    pub servers: Vec<ServerHealth>,
    pub active_alerts: Vec<MonitorAlert>,
}

pub struct ServerHealth {
    pub server_id: i64,
    pub name: String,
    pub host: String,
    pub monitored: bool,          // 每台是否启用（不含全局暂停）
    pub online: Option<bool>,     // 最近样本的 reachable；None = 还没有样本
    pub stale: bool,              // 最近样本超过 2×interval+30s
    pub last_sample_at: Option<String>,
    pub expected_services: Vec<String>,
    pub deployment_known: bool,   // false = 部署信息读取失败，服务状态只展示、不告警
    pub wg_expected: bool,
    pub sample: Option<MonitorSample>,
    pub local_error: Option<String>,
    pub active_alerts: usize,
    pub unacked_alerts: usize,
}

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
    pub services: Option<BTreeMap<String, String>>,
    pub wg_interfaces: Option<Vec<String>>,
    pub error: Option<String>,
}

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

pub struct MonitorPoint {
    pub ts: String,
    pub value: f64,
}
```

- `MonitorConfig` / `MonitorOverride` / `Thresholds` 的字段与 3.2 的 JSON 一一对应。
- 时间全部是 UTC RFC3339 毫秒字符串；前端负责本地化显示。
- 任何字段都不包含凭据、私钥、代理密码或完整 SSH 输出。

### 7.4 历史查询与降采样

- 允许范围：1 小时 / 24 小时 / 7 天；超出保留期或非法范围直接校验失败。
- 先按 `server_id + ts` 索引取范围内的原始行（7 天 × 每 60 秒 ≈ 10,080 行/台），
  再在 Rust 里按时间桶求平均。桶宽 `max(range_secs / 360, 1)` 秒，
  输出至多 360 点；桶内没有有效值的点直接省略（形成断点）。
- `mem_pct` / `disk_pct` 用 `used/total` 现算；`total=0` 或缺失时跳过该点。
- `connect_ms`、`load1` 直接取列值；不可达样本不产生点。
- 不引入 SQL 时间聚合：避免 SQLite 对 `%Z`/毫秒格式的解析差异，逻辑可单测。

## 8. 前端

### 8.1 导航与全局状态

- `ViewKey` 增加 `'monitor'`；`NAV_ITEMS` 在「服务器」之后插入「监控」。
- 角标：活动且未确认告警数；0 不显示，超过 99 显示 `99+`。
- `src/monitor/registry.ts` 仿 `src/jobs/registry.ts`：
  - 模块级单例 `summary`、`collecting`、`error`；
  - `installMonitorListener()` 在 App 的 `useEffect` 里调用一次（StrictMode 下用
    模块级标记防重复 `listen`）；
  - `useMonitorSummary()` 用 `useSyncExternalStore` 暴露快照；
  - `refreshMonitor()` 调 `serverMonitorSummary()` 合并进 store；
  - `applySummary(s)` 供手动采集/设置保存后立即更新。
- App 启动时安装监听并拉一次摘要；进入监控页时再拉一次，避免页面打开时数据过期。
- 仪表盘与导航共用同一个 store，不再各自轮询。

### 8.2 监控页

从上到下：

1. **页头**：标题「服务器监控」；「上次成功采集：2 分钟前（12:34）」；「立即采集」按钮
   （`collecting` 时禁用并显示 spinner）；「设置」按钮打开右侧抽屉。
2. **状态横幅**：
   - 全局暂停：「自动监控已暂停，仍可手动采集」；
   - `last_round_error`：「本轮采集失败：…」或「所有服务器均不可达，疑似本机网络异常」；
   - `config_error`：「监控配置无法解析，已使用默认值：…」；
   - 活动告警条：按 critical 优先排列，显示级别、服务器、消息、最近值与「确认」。
3. **服务器卡片网格**（窄屏 1 列，宽屏 2 列）：
   - 标题行：名称 + 地址、在线状态点 + 文字（在线/不可达/暂无数据/已暂停监控）、
     过期标记、最近样本时间；
   - 资源行：CPU / 内存 / 磁盘百分比条（数值 + 颜色），负载 `load1 / 核数`，
     运行时长，SSH 连接耗时，BBR 算法；
   - 服务行：`xray` / `ssserver` / `singbox` 徽章；已部署显示 active/inactive/failed/unknown，
     未部署灰显「未部署」；`deployment_known=false` 时只显示实际状态并标注
     「部署信息未知」，不推断哪些 unit 未部署；WG 接口列表或「未发现 WG 接口」；
     `sample.error` / `local_error` 用单行弱化红字展示（title 里放全文）；
   - 点击卡片后在该卡片下方内联展开趋势面板；同一时间只展开一台。
4. **趋势面板（卡片下方内联）**：指标切换（CPU / 内存 / 磁盘 / 负载 / 连接耗时）+ 范围切换
   （1 小时 / 24 小时 / 7 天）；纯 SVG 折线；显示最新值、最小/最大、起止时间；
   空态「暂无数据（应用运行时才会采集）」。按当前配置间隔的 2 倍判断断点，
   离线空档不连线。
5. **告警历史**：默认最近 100 条，可切换「包含已恢复」；列：级别、服务器、类型、
   说明、最近值、开始/恢复时间、状态、确认按钮（仅活动告警）。
6. **设置抽屉**：自动监控开关、采集间隔、五项阈值、三个计数、每台服务器启用列表；
   保存/取消/恢复默认；校验错误就地显示；保存成功后刷新 store 并关闭。

空态：

- 没有服务器：显示「还没有服务器，先去服务器页添加」+ 跳转按钮；
- 有服务器但全部停用：显示「所有服务器都已暂停监控」；
- 尚无样本：卡片显示「等待首次采集」。

### 8.3 趋势图

- `src/components/charts/LineChart.tsx`，纯 SVG，`viewBox="0 0 360 120"`，
  `vector-effect="non-scaling-stroke"` 保持线宽。
- 输入点按时间戳定位（不是按数组下标），因此离线空档在横轴上保留真实时长；
  相邻点时间差超过 `2 × interval` 时断开路径（`interval` 取当前配置；改过间隔后
  旧数据可能被连起来，这是可接受的展示近似）。
- y 轴取数据最小/最大并留 10% 边距；全等值时上下各留 1；显示 min/mid/max 三个刻度。
- x 轴只显示起止时间；1 小时显示 `HH:mm`，24 小时/7 天显示 `MM-DD HH:mm`。
- 无 hover tooltip（首版），但显示「最新 / 最小 / 最大」文字；SVG 带
  `<title>` 与 `aria-label`，颜色不是唯一信息载体。
- 不引入任何图表库。

### 8.4 仪表盘卡片

- 在现有状态卡网格中增加「服务器健康」卡：
  - 未采集：`尚未采集`；
  - 已采集：`在线 3/4 · 活动告警 2（未确认 1）`（分母 = 启用监控的服务器数）；
  - 有活动告警时用 warning/critical 边框；点击跳转「监控」。
- 监控配置错误时卡片显示「监控配置异常」。
- `Dashboard.tsx` 直接用 `useMonitorSummary()`；已有 `onNavigate` 负责跳转。

## 9. 错误处理与安全

- **单台隔离**：SSH 失败/脚本失败/本地凭据失败都只影响该台；其他台照常入库。
- **本地错误不写样本**：凭据解密或 DB 读取失败时该台本轮跳过，`local_error` 展示，
  不推进、不清零告警计数。
- **整轮错误**：DB 读取/写入失败记 `last_round_error`，发 tick，调度器继续下一轮；
  不弹系统对话框、不阻断应用。
- **配置回退**：坏 JSON 用默认值 + `config_error`；保存前校验，非法值不落库。
- **安全边界**：
  - 远端命令全部只读；不执行 sudo，不修改任何文件/服务；
  - 复用现有 known_hosts TOFU 与凭据解密，不新增信任路径；
  - 不把凭据/私钥/密码写进样本、告警、日志或事件；
  - 错误文本单行化并截断 200 字符；
  - 不新增监听端口、不上传数据到外部服务。
- **本机网络依赖**：监控与被控机之间走本机当前网络；本机断网时所有服务器都会失败。
  由 5.2 的守卫避免误报；UI 仍会显示「所有服务器均不可达」的状态横幅。
- **权限限制**：`wg show` 需要 root/CAP_NET_ADMIN；SSH 用户权限不足时 WG 为未知，
  不产生 WG 告警。服务检查假设 systemd；非 systemd 主机服务状态为未知、不告警。
- **时区/时钟**：样本用客户端 UTC 时间；系统时钟大幅回拨会影响趋势与保留期，
  本期不做额外校正（在风险中登记）。

## 10. 测试

### 10.1 后端

测试源码放仓库根 `tests/rust/`（不随 git 上传），沿用 `--features local-tests`
与模块内 `include!` 模式。

1. **解析器单测**（`parse.rs`）：
   - 完整金样：uptime/load/cpu/mem/disk/cores/bbr/svc/wg 全部解析；
   - CPU 两次采样计算（含 iowait、排除 guest）；Δtotal=0 → None；
   - `MemAvailable` 缺失 → 内存未知，不是 0；
   - `df -P` 的 `/` 行；无 `/` 行 → 磁盘未知；
   - 三行服务状态按顺序映射；少于三行 → 缺失项 unknown；
   - `wg_rc=0` 空输出 → `Some([])`；`wg_rc!=0` → None；
   - 缺 `##done` → 保留已解析段并带「输出不完整」错误。
2. **告警状态机单测**（`alerts.rs`，纯函数、表驱动）：
   - 首次样本：磁盘立即越界；CPU 3 次才开；服务 `failed` 立即开、`inactive` 不开；
   - CPU/内存/负载 3 次打开、2 次恢复；中间一次 unknown 冻结计数；
   - 离线 3 次打开（不可达样本）、2 次可达恢复；本地跳过不写入不改变计数；
   - 服务 active → inactive 连续 2 次打开；离线期间冻结；恢复后与最后可达样本比较；
   - WG 基线非空 → 空连续 2 次打开；首次空不开；`None` 不参与；
   - 磁盘 warning → critical 升级并清空 acked_at；critical → warning 降级；
   - 同一 (server, kind) 不会重复打开；恢复后同类型可再次打开；
   - 「≥2 台全部连接失败」守卫下不产生离线告警；单台不可达仍正常告警。
3. **存储单测**（`store.rs`，内存 SQLite + 全量迁移）：
   - v16→v17 迁移成功，`user_version=17`，旧表数据保留；
   - 样本插入/查询/按 server+ts 排序；
   - 历史降采样 ≤360 点、空桶省略、mem/disk 百分比计算；
   - 活动告警唯一索引；ack 幂等；恢复写 `resolved_at`；
   - 删除服务器级联删除样本与告警；
   - 保留期清理：7 天前样本删除、活动告警保留、90 天前已恢复告警删除。
4. **采集编排单测**（`scheduler.rs` + fake probe）：
   - 3 台：1 台 Connected、1 台 Unreachable、1 台 LocalError；三台互不影响；
   - 并发上限不超过 `concurrency`（fake 记录最大并发）；
   - 全部连接失败守卫不写样本；单台失败照常写样本；
   - 停止信号在 2 秒内返回、不写半轮结果；`collecting` 一定被清掉；
   - 配置暂停时不发起任何 probe；手动采集仍可用。
5. **命令层测试**：`summary`/`history`/`alerts`/`ack`/`set_config` 的参数校验与错误文案；
   配置非法不落库。

运行：`cargo test --features local-tests -- --skip net::runtime_tests
--skip net::runtime_service_tests`（网络测试自带跳过逻辑，仍带上跳过参数避免误跑）。

### 10.2 前端

沿用现有 vitest 配置（测试放 `tests/frontend/`，不随 git 上传）：

- `LineChart`：正常折线、时间断点、空数组、全等值、单点；
- `Monitor`：
  - 渲染服务器卡片与三种服务徽章；
  - 活动告警条与导航角标；
  - 确认按钮调用 `server_monitor_ack_cmd`；
  - 设置抽屉的区间/阈值校验、保存调用、错误展示；
  - 全局暂停与「本轮所有服务器不可达」横幅；
- store：`monitor://tick` 更新快照、`monitor://collecting` 切换按钮禁用、
  StrictMode 下只安装一次监听。

### 10.3 真机验收

用现有 4 台服务器，不人为制造告警（告警状态机由自动化测试覆盖）：

- 对比读数：`uptime`、`cat /proc/loadavg`、`free -k`、`df -P -k /`、
  `systemctl is-active xray ssserver singbox`、`wg show interfaces`（需要时用 sudo）；
  卡片数值应与命令一致（CPU 为约 1 秒窗口，允许合理波动）。
- 立即采集：点击后按钮禁用，完成后数据与「上次成功采集」时间更新；
- 暂停/恢复：暂停后 2 个间隔内无新样本；恢复后立即开始一轮；
- 重启应用：配置与历史保留，迁移版本为 17；应用关闭期间无样本；
- 退出：空闲时关闭应用，2 秒内退出；再次启动后无重复调度任务。

## 11. 验收标准

1. v16→v17 迁移可重复执行且保留旧数据；删除服务器级联清理监控数据。
2. 监控页对每台服务器展示：在线状态、SSH 连接耗时、CPU、内存、磁盘、负载、
   运行时长、核数、BBR、三个服务状态、WG 接口、最近样本时间与错误。
3. 自动轮次按配置间隔运行，单轮内部并发不超过配置值，轮次不重叠；
   手动采集可用，且与自动轮次互斥。
4. 趋势支持 1 小时/24 小时/7 天，返回 ≤360 点；离线空档断线；
   `cpu_pct`/`mem_pct`/`disk_pct`/`load1`/`connect_ms` 均可查看。
5. 告警规则按第 6 节工作：离线、CPU、内存、磁盘、负载、服务、WireGuard 的
   打开/恢复/级别调整/确认均有自动化测试；同一类型活动告警唯一。
6. 全不可达守卫生效：≥2 台全部连接失败时不写样本、不产生离线告警，UI 有明确提示。
7. 暂停/恢复、单台停用、配置持久化、坏配置回退、非法保存拒绝均符合第 3/5/8 节。
8. 应用退出时调度器 ≤2 秒停止，退出后不再产生样本；不影响现有启动/退出/网络收尾。
9. 无新增 npm/cargo 依赖；`cargo fmt --check`、`cargo clippy`、前端 typecheck/build、
   `check:size`、`check:org`、`check:sensitive` 通过。
10. 日志、事件、数据库和 UI 中不出现凭据、私钥或密码；错误文本单行且 ≤200 字符。

## 12. 实施顺序

1. **迁移与存储**：`migrations.rs` v17、`server_monitor/store.rs`、类型与存储单测。
2. **采集与解析**：`probe.rs`、`parse.rs`、脚本常量、解析单测。
3. **告警状态机**：`alerts.rs` 纯函数与表驱动单测。
4. **调度与生命周期**：`scheduler.rs`、`mod.rs`、`state.rs` 的 `monitor` 槽位、
   `main.rs` 启动/退出/注册；fake probe 编排测试。
5. **命令与事件**：`commands/monitor.rs`、`commands/ipc.rs` 壳、`invoke_handler!`。
6. **前端基础**：`api/types.ts`、`api/index.ts`、`src/monitor/registry.ts`、
   `App.tsx` 导航与角标。
7. **监控页**：`Monitor.tsx`、卡片、趋势图、告警历史、设置抽屉；前端测试。
8. **仪表盘与收尾**：Dashboard 卡片、CHANGELOG、
   `docs/design/ipc-interface-reference.md` 增加命令/事件表、全量检查与真机验收。

## 13. 风险与已知限制

- **应用关闭即无监控**：这是产品边界，不是缺陷；UI 与 CHANGELOG 必须写明。
- **本机网络共同故障**：守卫只覆盖「≥2 台全部连接失败」；部分失败时无法区分是服务器
  还是本机到该服务器的链路，按真实不可达处理。
- **监控走本机代理通道**：VPN/节点故障会让部分服务器显示不可达；这是管理通道的真实
  状态，不是独立探针。需要独立观测应另建侧路，本期不做。
- **SSH 权限**：非 root 用户看不到 WG、可能看不到部分系统指标；对应指标为未知，不告警。
- **systemd 假设**：服务状态依赖 `systemctl`；其他 init 系统为未知。
- **CPU 采样窗口短**：约 1 秒的瞬时值，可能错过短时尖峰；告警要求连续
  `breach_samples` 轮（默认 3）才能打开，避免尖峰误报。
- **磁盘只覆盖 `/`**：其他挂载点不在本期范围。
- **规模**：默认 60 秒、并发 2；服务器很多时轮次可能超过间隔。超过约 20 台建议把间隔
  调到 120 秒；本期不做分片、持久队列或按服务器错峰。
- **时钟回拨**：样本用客户端时钟排序；大幅回拨会影响趋势断点与 7 天清理，本期不校正。
- **DB 体积**：每台每小时 60 行、7 天约 1 万行；多台服务器仍在 SQLite 可承受范围，
  由保留期清理兜底。
