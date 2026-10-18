# vpn-ctl P4 测速设计规格

> 日期：2026-08-14。P1 骨架 / P2 服务器 / P3 中转已完成（master 097ca41）。

## 1. 范围与验收

- 节点页单节点测速 + 批量测速（启用节点全集或勾选集），延迟 + 带宽两项指标。
- 测速结果实时刷新（按节点推送），失败展示原因；结果持久化到 nodes 行。
- 测速任务可取消；App 退出时 sidecar 与任务全部清理。
- **验收**：批量测速 50 节点在 3 并发下全程可观察、可取消、可重测；失败节点原因可见；重启后上次结果仍在。

## 2. 已确认决策（老板拍板 2026-08-14）

| 项 | 决策 |
|---|---|
| mihomo 获取 | **打包内置**：仓库不提交二进制；构建时脚本从官方 releases 下载指定版本 + sha256 校验；包体 +约 15MB |
| 并发 | 用户可选 1–5，默认 3；批量与单发都支持 |
| 带宽方法 | 经 mihomo 混合端口代理下载固定 10MB 测试文件实测 bytes/s，30s 超时，可取消 |
| 延迟方法 | mihomo REST API `/proxies/{name}/delay`（HEAD http://www.gstatic.com/generate_204） |
| sidecar 生命周期 | 首次测速懒启动；App 退出即停；端口随机 + 仅绑 127.0.0.1 + 随机 token（照现有订阅服务模式） |

## 3. 架构

```
src-tauri/src/speed/
  mod.rs        # 测速域服务：提交任务、事件、结果写库的编排入口
  sidecar.rs    # mihomo 二进制定位/复制/启动/停止、config.yaml 渲染、端口/token 分配
  engine.rs     # SpeedEngine trait + MihomoEngine（REST 客户端 + 代理下载）
  engine_tests.rs
  sidecar_tests.rs
  tests.rs
jobs: JobRequest::SpeedTest(SpeedTestReq)  # kind='speed'，复用现有队列/取消
events: job://progress 复用（kind='speed'，message 带 node_id/指标/错误）
db:  v4 迁移 nodes ADD COLUMN last_check_error TEXT
前端: Nodes 页 + api/speed 包装 + SpeedTest 事件订阅
scripts/fetch-sidecar.mjs   # 下载 mihomo + sha256 校验 → src-tauri/binaries/
```

- **SpeedEngine trait**（可测试性核心）：`delay(name, url, timeout) -> Result<i64>`、
  `download_through(name, url, size_limit, timeout, cancel) -> Result<f64>`（bytes/s）。
  `MihomoEngine` 实现走真 HTTP；测试用 `FakeEngine`。二进制行为只在打包验证接触。

## 4. 数据模型

```sql
-- v4
ALTER TABLE nodes ADD COLUMN last_check_error TEXT;
```

nodes.latency（ms，NULL=未测/失败）、nodes.bandwidth（bytes/s，NULL 同）、
nodes.last_checked_at（rfc3339）、last_check_error（失败原因，成功时置 NULL）。

## 5. 核心流程

### 5.1 批量/单发提交

- `speed_test_cmd(node_ids: Vec<String>, concurrency: u8)` → 校验（1..=5、节点存在、
  非空）→ 建 jobs 行（kind='speed'，status=queued）→ 入队 JobRequest::SpeedTest。
- worker 顺序：确保 sidecar 运行（懒启动，失败则 job 落 failed 并给出可读原因）→
  按 concurrency 分窗口并发测 → 每节点完成即写库 + 发事件 → 全部完成 job 落 done。

### 5.2 单节点测速

1. `POST /proxies/{id}` 注入节点（clash 格式参数来自 nodes.params_json/addr/port——复用
   现有 YAML 渲染的节点定义逻辑，保持单测速与导出配置的节点形态一致）。
2. 延迟：`GET /proxies/{id}/delay?url=…&timeout=5000`，失败 → latency NULL + 错误文本。
3. 带宽：sidecar mixed-port 起 HTTP 代理，`GET https://speed.cloudflare.com/__down?bytes=10485760`
   经代理下载，30s 超时/取消感知；成功 bytes/s 写库，失败 → bandwidth NULL + 错误文本。
4. `DELETE /proxies/{id}` 清理。
5. 结果写库 + 事件 `job://progress {kind:'speed', job_id, step:'node', message:'<node_id>',
   progress: n/total}`；失败原因经 message 里 JSON 或专用 error 字段——沿用 JobProgress
   shape，error 字段放单节点错误文本，前端按 node_id 定位行。

### 5.3 取消

- 复用 `cancel_job_cmd`：worker 的 cancel flag + 正在进行的下载流 abort。
- sidecar 不因单任务取消而停（可继续测其余/重试）；App 退出时统一停。

### 5.4 sidecar 生命周期

- 启动：解析安装目录资源 `mihomo.exe`（打包内置）→ 复制到数据目录 `sidecar/mihomo.exe`
  （已存在且 sha256 匹配则跳过）→ 生成 `sidecar/config.yaml`（external-controller:
  127.0.0.1:{随机端口}，secret {随机 token}，mixed-port {随机端口}，proxies: []）→
  子进程启动 → 等待控制器 HTTP 就绪（健康轮询 ≤5s）→ 挂到 AppState。
- 停止：App 退出（drop/清理钩子）kill 子进程。
- 安全：仅 127.0.0.1 + 随机 token；控制器请求带 `Authorization: Bearer {token}`。

## 6. 命令与事件

| 命令 | 签名 | 说明 |
|---|---|---|
| `speed_test_cmd` | `(node_ids, concurrency) -> job_id` | 批量/单发统一入口 |
| `cancel_job_cmd` | 已有 | 取消测速任务 |

事件复用 `job://progress`：kind='speed'，`message` 为 node_id，`error` 为单节点失败原因，
`progress` 为完成数。

## 7. 测试策略

- engine：FakeEngine 全链路（延迟成功/失败、带宽成功/超时/取消、并发窗口调度、部分失败
  不阻塞整体、写库与事件断言）。
- sidecar：纯函数部分（config.yaml 渲染、端口/token 分配、版本校验）单测；
  真二进制启动/停止进打包验证与手工验收。
- 取消/超时/并发窗口为行为不变量测试：真实入口驱动、枚举输入空间、红灯验证。
- IPC：tauri::test 真实线格式（camelCase nodeIds/concurrency）。
- 前端：Nodes 页组件测试（单发 busy/结果、批量提交/取消、事件驱动行更新、并发选择）。

## 8. UI

节点页（Nodes 视图）：
- 每行尾部：延迟 ms / 带宽 MB/s / 上次测速时间 / 失败原因（红字）。
- 行内「测速」按钮（busy 态）；工具栏「批量测速」（默认全部启用节点）+ 并发下拉
  （1/3/5，默认 3）+ 「取消测速」（任务进行中显示）。
- kind='speed' 事件驱动行状态实时更新；任务终态后刷新列表拉取持久化结果。
- 无 emoji；错误带原因；所有按钮有 busy/禁用态。

## 9. 打包

- `scripts/fetch-sidecar.mjs`：下载指定 mihomo 版本（windows-amd64，默认 v1.19.10）到
  `src-tauri/binaries/mihomo.exe`，sha256 校验；`npm run fetch:sidecar` 手动触发，
  `build` 前自动执行。
- `tauri.conf.json` `bundle.resources` 引入 `binaries/mihomo.exe`；
  `check:package` 断言资源存在。
- 仓库 `.gitignore` 忽略 `src-tauri/binaries/`。

## 10. 风险与边界

- 带宽测试产生真实流量（每节点 10MB）：失败或超时也算一次流量，UI 明示。
- 国内网络下载 mihomo 可能慢：fetch 脚本支持 `MIHOMO_MIRROR_URL` 环境变量换源。
- 测速期间勿同时跑大流量任务（下载/导出）——不强制，README 提示。
