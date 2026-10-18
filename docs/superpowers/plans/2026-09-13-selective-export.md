# 导出选中节点 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 「设置 → 配置导出」能挑具体哪几条线路写进文件，而不是只能整份导出。

**Architecture:** 不新写配置生成逻辑——`build_yaml` / `build_singbox` 本就是纯函数，用**与 `rebuild_config` 完全相同的输入组合**调它们、只换节点列表，导出结果与「开启网络」所用配置天然一致。可见性判定抽成 `subs::SubService::visible_nodes` 单一口径，清单与导出共用。旧的整份导出命令保留不动（订阅服务下发走缓存那条路）。

**Tech Stack:** Rust（Tauri 2 backend）、rusqlite、serde_yaml、React 18 + TypeScript + Vitest、`mockIPC`。

**Spec:** `docs/superpowers/specs/2026-09-13-selective-export-design.md`

## Global Constraints

- **生成逻辑不许复制**。导出子集必须调用 `config::build_yaml` / `config::singbox::build_singbox`，不得新写一份映射。两套路径漂移是本设计要防的头号风险，Task 3 有一条断言专门盯它。
- **可见性只有一份实现**：`visible_nodes`。清单命令与导出命令都必须走它，禁止在任一处重现「节点启用 + 订阅启用」的判定。
- **空选集是错误**，不是「全部」：`Err("请至少选择一个节点")`。只有 `node_ids` 为空（`&[]`）才表示「全部可见节点」=旧行为。
- **旧命令 `export_yaml_cmd` / `export_singbox_cmd` 不动**，既有测试与订阅服务下发依赖它们。
- **暂存必须逐个列路径**（`git add <path>` 只影响指定文件），但注意：`src-tauri/src/subs/service.rs` 带着上一次会话的**未提交改动**，本任务会与它同文件——提交时必须在提交信息里说明，不要声称该文件只有本次改动。
- 注释、错误文案、提交信息一律**中文**。
- Rust 测试命令必须带 manifest（bin crate，`--lib` 不可用）：`cargo test --manifest-path src-tauri/Cargo.toml <filter>`；**只接受一个位置过滤参数**。
- 前端门禁：`npx tsc --noEmit` + `npm test`。

## 关键既有事实（实现前必读，均已核对源码）

1. **两个 builder 都是纯函数**：`config::build_yaml(nodes: &[Node], smart: bool, rules: &[RelayRule], servers: &[Server]) -> Result<(String, usize), ConfigError>`（`config/mod.rs:67`）；`config::singbox::build_singbox(nodes: &[Node]) -> Result<(String, usize), ConfigError>`（`singbox.rs:38`）。`usize` 是跳过的 ssr 数。二者**内部各自按 `n.enabled` 再过滤一次并排序**。
2. **`rebuild_config` 的输入组合**（`subs/service.rs:216-249`）＝ `visible_nodes` + `settings` 表的 `export_smart_split`（缺省视为开）+ `relay::list_relay_rules_inner(&self.db)` + `repos::servers::list()`。导出必须照抄这组输入。
3. **导出守卫**：`ensure_exportable_has_nodes(yaml)`（`commands.rs:274`，查 `proxies` 非空）与 `ensure_singbox_has_nodes(json)`（`:290`，查 `outbounds` 里有非 selector/urltest 的 tag）。两者都在 `save_export` 里作为 `guard` 调用，报错文案是 `EXPORT_EMPTY_HINT`。
4. **`save_export(content, skipped_ssr, guard, dialog)`**（`commands.rs:233`）负责弹对话框 + 写盘 + 组装 `ExportOutcome { path, skipped_ssr }`；取消返回 `Ok(None)`。
5. **前端测试夹具**：`Settings.test.tsx` 顶部用 `mockIPC` 拦命令（未 mock 的会 `throw new Error(\`未 mock 的命令: ${cmd}\`)`），现有导出用例在 `:78`（YAML）、`:89`（sing-box）、`:99`（ssr 计数），三个都直连 `export_*_cmd`，本次要改造为先过弹窗。
6. **弹窗样式先例**：`src/components/PeerConfigDialog.tsx`（`useEffect` 挂载时拉数据、`error` 态、`onClose` 回调）。
7. **`state.skipped_ssr` 是缓存路径的计数**（`AtomicUsize`），导出子集不能用它——子集的跳过数必须来自本次 `build_*` 的返回值。

---

### Task 1: `subs::SubService::visible_nodes` 抽出口径

**Files:**
- Modify: `src-tauri/src/subs/service.rs`（`rebuild_config` 在 `:216`）
- Test: `src-tauri/src/subs/service_tests.rs`（文件末尾追加）

**Interfaces:**
- Consumes: `repos::nodes::list(&conn, &creds)`、`repos::subs::list(&conn)`。
- Produces: `pub fn visible_nodes(&self) -> Result<Vec<Node>, SubError>`——「节点启用**其订阅也**启用；`sub_id=None` 恒可见」。Task 2/3 依赖它。

- [ ] **Step 1: 写失败的测试**

在 `src-tauri/src/subs/service_tests.rs` 末尾追加（该文件已有建订阅/节点的夹具，若无则照 `commands_tests.rs` 的 `test_state()` 写法就地建）：

```rust
#[tokio::test]
async fn visible_nodes_scopes_to_enabled_subscriptions() {
    use crate::db::repos;
    let (_d, state) = /* 该文件既有的 state 夹具 */;
    let sub = crate::domain::Subscription {
        id: "s1".into(),
        name: "机场A".into(),
        url: "https://example.com/a".into(),
        enabled: true,
        last_sync_at: None,
        sync_interval: 86400,
        last_sync_error: None,
        filter_json: r#"{"include":[],"exclude":[]}"#.into(),
    };
    repos::subs::insert(&state.db.lock(), &sub).expect("sub");
    let mk = |id: &str, sub_id: Option<&str>, enabled: bool| crate::domain::Node {
        id: id.into(),
        name: format!("节点{id}"),
        source: "import".into(),
        protocol: "ss".into(),
        addr: "1.2.3.4".into(),
        port: 8388,
        params_json: r#"{"password":"pw"}"#.into(),
        group_name: "默认分组".into(),
        sub_id: sub_id.map(str::to_string),
        relay_rule_id: None,
        enabled,
        latency: None, bandwidth: None, last_checked_at: None, last_check_error: None,
        direct_latency: None, hop_relay_ms: None, hop_landing_ms: None, hop_tunnel_ms: None,
    };
    for n in [
        mk("in-sub", Some("s1"), true),      // 订阅启用 + 节点启用 → 可见
        mk("node-off", Some("s1"), false),   // 节点禁用 → 不可见
        mk("self", None, true),              // 自建（无订阅）→ 恒可见
        mk("self-off", None, false),         // 自建但禁用 → 不可见
    ] {
        repos::nodes::upsert(&state.db.lock(), &n, &state.creds).expect("node");
    }
    let visible: Vec<String> = state
        .subs
        .visible_nodes()
        .expect("visible")
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert!(visible.contains(&"in-sub".to_string()));
    assert!(visible.contains(&"self".to_string()));
    assert!(!visible.contains(&"node-off".to_string()));
    assert!(!visible.contains(&"self-off".to_string()));

    // 订阅禁用 → 其节点随之不可见；自建不受影响
    repos::subs::update_enabled(&state.db.lock(), "s1", false).expect("disable sub");
    let visible2: Vec<String> = state
        .subs
        .visible_nodes()
        .expect("visible2")
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert!(!visible2.contains(&"in-sub".to_string()), "禁用订阅的节点必须不可见");
    assert!(visible2.contains(&"self".to_string()));
}
```

若 `repos::subs::update_enabled` 不存在，改用 `repos::subs::update` 传 `enabled: false` 的整行。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml visible_nodes_`
Expected: 编译失败，`no method named 'visible_nodes' found`

- [ ] **Step 3: 写实现**

在 `src-tauri/src/subs/service.rs` 的 `rebuild_config` **之前**插入，并把 `rebuild_config` 里那段过滤改成调用它：

```rust
    /// 「哪些节点进配置」的唯一口径：**节点启用（`build_*` 内部还会再过滤一次）且其订阅启用**；
    /// `sub_id=None`（自建/手工）恒可见。rebuild_config 与导出选中共用，避免两处判定漂移。
    pub fn visible_nodes(&self) -> Result<Vec<Node>, SubError> {
        let nodes = repos::nodes::list(&self.db.lock(), &self.creds)
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let enabled_subs: HashSet<String> = repos::subs::list(&self.db.lock())
            .map_err(|e| SubError::FetchFailed(e.to_string()))?
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.id)
            .collect();
        Ok(nodes
            .into_iter()
            .filter(|n| match &n.sub_id {
                Some(sid) => enabled_subs.contains(sid),
                None => true,
            })
            .collect())
    }
```

`rebuild_config` 开头改为：

```rust
    pub async fn rebuild_config(&self) -> Result<(), SubError> {
        let visible: Vec<Node> = self.visible_nodes()?;
        // 智能分流开关：设置表 export_smart_split（"1" 开，"0" 关，缺省视为开）
        let smart = repos::settings::get(&self.db.lock(), "export_smart_split")
            .map_err(|e| SubError::FetchFailed(e.to_string()))?
            .map(|v| v == "1")
            .unwrap_or(true);
        // 中转规则/服务器表：挂规则节点在导出为经中转机的地址（与上网客户端同口径）
        let relay_rules = crate::relay::list_relay_rules_inner(&self.db)
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let relay_servers = repos::servers::list(&self.db.lock())
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let (yaml, skipped_ssr) =
            crate::config::build_yaml(&visible, smart, &relay_rules, &relay_servers)
                .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        // ……以下保持原样（写两个缓存 + singbox 降级分支）
    }
```

**注意**：`rebuild_config` 里原来 `enabled_subs` 的 `HashSet` 导入若因此变成未使用，一并清理。

- [ ] **Step 4: 跑测试确认通过 + 无回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml subs::`
Expected: 新增用例 PASS，既有 subs 用例全 PASS（这一步是重构，行为必须完全不变）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/subs/service.rs src-tauri/src/subs/service_tests.rs
git commit -m "refactor(subs): 抽出 visible_nodes 统一「哪些节点进配置」的判定"
```

⚠️ 这两个文件此刻还带着上一次会话的未提交改动，提交信息里如实说明「含前次会话遗留改动」——不要只写上面那句。

---

### Task 2: `build_config_for` + 可导出清单命令

**Files:**
- Modify: `src-tauri/src/commands.rs`（导出区，`:312` 的 `export_yaml` 之前）
- Modify: `src-tauri/src/domain.rs`（新增 `ExportableNode`）
- Test: `src-tauri/src/commands_tests.rs`（末尾追加）

**Interfaces:**
- Consumes: Task 1 的 `state.subs.visible_nodes()`；`config::build_yaml` / `config::singbox::build_singbox`。
- Produces:
  - `enum ExportFormat { Yaml, Singbox }`（`commands.rs` 内，`#[derive(Debug, Clone, Copy, PartialEq)]`）
  - `async fn build_config_for(state: &Arc<AppState>, node_ids: &[String], format: ExportFormat) -> R<(String, usize)>`
  - `pub fn list_exportable_nodes(state: Arc<AppState>) -> R<Vec<ExportableNode>>`
  - `domain::ExportableNode { node: Node, exportable: bool, skip_reason: Option<String> }`（`#[derive(Debug, Clone, Serialize)]`）

- [ ] **Step 1: 写失败的测试**

`src-tauri/src/domain.rs` 先加类型（否则测试编不过）——见 Step 3 的类型定义，先写它。

`src-tauri/src/commands_tests.rs` 末尾追加：

```rust
#[tokio::test]
async fn build_config_for_filters_to_selected_ids() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "节点甲");
    seed_node(&state, "n2", "节点乙");
    let (yaml, skipped) =
        build_config_for(&state, &["n1".to_string()], ExportFormat::Yaml)
            .await
            .expect("build");
    assert_eq!(skipped, 0);
    assert!(yaml.contains("节点甲"));
    assert!(!yaml.contains("节点乙"), "未选中的节点不得进配置：{yaml}");
    // proxy-groups 成员同样只含选中项（按名字取「手动选择」组，不依赖组顺序）
    let v: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("yaml");
    let groups = v["proxy-groups"].as_sequence().expect("groups");
    let manual = groups
        .iter()
        .find(|g| g["name"].as_str() == Some("手动选择"))
        .expect("手动选择组");
    let members: Vec<&str> = manual["proxies"]
        .as_sequence()
        .expect("members")
        .iter()
        .filter_map(|m| m.as_str())
        .collect();
    assert!(members.contains(&"节点甲"));
    assert!(!members.contains(&"节点乙"));
}

#[tokio::test]
async fn build_config_for_empty_ids_means_all_visible() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "节点甲");
    seed_node(&state, "n2", "节点乙");
    let (all, _) = build_config_for(&state, &[], ExportFormat::Yaml)
        .await
        .expect("build all");
    let (one, _) = build_config_for(&state, &["n1".to_string()], ExportFormat::Yaml)
        .await
        .expect("build one");
    // 空 = 全部可见节点，且与整份导出（旧路径）逐字节一致
    assert!(all.contains("节点甲") && all.contains("节点乙"));
    state.subs.rebuild_config().await.expect("rebuild");
    let legacy = get_merged_config(state.clone()).await.expect("merged");
    assert_eq!(
        all, legacy,
        "空选集导出必须与 rebuild_config 的产物逐字节相同（两套路径不得漂移）"
    );
    assert_ne!(one, legacy, "选中子集必须真的比全量少");
}

#[tokio::test]
async fn list_exportable_nodes_marks_ssr_unexportable() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "正常节点");
    // ssr 节点：mihomo 与 sing-box 均已移除支持，必须标记为不可导出并带理由
    let mut ssr = seed_node(&state, "n2", "ssr节点");
    ssr.protocol = "ssr".into();
    crate::db::repos::nodes::upsert(&state.db.lock(), &ssr, &state.creds).expect("ssr");

    let list = list_exportable_nodes(state).expect("list");
    let ok = list.iter().find(|e| e.node.id == "n1").expect("n1");
    assert!(ok.exportable);
    assert!(ok.skip_reason.is_none());
    let bad = list.iter().find(|e| e.node.id == "n2").expect("n2");
    assert!(!bad.exportable, "ssr 必须标记为不可导出");
    assert!(bad.skip_reason.as_deref().unwrap_or("").contains("SSR"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml build_config_for_`
Expected: 编译失败，`cannot find function 'build_config_for'`

- [ ] **Step 3: 写实现**

`src-tauri/src/domain.rs` 末尾追加：

```rust
/// 导出候选节点：`exportable=false` 时 `skip_reason` 说明为什么导不出去。
/// 目前唯一不可导出的情况是 ssr（mihomo 与 sing-box 均已移除支持）。
#[derive(Debug, Clone, Serialize)]
pub struct ExportableNode {
    pub node: Node,
    pub exportable: bool,
    pub skip_reason: Option<String>,
}
```

`src-tauri/src/commands.rs` 导出区插入：

```rust
/// 导出目标格式。两种格式的生成逻辑与 rebuild_config 完全相同，只是节点列表可裁剪。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportFormat {
    Yaml,
    Singbox,
}

/// 按选中 id 生成配置与跳过计数。`node_ids` 为空 = 全部可见节点（等价旧行为）。
/// **输入组合照抄 rebuild_config**：可见节点 + smart 设置 + 中转规则 + 服务器表，
/// 否则导出结果会与「开启网络」用的配置漂移。
async fn build_config_for(
    state: &Arc<AppState>,
    node_ids: &[String],
    format: ExportFormat,
) -> R<(String, usize)> {
    let mut nodes = state.subs.visible_nodes().map_err(err)?;
    if !node_ids.is_empty() {
        // 不在可见集合里的 id 静默忽略：调用方的清单来自同一个 visible_nodes，
        // 出现差异说明中途有改动，以导出时刻的库状态为准
        nodes.retain(|n| node_ids.iter().any(|id| id == &n.id));
    }
    match format {
        ExportFormat::Yaml => {
            let smart = repos::settings::get(&state.db.lock(), "export_smart_split")
                .map_err(err)?
                .map(|v| v == "1")
                .unwrap_or(true);
            let rules = crate::relay::list_relay_rules_inner(&state.db).map_err(err)?;
            let servers = repos::servers::list(&state.db.lock()).map_err(err)?;
            crate::config::build_yaml(&nodes, smart, &rules, &servers).map_err(err)
        }
        ExportFormat::Singbox => {
            crate::config::singbox::build_singbox(&nodes).map_err(err)
        }
    }
}

/// 导出候选清单：与 `build_config_for` 共用 `visible_nodes`，所见即所得。
pub fn list_exportable_nodes(state: Arc<AppState>) -> R<Vec<crate::domain::ExportableNode>> {
    Ok(state
        .subs
        .visible_nodes()
        .map_err(err)?
        .into_iter()
        .map(|node| {
            let ssr = node.protocol == "ssr";
            crate::domain::ExportableNode {
                node,
                exportable: !ssr,
                skip_reason: ssr
                    .then(|| "mihomo 与 sing-box 已移除 SSR 支持".to_string()),
            }
        })
        .collect())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml build_config_for_`
再 Run: `cargo test --manifest-path src-tauri/Cargo.toml list_exportable_nodes_`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/domain.rs src-tauri/src/commands_tests.rs
git commit -m "feat(export): build_config_for 按选中 id 出配置 + 可导出清单命令"
```

---

### Task 3: `export_*_selected` 命令与 IPC 注册

**Files:**
- Modify: `src-tauri/src/commands.rs`（`export_singbox` 之后加核心函数 + `_cmd` 壳）
- Modify: `src-tauri/src/main.rs`（`generate_handler!`）
- Modify: `src-tauri/src/ipc_tests.rs`（`mock_app` 注册 + 用例）

**Interfaces:**
- Consumes: Task 2 的 `build_config_for`、`list_exportable_nodes`。
- Produces: IPC `list_exportable_nodes_cmd` / `export_yaml_selected_cmd(nodeIds)` / `export_singbox_selected_cmd(nodeIds)`。

- [ ] **Step 1: 写失败的测试**

`src-tauri/src/ipc_tests.rs` 的 `mock_app` 的 `generate_handler!` 里追加三行：

```rust
            crate::commands::list_exportable_nodes_cmd,
            crate::commands::export_yaml_selected_cmd,
            crate::commands::export_singbox_selected_cmd,
```

末尾追加用例（照 `server_throughput` 等既有用例的写法，导出需要注入 dialog 闭包，参考 commands_tests 里既有的导出测试注入方式——若 `export_yaml_selected` 的 dialog 参数不便经 IPC 注入，则本任务只测命令壳透传，核心断言放在 `commands_tests.rs`）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn list_exportable_nodes_cmd_ipc_returns_rows() {
    let (_d, state) = test_state();
    let (app, wv) = mock_app(state.clone());
    let v = invoke_ok(&wv, "list_exportable_nodes_cmd", json!({}));
    assert!(v.is_array(), "应为数组：{v}");
    drop(wv);
    let _ = app;
}
```

`src-tauri/src/commands_tests.rs` 末尾追加（**本任务最关键的断言**）：

```rust
#[tokio::test]
async fn export_yaml_selected_writes_only_selected_and_empty_selection_is_rejected() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "节点甲");
    seed_node(&state, "n2", "节点乙");
    let dir = tempfile::tempdir().expect("dir");
    let out = dir.path().join("out.yaml");

    // 只选 n1
    let path = out.clone();
    let r = export_yaml_selected(state.clone(), vec!["n1".into()], move || Some(path))
        .await
        .expect("export")
        .expect("写出");
    let written = std::fs::read_to_string(&r.path).expect("read");
    assert!(written.contains("节点甲"));
    assert!(!written.contains("节点乙"), "未选中的节点不得写进文件");

    // 空选集是错误，不是「全部」
    let e = export_yaml_selected(state.clone(), vec![], || Some(out.clone()))
        .await
        .expect_err("空选集必须被拒");
    assert!(e.to_string().contains("至少选择一个"), "错误文案：{e}");

    // 全选 = 与旧路径（缓存）逐字节相同
    state.subs.rebuild_config().await.expect("rebuild");
    let cached = get_merged_config(state.clone()).await.expect("merged");
    let path2 = dir.path().join("all.yaml");
    let p2 = path2.clone();
    let r2 = export_yaml_selected(
        state.clone(),
        vec!["n1".into(), "n2".into()],
        move || Some(p2),
    )
    .await
    .expect("export all")
    .expect("写出");
    assert_eq!(std::fs::read_to_string(&r2.path).expect("read2"), cached);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml export_yaml_selected`
Expected: 编译失败，`cannot find function 'export_yaml_selected'`

- [ ] **Step 3: 写实现**

`src-tauri/src/commands.rs` 的 `export_singbox` 之后插入：

```rust
/// 空选集守卫：空数组不等于「全部」，必须显式报错，否则用户以为筛了其实导了全量。
fn ensure_selection_non_empty(node_ids: &[String]) -> Result<(), AppError> {
    if node_ids.is_empty() {
        return Err(AppError::Message("请至少选择一个节点".into()));
    }
    Ok(())
}

/// 导出选中节点为 YAML（取消返回 None）。生成逻辑与整份导出共用 build_config_for。
pub async fn export_yaml_selected(
    state: Arc<AppState>,
    node_ids: Vec<String>,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    ensure_selection_non_empty(&node_ids)?;
    let (yaml, skipped_ssr) = build_config_for(&state, &node_ids, ExportFormat::Yaml).await?;
    save_export(yaml, skipped_ssr, ensure_exportable_has_nodes, dialog).await
}

/// 导出选中节点为 sing-box config.json（取消返回 None）。
pub async fn export_singbox_selected(
    state: Arc<AppState>,
    node_ids: Vec<String>,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    ensure_selection_non_empty(&node_ids)?;
    if let Some(reason) = state.config_singbox_error.read().await.clone() {
        return Err(AppError::Message(format!("sing-box 导出不可用：{reason}")));
    }
    let (json, skipped_ssr) = build_config_for(&state, &node_ids, ExportFormat::Singbox).await?;
    save_export(json, skipped_ssr, ensure_singbox_has_nodes, dialog).await
}
```

同文件的 `_cmd` 壳区（`export_singbox_cmd` 之后）：

```rust
#[tauri::command]
pub fn list_exportable_nodes_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<Vec<crate::domain::ExportableNode>> {
    list_exportable_nodes(state.inner().clone())
}

#[tauri::command]
pub async fn export_yaml_selected_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    node_ids: Vec<String>,
) -> R<Option<ExportOutcome>> {
    export_yaml_selected(state.inner().clone(), node_ids, || {
        native_save_dialog("YAML", &["yaml", "yml"], "vpn-ctl.yaml")
    })
    .await
}

#[tauri::command]
pub async fn export_singbox_selected_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    node_ids: Vec<String>,
) -> R<Option<ExportOutcome>> {
    export_singbox_selected(state.inner().clone(), node_ids, || {
        native_save_dialog("sing-box", &["json"], "yunyi-singbox.json")
    })
    .await
}
```

`src-tauri/src/main.rs` 的 `generate_handler!` 里，`commands::export_singbox_cmd,` 之后插入三行同名命令。

- [ ] **Step 4: 跑测试确认通过 + 全量后端**

Run: `cargo test --manifest-path src-tauri/Cargo.toml export_`
再 Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/main.rs src-tauri/src/ipc_tests.rs src-tauri/src/commands_tests.rs
git commit -m "feat(export): 选中导出命令与 IPC（空选集拒绝 + 全选等价旧路径）"
```

---

### Task 4: 前端 api 封装与选择弹窗组件

**Files:**
- Modify: `src/api/index.ts`（导出区）、`src/api/types.ts`（新增 `ExportableNode`）
- Create: `src/components/ExportSelectDialog.tsx`
- Test: 无独立用例（由 Task 5 的设置页用例覆盖），但需 `npx tsc --noEmit` 通过

**Interfaces:**
- Consumes: Task 3 的三个 IPC。
- Produces: `api.listExportableNodes()`、`api.exportYamlSelected(ids)`、`api.exportSingboxSelected(ids)`；`<ExportSelectDialog format onDone onClose />`。

- [ ] **Step 1: 写类型与 api**

`src/api/types.ts` 追加：

```ts
/** 导出候选节点：exportable=false 时 skip_reason 说明原因（目前仅 ssr） */
export interface ExportableNode {
  node: Node
  exportable: boolean
  skip_reason: string | null
}
```

`src/api/index.ts` 的 `exportSingbox` 之后追加：

```ts
/** 导出候选清单：与导出共用后端 visible_nodes，所见即所得 */
export function listExportableNodes(): Promise<ExportableNode[]> {
  return invoke('list_exportable_nodes_cmd')
}

/** 导出选中节点（nodeIds 为空是错误，前端不会走到） */
export function exportYamlSelected(nodeIds: string[]): Promise<ExportOutcome | null> {
  return invoke('export_yaml_selected_cmd', { nodeIds })
}

export function exportSingboxSelected(nodeIds: string[]): Promise<ExportOutcome | null> {
  return invoke('export_singbox_selected_cmd', { nodeIds })
}
```

`ExportableNode` 记得加进 `types.ts` 的 import 列表。

- [ ] **Step 2: 写组件**

`src/components/ExportSelectDialog.tsx`：

```tsx
// 导出选择弹窗：拉可导出清单、勾选后交给调用方导出。
// 不可导出的节点（ssr）置灰不可勾，避免「勾了却导不出去」。
import { useEffect, useState } from 'react'
import { listExportableNodes } from '../api'
import type { ExportableNode } from '../api/types'

interface Props {
  /** 弹窗标题里显示的格式名与导出按钮文案 */
  formatLabel: string
  onExport: (nodeIds: string[]) => Promise<void>
  onClose: () => void
}

export default function ExportSelectDialog({ formatLabel, onExport, onClose }: Props) {
  const [items, setItems] = useState<ExportableNode[]>([])
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const list = await listExportableNodes()
        if (!alive) return
        setItems(list)
        // 默认全勾可导出的；ssr 不勾
        setSelected(new Set(list.filter((e) => e.exportable).map((e) => e.node.id)))
      } catch (e) {
        // 不静默降级为「导出全部」：拉不到清单就让用户看见失败
        if (alive) setError(String(e))
      } finally {
        if (alive) setLoading(false)
      }
    })()
    return () => {
      alive = false
    }
  }, [])

  const toggle = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const handleConfirm = async () => {
    setBusy(true)
    setError(null)
    try {
      await onExport([...selected])
      onClose()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="rounded border border-neutral-700 bg-neutral-900 p-4 text-sm">
      <h4 className="mb-2 font-medium">选择要导出的节点（{formatLabel}）</h4>
      {loading && <p className="text-neutral-400">加载中…</p>}
      {error && (
        <p role="alert" className="mb-2 rounded bg-red-950 px-3 py-2 text-red-300">
          {error}
        </p>
      )}
      <ul className="mb-3 max-h-64 space-y-1 overflow-y-auto">
        {items.map((e) => (
          <li key={e.node.id} className="flex items-center gap-2">
            <input
              type="checkbox"
              aria-label={`选择 ${e.node.name}`}
              checked={selected.has(e.node.id)}
              disabled={!e.exportable}
              onChange={() => toggle(e.node.id)}
            />
            <span className={e.exportable ? '' : 'text-neutral-500'}>{e.node.name}</span>
            <span className="text-xs text-neutral-500">{e.node.protocol}</span>
            {!e.exportable && (
              <span className="text-xs text-amber-300">✗ {e.skip_reason ?? '不可导出'}</span>
            )}
          </li>
        ))}
      </ul>
      {!loading && items.length === 0 && !error && (
        <p className="mb-2 text-neutral-400">没有可导出的节点。</p>
      )}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={() => void handleConfirm()}
          disabled={busy || selected.size === 0}
          className="rounded bg-blue-600 px-3 py-1.5 text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? '导出中…' : `导出选中 ${selected.size}`}
        </button>
        <button
          type="button"
          onClick={onClose}
          disabled={busy}
          className="rounded bg-neutral-800 px-3 py-1.5 hover:bg-neutral-700 disabled:opacity-50"
        >
          取消
        </button>
      </div>
    </div>
  )
}
```

- [ ] **Step 3: 类型门禁**

Run: `npx tsc --noEmit`
Expected: 无输出

- [ ] **Step 4: 提交**

```bash
git add src/api/index.ts src/api/types.ts src/components/ExportSelectDialog.tsx
git commit -m "feat(export): 前端 api 与导出选择弹窗组件"
```

---

### Task 5: 设置页接线与测试

**Files:**
- Modify: `src/views/Settings.tsx`（导出区 `:184-220`）
- Modify: `src/views/Settings.test.tsx`（mockIPC 补三个命令 + 改造 3 个既有用例 + 新增用例）

**Interfaces:**
- Consumes: Task 4 的组件与 api。
- Produces: 用户点「导出 YAML」先见选择弹窗。

- [ ] **Step 1: 先补 mockIPC，让既有用例先红**

`Settings.test.tsx` 默认 `mockIPC` 的 switch 里追加：

```ts
      case 'list_exportable_nodes_cmd':
        return [
          { node: { ...nodeFixture('n1', '节点甲', 'ss') }, exportable: true, skip_reason: null },
          { node: { ...nodeFixture('n2', 'ssr节点', 'ssr') }, exportable: false, skip_reason: 'mihomo 与 sing-box 已移除 SSR 支持' },
        ]
      case 'export_yaml_selected_cmd':
        return { path: 'C:\\out\\vpn-ctl.yaml', skipped_ssr: 0 }
      case 'export_singbox_selected_cmd':
        return { path: 'C:\\out\\yunyi-singbox.json', skipped_ssr: 0 }
```

文件顶部加夹具（`Node` 字段较多，照 `Nodes.test.tsx` 的 `baseNodes` 补全）：

```ts
const nodeFixture = (id: string, name: string, protocol: string): Node => ({
  id, name, protocol,
  source: 'self-built', addr: '1.2.3.4', port: 8388,
  params_json: '{}', group_name: '自建', sub_id: null, relay_rule_id: null,
  enabled: true, latency: null, bandwidth: null, last_checked_at: null,
  last_check_error: null, direct_latency: null,
  hop_relay_ms: null, hop_landing_ms: null, hop_tunnel_ms: null,
})
```

改造三个既有导出用例：`导出 YAML 并展示路径`（`:78`）与 `导出 sing-box 并展示路径`（`:89`）在点击导出按钮后**先点弹窗里的确认按钮**：

```tsx
    await user.click(screen.getByRole('button', { name: '导出 YAML' }))
    await user.click(await screen.findByRole('button', { name: /导出选中/ }))
```

`导出结果含 ssr 跳过计数…`（`:99`）的覆盖 mock 里加 `list_exportable_nodes_cmd` 与 `export_singbox_selected_cmd`（返回 `skipped_ssr: 2`），同样加一步确认点击。

新增用例：

```tsx
  it('导出前弹出节点选择，默认全勾可导出项（ssr 置灰）', async () => {
    const user = userEvent.setup()
    render(<Settings />)
    await screen.findByText('订阅服务')
    await user.click(screen.getByRole('button', { name: '导出 YAML' }))
    expect(await screen.findByLabelText('选择 节点甲')).toBeChecked()
    const ssr = screen.getByLabelText('选择 ssr节点')
    expect(ssr).not.toBeChecked()
    expect(ssr).toBeDisabled()
    expect(screen.getByText(/SSR 支持/)).toBeInTheDocument()
  })

  it('只导出勾选的节点 id', async () => {
    const calls: Array<Record<string, unknown>> = []
    mockIPC((cmd, args) => {
      if (cmd === 'export_yaml_selected_cmd') {
        calls.push(args as Record<string, unknown>)
        return { path: 'C:\\out\\vpn-ctl.yaml', skipped_ssr: 0 }
      }
      // 其余命令沿用默认实现（此处需把默认 mock 抽成函数复用）
      return defaultMock(cmd, args)
    })
    const user = userEvent.setup()
    render(<Settings />)
    await screen.findByText('订阅服务')
    await user.click(screen.getByRole('button', { name: '导出 YAML' }))
    await user.click(await screen.findByLabelText('选择 节点甲')) // 取消勾选
    await user.click(screen.getByLabelText('选择 节点甲'))          // 再勾回来 → 单选
    await user.click(screen.getByRole('button', { name: /导出选中 1/ }))
    await waitFor(() => expect(calls).toHaveLength(1))
    expect(calls[0]).toMatchObject({ nodeIds: ['n1'] })
  })

  it('取消选择弹窗不发导出请求', async () => {
    const calls: number[] = []
    // 同上，仅记录 export_yaml_selected_cmd 调用次数
    const user = userEvent.setup()
    render(<Settings />)
    await screen.findByText('订阅服务')
    await user.click(screen.getByRole('button', { name: '导出 YAML' }))
    await user.click(await screen.findByRole('button', { name: '取消' }))
    expect(screen.queryByLabelText('选择 节点甲')).not.toBeInTheDocument()
    expect(calls).toHaveLength(0)
  })
```

**若把默认 mock 抽成 `defaultMock(cmd, args)` 改动过大，可改为在用例内 `mockIPC` 里对 `export_yaml_selected_cmd` 记调用、其余 case 复刻默认实现**——以最小改动为准。

- [ ] **Step 2: 跑测试确认失败**

Run: `npm test -- Settings`
Expected: 新用例失败（找不到「导出选中」按钮 / 找不到选择弹窗）；既有 3 个导出用例也失败

- [ ] **Step 3: 写实现（Settings.tsx）**

导入区加 `ExportSelectDialog`；state 加：

```tsx
  // 待导出的格式：null = 未在选；'yaml' | 'singbox' = 弹窗已开
  const [exportTarget, setExportTarget] = useState<'yaml' | 'singbox' | null>(null)
```

两个按钮改为开弹窗（不再直接导出）：

```tsx
            onClick={() => setExportTarget('yaml')}
            …
            onClick={() => setExportTarget('singbox')}
```

按钮下方（`{exported && …}` 之前）插入：

```tsx
        {exportTarget && (
          <ExportSelectDialog
            formatLabel={exportTarget === 'yaml' ? 'YAML' : 'sing-box'}
            onExport={async (ids) => {
              setError(null)
              setExported(
                exportTarget === 'yaml'
                  ? await exportYamlSelected(ids)
                  : await exportSingboxSelected(ids),
              )
            }}
            onClose={() => setExportTarget(null)}
          />
        )}
```

同时删掉不再使用的 `handleExportYaml` / `handleExportSingbox`，并把 `exportYaml` / `exportSingbox` 从 import 里移除（改为导入 selected 版本）。

- [ ] **Step 4: 跑测试确认通过**

Run: `npm test -- Settings`
Expected: 全 PASS

- [ ] **Step 5: 全量前端门禁**

Run: `npx tsc --noEmit` 再 `npm test`
Expected: tsc 无输出；测试全 PASS

- [ ] **Step 6: 提交**

```bash
git add src/views/Settings.tsx src/views/Settings.test.tsx
git commit -m "feat(export): 设置页导出先选节点（默认全勾可导出项，ssr 置灰）"
```

---

### Task 6: CHANGELOG 与总门禁

**Files:**
- Modify: `CHANGELOG.md`（`## [未发布]` 节内追加条目）

- [ ] **Step 1: 写文档**

在 `CHANGELOG.md` 的 `## [未发布]` 节里，节点改名条目**之后**追加：

```markdown
- **配置导出可选节点**：导出 YAML / sing-box 前先勾选要写进文件的线路；清单与导出
  共用同一套可见性判定（启用节点 + 其订阅启用），所见即所得
- **不可导出的节点有明确提示**：ssr 节点在清单里置灰并注明原因（两种客户端均已移除
  支持），不再静默剔除；一条都没勾时导出按钮禁用，后端同样拒绝空选集
```

- [ ] **Step 2: 总门禁**

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` 全 PASS
- [ ] `npx tsc --noEmit` 无输出
- [ ] `npm test` 全 PASS
- [ ] `git status --short` 核对：上次会话的遗留改动仍在未提交状态（除 `subs/service.rs` 已随 Task 1 提交，需在报告中说明）

- [ ] **Step 3: 提交**

```bash
git add CHANGELOG.md
git commit -m "docs(export): CHANGELOG 记录导出选中节点"
```
