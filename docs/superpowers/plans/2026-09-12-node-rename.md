# 节点改名 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在「节点管理」页给任意一条节点改名，且 `clash_raw` 节点的名字改完后配置仍然自洽。

**Architecture:** 一行 `UPDATE nodes SET name` 不够——`config/mod.rs:215` 对 Clash 来源节点输出的是 YAML 片段自带的 `name`（忽略 `n.name`），而 proxy-groups 成员取 `n.name`；只改一半会让分组指向不存在的成员，整份配置静默失效。所以改名收进 `repos::nodes::rename` 一个函数里做两件事（改 `nodes.name` + 同步改写片段里的 `name`）。命令层负责校验（空名/重名拒绝）与重建配置缓存，沿用 `set_node_enabled` 的既有模式。前端在名称单元格做就地编辑。

**Tech Stack:** Rust（Tauri 2 backend）、rusqlite、serde_yaml、React 18 + TypeScript + Vitest（frontend）、`mockIPC`（`@tauri-apps/api/mocks`）。

**Spec:** `docs/superpowers/specs/2026-09-12-node-rename-design.md`

## Global Constraints

- **节点名是多个消费方的键，不只是显示**：`config/mod.rs:90`/`:148` 的 proxy-groups 成员、`config/singbox.rs:171` 等 7 处的 outbound `tag`、`net/runtime.rs:310` 的 `netSelect`、`subs/service.rs:303-304` 的过滤器正则。任何改动都要保证这三者不分裂。
- **空名与重名必须在写库前拒绝**。名字直接当 sing-box tag 和分组引用键，重名/空名会静默生成坏配置。
- **零宽字符与首尾空白必须剔除**：直接复用 `servers::clean_text`（`src-tauri/src/servers/mod.rs:48`），不要新写一份。`db/repos.rs` **不得**依赖 `servers` 模块。
- **订阅节点改名下次同步会回到原名**，这是已接受的边界（`stable_node_id` 由名字派生，`subs/service.rs:310`）。不做墓碑表。
- **暂存文件必须逐个列路径，禁止 `git add -A` / `git add .`**。工作区有上一次会话遗留的未提交改动（`git status` 里的 25 处 + 2 个未跟踪文件），全量暂存会把它们卷进本次提交。
- 注释、错误文案、提交信息一律**中文**，与仓库现有风格一致。
- Rust 测试命令必须带 manifest（bin crate，`--lib` 不可用）：`cargo test --manifest-path src-tauri/Cargo.toml <filter>`。
- 前端门禁：`npx tsc --noEmit` + `npm test`。

## 关键既有事实（实现前必读，均已核对源码）

1. **测试夹具**：`db/tests.rs` 用 `new_test_db() -> (TempDir, Db)`（`:31`）与单例 `test_creds()`（`:9`）；`commands_tests.rs` 用 `test_state() -> (TempDir, Arc<AppState>)`（`:14`）；`ipc_tests.rs` 用 `test_state()`（`:26`）+ `mock_app(state)`（`:36`）+ `invoke_ok(&wv, cmd, json!{…})`（`:86`）。
2. **`params_json` 是加密列**：写走 `crypto::seal_param`，读走 `crypto::open_param`（`db/repos.rs:71-73`、`:204`）。`open_param` 对无 `enc:v1:` 前缀的行按**明文**返回，所以测试里直接用明文 JSON 建行是合法的（legacy 兼容路径）。
3. **`node` 的 `sub_id = None` 在 `rebuild_config` 里视为可见**（`subs/service.rs:225-231`：无 `sub_id` 一律保留）。所以拷问缓存的断言用自建节点（`sub_id: None`）最省事，不需要建订阅。
4. **`ConfigError` 未知协议会报错**：`build_yaml` 对没有 `clash_raw` 的未知协议返回 `BuildFailed`。测试节点用 `protocol: "ss"` 并带 `password` 参数即可正常生成。
5. **`serde_yaml::to_string` 对 `Mapping` 可用**，`Value::String(k)` 可直接作 `Mapping` 的键（`config/mod.rs:207-211` 已有先例）。
6. **前端测试用 `mockIPC` 拦命令**，未 mock 的命令会 `throw new Error(\`未 mock 的命令: ${cmd}\`)`（`Nodes.test.tsx:135`）——新增命令后必须同步补 handler，否则用例会以「未 mock 的命令」失败。
7. **既有测试不得被破坏**：`Nodes.test.tsx` 现有 30 个用例靠 `getAllByRole('button', { name: '删除' })` / `'测速'` 取按钮，新增「改名」按钮不与之冲突。

---

### Task 1: `repos::nodes::rename`（含 clash_raw 名字同步）

**Files:**
- Modify: `src-tauri/src/db/repos.rs`（在 `nodes` 模块的 `delete`（`:102-105`）之后插入）
- Test: `src-tauri/src/db/tests.rs`（在 `delete_for_sub_except_scopes_and_empties`（`:123`）之前插入）

**Interfaces:**
- Consumes: `crate::crypto::seal_param` / `open_param`（`crypto/mod.rs:158`、`:173`）、`crate::db::repos::nodes` 模块内既有私有辅助风格。
- Produces: `repos::nodes::rename(conn: &Connection, id: &str, name: &str, creds: &CredentialService) -> rusqlite::Result<bool>`（`true` = 名字确实变了；节点不存在或名字未变返回 `false`）。

- [ ] **Step 1: 写失败的测试**

在 `src-tauri/src/db/tests.rs` 的 `fn delete_for_sub_except_scopes_and_empties()` 之前插入：

```rust
#[test]
fn rename_updates_name_and_clash_raw_fragment() {
    let (_dir, db) = new_test_db();
    let node = Node {
        id: "n-rename".into(),
        name: "旧名".into(),
        source: "import".into(),
        protocol: "ss".into(),
        addr: "1.2.3.4".into(),
        port: 8388,
        params_json:
            r#"{"password":"pw","clash_raw":"name: 旧名\ntype: ss\nserver: 1.2.3.4\nport: 8388\ncipher: aes-128-gcm\npassword: pw\n"}"#
                .into(),
        group_name: "默认分组".into(),
        sub_id: None,
        relay_rule_id: None,
        enabled: true,
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
        direct_latency: None,
        hop_relay_ms: None,
        hop_landing_ms: None,
        hop_tunnel_ms: None,
    };
    repos::nodes::upsert(&db.lock(), &node, &test_creds()).expect("upsert");

    let changed =
        repos::nodes::rename(&db.lock(), "n-rename", "新名", &test_creds()).expect("rename");
    assert!(changed, "名字变了必须返回 true");

    let got = repos::nodes::get(&db.lock(), "n-rename", &test_creds())
        .expect("get")
        .expect("节点存在");
    assert_eq!(got.name, "新名");
    // 片段自带 name 是 proxies 里真正输出的名字：不同步改写会让 proxy-groups 指向
    // 一个不存在于 proxies 的成员，整份 mihomo 配置失效
    let params: serde_json::Value = serde_json::from_str(&got.params_json).expect("params 为 JSON");
    let raw = params["clash_raw"].as_str().expect("clash_raw 仍在");
    let frag: serde_yaml::Value = serde_yaml::from_str(raw).expect("片段仍是合法 YAML");
    assert_eq!(
        frag["name"].as_str(),
        Some("新名"),
        "clash_raw 片段的 name 必须同步改写"
    );
    assert_eq!(frag["cipher"].as_str(), Some("aes-128-gcm"), "其余字段不得丢");
    assert_eq!(params["password"], serde_json::json!("pw"), "其余参数不得丢");
}

#[test]
fn rename_is_idempotent_and_missing_id_is_noop() {
    let (_dir, db) = new_test_db();
    let node = Node {
        id: "n2".into(),
        name: "同名".into(),
        source: "self-built".into(),
        protocol: "ss".into(),
        addr: "5.6.7.8".into(),
        port: 8388,
        params_json: r#"{"password":"pw"}"#.into(),
        group_name: "自建".into(),
        sub_id: None,
        relay_rule_id: None,
        enabled: true,
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
        direct_latency: None,
        hop_relay_ms: None,
        hop_landing_ms: None,
        hop_tunnel_ms: None,
    };
    repos::nodes::upsert(&db.lock(), &node, &test_creds()).expect("upsert");

    // 名字未变、id 不存在：都返回 false（上层据此跳过配置重建）
    assert!(!repos::nodes::rename(&db.lock(), "n2", "同名", &test_creds()).expect("noop"));
    assert!(!repos::nodes::rename(&db.lock(), "不存在", "随便", &test_creds()).expect("missing"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml rename_`
Expected: 编译失败，`no function or associated item named 'rename' found for struct 'nodes'`（两个用例都编不过）

（cargo test 只接受**一个**位置过滤参数；`rename_` 同时命中本任务的 `rename_updates_…` 与 `rename_is_idempotent_…`）

- [ ] **Step 3: 写实现**

在 `src-tauri/src/db/repos.rs` 的 `nodes` 模块内，`pub fn delete` 之后插入：

```rust
    /// 单条节点改名。**必须同时改写 clash_raw 片段里的 name**：`config::node_mapping`
    /// （config/mod.rs:215）对 raw 片段自带 name 时原样输出、忽略 `n.name`，而
    /// proxy-groups 成员与 sing-box tag 取的都是 `n.name` —— 只改一边，分组就指向一个
    /// 不存在于 proxies 的成员，整份配置静默失效。
    ///
    /// 返回名字是否真的变了：false（节点不存在 / 名字未变）时上层跳过配置重建。
    pub fn rename(
        conn: &Connection,
        id: &str,
        name: &str,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<bool> {
        let cur_name: Option<String> = conn
            .query_row("SELECT name FROM nodes WHERE id=?1", params![id], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(cur_name) = cur_name else {
            return Ok(false);
        };
        if cur_name == name {
            return Ok(false);
        }
        let stored: String = conn.query_row(
            "SELECT params_json FROM nodes WHERE id=?1",
            params![id],
            |r| r.get(0),
        )?;
        conn.execute(
            "UPDATE nodes SET name=?2 WHERE id=?1",
            params![id, name],
        )?;
        if let Some(new_json) =
            rewrite_clash_raw_name(&crate::crypto::open_param(creds, &stored), name)
        {
            conn.execute(
                "UPDATE nodes SET params_json=?2 WHERE id=?1",
                params![id, crate::crypto::seal_param(creds, &new_json)],
            )?;
        }
        Ok(true)
    }
```

然后在该模块内（`row_to_node` 之前）加辅助函数：

```rust
    /// 把 params_json 里 clash_raw 片段的 name 改成 `new_name`。
    /// 没有 clash_raw、片段非映射、或片段已经是该名字 → None（无需回写）。
    /// 片段原本没有 name 键也照样写入：既然节点有名字，proxies 就该输出它。
    fn rewrite_clash_raw_name(params_json: &str, new_name: &str) -> Option<String> {
        let mut p: serde_json::Value = serde_json::from_str(params_json).ok()?;
        let raw = p.get("clash_raw")?.as_str()?.to_string();
        let mut frag: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
        let map = frag.as_mapping_mut()?;
        let key = serde_yaml::Value::String("name".into());
        if map.get(&key).and_then(|v| v.as_str()) == Some(new_name) {
            return None;
        }
        map.insert(key, serde_yaml::Value::String(new_name.into()));
        p["clash_raw"] = serde_json::Value::String(serde_yaml::to_string(&frag).ok()?);
        serde_json::to_string(&p).ok()
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml rename_`
Expected: `rename_updates_name_and_clash_raw_fragment` 与 `rename_is_idempotent_and_missing_id_is_noop` 均 PASS

- [ ] **Step 5: 确认没打破既有 DB 测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml db::tests`
Expected: 全 PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/db/repos.rs src-tauri/src/db/tests.rs
git commit -m "feat(node): repos::nodes::rename 改名并同步改写 clash_raw 片段 name"
```

---

### Task 2: `commands::rename_node`（校验 + 重建配置缓存）

**Files:**
- Modify: `src-tauri/src/commands.rs`（在 `delete_node`（`:146-151`）之后插入）
- Test: `src-tauri/src/commands_tests.rs`（在文件末尾追加）

**Interfaces:**
- Consumes: Task 1 的 `repos::nodes::rename(conn, id, name, creds) -> rusqlite::Result<bool>`；`servers::clean_text(&str) -> String`（`servers/mod.rs:48`）；`state.subs.rebuild_config()`（`subs/service.rs:216`）；`repos::nodes::list(&conn, &creds)`。
- Produces: `commands::rename_node(state: Arc<AppState>, id: String, name: String) -> R<()>`。

- [ ] **Step 1: 写失败的测试**

在 `src-tauri/src/commands_tests.rs` 顶部 `use` 区后面（`test_state` 之前）加夹具：

```rust
/// 直接插一条自建节点（sub_id=None → 在 rebuild_config 里恒可见，不需要建订阅）。
fn seed_node(state: &Arc<AppState>, id: &str, name: &str) -> crate::domain::Node {
    let node = crate::domain::Node {
        id: id.into(),
        name: name.into(),
        source: "self-built".into(),
        protocol: "ss".into(),
        addr: "1.2.3.4".into(),
        port: 8388,
        params_json: r#"{"password":"pw"}"#.into(),
        group_name: "自建".into(),
        sub_id: None,
        relay_rule_id: None,
        enabled: true,
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
        direct_latency: None,
        hop_relay_ms: None,
        hop_landing_ms: None,
        hop_tunnel_ms: None,
    };
    crate::db::repos::nodes::upsert(&state.db.lock(), &node, &state.creds).expect("seed node");
    node
}
```

在文件末尾追加用例：

```rust
#[tokio::test]
async fn rename_node_trims_whitespace_and_zero_width() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "旧名");
    rename_node(state.clone(), "n1".into(), "\u{200B}  新名  ".into())
        .await
        .expect("rename");
    let nodes = list_nodes(state).expect("nodes");
    assert_eq!(nodes[0].name, "新名", "首尾空白与零宽字符必须剔除");
}

#[tokio::test]
async fn rename_node_rejects_blank_and_duplicate_names() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "节点A");
    seed_node(&state, "n2", "节点B");

    let blank = rename_node(state.clone(), "n1".into(), "   ".into())
        .await
        .expect_err("空名必须被拒");
    assert!(
        blank.to_string().contains("不能为空"),
        "错误文案应说明空名：{blank}"
    );
    let dup = rename_node(state.clone(), "n1".into(), "节点B".into())
        .await
        .expect_err("重名必须被拒");
    assert!(
        dup.to_string().contains("已存在同名节点"),
        "错误文案应说明重名：{dup}"
    );
    // 拒绝后原样不动
    let nodes = list_nodes(state).expect("nodes");
    assert!(nodes.iter().any(|n| n.name == "节点A"));
}

#[tokio::test]
async fn rename_node_rejects_missing_id() {
    let (_d, state) = test_state();
    let e = rename_node(state, "不存在".into(), "新名".into())
        .await
        .expect_err("节点不存在必须报错");
    assert!(e.to_string().contains("节点不存在"), "错误文案：{e}");
}

#[tokio::test]
async fn rename_node_rebuilds_merged_config() {
    let (_d, state) = test_state();
    seed_node(&state, "n1", "旧名-A");
    // 先建一次缓存，确保下面的断言是「改名后重建过」而不是首次生成
    state.subs.rebuild_config().await.expect("rebuild");
    assert!(
        get_merged_config(state.clone())
            .await
            .expect("merged")
            .contains("旧名-A")
    );

    rename_node(state.clone(), "n1".into(), "新名-B".into())
        .await
        .expect("rename");

    let merged = get_merged_config(state).await.expect("merged2");
    assert!(merged.contains("新名-B"), "新名必须进合并配置：{merged}");
    assert!(!merged.contains("旧名-A"), "旧名不得残留：{merged}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml rename_node_`
Expected: 编译失败，`cannot find function 'rename_node' in this scope`

- [ ] **Step 3: 写实现**

在 `src-tauri/src/commands.rs` 的 `delete_node` 之后插入：

```rust
/// 节点改名。名字不是装饰：它直接当 sing-box 的 tag 与 Clash 的 proxy 名、也是
/// proxy-groups 成员与「出口节点」选择的键（config/mod.rs:90、net/runtime.rs:310），
/// 所以空名与重名在写库前拒绝——否则会静默生成一份坏配置。
///
/// 自建节点改了就稳；订阅节点下次同步会按 `stable_node_id`（由名字派生）重建回原名，
/// 这是已接受的边界，不做墓碑表。
pub async fn rename_node(state: Arc<AppState>, id: String, name: String) -> R<()> {
    let name = servers::clean_text(&name);
    if name.is_empty() {
        return Err(AppError::Message("节点名不能为空".into()));
    }
    let changed = {
        let conn = state.db.lock();
        // 先判存在：repos::rename 对不存在的 id 返回 false，与「名字没变」不可区分
        if repos::nodes::get(&conn, &id, &state.creds)
            .map_err(err)?
            .is_none()
        {
            return Err(AppError::Message("节点不存在".into()));
        }
        if repos::nodes::list(&conn, &state.creds)
            .map_err(err)?
            .into_iter()
            .any(|n| n.id != id && n.name == name)
        {
            return Err(AppError::Message(format!("已存在同名节点：{name}")));
        }
        repos::nodes::rename(&conn, &id, &name, &state.creds).map_err(err)?
    };
    if changed {
        // 名字进合并配置（proxies 名/tag/分组成员），立即重建缓存避免下发/导出读到旧名
        state.subs.rebuild_config().await.map_err(err)?;
    }
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml rename_node_`
Expected: 4 个用例全 PASS

- [ ] **Step 5: 跑全量后端测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS（新增 4 个用例，无回归）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/commands_tests.rs
git commit -m "feat(node): rename_node 命令——空名/重名拦截 + 重建配置缓存"
```

---

### Task 3: IPC 壳与注册

**Files:**
- Modify: `src-tauri/src/commands.rs`（在 `delete_node_cmd` 之后，紧跟 `get_merged_config_cmd` 之前）
- Modify: `src-tauri/src/main.rs`（`generate_handler!` 列表，`commands::delete_node_cmd` 之后）
- Modify: `src-tauri/src/ipc_tests.rs`（`mock_app` 的 `generate_handler!` 列表 + 新用例）

**Interfaces:**
- Consumes: Task 2 的 `commands::rename_node(Arc<AppState>, String, String) -> R<()>`。
- Produces: IPC 命令名 `rename_node_cmd`，参数 `id` / `name`（小写，无 camelCase 转换问题）。

- [ ] **Step 1: 写失败的测试**

在 `src-tauri/src/ipc_tests.rs` 的 `mock_app` 的 `generate_handler!` 列表里加一行（放在 `crate::commands::enable_bbr_cmd,` 之前）：

```rust
            crate::commands::rename_node_cmd,
```

在文件末尾追加用例：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn rename_node_cmd_ipc_renames_node() {
    let (_d, state) = test_state();
    let node = crate::domain::Node {
        id: "n-ipc".into(),
        name: "旧名".into(),
        source: "self-built".into(),
        protocol: "ss".into(),
        addr: "1.2.3.4".into(),
        port: 8388,
        params_json: r#"{"password":"pw"}"#.into(),
        group_name: "自建".into(),
        sub_id: None,
        relay_rule_id: None,
        enabled: true,
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
        direct_latency: None,
        hop_relay_ms: None,
        hop_landing_ms: None,
        hop_tunnel_ms: None,
    };
    crate::db::repos::nodes::upsert(&state.db.lock(), &node, &state.creds).expect("seed");
    let (app, wv) = mock_app(state.clone());
    // 参数名 id/name 均为单个小写词，无需 camelCase 转换
    let v = invoke_ok(&wv, "rename_node_cmd", json!({ "id": "n-ipc", "name": "新名" }));
    assert!(v.is_null(), "命令应返回 null：{v}");
    let nodes = crate::commands::list_nodes(state).expect("nodes");
    assert_eq!(nodes[0].name, "新名");
    drop(wv);
    let _ = app;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml rename_node_cmd_ipc`
Expected: 编译失败，`cannot find function 'rename_node_cmd' in this scope`

- [ ] **Step 3: 写实现**

`src-tauri/src/commands.rs` 在 `delete_node_cmd` 之后插入：

```rust
#[tauri::command]
pub async fn rename_node_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: String,
    name: String,
) -> R<()> {
    rename_node(state.inner().clone(), id, name).await
}
```

`src-tauri/src/main.rs` 的 `generate_handler!` 里，`commands::delete_node_cmd,` 之后插入：

```rust
            commands::rename_node_cmd,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml rename_node_cmd_ipc`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/main.rs src-tauri/src/ipc_tests.rs
git commit -m "feat(node): 注册 rename_node_cmd IPC"
```

---

### Task 4: 前端 API 封装

**Files:**
- Modify: `src/api/index.ts`（`deleteNode`（`:77-79`）之后）

**Interfaces:**
- Consumes: Task 3 的 IPC 命令 `rename_node_cmd`。
- Produces: `renameNode(id: string, name: string): Promise<void>`（Task 5 依赖它）。

- [ ] **Step 1: 写实现**

在 `deleteNode` 之后插入：

```ts
/** 单条节点改名。clash_raw 节点的片段 name 由后端同步改写；订阅节点下次同步回原名 */
export function renameNode(id: string, name: string): Promise<void> {
  return invoke('rename_node_cmd', { id, name })
}
```

- [ ] **Step 2: 类型门禁**

Run: `npx tsc --noEmit`
Expected: 无输出（通过）

- [ ] **Step 3: 提交**

```bash
git add src/api/index.ts
git commit -m "feat(node): api.renameNode 封装"
```

---

### Task 5: 节点页就地改名 UI

**Files:**
- Modify: `src/views/Nodes.tsx`（import 区、state 区、handler 区、名称单元格 `:348`、操作列 `:426-444`）
- Test: `src/views/Nodes.test.tsx`（mockIPC 补 handler + 新用例）

**Interfaces:**
- Consumes: Task 4 的 `renameNode(id, name)`。
- Produces: 用户可见的「改名」按钮与就地编辑输入框。

- [ ] **Step 1: 写失败的测试**

`src/views/Nodes.test.tsx`：在状态变量声明区（`relayToggleCalls` 之后）加：

```ts
let renameCalls: Array<{ id: string; name: string }>
```

在 `beforeEach` 的 `relayToggleCalls = []` 之后加：

```ts
  renameCalls = []
```

在 `mockIPC` 的 `switch` 里、`case 'delete_node_cmd':` 之前插入 handler：

```ts
      case 'rename_node_cmd': {
        // 与后端同口径：空名/重名拒绝，成功则改名
        const a = args as { id: string; name: string }
        const trimmed = a.name.trim()
        if (trimmed === '') {
          throw new Error('节点名不能为空')
        }
        if (nodesStore.some((n) => n.id !== a.id && n.name === trimmed)) {
          throw new Error(`已存在同名节点：${trimmed}`)
        }
        renameCalls.push({ id: a.id, name: trimmed })
        const n = nodesStore.find((x) => x.id === a.id)
        if (n) n.name = trimmed
        return null
      }
```

在 `describe('节点页', ...)` 内、`it('删除节点', ...)` 之后追加 5 个用例：

```tsx
  it('点改名出现预填当前名的输入框', async () => {
    const user = userEvent.setup()
    render(<Nodes />)
    await screen.findByText('香港-01')
    await user.click(screen.getAllByRole('button', { name: '改名' })[0])
    const box = screen.getByLabelText('新名称 香港-01')
    expect(box).toHaveValue('香港-01')
    expect(screen.getByRole('button', { name: '保存' })).toBeInTheDocument()
  })

  it('保存改名：提交 {id,name} 并刷新列表', async () => {
    const user = userEvent.setup()
    render(<Nodes />)
    await screen.findByText('香港-01')
    await user.click(screen.getAllByRole('button', { name: '改名' })[0])
    const box = screen.getByLabelText('新名称 香港-01')
    await user.clear(box)
    await user.type(box, '香港-改名后')
    await user.click(screen.getByRole('button', { name: '保存' }))
    await waitFor(() => {
      expect(renameCalls).toEqual([{ id: 'n1', name: '香港-改名后' }])
    })
    expect(await screen.findByText('香港-改名后')).toBeInTheDocument()
    expect(screen.queryByLabelText('新名称 香港-01')).not.toBeInTheDocument()
  })

  it('Esc 与取消都不发请求', async () => {
    const user = userEvent.setup()
    render(<Nodes />)
    await screen.findByText('香港-01')

    await user.click(screen.getAllByRole('button', { name: '改名' })[0])
    await user.type(screen.getByLabelText('新名称 香港-01'), '不该提交')
    await user.keyboard('{Escape}')
    expect(screen.queryByLabelText('新名称 香港-01')).not.toBeInTheDocument()

    await user.click(screen.getAllByRole('button', { name: '改名' })[0])
    await user.click(screen.getByRole('button', { name: '取消' }))
    expect(screen.queryByLabelText('新名称 香港-01')).not.toBeInTheDocument()
    expect(renameCalls).toEqual([])
  })

  it('空名与纯空白不提交', async () => {
    const user = userEvent.setup()
    render(<Nodes />)
    await screen.findByText('香港-01')
    await user.click(screen.getAllByRole('button', { name: '改名' })[0])
    const box = screen.getByLabelText('新名称 香港-01')
    await user.clear(box)
    expect(screen.getByRole('button', { name: '保存' })).toBeDisabled()
    await user.type(box, '   ')
    expect(screen.getByRole('button', { name: '保存' })).toBeDisabled()
    expect(renameCalls).toEqual([])
  })

  it('后端重名报错上屏', async () => {
    const user = userEvent.setup()
    render(<Nodes />)
    await screen.findByText('香港-01')
    await user.click(screen.getAllByRole('button', { name: '改名' })[0])
    const box = screen.getByLabelText('新名称 香港-01')
    await user.clear(box)
    await user.type(box, '日本-01')
    await user.click(screen.getByRole('button', { name: '保存' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('已存在同名节点')
  })
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npm test -- Nodes`
Expected: 5 个新用例失败——`Unable to find role="button" and name "改名"`；其余既有用例仍 PASS

- [ ] **Step 3: 写实现**

`src/views/Nodes.tsx`：

(a) import 区（`toggleNodeRelay,` 之后）加 `renameNode,`，保持字母序：

```ts
import {
  cancelJob,
  deleteNode,
  listJobs,
  listNodes,
  listRelayRules,
  renameNode,
  setNodeEnabled,
  speedTest,
  toggleNodeRelay,
} from '../api'
```

(b) state 区（`const [selected, setSelected] = ...` 之后）加：

```tsx
  // 就地改名：同一时间只允许一行处于编辑态
  const [editingName, setEditingName] = useState<{ id: string; draft: string } | null>(null)
  const [savingName, setSavingName] = useState(false)
```

(c) handler 区（`handleDelete` 之后）加：

```tsx
  const handleRenameSave = async (id: string) => {
    if (!editingName || savingName) return
    const name = editingName.draft.trim()
    if (!name) return
    setError(null)
    setSavingName(true)
    try {
      // 后端会剔除零宽字符并再次校验；报错（撞名/空名）落顶部错误条
      await renameNode(id, name)
      await refresh()
      setEditingName(null)
    } catch (e) {
      setError(String(e))
    } finally {
      setSavingName(false)
    }
  }
```

(d) 名称单元格（`:348` 的 `<td className="px-3 py-2">{n.name}</td>`）替换为：

```tsx
                  <td className="px-3 py-2">
                    {editingName?.id === n.id ? (
                      <input
                        autoFocus
                        aria-label={`新名称 ${n.name}`}
                        value={editingName.draft}
                        onChange={(e) => setEditingName({ id: n.id, draft: e.target.value })}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') void handleRenameSave(n.id)
                          if (e.key === 'Escape') setEditingName(null)
                        }}
                        className="w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
                      />
                    ) : (
                      n.name
                    )}
                  </td>
```

(e) 操作列（`:426-444` 的 `<div className="flex items-center gap-2">` 内，`测速` 按钮之前）插入改名按钮与保存/取消分支：

```tsx
                      {editingName?.id === n.id ? (
                        <>
                          <button
                            type="button"
                            onClick={() => void handleRenameSave(n.id)}
                            disabled={savingName || editingName.draft.trim() === ''}
                            className="rounded bg-blue-600 px-2 py-1 text-xs font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
                          >
                            {savingName ? '保存中…' : '保存'}
                          </button>
                          <button
                            type="button"
                            onClick={() => setEditingName(null)}
                            disabled={savingName}
                            className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
                          >
                            取消
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          onClick={() => setEditingName({ id: n.id, draft: n.name })}
                          className="rounded bg-neutral-800 px-2 py-1 text-xs text-amber-300 hover:bg-neutral-700"
                        >
                          改名
                        </button>
                      )}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npm test -- Nodes`
Expected: 全部 PASS（既有 30 个 + 新增 5 个）

- [ ] **Step 5: 前端门禁**

Run: `npx tsc --noEmit`
Expected: 无输出（通过）

- [ ] **Step 6: 提交**

```bash
git add src/views/Nodes.tsx src/views/Nodes.test.tsx
git commit -m "feat(node): 节点页就地改名（输入框 + 保存/取消 + 错误上屏）"
```

---

### Task 6: 文档

**Files:**
- Modify: `CHANGELOG.md`（顶部新建 `## [Unreleased]` 节）

**Interfaces:**
- Consumes: 前 5 个任务的用户可见行为。
- Produces: 发版时 Release notes 的来源条目。

- [ ] **Step 1: 写文档**

在 `CHANGELOG.md` 的 `## [0.2.4] - 2026-09-01` 之前插入：

```markdown
## [未发布]

节点改名与既有限制说明：

- **节点改名**：「节点管理」页每行新增「改名」，就地编辑名称。空名与重名会被拒绝
  （节点名直接用作 sing-box tag 与代理分组引用，重名会生成无效配置）
- **Clash 来源节点**：改名时同步改写原始配置片段里的 `name`，保证代理列表与分组
  引用一致
- **已知边界**：订阅同步会按订阅里的原名重建节点行，因此订阅节点的改名在下次同步
  后失效；部署产生的自建节点在下次重新部署同一服务器时回到 `服务器名-协议`
```

- [ ] **Step 2: 提交**

```bash
git add CHANGELOG.md
git commit -m "docs: CHANGELOG 记录节点改名"
```

---

## 完成前总门禁

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` 全 PASS
- [ ] `npx tsc --noEmit` 无输出
- [ ] `npm test` 全 PASS
- [ ] `git status --short` 确认：本次只新增了上述已提交的文件，上次会话遗留的 25 处改动**仍未提交**（没被顺手卷进来）
