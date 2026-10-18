# P4 测速实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 节点页单发/批量测速（延迟 + 带宽），mihomo sidecar 打包内置，结果实时推送并持久化。

**Architecture:** 复用现有 job 队列（新增 JobRequest::SpeedTest）；speed/ 模块管 sidecar 生命周期与测速引擎；SpeedEngine trait 隔离真实 HTTP（测试用 FakeEngine）；事件复用 job://progress（kind='speed'）。

**Tech Stack:** Rust（reqwest 已依赖）、Tauri 2（bundle.resources）、React 19、Vitest。

**规格:** `docs/superpowers/specs/2026-08-14-vpn-ctl-p4-design.md`（权威事实源；本计划与其冲突时以规格为准）。

## Global Constraints

- mihomo **打包内置**：仓库不提交二进制；`scripts/fetch-sidecar.mjs` 下载 v1.19.10 windows-amd64 + sha256 校验 → `src-tauri/binaries/mihomo.exe`；`.gitignore` 忽略该目录；`tauri.conf.json` `bundle.resources` 引入。
- 并发 1–5（u8），默认 3；批量与单发统一走 `speed_test_cmd(node_ids, concurrency)`。
- 带宽：经 sidecar mixed-port 代理下载 `https://speed.cloudflare.com/__down?bytes=10485760`（10MB），30s 超时，取消感知，结果 bytes/s（i64）。
- 延迟：`GET /proxies/{name}/delay?url=http://www.gstatic.com/generate_204&timeout=5000`（ms，i64）。
- sidecar：懒启动（首次测速）、App 退出即停；external-controller/mixed-port 均 127.0.0.1 随机端口 + 随机 token（16+ hex）；控制器请求带 `Authorization: Bearer {token}`。
- 注入代理名一律用 **node.id**（唯一、无 URL 转义问题）；节点形态转换复用 `config/` 的映射函数，与导出 YAML 一致。
- 每节点完成即写库（latency/bandwidth/last_checked_at/last_check_error）+ 发事件（kind='speed'，message=node_id，progress=已完成数，error=单节点失败原因）。**单节点失败不判任务失败**；仅 sidecar 启动失败/前置校验失败才落 failed。取消：标志置位后中止在途下载、跳过未开始节点，runner 返回 Err（队列层落 cancelled）。
- TypeScript 禁 any；所有按钮 busy/禁用态；无 emoji；错误带原因。
- 文件 ≤1000 行；模块中文说明；测试只 mock 外部依赖。
- 超时/取消/并发窗口必须有不变量测试（真实入口 + 可枚举输入 + 红灯验证），用 `tokio::time::pause()` 压虚拟时间。
- 每任务跑 `cargo test`（Rust）或 `npm test`/`npx tsc`（前端）+ `cargo clippy` + `cargo fmt`；提交信息带 `p4:` 前缀。

## 文件结构

```
src-tauri/src/
  speed/mod.rs           # 测速执行器：任务编排、并发窗口、写库、事件
  speed/sidecar.rs       # mihomo 二进制定位/复制/启动/停止、config 渲染、端口/token
  speed/engine.rs        # SpeedEngine trait + MihomoEngine（reqwest）
  speed/tests.rs         # 执行器不变量测试（FakeEngine）
  speed/engine_tests.rs  # MihomoEngine 纯逻辑（URL 构造/响应解析）
  speed/sidecar_tests.rs # config 渲染/端口分配/token 纯函数
  domain.rs              # + SpeedTestReq
  jobs/mod.rs            # + JobRequest::SpeedTest + kind()
  servers/deploy.rs      # ProdRunner 分发 SpeedTest → speed::run_speed_job
  db/migrations.rs       # v4: nodes ADD COLUMN last_check_error TEXT
  db/repos.rs            # nodes::update_speed
  config/mod.rs          # node_proxy_json(node, proxy_name)
  state.rs               # + sidecar: Mutex<Option<RunningSidecar>>, sidecar_resource: RwLock<Option<PathBuf>>, data_dir: PathBuf
  commands.rs            # speed_test / speed_test_cmd
  main.rs                # 注册命令、资源路径注入、退出时 stop sidecar
  ipc_tests.rs           # speed_test_cmd 线格式测试
scripts/fetch-sidecar.mjs
tauri.conf.json          # bundle.resources
.gitignore               # src-tauri/binaries/
src/api/types.ts         # Node + last_check_error
src/api/index.ts         # speedTest(nodeIds, concurrency)
src/views/Nodes.tsx      # 行测速/批量/并发/取消/实时事件（测试 Nodes.test.tsx）
```

## Task 1: DB v4 迁移 + nodes 写回

**Files:**
- Modify: `src-tauri/src/db/migrations.rs`（追加 v4）、`src-tauri/src/db/repos.rs`（nodes 模块）、`src-tauri/src/db/tests.rs`

**Interfaces:**
- Produces: `repos::nodes::update_speed(conn: &Connection, id: &str, latency: Option<i64>, bandwidth: Option<i64>, error: Option<&str>) -> rusqlite::Result<()>`

- [ ] **Step 1: 写失败测试**

```rust
// db/tests.rs
#[test]
fn migration_v4_adds_last_check_error_and_update_speed_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::Db::open(&dir.path().join("t.db")).unwrap();
    let conn = db.lock();
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(ver, 4);
    conn.execute("INSERT INTO nodes (id,name,source,protocol,addr,port,params_json,group_name) VALUES ('n1','节点','manual','ss','1.1.1.1',443,'{}','默认分组')", []).unwrap();
    repos::nodes::update_speed(&conn, "n1", Some(120), Some(10485760), None).unwrap();
    let (lat, bw, err): (Option<i64>, Option<i64>, Option<String>) = conn
        .query_row("SELECT latency, bandwidth, last_check_error FROM nodes WHERE id='n1'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((lat, bw), (Some(120), Some(10485760)));
    assert!(err.is_none());
    // 失败路径：错误文本入库、指标清空
    repos::nodes::update_speed(&conn, "n1", None, None, Some("连接超时")).unwrap();
    let (lat2, err2): (Option<i64>, Option<String>) = conn
        .query_row("SELECT latency, last_check_error FROM nodes WHERE id='n1'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(lat2, None);
    assert_eq!(err2.as_deref(), Some("连接超时"));
    // last_checked_at 每次尝试都刷新
    let checked: String = conn.query_row("SELECT last_checked_at FROM nodes WHERE id='n1'", [], |r| r.get(0)).unwrap();
    assert!(!checked.is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**（update_speed 不存在）
- [ ] **Step 3: 实现**。migrations.rs MIGRATIONS 追加：

```sql
-- v4：节点测速失败原因（成功置 NULL；latency/bandwidth 已有，P4 填充）
ALTER TABLE nodes ADD COLUMN last_check_error TEXT;
```

repos::nodes::update_speed 单语句 UPDATE（`latency=?2, bandwidth=?3, last_checked_at=?4, last_check_error=?5`，last_checked_at 用 `chrono::Utc::now().to_rfc3339()`）。同时把既有 user_version 断言 3 → 4（`migrations_are_idempotent`、`failed_migration_rolls_back_atomically`、`migration_v2_rebuilds...` 里的 3）。
- [ ] **Step 4: 跑全量 `cargo test`**
- [ ] **Step 5: 提交** `feat(p4): v4 迁移 nodes 测速字段与 update_speed 写回`

## Task 2: 单节点代理 JSON 导出

**Files:** Modify `src-tauri/src/config/mod.rs`（+ `node_proxy_json`）、`src-tauri/src/config/tests.rs`

**Interfaces:**
- Produces: `pub fn node_proxy_json(node: &Node, proxy_name: &str) -> Result<serde_json::Value, ConfigError>` — 复用 ss/vmess/vless/trojan/clash_raw 映射，`name` 覆盖为 proxy_name；clash_raw 分支解析 YAML 后同样覆盖 name。

- [ ] **Step 1: 写失败测试**（config/tests.rs）：对 ss/vless 节点断言返回 JSON 的 name=覆盖名、server/port/关键参数字段与 build_yaml 中一致；clash_raw 节点原样字段保留且 name 被覆盖。
- [ ] **Step 2: 红灯** → **Step 3: 实现**（提取 build_yaml 内循环体为共享 helper，转换 `serde_yaml::Value::Mapping(map)` → `serde_json::to_value(...)`；serde_yaml::Value 已实现 Serialize）
- [ ] **Step 4: `cargo test config::`** → **Step 5: 提交** `feat(p4): 单节点 clash 代理 JSON 导出`

## Task 3: sidecar 管理

**Files:** Create `src-tauri/src/speed/sidecar.rs`、`src-tauri/src/speed/sidecar_tests.rs`、`src-tauri/src/speed/mod.rs`（先空模块声明）；Modify `src-tauri/src/state.rs`、`src-tauri/src/main.rs`（资源注入+退出清理）

**Interfaces:**
- Consumes: AppState 现有结构。
- Produces:
  - `pub struct RunningSidecar { pub controller_port: u16, pub mixed_port: u16, pub token: String, pub child: std::process::Child }`（用 std::process 而非 tokio::process——不新增 tokio feature；spawn/kill/wait 均为快速同步调用）
  - `pub const SIDECAR_DIR: &str = "sidecar"; pub const SIDECAR_EXE: &str = "mihomo.exe";`
  - `pub const SIDECAR_EXE_SHA256: &str = "a4feef7e4f8cbaf19e2079b3caa3e23821dfe419ad2e39e0539b843bd2d4d1be";`（mihomo-windows-amd64.exe v1.19.10，31,053,824 字节；2026-08-14 从官方 release 下载后本机 sha256sum 计算，与 fetch-sidecar.mjs 的 zip digest 校验链一致）
  - `pub fn render_sidecar_config(controller_port: u16, mixed_port: u16, token: &str) -> String`
  - `pub fn random_token() -> String`（16+ 随机 hex，`rand` 已依赖）
  - `pub fn alloc_local_ports() -> std::io::Result<(u16, u16)>`（`TcpListener::bind(127.0.0.1:0)` 两次取端口后 drop）
  - `pub async fn ensure_started(state: &AppState) -> Result<(u16, u16, String), String>`（controller_port, mixed_port, token；已运行则直接返回）
  - `pub async fn stop_all(state: &AppState)`（kill child + 置 None）
- AppState 新增 `pub sidecar: tokio::sync::Mutex<Option<RunningSidecar>>`、`pub sidecar_resource: std::sync::RwLock<Option<std::path::PathBuf>>`（默认 None，测试构造不变）、`pub data_dir: std::path::PathBuf`（new_with_store 内由 `db_path.parent()` 派生）。
- main.rs：setup 里把 `app.path().resolve("binaries/mihomo.exe", tauri::path::BaseDirectory::Resource)` 的结果写进 `state.sidecar_resource`（Err 时置 None 不 panic）；末尾 `.run(tauri::generate_context!())` 改为 `.build(tauri::generate_context!()).expect("build tauri app").run(|app, event| { if let tauri::RunEvent::ExitRequested { .. } = event { let state = app.state::<Arc<AppState>>(); let _ = tauri::async_runtime::block_on(speed::stop_all(&state)); } })`。

**行为:**
- ensure_started：已运行 → 返回既有端口；否则解析二进制（数据目录 `{data_dir}/sidecar/mihomo.exe` 存在且 sha256 匹配则直接用，否则从 sidecar_resource 复制）→ render config 写 `sidecar/config.yaml`（mixed-port 字段名 `mixed-port`、external-controller `127.0.0.1:{port}`、secret token、proxies: []）→ `tokio::process::Command` spawn（工作目录 sidecar/）→ 轮询 `GET http://127.0.0.1:{port}/version`（Bearer token，≤5s，200ms 间隔）→ 返回。任一步失败给中文可读错误。
- stop_all：child.kill() + await。
- 不变量：端口/路径/token 纯函数可枚举测试；config 渲染金样断言含 mixed-port/external-controller/secret。

- [ ] **Step 1: 先写纯函数测试**（render 金样、token 长度/字符集、alloc 端口互异且可重绑）
- [ ] **Step 2: 红灯** → **Step 3: 实现** → **Step 4: `cargo test speed::`**
- [ ] **Step 5: 提交** `feat(p4): mihomo sidecar 生命周期管理（懒启动/退出清理/随机端口+token）`

## Task 4: 测速引擎（trait + MihomoEngine + FakeEngine）

**Files:** Create `src-tauri/src/speed/engine.rs`、`src-tauri/src/speed/engine_tests.rs`

**Interfaces:**
- Produces:

```rust
#[async_trait::async_trait]
pub trait SpeedEngine: Send + Sync {
    async fn put_proxy(&self, name: &str, proxy: serde_json::Value) -> Result<(), String>;
    async fn delete_proxy(&self, name: &str) -> Result<(), String>;
    async fn delay(&self, name: &str, url: &str, timeout_ms: u64) -> Result<i64, String>;
    async fn download_through(&self, mixed_port: u16, url: &str, size_limit: usize, timeout: std::time::Duration, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<f64, String>; // bytes/s
}
pub struct MihomoEngine { http: reqwest::Client, controller: String, token: String }
impl MihomoEngine { pub fn new(controller_port: u16, token: String) -> Self }
#[cfg(test)] pub struct FakeEngine { …注入延迟/结果/慢下载/取消敏感 }
```

**MihomoEngine 行为:**
- put/delete：`PUT/DELETE {controller}/proxies/{name}`，Bearer 头；非 2xx → Err(状态+body 前 200 字符)。
- delay：`GET {controller}/proxies/{name}/delay?url={url}&timeout={timeout_ms}`，解析 `{"delay": n}`；非 2xx/缺 delay → Err。
- download_through：`reqwest::Client::builder().proxy(Proxy::all(format!("http://127.0.0.1:{mixed_port}")))`，GET url 流式读 body，累计 bytes 到 size_limit 或超时（`tokio::time::timeout`）；cancel 置位 → 立即 Err("已取消")；结束 `bytes / elapsed_secs`。

- [ ] **Step 1: FakeEngine + 单测**（URL/头/解析逻辑：给 FakeEngine 注入延迟值断言透传；engine_tests 里用 dev-dep **wiremock 0.6.5**（Cargo.toml 已有）模拟控制器断言 MihomoEngine 的请求路径、Bearer 头、delay 解析、非 2xx 错误文本——不起本地 axum）
- [ ] **Step 2-4: 红灯→实现→`cargo test speed::`**
- [ ] **Step 5: 提交** `feat(p4): SpeedEngine trait + MihomoEngine REST/代理下载实现`

## Task 5: 测速执行器 + job 队列集成（不变量测试）

**Files:** Modify `src-tauri/src/domain.rs`（SpeedTestReq）、`src-tauri/src/jobs/mod.rs`（JobRequest::SpeedTest + kind/job_id）、`src-tauri/src/servers/deploy.rs`（ProdRunner 分发）、`src-tauri/Cargo.toml`（tokio features += `"test-util"`——`tokio::time::pause` 不变量测试需要）、Create `src-tauri/src/speed/mod.rs`（run_speed_job）、`src-tauri/src/speed/tests.rs`；Modify `src-tauri/src/jobs/tests.rs`（kind 断言若需要）

**Interfaces:**
- Consumes: Task 1/3/4 产物。
- Produces: `pub async fn run_speed_job(state: &AppState, req: SpeedTestReq, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>, emit: crate::jobs::EmitFn) -> Result<(), String>`

```rust
// domain.rs
#[derive(Debug, Clone)]
pub struct SpeedTestReq { pub job_id: i64, pub node_ids: Vec<String>, pub concurrency: u8 }
```

**行为:**
1. 校验已由命令层做（job 层信任）；节点装载：按 node_ids 顺序查 nodes（缺失节点 → 该节点结果 error="节点不存在"，不中断）。
2. `sidecar::ensure_started` 失败 → Err（job failed）。
3. 并发窗口：节点按 `nodes.chunks(concurrency as usize)` 分块，块内用 `tokio::task::JoinSet` 并发执行、`join_next()` 收集；块间检查 cancel（置位 → 未开始的块跳过）。**不新增 futures 依赖**（Cargo.toml 现无 futures）。
4. 测一个：put_proxy(id, node_proxy_json(node, id)) → delay(gstatic 204, 5000) → 成功再 download_through(mixed_port, 10MB, 30s, cancel)（延迟失败仍记录 latency=NULL + 错误，跳过带宽）→ delete_proxy → `repos::nodes::update_speed` → emit `JobProgress{ job_id, kind:"speed", step:"node", message: node_id, progress: 完成数, error: 单节点错误或 None }`。
5. 常量：`pub const DELAY_URL`, `pub const SPEED_TEST_URL: &str = "https://speed.cloudflare.com/__down?bytes=10485760"; pub const DOWNLOAD_TIMEOUT: Duration = 30s; pub const SIZE_LIMIT: usize = 10*1024*1024;`
6. 全部完成 → Ok；cancel 置位 → 在途下载 Err("已取消")、未开始跳过、返回 Err("已取消")。

**不变量测试（speed/tests.rs，FakeEngine + tokio::time::pause）:**
- 并发窗口：FakeEngine 记录同时活跃数，3 并发下 max≤3；6 节点全完成。
- 部分失败隔离：1 节点 delay 失败 → 该节点 latency/带宽 NULL + 错误入库，其余成功，job Ok。
- 取消：download 挂起时置 cancel → job Err("已取消")，未开始节点不被测（FakeEngine 计数）。
- 超时：download 不返回 + pause 推进 30s → 该节点错误「下载超时」，job 不挂死。
- 缺节点 id：结果 error="节点不存在"。
- 事件顺序：事件数 == 节点数，message=node_id。

ProdRunner 加分支：`JobRequest::SpeedTest(r) => speed::run_speed_job(&self.state()?, r, cancel, emit).await`（state 经现有 OnceLock 获取——照 LinkBuild 分支写法）。

- [ ] **Step 1: 先写不变量测试（红灯）** → **Step 2-3: 实现** → **Step 4: `cargo test` 全量 + clippy**
- [ ] **Step 5: 提交** `feat(p4): 测速执行器与 job 队列集成（并发窗口/取消/超时不变量）`

## Task 6: 命令层 + IPC

**Files:** Modify `src-tauri/src/commands.rs`、`src-tauri/src/main.rs`（注册 speed_test_cmd）、`src-tauri/src/ipc_tests.rs`

**Interfaces:**
- Produces:
  - `pub fn speed_test(state: Arc<AppState>, node_ids: Vec<String>, concurrency: u8) -> R<i64>`：校验 node_ids 非空（去重保序）、concurrency 1..=5、所有节点存在（任一缺失 → Err 列名）→ jobs 行 kind='speed' queued → submit（失败回写 failed 并 Err——照 teardown_link 模式）。
  - `#[tauri::command] pub async fn speed_test_cmd(state: tauri::State<'_, Arc<AppState>>, node_ids: Vec<String>, concurrency: u8) -> R<i64>`——委托 `speed_test(state.inner().clone(), ...)`，照 start_deploy/start_deploy_cmd 双层模式。

- [ ] **Step 1: 写失败测试**
  - commands_tests：空 ids Err；concurrency 0/6 Err；缺节点 Err 不产生 job 行；成功 → job 行 kind='speed' 且 queued。
  - ipc_tests：`invoke_ok("speed_test_cmd", json!({"nodeIds":[...], "concurrency":3}))` → job_id>0 且 kind='speed'；`concurrency: 9` → Err 文本含并发范围；mock_builder 注册 speed_test_cmd。
- [ ] **Step 2-4: 红灯→实现→全量测试**
- [ ] **Step 5: 提交** `feat(p4): speed_test_cmd 命令与真实 IPC 线格式测试`

## Task 7: 前端 UI + 打包 + 文档

**Files:** Modify `src/api/types.ts`（Node + `last_check_error: string | null`）、`src/api/index.ts`（`speedTest(nodeIds: string[], concurrency: number)`，节点 id 为字符串）、`src/views/Nodes.tsx`（节点视图，测试 `src/views/Nodes.test.tsx`）、`scripts/fetch-sidecar.mjs`（Create）、`tauri.conf.json`、`.gitignore`、`package.json`（fetch:sidecar 脚本 + build 前置）、`README.md`、`docs/`（spec 已存在；同步 README 节点页说明）

**UI 行为:**
- 节点行尾部：延迟（ms）/带宽（MB/s，`formatBytes` 风格）/上次测速时间；`last_check_error` 红字展示。
- 行内「测速」按钮（busy 态「测速中…」）。
- 工具栏：「批量测速」（默认全部启用节点，含选中行时只测选中）→ `speedTest(ids, concurrency)`；并发下拉 1/3/5 默认 3；进行中显示「取消测速」（cancelJob）。
- 订阅 `job://progress` kind='speed'：按 message=node_id 更新行内结果（完成节点即时显示），终态（progress=100 或 error）后刷新节点列表拉持久化结果。
- 组件测试（Vitest）：单发调用参数、busy 态、批量默认 ids、并发选择、事件驱动行更新、取消调用 cancelJob、失败原因展示。
- 全部按钮禁用态；无 emoji。

**打包:**
- `scripts/fetch-sidecar.mjs`：下载 `https://github.com/MetaCubeX/mihomo/releases/download/v1.19.10/mihomo-windows-amd64-v1.19.10.zip`（`MIHOMO_MIRROR_URL` 环境变量可换源前缀，`MIHOMO_VER` 可换版本——默认 v1.19.10，两处哈希常量与默认版本匹配并注释来源）→ 校验 zip sha256 == `22f301591e522e81ecbd4ae2987b2fcb4642c234c870094904188c4dc1aceb60`（GitHub API assets digest）→ `powershell Expand-Archive` 解压到临时目录 → 内部文件 `mihomo-windows-amd64.exe` 校验 sha256 == `a4feef7e4f8cbaf19e2079b3caa3e23821dfe419ad2e39e0539b843bd2d4d1be`（2026-08-14 本机 sha256sum，与 sidecar.rs `SIDECAR_EXE_SHA256` 同值）→ 重命名为 `mihomo.exe` 落盘 `src-tauri/binaries/mihomo.exe` → 清理临时目录；任一步校验失败删除半成品并退出非零。
- `tauri.conf.json`：`bundle.resources: ["binaries/mihomo.exe"]`。
- `package.json`：`"fetch:sidecar": "node scripts/fetch-sidecar.mjs"`；`build` 前置该命令。
- 检查现有 `check:package` 脚本并加断言（resources 文件存在、大小 > 1MB）。
- `.gitignore` 加 `src-tauri/binaries/`。

- [ ] **Step 1: 组件测试先写（红灯）** → **实现** → `npm test` + `npx tsc --noEmit`
- [ ] **Step 2: fetch-sidecar.mjs 写好并执行一次验证下载/校验流程（若网络受限：留脚本 + 文档说明，check:package 对该文件缺失给可读错误而非静默）**
- [ ] **Step 3: 文档同步（README 节点页测速说明）**
- [ ] **Step 4: 提交** `feat(p4): 节点页测速 UI + sidecar 打包集成 + 文档`

## 终审

全部任务后：`cargo test`、`cargo clippy`、`cargo clippy --tests`、`cargo fmt --check`、`npm test`、`npx tsc --noEmit`、`npm run build`（含 fetch:sidecar）、`npm run check:package`；真实 App 手工验收（启动 → 单发测速一个节点 → 结果展示 → 批量 + 取消）。
