# P7 实施计划：交付面扩展（ssr/hysteria2 解析 + sing-box 导出）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 ssr/hysteria2 URI 解析缺口（订阅含 ssr 行不再整单失败），新增 sing-box 全协议导出，Clash/sing-box 双导出对 ssr 节点跳过+提示。

**Architecture:** 解析层（subs/parse.rs）新增两个 URI 分支归一化入 ParsedNode；导出层新增 config/singbox.rs 与 Clash 导出对称；ssr 跳过策略在 config 层集中（build_yaml 返回跳过计数），rebuild_config 双缓存（yaml + singbox json + skipped 计数）。

**Tech Stack:** Rust（serde_json/serde_yaml/url/base64 已有）、React 19 + Tauri 2 IPC。

**规格：** `docs/superpowers/specs/2026-08-14-vpn-ctl-p7-export-design.md`（老板已扫、官方文档核实字段）

## Global Constraints

- 无 emoji（UI 铁律）；错误必须可读中文且带原因；凭据不进日志/错误文本。
- 修改公共导出/签名前 `rg` 搜索定义、引用、测试、mock 并全部同步。
- 每个功能有针对性测试；只 mock 外部依赖，不 mock 被测模块自身。
- 单文件 ≤1000 行；生产源码无 TODO/console.log/debugger。
- sing-box type 名以官方为准：`shadowsocks`/`vmess`/`vless`/`trojan`/`hysteria2`；ssr 双生态已移除（mihomo + sing-box 1.6+）→ 跳过+提示，禁止映射为 ss。
- SSR URI 外层 base64 无填充惯例；查询参数内层 base64；remarks 空回落「未命名节点」。
- 中文模块说明随文件修改同步。

---

### Task 1: ssr:// 与 hysteria2:// URI 解析

**Files:**
- Modify: `src-tauri/src/subs/parse.rs`（parse_uri 新增两分支）
- Test: `src-tauri/src/subs/parse_tests.rs`

**Interfaces:**
- Consumes: `ParsedNode` 结构（parse.rs 内定义，不变）、`percent_decode`、`decode_ss_userinfo`、`err()` 闭包模式
- Produces: `ParsedNode { protocol: "ssr" | "hysteria2", params: {...} }`——Task 2/3 消费

- [ ] **Step 1: 写失败测试**

`ssr://` 金样（真实样例，base64 无填充）：构造 `ssr://host:443:auth_sha1_v4:chacha20:tls1.2_ticket_auth:base64(password)/?obfsparam=...&protoparam=...&remarks=...&group=...`（外层整体 base64）；断言 protocol="ssr"、addr/port、params 全字段（protocol/method/obfs/password/obfsparam/protoparam）、remarks 为节点名；remarks 缺失回落「未命名节点」；坏 base64 → 可读错误含「无法解析 URI」。

`hysteria2://password@host:443/?insecure=1&sni=example.com&obfs=salamander&obfs-password=pw#name` 断言 protocol="hysteria2"、password/uuid 之外 params 含 query 全量（insecure/sni/obfs/obfs-password）、name 解码正确。

整单回归：`parse_uri_list` 混合 vmess+ssr+hysteria2 行全成功（此前 ssr 行使整单失败的回归钉死）。

- [ ] **Step 2: 运行确认失败**（cargo test subs::parse_tests——新测试红）
- [ ] **Step 3: 实现**

parse_uri 内新增分支（在 ss:// 分支后）：
1. `ssr://`：strip 前缀 → `decode_ss_userinfo`（URL_SAFE → URL_SAFE_NO_PAD 回退）解外层 → `host:port:protocol:method:obfs:base64pw`（`split('?')` 分主体与查询；主体 `rsplit_once(':')` 出 port，再逐段 split 出 host/protocol/method/obfs/base64pw——共 5 段，`splitn(6, ':')` 校验段数≥6 否则 err）；password = 内层 base64 解码（同引擎回退，失败给可读错）；查询参数逐个 `split_once('=')`、key 值 base64 解码（解码失败用原值）；remarks → name；params 插 protocol/method/obfs/password/obfsparam/protoparam。
2. `hysteria2://`：并入现有 trojan/vless url 解析路径（`uri.starts_with("hysteria2://")` 加入条件，protocol 三分支），userinfo 作 password 键。

- [ ] **Step 4: 运行确认通过**（cargo test subs::parse_tests + 全量 cargo test）
- [ ] **Step 5: Commit** `feat(p7): ssr:// 与 hysteria2:// URI 解析`

---

### Task 2: ssr 跳过策略（Clash 导出 + 测速面）

**Files:**
- Modify: `src-tauri/src/config/mod.rs`（build_yaml 签名 + ssr 跳过）
- Modify: `src-tauri/src/subs/service.rs`（rebuild_config 调用方）
- Modify: `src-tauri/src/speed/mod.rs`（node_proxy_json 调用方——ssr 节点测速跳过）
- Modify: `src-tauri/src/state.rs`（新增 config_singbox 缓存 + skipped_ssr 计数——Task 3 备用，本任务只加 skipped_ssr）
- Test: `src-tauri/src/config/tests.rs`、`src-tauri/src/subs/service_tests.rs`

**Interfaces:**
- Consumes: Task 1 的 ParsedNode（protocol "ssr"）
- Produces: `build_yaml(nodes) -> Result<(String, usize), ConfigError>`（yaml + 跳过的 ssr 数）；`AppState.skipped_ssr: usize`；`SubService.rebuild_config()` 填充之

- [ ] **Step 1: 写失败测试**：含 ssr 节点的列表 build_yaml → yaml 无 ssr 项 + 计数 1；无 ssr → 计数 0；`node_proxy_json` 对 ssr 返回专有错误（测速跳过语义）
- [ ] **Step 2: 运行确认失败**
- [ ] **Step 3: 实现**：build_yaml 内 node_mapping 前先判 `n.protocol == "ssr"` → 计数+跳过（不报错）；返回值改元组；service.rs rebuild_config 存 yaml + skipped_ssr 到 state；speed/mod.rs 对 node_proxy_json 的 ssr 错误 → 跳过该节点测速（结果标记「mihomo 不支持 SSR」）；state.rs 加 `pub skipped_ssr: usize`（RwLock 或随 config 缓存同锁——按现状结构定）
- [ ] **Step 4: 全量 cargo test 通过 + clippy + fmt**
- [ ] **Step 5: Commit** `feat(p7): ssr 节点双导出与测速跳过策略`

---

### Task 3: sing-box 导出模块

**Files:**
- Create: `src-tauri/src/config/singbox.rs`
- Modify: `src-tauri/src/config/mod.rs`（`pub mod singbox;`）
- Modify: `src-tauri/src/subs/service.rs`（rebuild_config 同步重建 singbox 缓存）
- Modify: `src-tauri/src/state.rs`（config_singbox 缓存——如 Task 2 未加则此处加）
- Test: `src-tauri/src/config/singbox_tests.rs` 或并入 config/tests.rs（新建 `src-tauri/src/config/singbox_tests.rs`）

**Interfaces:**
- Consumes: `Node`（domain）、`ConfigError`（复用或本模块定义 SingboxError——复用 ConfigError）
- Produces: `pub fn build_singbox(nodes: &[Node]) -> Result<(String, usize), ConfigError>`（json + skipped_ssr 数）

- [ ] **Step 1: 写失败测试**（金样按官方字段，2026-08-14 经 context7 核实）：
  - ss 节点 → outbound `type: "shadowsocks"` server/server_port/method/password
  - vmess → uuid/security/alter_id；vless → uuid/flow；trojan → password；hysteria2 → password/obfs{type,password}/tls{enabled,server_name,insecure}/up_mbps/down_mbps（params 有则用，无则默认 100）
  - clash_raw 来源（yaml 片段 `{type: ss, server, port, cipher, password}`）→ 映射为 shadowsocks 出站（cipher→method）
  - 含 ssr → 跳过 + 计数
  - 顶层结构断言：log/dns/inbounds（mixed 127.0.0.1:7890）/outbounds/selector+urltest 组/route.final；JSON 经 serde_json 可解析
  - 未知协议 → 可读错误
- [ ] **Step 2: 运行确认失败**
- [ ] **Step 3: 实现**：build_singbox 按规格 §2.4；节点按名称排序（与 build_yaml 一致）；tag = 节点名；选择组 tag「自动选择」+ urltest 组 tag「自动选择」的 urltest（与 Clash 导出同构：selector「自动选择」指向 urltest 组）
- [ ] **Step 4: 全量 cargo test + clippy + fmt**
- [ ] **Step 5: Commit** `feat(p7): sing-box 全协议导出模块`

---

### Task 4: 命令层与前端（导出 sing-box）

**Files:**
- Modify: `src-tauri/src/commands.rs`（`export_singbox` + `export_singbox_cmd`；export_yaml 返回提示时附 skipped 提示）
- Modify: `src-tauri/src/ipc_tests.rs` 或 `commands_tests.rs`（新命令测试）
- Modify: `src/api/index.ts`（`exportSingbox()`）
- Modify: `src/views/Settings.tsx`（「导出 sing-box」按钮 + skipped 提示展示）
- Modify: `src/views/Settings.test.tsx`

**Interfaces:**
- Consumes: Task 2/3 的 state 缓存（config_singbox/skipped_ssr）
- Produces: IPC `export_singbox_cmd` → `Option<String>`（保存路径，取消 None）——与 export_yaml_cmd 对称

- [ ] **Step 1: 写失败测试**：commands_tests 构造含 ssr 的节点 → export_singbox 返回路径且缓存 JSON 可解析；Settings.test 点「导出 sing-box」→ 显示已导出路径；skipped>0 时导出消息含提示
- [ ] **Step 2: 运行确认失败**
- [ ] **Step 3: 实现**：commands.rs 复制 export_yaml 模式（rfd 对话框 filter JSON、文件名 yunyi-singbox.json——品牌名只用于用户可见文件名，内部路径不变；spawn_blocking 写盘）；前端 Settings.tsx 配置导出区加按钮与消息（无 emoji）；api/index.ts 加函数
- [ ] **Step 4: cargo test + vitest（Settings）+ tsc 全绿**
- [ ] **Step 5: Commit** `feat(p7): sing-box 导出命令与前端入口`

---

### Task 5: 文档同步（R0）

**Files:**
- Modify: `README.md`（P7 功能节）
- Modify: 规格状态行（已批准）

- [ ] README P7 节：ssr 修复说明、sing-box 导出说明、双导出 ssr 跳过提示
- [ ] Commit `docs(p7): README 与规格状态同步`

---

## 验收（全部任务完成后）

1. 订阅导入含 ssr/hysteria2 行成功落库（回归：此前整单失败）
2. Clash 导出与 sing-box 导出双路径含 ssr 节点 → 跳过 + 计数提示
3. sing-box 导出的 config.json 可被 `jq`/serde 解析且字段与官方文档一致
4. CI（P6）全绿；红灯验证已做（P6 验收）
