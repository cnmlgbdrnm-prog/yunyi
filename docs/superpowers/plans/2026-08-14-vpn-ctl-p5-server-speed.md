# P5 服务器测速实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 服务器页测速（本机 TCP 延迟 + 服务器下载带宽 + 服务器间 iperf3 吞吐），一键安装 iperf3，结果实时推送并持久化。

**Architecture:** 复用 job 队列（新增 JobRequest::ServerSpeed/Throughput，kind='server_speed'）；新建 server_speed/ 模块（phase1.rs 延迟与带宽、iperf.rs 对测与安装）；SSH 复用既有 SshSession（russh）；事件复用 job://progress（终态语义与 P4 完全一致：成功 step='完成'+progress=100，失败/取消 step='结束'+error；单台事件 step='server'+message=server_id，error 非空非终态）。

**Tech Stack:** Rust（tokio、russh、rusqlite 均已有）、React 19、Vitest。

**规格:** `docs/superpowers/specs/2026-08-14-vpn-ctl-p5-server-speed-design.md`（权威事实源；冲突时以规格为准）。

## Global Constraints

- 节点测速保留不动；本计划只加服务器测速。
- 延迟：本机 TCP 连 `server.host:server.port`，3 采样取最小 ms，单次 ≤5s 超时。
- 带宽：SSH exec `curl -o /dev/null -s -w '%{speed_download}' --max-time 30 https://speed.cloudflare.com/__down?bytes=10485760`，解析 stdout f64 → bytes/s；退出 127 或 stderr 含 "not found" →「服务器缺少 curl，无法测下载带宽」。
- 吞吐：目标机 `iperf3 -s --one-off -p {PORT}`（PORT 默认 5201）、源机 `iperf3 -c {addr} -t {secs} -p {PORT} -J`，解析 `end.sum_sent.bits_per_second`（缺失回退 `sum_received`）；seconds 5–30 默认 15；**目标地址**：两机存在已建 relay_rule（status built/imported，先 `rg` 确认枚举值）时用 `landing_wg_ip`，否则目标公网 host；结束补发 `pkill -f 'iperf3 -s'` 兜底。
- 安装 iperf3：system 含 ubuntu/debian → apt-get，centos/rhel/fedora → dnf（无则 yum），alpine → apk；system 空/未知 → 单条探测脚本按 apt-get→dnf→yum→apk 顺序 `command -v` 探测，全无 →「无法识别服务器系统，请手动安装 iperf3」；装后 `iperf3 -v` 验证。
- 单台失败不判任务失败；写库（servers 表/throughput 表）每台/每次完成即写 + 事件；取消中止在途 SSH（future drop + russh 通道关闭）；错误全部可读中文且携带原因。
- TS 禁 any；无 emoji；所有按钮 busy/禁用态；文件 ≤1000 行；中文模块说明；测试只 mock 外部依赖。
- 超时/取消/并发窗口必须有不变量测试（真实入口 + 可枚举输入 + 红灯验证，tokio::time::pause 压虚拟时间）。
- BBR：**惰性检测**（行内按钮触发，页面加载不自动 SSH——老板明确要求）；开启脚本幂等（/etc/sysctl.conf 用 grep -q 防重复追加）；modprobe 失败 →「内核不支持 BBR」。
- SSH 相关测试用既有 FakeSshServer（`src-tauri/src/ssh/tests.rs`，russh 回环 + push_script/block_on/release 旋钮）。
- 每任务 `cargo test`/`npm test` + clippy + fmt；提交信息 `p5:` 前缀。

## 文件结构

```
src-tauri/src/
  server_speed/mod.rs         # 阶段1 编排：批量窗口、每台延迟+带宽、写库、事件、终态
  server_speed/phase1.rs      # tcp_latency_ms + curl_bandwidth + parse_speed_download
  server_speed/iperf.rs       # parse_iperf_json + iperf_install_cmd + run_throughput_job + install_iperf3_on
  server_bbr.rs               # BBR 检测/开启脚本与执行（T7）
  server_speed/phase1_tests.rs / iperf_tests.rs / tests.rs
  domain.rs                   # + ServerSpeedReq/ThroughputReq/ThroughputTest；Server + latency/bandwidth/last_checked_at/last_check_error
  jobs/mod.rs                 # + JobRequest::ServerSpeed/Throughput + kind()
  servers/deploy.rs           # 两分支分发
  db/migrations.rs            # v5：servers 4 列 + server_throughput 表
  db/repos.rs                 # servers::update_speed + throughput::insert/list
  commands.rs                 # server_speed/throughput/install_iperf3/list_throughput + _cmd
  main.rs                     # 注册 4 命令
  ipc_tests.rs                # 线格式
src/api/types.ts / index.ts   # Server 字段 + 4 个 api
src/views/Servers.tsx / Servers.test.tsx
README.md
```

## Task 1: v5 迁移 + repos + 领域类型

**Files:** Modify `src-tauri/src/db/migrations.rs`、`src-tauri/src/db/repos.rs`、`src-tauri/src/domain.rs`、`src-tauri/src/db/tests.rs`

**Interfaces:**
- Consumes: 既有 MIGRATIONS 数组、nodes::update_speed 模式。
- Produces:
  - `repos::servers::update_speed(conn: &Connection, id: i64, latency: Option<i64>, bandwidth: Option<i64>, error: Option<&str>) -> rusqlite::Result<()>`（单语句 UPDATE，last_checked_at 用 chrono::Utc 每次刷新——照 nodes::update_speed）
  - `repos::throughput::insert(conn: &Connection, from_id: i64, to_id: i64, bps: Option<i64>, error: Option<&str>) -> rusqlite::Result<i64>`
  - `repos::throughput::list(conn: &Connection) -> rusqlite::Result<Vec<ThroughputTest>>`（ORDER BY id DESC LIMIT 50）
  - domain: `Server` + `latency: Option<i64>, bandwidth: Option<i64>, last_checked_at: Option<String>, last_check_error: Option<String>`；`ServerSpeedReq { job_id: i64, server_ids: Vec<i64>, concurrency: u8 }`；`ThroughputReq { job_id: i64, from_server_id: i64, to_server_id: i64, seconds: u8 }`；`ThroughputTest { id: i64, from_server_id: i64, to_server_id: i64, bits_per_second: Option<i64>, checked_at: String, error: Option<String> }`（均 derive Debug/Clone/Serialize/Deserialize 照域类型既有风格）

- [ ] **Step 1: 失败测试**（db/tests.rs）：user_version==5；servers 表插行 → update_speed 成功/失败两路径断言（照 P4 nodes 测试）；throughput insert（成功 bps/失败 error）→ list 顺序与字段。
- [ ] **Step 2: 红灯**（update_speed/throughput 不存在）→ **Step 3: 实现**。v5 SQL（照规格 §4，bits_per_second 可空）；`rg "user_version" src-tauri/src` 全部断言 4→5；`rg "Server \{" src-tauri/src` 所有字面量构造点补 4 字段。
- [ ] **Step 4: `cargo test`** → **Step 5: 提交** `feat(p5): v5 迁移 servers 测速列与 throughput 表、repos 写回、领域类型`

## Task 2: phase1 — TCP 延迟与 curl 带宽

**Files:** Create `src-tauri/src/server_speed/mod.rs`（先声明 phase1/iperf 模块）、`src-tauri/src/server_speed/phase1.rs`、`src-tauri/src/server_speed/phase1_tests.rs`；Modify `src-tauri/src/main.rs`（`mod server_speed;`）

**Interfaces:**
- Produces:
  - `pub const SPEED_TEST_URL: &str = "https://speed.cloudflare.com/__down?bytes=10485760";`（与 P4 speed 模块同值；如需共享可 `pub use crate::speed::SPEED_TEST_URL`——先 `rg` 确认 P4 常量位置与可见性，优先复用不重复定义）
  - `pub async fn tcp_latency_ms(host: &str, port: u16, samples: u8) -> Result<i64, String>`——串行 samples 次 `tokio::net::TcpStream::connect` 计时（单次 `tokio::time::timeout(Duration::from_secs(5), ...)`），返回最小 ms；全部失败 → Err(「TCP 连接失败: {host}:{port} {最后一个错误}」)。
  - `pub async fn curl_bandwidth(session: &mut SshSession, url: &str, timeout: Duration) -> Result<f64, String>`——`session.run("下载测速", cmd, timeout)`，cmd 为规格的 curl 命令行；退出 127 或 stderr 含 "not found" → Err(「服务器缺少 curl，无法测下载带宽」)；超时错误照 ssh::SshError 形态透出（其 Display 已可读，直接 String 化）。
  - `pub fn parse_speed_download(stdout: &str) -> Result<f64, String>`——trim 解析 f64（≤0 或 NaN 视为失败）。

- [ ] **Step 1: 失败测试**：
  - phase1_tests：本机 `TcpListener::bind(("127.0.0.1", 0))` 真连——3 采样返回 ≈0ms 且 listener accept 计数==3；连未监听端口 → Err 含 host:port；samples=1 也成立（枚举 samples 1/3/5）。
  - FakeSshServer：curl 正常 stdout "10485760.000" → 解析 10485760.0；退出 127 + stderr "command not found" → Err 含「缺少 curl」；脚本挂起（block_on）+ `tokio::time::pause` 推进 timeout → Err 超时且不挂死；parse_speed_download 空/NaN/"abc" 枚举。
- [ ] **Step 2: 红灯** → **Step 3: 实现** → **Step 4: `cargo test server_speed::`** → **Step 5: 提交** `feat(p5): TCP 延迟三采样最小与 SSH curl 带宽解析`

## Task 3: iperf — JSON 解析、安装命令、对测编排

**Files:** Create `src-tauri/src/server_speed/iperf.rs`、`src-tauri/src/server_speed/iperf_tests.rs`

**Interfaces:**
- Consumes: Task 1（ThroughputTest/insert）、ssh::SshSession、域 Server。
- Produces:
  - `pub const IPERF_PORT: u16 = 5201;`
  - `pub fn parse_iperf_json(s: &str) -> Result<i64, String>`——`serde_json` 解析 `end.sum_sent.bits_per_second`（i64），缺失回退 `end.sum_received.bits_per_second`，都没有 → Err(「iperf3 输出缺少吞吐数据」)。
  - `pub fn iperf_install_cmd(system: &str) -> String`——映射表（小写包含匹配：ubuntu/debian → `apt-get update -qq && apt-get install -y iperf3`；centos/rhel/fedora → `if command -v dnf >/dev/null 2>&1; then dnf install -y iperf3; else yum install -y iperf3; fi`；alpine → `apk add iperf3`）；空/未知 → 探测脚本（apt-get→dnf→yum→apk，全无 echo "NO_PACKAGE_MANAGER" 且 exit 1）。
  - `pub async fn run_throughput_job(state: &AppState, req: ThroughputReq, cancel: Arc<AtomicBool>, emit: EmitFn) -> Result<(), String>`——签名与 P4 `speed::run_speed_job` 同构（Task 4 只做 JobRequest 分发）：
    1. 查两服务器（缺 → Err）与 relay_rule（`rg "find_by|list" src-tauri/src/db/repos.rs` 找按两 server 配对规则的方法；存在 built/imported 规则 → 目标地址 = rule.landing_wg_ip，否则 to.host）。目标地址放事件 message（「经 WG 隧道 {ip}」或「经公网 {host}」）。
    2. SSH 会话获取：照 `commands.rs::test_server_connection` 的凭据解密+connect 模式（先读该函数，若逻辑可复用则提取共享 helper 或照抄最小模式；凭据不进日志）。
    3. 目标机 `iperf3 -s --one-off -p {PORT}` 后台（run + spawn 型——`session.run` 是同步等待，服务端需后台：命令尾部 ` &` 或复用 P3 relay 的异步命令模式，先 `rg "spawn|&" src-tauri/src/relay` 看既有做法）；sleep 1s 等端口（用 `tokio::time::sleep`，测试用 pause）。
    4. 源机 `iperf3 -c {addr} -t {secs} -p {PORT} -J`，超时 secs+30s；cancel 检查置位即提前 Err(「已取消」)。
    5. 解析 JSON → `repos::throughput::insert`（失败 insert error 行）→ 事件（step='iperf'，message=阶段文案，终态在 Task 4 的 run_throughput_job 包装里统一发——本函数发阶段事件与结果事件即可，照 P4 分工）。
    6. 兜底清理：向目标机 run `pkill -f 'iperf3 -s' || true`（忽略退出码）。
- `pub async fn install_iperf3_on(state: &AppState, server_id: i64) -> Result<String, String>`——连 SSH → `iperf3 -v` 已存在则直接返回「已安装」→ run iperf_install_cmd(system)（超时 300s，命令输出含 NO_PACKAGE_MANAGER 或退出非零 → Err 带 stderr）→ 验证 `iperf3 -v` → Ok(「安装完成」)。Task 5 的 `install_iperf3` 命令直接委托此函数（异步命令，UI busy）。

- [ ] **Step 1: 失败测试**（iperf_tests）：
  - parse_iperf_json 金样：sum_sent 有值/仅 sum_received/两者皆无/非 JSON（枚举）。
  - iperf_install_cmd：ubuntu/debian/centos/unknown（含探测脚本顺序断言）/alpine（枚举 system 输入空间——含大小写）。
  - run_throughput_job 用 FakeSshServer 双机（russh 回环，两台 FakeSshServer）：脚本序列断言（目标机先 `iperf3 -s --one-off`、源机 `iperf3 -c ... -t 15 ... -J`、终末 pkill）；JSON 金样注入 → throughput 表 insert 断言 + 事件断言；目标机脚本挂起 + pause → 超时；cancel 置位 → 「已取消」且 pkill 兜底仍发出。
- [ ] **Step 2: 红灯** → **Step 3: 实现** → **Step 4: `cargo test server_speed::`** → **Step 5: 提交** `feat(p5): iperf3 对测编排与安装命令（WG 地址优先/超时/兜底清理）`

## Task 4: job 队列集成 + 阶段1 编排（不变量测试）

**Files:** Modify `src-tauri/src/domain.rs`（无——类型已在 T1）、`src-tauri/src/jobs/mod.rs`、`src-tauri/src/servers/deploy.rs`、Create `src-tauri/src/server_speed/tests.rs`；Modify `src-tauri/src/jobs/tests.rs`

**Interfaces:**
- Consumes: T1-T3。
- Produces: `pub async fn run_server_speed_job(state: &AppState, req: ServerSpeedReq, cancel: Arc<AtomicBool>, emit: EmitFn) -> Result<(), String>`（mod.rs）
- jobs: `JobRequest::ServerSpeed(ServerSpeedReq)` / `JobRequest::Throughput(ThroughputReq)`；`job_id()` 各返 req.job_id；`kind()` 均 `"server_speed"`。
- deploy.rs ProdRunner：`JobRequest::ServerSpeed(r) => server_speed::run_server_speed_job(...).await`、`JobRequest::Throughput(r) => server_speed::iperf::run_throughput_job(...).await`（照 SpeedTest 分支写法）。

**行为（run_server_speed_job）:**
1. 节点装载：按 server_ids 查 servers（缺失 → 该台结果 error=「服务器不存在」）。
2. 并发窗口：`chunks(concurrency as usize)` + JoinSet（照 P4 run_speed_job）；块间 cancel 检查。
3. 每台：SSH 会话获取（失败 → 该台 error）→ tcp_latency_ms(host, port, 3) 与 curl_bandwidth 各自独立执行（延迟失败不阻断带宽；两者任一成功即写成功值 + error 记另一项失败原因）→ `servers::update_speed` → 事件 step='server' message=server_id error=该台综合错误|null。
4. 全部完成 → 终态事件 step='完成' progress=100 → Ok；cancel → 在途中止、返回 Err(「已取消」)（worker 发 '结束' 终态）。
5. 常量：`pub const LATENCY_SAMPLES: u8 = 3; pub const DOWNLOAD_TIMEOUT: Duration = 45s;`（curl 30s max-time + 余量）

**不变量测试（server_speed/tests.rs，FakeSshServer + tokio::time::pause）:**
- 并发窗口：FakeSshServer 记录同时在途连接数，3 并发 ≤3；4 台全完成。
- 单台失败隔离：1 台 TCP 连不上 + curl 成功 → latency NULL + bandwidth 有值 + error 含 TCP 失败原因，任务 Ok。
- 取消：curl 脚本挂起（block_on）→ 置 cancel → release → job Err(「已取消」)，未开始台不被测。
- 超时：curl 挂起 + pause 推进 45s → 该台错误「下载测速超时」，不挂死。
- 缺 id：该台 error=「服务器不存在」，其余正常，Ok。
- 事件：事件数 == 台数 + 1 终态；终态 progress==100 step=='完成'；kind()=='server_speed' 钉死测试（jobs/tests）。
- 终态失败语义：run_throughput_job 的 Err 与 worker 补发 '结束' 事件互斥（照 P4 已有模式）。

- [ ] **Step 1: 先写不变量测试（红灯）** → **Step 2-3: 实现** → **Step 4: `cargo test` 全量 + clippy** → **Step 5: 提交** `feat(p5): 服务器测速执行器与 job 队列集成（并发/取消/超时不变量）`

## Task 5: 命令层 + IPC

**Files:** Modify `src-tauri/src/commands.rs`、`src-tauri/src/main.rs`（注册 4 命令）、`src-tauri/src/ipc_tests.rs`、`src-tauri/src/commands_tests.rs`

**Interfaces:**
- Produces（双层照 start_deploy/start_deploy_cmd 模式）：
  - `pub fn server_speed(state: Arc<AppState>, server_ids: Vec<i64>, concurrency: u8) -> R<i64>`——校验非空/去重保序/1..=5/全部存在 → jobs 行 kind='server_speed' queued → submit（失败回写 failed 并 Err）。
  - `pub fn server_throughput(state: Arc<AppState>, from_server_id: i64, to_server_id: i64, seconds: u8) -> R<i64>`——from!=to、两服务器存在、seconds 5..=30。
  - `pub async fn install_iperf3(state: Arc<AppState>, server_id: i64) -> R<String>`——委托 `server_speed::iperf::install_iperf3_on`。
  - `pub fn list_throughput(state: Arc<AppState>) -> R<Vec<ThroughputTest>>`
  - `#[tauri::command] pub async fn server_speed_cmd(state: tauri::State<'_, Arc<AppState>>, server_ids: Vec<i64>, concurrency: u8) -> R<i64>`（以及 server_throughput_cmd / install_iperf3_cmd / list_throughput_cmd）

- [ ] **Step 1: 失败测试**：commands_tests（空 ids/concurrency 0/6/缺 id/from==to/seconds 4 与 31 → Err 不产 job 行；成功 → kind='server_speed' queued）；ipc_tests（invoke_ok 线格式 camelCase：`serverIds`/`concurrency`/`fromServerId`/`toServerId`/`seconds`/`serverId`；非法参数 Err 文本；mock_builder 注册 4 命令）。
- [ ] **Step 2-4: 红灯→实现→全量测试**
- [ ] **Step 5: 提交** `feat(p5): 服务器测速命令与真实 IPC 线格式测试`

## Task 6: 前端 UI + 文档

**Files:** Modify `src/api/types.ts`（Server + 4 字段、ThroughputTest、JobProgress.kind + 'server_speed'）、`src/api/index.ts`（`serverSpeed(serverIds: number[], concurrency: number)`、`serverThroughput(fromServerId: number, toServerId: number, seconds: number)`、`installIperf3(serverId: number)`、`listThroughput(): Promise<ThroughputTest[]>`）、`src/views/Servers.tsx`、`src/views/Servers.test.tsx`、`README.md`

**UI 行为（照规格 §8）:**
- 行尾新增列：延迟 ms / 下载带宽 MB/s（`(b/1048576).toFixed(2)`）/ 上次测速（复用 `src/utils/checkedAtFormat.ts`）/ 失败原因红字。
- 行内「测速」（busy「测速中…」）；行错误含「缺少 iperf3」时该行出现「安装 iperf3」按钮（busy，装完行内提示并清错误）。
- 工具栏：「批量测速」（全部服务器）+ 并发下拉（1/3/5 默认 3）+ 任务进行中「取消测速」（cancelJob）；「吞吐对测」：源/目标下拉（默认取第一条 built/imported relay_rule 的 relay_server_id→landing_server_id，用 list_relay_rules() 拉）+ 秒数（5/15/30 默认 15）+ 最近对测结果（listThroughput() 首行，Mbps = bits/1e6 两位小数）。
- 事件订阅照 Nodes.tsx 模式：kind=='server_speed' + jobId 过滤；step=='server' 按 message=server_id 更新行；step=='完成'/'结束' → 清任务态 + refresh + 对测任务时另刷 listThroughput。终态判定只看 step。
- 组件测试 ≥8：单发参数、busy、批量 ids、并发值、取消、事件行更新、终态 refresh、安装按钮调用与 busy、对测默认源/目标与秒数、对测结果展示、失败原因红字、忽略其他 kind/jobId。
- Server 类型 4 新字段会波及 Servers.test.tsx 既有 fixture（`rg "system: ''"` 等构造点同步补 `latency: null, bandwidth: null, last_checked_at: null, last_check_error: null`）。
- README：服务器页测速说明（阶段1/阶段2/安装 iperf3/防火墙 5201/流量提示）。
- 禁止启动 App 验收（用 npm test + npx tsc --noEmit + npm run build）。

- [ ] **Step 1: 组件测试先写（红灯）** → **实现** → `npm test` + `npx tsc --noEmit` + `npm run build`
- [ ] **Step 2: README 同步**
- [ ] **Step 3: 提交** `feat(p5): 服务器页测速 UI（单发/批量/对测/安装 iperf3）与文档`

## Task 7: BBR 检测与开启（后端归 Batch B，UI 归 Batch C）

**Files:** Create `src-tauri/src/server_bbr.rs`（含 bbr_tests 子模块或相邻测试文件，照仓库风格）；Modify `src-tauri/src/main.rs`（mod + 注册）、`src-tauri/src/commands.rs`、`src-tauri/src/ipc_tests.rs`；UI 归 Task 6 范围

**Interfaces:**
- Produces:
  - `pub async fn check_bbr(session: &mut SshSession) -> Result<String, String>`——run `sysctl -n net.ipv4.tcp_congestion_control`（超时 15s）→ Ok(算法名 trim)；非零退出/空输出 → Err 带原因。
  - `pub async fn enable_bbr(session: &mut SshSession) -> Result<String, String>`——run 开启脚本（超时 30s）→ 脚本尾行回读当前算法；含 "NO_BBR_MODULE" → Err(「内核不支持 BBR（modprobe tcp_bbr 失败）」)；非零 → Err 带 stderr；成功 → Ok(「已开启 bbr」)。
  - `pub const BBR_ENABLE_SCRIPT: &str`：
    ```bash
    grep -qw bbr /proc/sys/net/ipv4/tcp_available_congestion_control 2>/dev/null || modprobe tcp_bbr 2>/dev/null || { echo NO_BBR_MODULE; exit 1; }
    sysctl -w net.core.default_qdisc=fq >/dev/null
    sysctl -w net.ipv4.tcp_congestion_control=bbr >/dev/null
    grep -q 'net.core.default_qdisc' /etc/sysctl.conf 2>/dev/null || echo 'net.core.default_qdisc=fq' >> /etc/sysctl.conf
    grep -q 'net.ipv4.tcp_congestion_control' /etc/sysctl.conf 2>/dev/null || echo 'net.ipv4.tcp_congestion_control=bbr' >> /etc/sysctl.conf
    sysctl -n net.ipv4.tcp_congestion_control
    ```
  - 命令层：`pub async fn check_bbr(state: Arc<AppState>, server_id: i64) -> R<String>`（连 SSH 委托 check_bbr；会话获取照 test_server_connection 模式）、`pub async fn enable_bbr(state: Arc<AppState>, server_id: i64) -> R<String>`；`#[tauri::command]` 的 `check_bbr_cmd`/`enable_bbr_cmd`（tauri::State 委托）。

- [ ] **Step 1: 失败测试**：
  - 单元（FakeSshServer）：check 返回 "bbr"/"cubic"/空输出/非零；enable 成功金样（断言脚本被完整执行且返回「已开启 bbr」）、modprobe 失败（NO_BBR_MODULE → 内核不支持）、/etc/sysctl.conf 幂等由脚本内容断言（recorded() 里 grep -q 守卫存在）。
  - commands_tests：server 不存在 → Err；ipc_tests：check_bbr_cmd/enable_bbr_cmd 线格式（serverId camelCase）+ mock_builder 注册。
- [ ] **Step 2-4: 红灯→实现→`cargo test` 全量**
- [ ] **Step 5: 提交**（Batch B 内独立 commit）`feat(p5): 服务器 BBR 检测与一键开启（幂等脚本+内核不支持诊断）`

**UI 部分（归 Task 6）**：服务器行内「BBR」按钮（惰性）：点击 → check_bbr_cmd（busy「检测中…」）→ 显示当前算法；非 bbr → 按钮变「开启 BBR」（busy「开启中…」）→ 成功后显示「已开启 bbr」并清错误。页面加载不发任何 BBR 请求。组件测试：惰性（挂载零调用）、检测结果展示、非 bbr 出现开启按钮、开启成功文案、开启失败原因、busy 态。

## 终审

全部任务后：`cargo test`、`cargo clippy --all-targets`、`cargo fmt --check`、`npm test`、`npx tsc --noEmit`、`npm run build`、`npm run check:package`；真实 App 手工验收（老板 3 台真机：每台延迟/带宽；ashura↔VM-irO0oMfP 对测走 WG 地址；BBR 检测/开启真实生效；失败原因可见；重启后结果仍在；退出无残留）。
