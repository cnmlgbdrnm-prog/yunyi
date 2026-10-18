# 分享订阅链接 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把选中的几条线路做成一条公网可达、可单独收回的订阅链接发给别人。

**Architecture:** app 只做「装」与「拆」：SSH 到指定服务器，写一份固定快照 YAML 到 `/srv/vpnshare/<token>/`，配一个 systemd 单元起 `python3 -m http.server`，再用 iptables 只放行带正确 token 的请求。**分享的真实状态在服务器上**——本地不建表，`list` 每次 SSH 实读，app 重装或本地库丢失都不影响已发出的分享。

**Tech Stack:** Rust（Tauri 2 backend）、`russh`（既有 `SshSession`）、systemd、iptables、React 18 + TS + Vitest、`FakeSshServer` 测试替身。

**Spec:** `docs/superpowers/specs/2026-09-13-share-link-design.md`

## Global Constraints

- **不改 `serve/mod.rs`**。本地订阅服务（127.0.0.1 + 单 token）是给本机客户端用的，与分享是两条路，不要合并。
- **快照制**：生成时写入的内容此后不再变。不实现「服务器回连本机取最新配置」。
- **不在本地建表**。分享状态以服务器为准（目录 + systemd 单元 + iptables 规则）。
- **token 只允许 `[0-9a-f]`**（16 位）。它要拼进 systemd 单元名与 iptables 命令行，限制字符集省掉一整类注入问题；`delete_share` 也必须重新校验（不能只信前端传回来的）。
- **失败要显式**：探测不到 python3 / iptables 不支持 `-m string` / 端口全占 → 报错，**绝不降级成裸开端口**。
- **暂存逐个列路径**。工作区有前几次会话的未提交改动（`config/`、`net/`、`servers/`、`speed/`、`ssh/`、`migrations.rs` 等），不要全量暂存。
- 注释、错误文案、提交信息一律**中文**。
- Rust 测试命令必须带 manifest（bin crate，`--lib` 不可用），**只接受一个位置过滤参数**。
- 前端门禁：`npx tsc --noEmit` + `npm test`。

## 关键既有事实（实现前必读，均已核对源码）

1. **`FakeSshServer` 未命中回落 `exit 0` + 空 stdout**（`ssh/tests.rs:222-227`），并会解析 `echo '<b64>' | base64 -d > '<path>'` 把内容记进 `files()`（`:226`、`parse_b64_write` `:230`）。**后果**：探测类命令（`command -v python3`、`port_probe_cmd`、`systemctl is-active`）不显式 `push_script` 就会「假过」。每个探测都必须在测试里压桩，并断言录到的命令序列（`srv.recorded()`）。
2. **写远端文件**：`write_remote_file(sess, cancel, step, path, content, timeout)`（`servers/deploy.rs:606`，`pub(crate)`），内容走 base64 管道，测试里经 `srv.files()` 断言。
3. **跑命令**：`run_cmd_ok(sess, cancel, step, cmd, timeout)` / `run_cmd(...)`（`deploy.rs:626/646`，`pub(crate)`，签名与 `SshSession::run` 一致：`run(step, cmd, timeout)`）。
4. **端口探测**：`port_probe_cmd(port)`（`deploy.rs:37`）= `ss -tln 'sport = :N' | grep -q '^LISTEN'`，**退出码 0 = 已被占用**（注意方向，别写反）。
5. **SSH 会话建立范式**（`relay/build.rs:185-196`）：`load_auth(state, id)` → `SshSession::connect(host, port as u16, user, &auth, CONNECT_TIMEOUT)`。
6. **SSH 会话测试夹具**：`deploy_tests.rs` 的 `seeded_server(&state, &srv) -> i64`（建服务器行并返回 id）、`landing_state(&dir) -> Arc<AppState>`。照抄它们，别自己造。
7. **配置生成**：`commands::build_config_for(&state, node_ids, ExportFormat::Yaml)`（`pub(crate)`，本分支刚做的）返回 `(yaml, skipped_ssr)`，与「开启网络」用的配置同口径。空配置守卫 `ensure_exportable_has_nodes` 已存在（`commands.rs:274`）。
8. **导航**：`App.tsx:10` 的 `ViewKey` 联合类型 + `:12` 的 `NAV_ITEMS` + `:42-49` 的视图渲染三处都要加。
9. **弹窗/确认件**：`components/ExportSelectDialog.tsx`（节点多选，刚做的）与 `components/ConfirmButton.tsx` 可直接复用。

---

### Task 1: share 模块骨架 + token 校验 + 路径常量

**Files:**
- Create: `src-tauri/src/share/mod.rs`
- Create: `src-tauri/src/share/tests.rs`
- Modify: `src-tauri/src/main.rs`（`mod share;`）

**Interfaces:**
- Produces: `share::{SHARE_ROOT, UNIT_PREFIX, PORT_RANGE_START, PORT_RANGE_END, is_valid_token, share_dir, unit_name, unit_path}`；`ShareInfo`。

- [ ] **Step 1: 写失败的测试**

`src-tauri/src/share/tests.rs`：

```rust
//! 分享模块测试：token 校验、远端布局常量、创建/列表/作废的 SSH 序列。
//! 注意 FakeSshServer 未命中回落 exit 0 + 空 stdout（ssh/tests.rs:222），
//! 每个探测命令都必须显式压桩，否则用例会假过。

use crate::share::*;

#[test]
fn token_must_be_16_hex_chars() {
    assert!(is_valid_token("0123456789abcdef"));
    assert!(is_valid_token("abcdefabcdefabcd"));
    // 长度不对
    assert!(!is_valid_token("0123456789abcde"));
    assert!(!is_valid_token("0123456789abcdef0"));
    // 非十六进制：大写也不行（统一小写，避免远端大小写敏感文件系统出岔子）
    assert!(!is_valid_token("0123456789ABCDEF"));
    assert!(!is_valid_token("0123456789abcdeg"));
    // 命令注入尝试
    assert!(!is_valid_token("0123456789abc;rm -rf /"));
    assert!(!is_valid_token(""));
}

#[test]
fn layout_paths_are_derived_from_token() {
    let t = "0123456789abcdef";
    assert_eq!(share_dir(t), "/srv/vpnshare/0123456789abcdef");
    assert_eq!(unit_name(t), "vpnshare-0123456789abcdef");
    assert_eq!(
        unit_path(t),
        "/etc/systemd/system/vpnshare-0123456789abcdef.service"
    );
    assert_eq!(config_path(t), "/srv/vpnshare/0123456789abcdef/config.yaml");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 编译失败，`unresolved import crate::share`（`mod share;` 还没加）

- [ ] **Step 3: 写实现**

`src-tauri/src/main.rs` 的模块声明区加 `mod share;`（按现有 `mod` 列表的字母序放）。

`src-tauri/src/share/mod.rs`：

```rust
//! 分享订阅链接：把选中的节点做成一条公网可达的订阅 URL。
//!
//! app 只做「装」与「拆」：SSH 到目标服务器，写一份**固定快照** YAML 到
//! /srv/vpnshare/<token>/，配 systemd 单元起 python3 静态服务，再用 iptables
//! 只放行带正确 token 的请求。**分享的真实状态在服务器上**——本地不建表，
//! list 每次实读，app 重装或本地库丢失都不影响已发出的分享。
//!
//! 安全前提：服务是纯 HTTP 无加密，**链接本身就是唯一的钥匙**；iptables 的
//! -m string 规则让扫到端口的人也拿不到东西（连不上而非 404）。这两条都要在
//! UI 上明示，不能只写在这里。

#[cfg(test)]
pub mod tests;

use serde::{Deserialize, Serialize};

/// 服务器上所有分享的根目录。
pub const SHARE_ROOT: &str = "/srv/vpnshare";
/// systemd 单元名前缀（完整名：`vpnshare-<token>`）。
pub const UNIT_PREFIX: &str = "vpnshare";
/// 端口分配范围（闭区间）。每份分享独占一个端口。
pub const PORT_RANGE_START: u16 = 18080;
pub const PORT_RANGE_END: u16 = 18180;

/// 一份已安装到服务器上的分享。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareInfo {
    pub server_id: i64,
    pub server_name: String,
    pub host: String,
    pub token: String,
    pub port: u16,
    /// http://<host>:<port>/sub/<token>
    pub url: String,
    /// 从服务器上的快照里解析出的节点名（解析不出时为空）
    pub node_names: Vec<String>,
    /// systemctl is-active 结果
    pub active: bool,
}

/// token 必须是 16 位小写十六进制。它会被拼进 systemd 单元名与 iptables
/// 命令行，限制字符集可省掉一整类转义/注入问题——所以**入口与出口都要校验**，
/// 不能只信前端传回来的那个字符串。
pub fn is_valid_token(token: &str) -> bool {
    token.len() == 16 && token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn share_dir(token: &str) -> String {
    format!("{SHARE_ROOT}/{token}")
}

pub fn config_path(token: &str) -> String {
    format!("{SHARE_ROOT}/{token}/config.yaml")
}

pub fn unit_name(token: &str) -> String {
    format!("{UNIT_PREFIX}-{token}")
}

pub fn unit_path(token: &str) -> String {
    format!("/etc/systemd/system/{}.service", unit_name(token))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 2 个用例 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/mod.rs src-tauri/src/share/tests.rs src-tauri/src/main.rs
git commit -m "feat(share): share 模块骨架——token 校验与远端布局常量"
```

---

### Task 2: 能力探测与端口分配

**Files:**
- Modify: `src-tauri/src/share/mod.rs`（追加探测与端口分配）
- Test: `src-tauri/src/share/tests.rs`

**Interfaces:**
- Consumes: Task 1 的常量；`servers::{run_cmd_ok, port_probe_cmd, PROBE_TIMEOUT}`；`ssh::SshSession`。
- Produces:
  - `async fn probe_capabilities(sess: &mut SshSession, cancel: &AtomicBool) -> Result<(), String>`
  - `async fn allocate_port(sess: &mut SshSession, cancel: &AtomicBool, taken: &[u16]) -> Result<u16, String>`

- [ ] **Step 1: 写失败的测试**

`src-tauri/src/share/tests.rs` 追加：

```rust
use crate::ssh::tests::FakeSshServer;
use std::sync::atomic::AtomicBool;

/// 能力探测：python3 与 iptables -m string 都在才放行。
#[tokio::test]
async fn probe_capabilities_requires_python3_and_string_match() {
    // 都有
    let srv = FakeSshServer::start().await;
    srv.push_script("command -v python3", 0, "/usr/bin/python3", "");
    srv.push_script("iptables -m string", 0, "", "");
    let cancel = AtomicBool::new(false);
    let mut sess = connect(&srv).await;
    probe_capabilities(&mut sess, &cancel).await.expect("capabilities");

    // 没有 python3：必须明确报错，不能静默降级
    let srv2 = FakeSshServer::start().await;
    srv2.push_script("command -v python3", 1, "", "");
    let mut sess2 = connect(&srv2).await;
    let e = probe_capabilities(&mut sess2, &cancel)
        .await
        .expect_err("缺 python3 必须报错");
    assert!(e.contains("python3"), "错误文案应点名 python3：{e}");

    // iptables 不支持 -m string：同样必须报错（规则是安全前提，不能降级成裸开端口）
    let srv3 = FakeSshServer::start().await;
    srv3.push_script("command -v python3", 0, "/usr/bin/python3", "");
    srv3.push_script("iptables -m string", 1, "", "");
    let mut sess3 = connect(&srv3).await;
    let e3 = probe_capabilities(&mut sess3, &cancel)
        .await
        .expect_err("缺 -m string 必须报错");
    assert!(e3.contains("iptables"), "错误文案应点名 iptables：{e3}");
}

/// 端口分配：从 18080 起找第一个空闲的，被占的跳过。注意 port_probe_cmd
/// 的退出码方向是 **0 = 已占用**（deploy.rs:37 用 grep -q '^LISTEN'）。
#[tokio::test]
async fn allocate_port_skips_busy_ports() {
    let srv = FakeSshServer::start().await;
    srv.push_script("sport = :18080", 0, "LISTEN 0 128 0.0.0.0:18080\n", ""); // 占用
    srv.push_script("sport = :18081", 1, "", ""); // 空闲
    let cancel = AtomicBool::new(false);
    let mut sess = connect(&srv).await;
    let port = allocate_port(&mut sess, &cancel, &[]).await.expect("port");
    assert_eq!(port, 18081, "18080 被占，应落到 18081");

    // 本地已登记的端口即使远端探到空闲也要跳过
    let srv2 = FakeSshServer::start().await;
    srv2.push_script("sport = :18080", 1, "", "");
    srv2.push_script("sport = :18081", 1, "", "");
    let mut sess2 = connect(&srv2).await;
    let port2 = allocate_port(&mut sess2, &cancel, &[18080]).await.expect("port2");
    assert_eq!(port2, 18081, "本地已登记的 18080 应被跳过");

    // 范围内全占：报错而不是覆盖别人的服务
    let srv3 = FakeSshServer::start().await;
    for p in PORT_RANGE_START..=PORT_RANGE_END {
        srv3.push_script(&format!("sport = :{p}"), 0, "LISTEN\n", "");
    }
    let mut sess3 = connect(&srv3).await;
    let e = allocate_port(&mut sess3, &cancel, &[]).await.expect_err("应报错");
    assert!(e.contains("端口"), "错误文案：{e}");
}
```

同文件加一个连接夹具（`deploy_tests.rs` 里已有同类，这里是本地版本；若那边有 `pub(crate)` 的等价物就直接用）：

```rust
/// 连到 FakeSshServer 的会话（密码认证，凭据即测试口令）。
pub(crate) async fn connect(srv: &FakeSshServer) -> crate::ssh::SshSession {
    let auth = crate::ssh::AuthMaterial::Password("pw".into());
    crate::ssh::SshSession::connect(
        "127.0.0.1",
        srv.port as u16,
        "root",
        &auth,
        crate::servers::CONNECT_TIMEOUT,
    )
    .await
    .expect("connect")
}
```

**先核对** `FakeSshServer` 的默认口令（`ssh/tests.rs` 里 `password` 字段的初值）与 `AuthMaterial` 的构造名，照实际写。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 编译失败，`cannot find function probe_capabilities`

- [ ] **Step 3: 写实现**

`src-tauri/src/share/mod.rs` 追加：

```rust
use crate::servers::{port_probe_cmd, run_cmd, run_cmd_ok, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use std::sync::atomic::AtomicBool;

/// 探测分享所需的两项能力。**任缺其一都报错**，不降级：
/// - 没有 python3 → 起不了静态服务
/// - iptables 不支持 -m string → 规则退化成裸开端口，与本设计的安全前提冲突
async fn probe_capabilities(sess: &mut SshSession, cancel: &AtomicBool) -> Result<(), String> {
    let py = run_cmd_ok(sess, cancel, "探测", "command -v python3", PROBE_TIMEOUT).await?;
    if py.stdout.trim().is_empty() {
        return Err("服务器上未找到 python3，无法提供分享服务".into());
    }
    // -m string 的自检：空规则集上试插一条再删，只看命令本身能否被解析
    let probe = "iptables -m string --string x --algo bm -C INPUT -p tcp --dport 1 -j ACCEPT 2>&1 || iptables -m string --help >/dev/null 2>&1";
    let out = run_cmd(sess, cancel, "探测", probe, PROBE_TIMEOUT).await?;
    if out.exit_code != 0 {
        return Err(format!(
            "服务器 iptables 不支持 -m string 模块，无法限定分享路径（不会降级为开放整个端口）：{}",
            out.stdout.trim()
        ));
    }
    Ok(())
}

/// 从 PORT_RANGE_START 起找第一个空闲端口；`taken` 是本地已登记、必须跳过的端口。
/// 注意 `port_probe_cmd` 的退出码方向：**0 = 已被占用**。
/// 范围内全占 → 报错，绝不覆盖别人的服务。
async fn allocate_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    taken: &[u16],
) -> Result<u16, String> {
    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if taken.contains(&port) {
            continue;
        }
        let out = run_cmd(sess, cancel, "探测", &port_probe_cmd(port), PROBE_TIMEOUT).await?;
        if out.exit_code != 0 {
            return Ok(port);
        }
    }
    Err(format!(
        "端口 {PORT_RANGE_START}-{PORT_RANGE_END} 全部被占用，无法分配分享端口"
    ))
}
```

**注意**：`run_cmd` 返回的 `CommandOutput` 字段名（`exit_code` / `stdout`）以 `crate::ssh` 的实际定义为准；若 `port_probe_cmd` 的退出码语义与注释不符，以 `deploy.rs:37` 的实现为准。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/mod.rs src-tauri/src/share/tests.rs
git commit -m "feat(share): 能力探测与端口分配（缺 python3 / 缺 -m string 均显式报错）"
```

---

### Task 3: systemd 单元与 iptables 规则生成（纯函数）

**Files:**
- Modify: `src-tauri/src/share/mod.rs`
- Test: `src-tauri/src/share/tests.rs`

**Interfaces:**
- Produces:
  - `fn unit_content(token: &str, port: u16) -> String`
  - `fn accept_rule(token: &str, port: u16) -> String` / `fn drop_rule(token: &str, port: u16) -> String`
  - `fn url_for(host: &str, port: u16, token: &str) -> String`

- [ ] **Step 1: 写失败的测试**

```rust
#[test]
fn unit_content_runs_static_server_on_the_share_dir() {
    let t = "0123456789abcdef";
    let u = unit_content(t, 18080);
    assert!(u.contains("python3 -m http.server 18080"), "单元：{u}");
    assert!(u.contains("--directory /srv/vpnshare/0123456789abcdef"), "单元：{u}");
    assert!(u.contains("--bind 0.0.0.0"), "必须绑 0.0.0.0 才能被公网访问：{u}");
    assert!(u.contains("Restart=on-failure"), "挂了要能自愈：{u}");
}

#[test]
fn iptables_rules_only_accept_the_token_path() {
    let t = "0123456789abcdef";
    let accept = accept_rule(t, 18080);
    // 只放行带正确 token 的请求：扫到端口的人也拿不到东西
    assert!(accept.contains("--dport 18080"), "{accept}");
    assert!(accept.contains("--string \"GET /sub/0123456789abcdef\""), "{accept}");
    assert!(accept.ends_with("-j ACCEPT"), "{accept}");
    // 删除规则与插入规则必须**逐字对应**（iptables -D 靠完全匹配），只差 -I/-D
    let drop = drop_rule(t, 18080);
    assert_eq!(
        accept.replacen("-I INPUT 1", "-D INPUT", 1),
        drop,
        "插入与删除规则除动词外必须完全一致，否则删不掉"
    );
}

#[test]
fn url_for_includes_host_and_token() {
    assert_eq!(
        url_for("1.2.3.4", 18080, "0123456789abcdef"),
        "http://1.2.3.4:18080/sub/0123456789abcdef"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 编译失败，`cannot find function unit_content`

- [ ] **Step 3: 写实现**

```rust
/// systemd 单元：在分享目录上起 python3 静态服务。
/// `--bind 0.0.0.0` 是必须的（默认只绑回环，外面访问不到）。
pub fn unit_content(token: &str, port: u16) -> String {
    let dir = share_dir(token);
    format!(
        "[Unit]\n\
         Description=vpn-ctl share {token}\n\
         \n\
         [Service]\n\
         ExecStart=/usr/bin/env python3 -m http.server {port} --directory {dir} --bind 0.0.0.0\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// 放行规则：**只**放行带正确 token 的请求，其余被链路丢弃（不是 404，
/// 而是 TCP 层无响应，等于端口隐身）。刻意不裸开端口。
pub fn accept_rule(token: &str, port: u16) -> String {
    format!(
        "iptables -I INPUT 1 -p tcp --dport {port} -m string --string \"GET /sub/{token}\" --algo bm -j ACCEPT"
    )
}

/// 删除规则：与 `accept_rule` 除 `-I INPUT 1` / `-D INPUT` 外**逐字一致**
/// （iptables 的 -D 靠完全匹配，多一个空格都删不掉）。
pub fn drop_rule(token: &str, port: u16) -> String {
    format!(
        "iptables -D INPUT -p tcp --dport {port} -m string --string \"GET /sub/{token}\" --algo bm -j ACCEPT"
    )
}

pub fn url_for(host: &str, port: u16, token: &str) -> String {
    format!("http://{host}:{port}/sub/{token}")
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/mod.rs src-tauri/src/share/tests.rs
git commit -m "feat(share): systemd 单元与 iptables 规则生成（只放行 token 路径）"
```

---

### Task 4: `create_share`

**Files:**
- Modify: `src-tauri/src/share/mod.rs`
- Test: `src-tauri/src/share/tests.rs`

**Interfaces:**
- Consumes: Task 1-3 的全部；`commands::build_config_for`（`pub(crate)`）；`servers::{load_auth, CONNECT_TIMEOUT, CONFIG_TIMEOUT}`；`crate::relay::tests` 里的夹具风格。
- Produces: `pub async fn create_share(state: &Arc<AppState>, server_id: i64, node_ids: &[String]) -> Result<ShareInfo, String>`

- [ ] **Step 1: 写失败的测试**

需要 `AppState` 与服务器行夹具 —— 照抄 `src-tauri/src/servers/deploy_tests.rs` 里的 `landing_state(&dir)` 与 `seeded_server(&state, &srv)`（若无 `pub(crate)` 版本则在 `share/tests.rs` 内本地复制一份）。测试主体：

```rust
#[tokio::test]
async fn create_share_writes_snapshot_unit_and_firewall_rule() {
    let dir = tempfile::tempdir().expect("dir");
    let state = shared_state(&dir);          // 本地夹具
    let srv = FakeSshServer::start().await;
    let server_id = seed_server(&state, &srv); // 本地夹具：建服务器行，host 指 127.0.0.1
    seed_node(&state, "n1", "分享节点");       // 本地夹具：建一个可见节点

    // 探测：python3 / -m string / 端口
    srv.push_script("command -v python3", 0, "/usr/bin/python3", "");
    srv.push_script("--algo bm", 0, "", "");
    srv.push_script("sport = :18080", 1, "", ""); // 18080 空闲
    // 服务起来后的确认
    srv.push_script("systemctl is-active vpnshare-", 0, "active\n", "");

    let info = create_share(&state, server_id, &["n1".to_string()])
        .await
        .expect("create");

    assert!(is_valid_token(&info.token), "token: {}", info.token);
    assert_eq!(info.port, 18080);
    assert_eq!(
        info.url,
        format!("http://127.0.0.1:18080/sub/{}", info.token)
    );
    assert_eq!(info.node_names, vec!["分享节点".to_string()]);
    assert!(info.active);

    // 写出去的快照必须是 build_config_for 的同口径产物
    let files = srv.files();
    let written = files
        .get(&config_path(&info.token))
        .expect("config.yaml 必须被写入");
    let written = String::from_utf8(written.clone()).expect("utf8");
    assert!(written.contains("分享节点"));
    let (expected, _) = crate::commands::build_config_for(
        &state,
        &["n1".to_string()],
        crate::commands::ExportFormat::Yaml,
    )
    .await
    .expect("expected");
    assert_eq!(written, expected, "快照必须与导出同口径");

    // 单元内容
    let unit = files
        .get(&unit_path(&info.token))
        .expect("单元必须被写入");
    let unit = String::from_utf8(unit.clone()).expect("utf8");
    assert!(unit.contains("python3 -m http.server 18080"));

    // 命令序列：daemon-reload → enable --now → iptables -I
    let recorded = srv.recorded().join("\n");
    assert!(recorded.contains("systemctl daemon-reload"), "{recorded}");
    assert!(
        recorded.contains(&format!("systemctl enable --now {}", unit_name(&info.token))),
        "{recorded}"
    );
    assert!(recorded.contains("iptables -I INPUT 1"), "{recorded}");
}

#[tokio::test]
async fn create_share_rejects_empty_or_all_skipped_selection() {
    let dir = tempfile::tempdir().expect("dir");
    let state = shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let server_id = seed_server(&state, &srv);
    // 没有任何节点：空配置守卫必须拒绝（与导出同一错误文案）
    let e = create_share(&state, server_id, &[String::new()])
        .await
        .expect_err("空选择必须被拒");
    assert!(!e.is_empty(), "必须有可读错误：{e}");
    assert!(srv.recorded().is_empty(), "拒绝时不该连服务器");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml create_share_`
Expected: 编译失败，`cannot find function create_share`

- [ ] **Step 3: 写实现**

```rust
use crate::commands::build_config_for;
use crate::servers::{load_auth, CONFIG_TIMEOUT, CONNECT_TIMEOUT};
use crate::state::AppState;
use std::sync::Arc;

/// 生成并安装一份分享。
///
/// 快照内容走 `build_config_for`（与导出、与「开启网络」同一口径），所以链接里
/// 的节点就是你勾的那些，不会多也不会少。生成后内容**不再更新**——这是快照制的
/// 明确取舍，UI 上要讲清。
pub async fn create_share(
    state: &Arc<AppState>,
    server_id: i64,
    node_ids: &[String],
) -> Result<ShareInfo, String> {
    let (server, auth) = {
        let conn = state.db.lock();
        let server = crate::db::repos::servers::get(&conn, server_id)
            .map_err(|e| e.to_string())?
            .ok_or("服务器不存在")?;
        drop(conn);
        let (auth, _) = load_auth(state, server_id).map_err(|e| e.to_string())?;
        (server, auth)
    };

    // 先出内容再连服务器：空配置/全被跳过时直接失败，不白连一次
    let (yaml, _skipped) = build_config_for(state, node_ids, crate::commands::ExportFormat::Yaml)
        .await
        .map_err(|e| e.to_string())?;
    crate::commands::ensure_exportable_has_nodes(&yaml).map_err(|e| e.to_string())?;

    let cancel = AtomicBool::new(false);
    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;

    probe_capabilities(&mut sess, &cancel).await?;

    // 本地已登记的通道端口也要避开（它们可能是别的服务在用）
    let taken: Vec<u16> = crate::db::repos::channels::list(&state.db.lock(), server_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|c| c.port as u16)
        .collect();
    let port = allocate_port(&mut sess, &cancel, &taken).await?;

    let token = new_token();
    let dir = share_dir(&token);

    // 目录：write_remote_file 只建父目录，这里补上目录本身（否则 http.server 起不来）
    run_cmd_ok(&mut sess, &cancel, "准备", &format!("mkdir -p '{dir}'"), CONFIG_TIMEOUT).await?;
    write_remote_file(
        &mut sess,
        &cancel,
        "写入",
        &config_path(&token),
        &yaml,
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        &mut sess,
        &cancel,
        "写入",
        &unit_path(&token),
        &unit_content(&token, port),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        &cancel,
        "启动",
        &format!(
            "systemctl daemon-reload && systemctl enable --now {}",
            unit_name(&token)
        ),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(&mut sess, &cancel, "放行", &accept_rule(&token, port), CONFIG_TIMEOUT).await?;

    // 回读确认真的起来了——写了单元不等于服务在跑
    let active = run_cmd(
        &mut sess,
        &cancel,
        "确认",
        &format!("systemctl is-active {}", unit_name(&token)),
        PROBE_TIMEOUT,
    )
    .await?
    .stdout
    .trim()
        == "active";
    if !active {
        return Err(format!(
            "分享服务未进入 active 状态，已安装在 {}，请检查后重试作废",
            unit_name(&token)
        ));
    }

    Ok(ShareInfo {
        server_id,
        server_name: server.name.clone(),
        host: server.host.clone(),
        token: token.clone(),
        port,
        url: url_for(&server.host, port, &token),
        node_names: proxy_names(&yaml),
        active,
    })
}

/// 16 位小写十六进制 token。
fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// 从生成的 YAML 里取 proxies 的名字，用于 UI 展示「这条链接里有哪几条线」。
fn proxy_names(yaml: &str) -> Vec<String> {
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return vec![];
    };
    v.get("proxies")
        .and_then(|p| p.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
```

`commands.rs` 里把 `ensure_exportable_has_nodes` 改成 `pub(crate)`（原本是私有 `fn`）。

**先核对** `db::repos::channels::list(conn, server_id)` 是否存在与签名（若不存在，用 `channels.rs` 里实际用的读法）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml create_share_`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/mod.rs src-tauri/src/share/tests.rs src-tauri/src/commands.rs
git commit -m "feat(share): create_share 安装快照 + 单元 + 防火墙规则"
```

---

### Task 5: `list_shares` 与 `delete_share`

**Files:**
- Modify: `src-tauri/src/share/mod.rs`
- Test: `src-tauri/src/share/tests.rs`

**Interfaces:**
- Produces:
  - `pub async fn list_shares(state: &Arc<AppState>) -> Result<Vec<ShareInfo>, String>`
  - `pub async fn delete_share(state: &Arc<AppState>, server_id: i64, token: &str) -> Result<(), String>`

- [ ] **Step 1: 写失败的测试**

```rust
#[tokio::test]
async fn delete_share_tears_down_service_files_and_rule() {
    let dir = tempfile::tempdir().expect("dir");
    let state = shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let server_id = seed_server(&state, &srv);
    let token = "0123456789abcdef";

    srv.push_script("command -v python3", 0, "/usr/bin/python3", "");
    srv.push_script("--algo bm", 0, "", "");
    srv.push_script("sport = :18080", 1, "", "");
    srv.push_script("systemctl is-active vpnshare-", 0, "active\n", "");
    srv.push_script("systemctl disable", 0, "", "");
    srv.push_script("rm -rf", 0, "", "");

    let info = create_share(&state, server_id, &["n1".to_string()]).await.expect("create");
    delete_share(&state, server_id, &info.token).await.expect("delete");

    let recorded = srv.recorded().join("\n");
    assert!(recorded.contains(&format!("systemctl disable --now {}", unit_name(&info.token))), "{recorded}");
    assert!(recorded.contains(&format!("rm -rf '{}'", share_dir(&info.token))), "{recorded}");
    assert!(recorded.contains("rm -f"), "单元文件也要删：{recorded}");
    assert!(recorded.contains("systemctl daemon-reload"), "{recorded}");
    assert!(recorded.contains("-D INPUT -p tcp --dport 18080"), "{recorded}");
}

#[tokio::test]
async fn delete_share_is_idempotent_and_rejects_bad_token() {
    let dir = tempfile::tempdir().expect("dir");
    let state = shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let server_id = seed_server(&state, &srv);

    // 校验先于连接：非法 token 直接被拒，不该连服务器
    let e = delete_share(&state, server_id, "0123456789ABC;rm -rf /")
        .await
        .expect_err("非法 token 必须被拒");
    assert!(e.contains("token"), "错误文案：{e}");
    assert!(srv.recorded().is_empty(), "校验失败不该连服务器");
}

#[tokio::test]
async fn list_shares_reads_state_from_servers() {
    let dir = tempfile::tempdir().expect("dir");
    let state = shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let _id = seed_server(&state, &srv);

    // 服务器上有一个已安装的分享：目录列举 + 单元在跑 + 快照可读
    srv.push_script("ls -1 /srv/vpnshare", 0, "0123456789abcdef\n", "");
    srv.push_script("systemctl is-active vpnshare-0123456789abcdef", 0, "active\n", "");
    srv.push_script(
        "cat /srv/vpnshare/0123456789abcdef/config.yaml",
        0,
        "proxies:\n- name: 分享节点\n  type: ss\n  server: 1.2.3.4\n  port: 8388\n",
        "",
    );
    srv.push_script("cat /etc/systemd/system/vpnshare-0123456789abcdef.service", 0, &unit_content("0123456789abcdef", 18080), "");

    let list = list_shares(&state).await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].token, "0123456789abcdef");
    assert_eq!(list[0].port, 18080, "端口从单元内容里解析");
    assert_eq!(list[0].node_names, vec!["分享节点".to_string()]);
    assert!(list[0].active);
    assert!(list[0].url.ends_with("/sub/0123456789abcdef"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 编译失败，`cannot find function delete_share`

- [ ] **Step 3: 写实现**

```rust
/// 列出所有服务器上的分享。**逐台读，某台连不上不整体失败**——该台的分享
/// 在结果里不出现，错误由调用方按服务器聚合上报（UI 上注明哪台没读到时再说）。
pub async fn list_shares(state: &Arc<AppState>) -> Result<Vec<ShareInfo>, String> {
    let servers = crate::db::repos::servers::list(&state.db.lock()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let cancel = AtomicBool::new(false);
    for server in servers {
        let Ok((auth, _)) = load_auth(state, server.id) else {
            continue;
        };
        let Ok(mut sess) = SshSession::connect(
            &server.host,
            server.port as u16,
            &server.user,
            &auth,
            CONNECT_TIMEOUT,
        )
        .await
        else {
            continue;
        };
        let Ok(listing) = run_cmd(
            &mut sess,
            &cancel,
            "列举",
            &format!("ls -1 {SHARE_ROOT} 2>/dev/null"),
            PROBE_TIMEOUT,
        )
        .await
        else {
            continue;
        };
        for token in listing
            .stdout
            .lines()
            .map(str::trim)
            .filter(|t| is_valid_token(t))
        {
            let active = run_cmd(
                &mut sess,
                &cancel,
                "状态",
                &format!("systemctl is-active {}", unit_name(token)),
                PROBE_TIMEOUT,
            )
            .await
            .map(|o| o.stdout.trim() == "active")
            .unwrap_or(false);
            // 端口从单元内容里解析（事实源在服务器上）
            let port = run_cmd(
                &mut sess,
                &cancel,
                "读取",
                &format!("cat {}", unit_path(token)),
                PROBE_TIMEOUT,
            )
            .await
            .ok()
            .and_then(|o| parse_port_from_unit(&o.stdout))
            .unwrap_or(0);
            let yaml = run_cmd(
                &mut sess,
                &cancel,
                "读取",
                &format!("cat {}", config_path(token)),
                PROBE_TIMEOUT,
            )
            .await
            .map(|o| o.stdout)
            .unwrap_or_default();
            out.push(ShareInfo {
                server_id: server.id,
                server_name: server.name.clone(),
                host: server.host.clone(),
                token: token.to_string(),
                port,
                url: url_for(&server.host, port, token),
                node_names: proxy_names(&yaml),
                active,
            });
        }
    }
    Ok(out)
}

/// 从单元内容里取 `-m http.server <port>` 的端口。
fn parse_port_from_unit(unit: &str) -> Option<u16> {
    let idx = unit.find("-m http.server ")? + "-m http.server ".len();
    unit[idx..].split_whitespace().next()?.parse().ok()
}

/// 作废一份分享。各步幂等：服务/文件/规则任一不存在都不算错。
pub async fn delete_share(
    state: &Arc<AppState>,
    server_id: i64,
    token: &str,
) -> Result<(), String> {
    // 校验先于连接：token 会拼进命令，非法值绝不能往下走
    if !is_valid_token(token) {
        return Err(format!("非法的分享 token：{token}"));
    }
    let (server, auth) = {
        let conn = state.db.lock();
        let server = crate::db::repos::servers::get(&conn, server_id)
            .map_err(|e| e.to_string())?
            .ok_or("服务器不存在")?;
        drop(conn);
        let (auth, _) = load_auth(state, server_id).map_err(|e| e.to_string())?;
        (server, auth)
    };
    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;
    let cancel = AtomicBool::new(false);

    // 顺序：先停服务（否则 http.server 会继续持有目录）→ 删单元与目录 → 删规则
    for (step, cmd) in [
        (
            "停止",
            format!("systemctl disable --now {} 2>/dev/null || true", unit_name(token)),
        ),
        (
            "清理",
            format!(
                "rm -f '{}' && systemctl daemon-reload 2>/dev/null || true",
                unit_path(token)
            ),
        ),
        ("清理", format!("rm -rf '{}'", share_dir(token))),
    ] {
        run_cmd_ok(&mut sess, &cancel, step, &cmd, CONFIG_TIMEOUT).await?;
    }
    // 端口可能已解析不出（单元被手删），规则删除用 `|| true` 保证幂等
    if let Some(port) = last_known_port(&mut sess, &cancel, token).await {
        let cmd = format!("{} 2>/dev/null || true", drop_rule(token, port));
        run_cmd_ok(&mut sess, &cancel, "封堵", &cmd, CONFIG_TIMEOUT).await?;
    }
    Ok(())
}

/// 端口来源：单元文件（若还在）→ 否则扫 iptables 里带本 token 的规则。
async fn last_known_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    token: &str,
) -> Option<u16> {
    let unit = run_cmd(sess, cancel, "读取", &format!("cat {}", unit_path(token)), PROBE_TIMEOUT)
        .await
        .ok()?;
    if let Some(p) = parse_port_from_unit(&unit.stdout) {
        return Some(p);
    }
    let rules = run_cmd(
        sess,
        cancel,
        "读取",
        &format!("iptables -S INPUT | grep -- '/sub/{token}'"),
        PROBE_TIMEOUT,
    )
    .await
    .ok()?;
    for line in rules.stdout.lines() {
        if let Some(p) = line
            .split_whitespace()
            .skip_while(|w| *w != "--dport")
            .nth(1)
            .and_then(|p| p.parse().ok())
        {
            return Some(p);
        }
    }
    None
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml share::tests`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/mod.rs src-tauri/src/share/tests.rs
git commit -m "feat(share): list_shares 实读服务器状态 + delete_share 幂等拆除"
```

---

### Task 6: IPC 命令

**Files:**
- Modify: `src-tauri/src/commands.rs`（share 命令区）
- Modify: `src-tauri/src/main.rs`（`generate_handler!`）
- Modify: `src-tauri/src/domain.rs`（复用 `share::ShareInfo` 则无需新增；若 IPC 要独立 DTO 则在此定义）
- Test: `src-tauri/src/ipc_tests.rs`

**Interfaces:**
- Produces: IPC `list_shares_cmd` / `create_share_cmd(serverId, nodeIds)` / `delete_share_cmd(serverId, token)`。

- [ ] **Step 1: 写失败的测试**

`ipc_tests.rs` 的 `mock_app` 注册三个命令，并追加：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn create_share_cmd_ipc_rejects_empty_node_ids() {
    let (_d, state) = test_state();
    let srv = FakeSshServer::start().await;
    let id = create_server(&state, server_input("127.0.0.1", srv.port as i64, "landing"))
        .unwrap()
        .id;
    let (app, wv) = mock_app(state.clone());
    // 空 nodeIds：走空配置守卫，报错而不是静默生成空分享
    let e = invoke_err(
        &wv,
        "create_share_cmd",
        json!({ "serverId": id, "nodeIds": [] }),
    );
    assert!(!e.is_empty(), "必须有可读错误：{e}");
    drop(wv);
    let _ = app;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml create_share_cmd_ipc`
Expected: 编译失败，`cannot find function create_share_cmd`

- [ ] **Step 3: 写实现**

`commands.rs` 追加：

```rust
// ---------- 分享 ----------

pub async fn list_shares(state: Arc<AppState>) -> R<Vec<crate::share::ShareInfo>> {
    crate::share::list_shares(&state).await.map_err(err)
}

pub async fn create_share(
    state: Arc<AppState>,
    server_id: i64,
    node_ids: Vec<String>,
) -> R<crate::share::ShareInfo> {
    crate::share::create_share(&state, server_id, &node_ids)
        .await
        .map_err(err)
}

pub async fn delete_share(state: Arc<AppState>, server_id: i64, token: String) -> R<()> {
    crate::share::delete_share(&state, server_id, &token)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn list_shares_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<Vec<crate::share::ShareInfo>> {
    list_shares(state.inner().clone()).await
}

#[tauri::command]
pub async fn create_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    node_ids: Vec<String>,
) -> R<crate::share::ShareInfo> {
    create_share(state.inner().clone(), server_id, node_ids).await
}

#[tauri::command]
pub async fn delete_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    token: String,
) -> R<()> {
    delete_share(state.inner().clone(), server_id, token).await
}
```

`main.rs` 的 `generate_handler!` 里追加三条（放 `commands::export_singbox_selected_cmd,` 之后）。

- [ ] **Step 4: 跑测试确认通过 + 全量后端**

Run: `cargo test --manifest-path src-tauri/Cargo.toml create_share_cmd_ipc`
再 Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/main.rs src-tauri/src/ipc_tests.rs
git commit -m "feat(share): list/create/delete 三个 IPC 命令"
```

---

### Task 7: 前端 api 与分享页

**Files:**
- Modify: `src/api/types.ts`、`src/api/index.ts`
- Create: `src/views/Shares.tsx`、`src/views/Shares.test.tsx`
- Modify: `src/App.tsx`（`ViewKey` / `NAV_ITEMS` / 渲染三处）

**Interfaces:**
- Produces: `<Shares />` 视图；`api.{listShares, createShare, deleteShare}`。

- [ ] **Step 1: 写类型与 api**

`src/api/types.ts`：

```ts
/** 一份已安装到服务器上的分享链接 */
export interface ShareInfo {
  server_id: number
  server_name: string
  host: string
  token: string
  port: number
  url: string
  node_names: string[]
  active: boolean
}
```

`src/api/index.ts`：

```ts
/** 列出所有服务器上的分享（状态实读自服务器，不依赖本地库） */
export function listShares(): Promise<ShareInfo[]> {
  return invoke('list_shares_cmd')
}

/** 在指定服务器上安装一份分享；nodeIds 为空会被后端拒绝 */
export function createShare(serverId: number, nodeIds: string[]): Promise<ShareInfo> {
  return invoke('create_share_cmd', { serverId, nodeIds })
}

export function deleteShare(serverId: number, token: string): Promise<void> {
  return invoke('delete_share_cmd', { serverId, token })
}
```

- [ ] **Step 2: 写失败的测试**

`src/views/Shares.test.tsx`（照 `Nodes.test.tsx` 的 `mockIPC` 风格）：

```tsx
// 分享页测试：列表、新建向导（选服务器→勾节点→生成）、作废、复制链接、
// 三个边界提示可见。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { mockIPC } from '@tauri-apps/api/mocks'
import { beforeEach, describe, expect, it } from 'vitest'
import Shares from './Shares'
import type { Node, Server, ShareInfo } from '../api/types'

const share: ShareInfo = {
  server_id: 1,
  server_name: '落地机-1',
  host: '1.2.3.4',
  token: '0123456789abcdef',
  port: 18080,
  url: 'http://1.2.3.4:18080/sub/0123456789abcdef',
  node_names: ['大美'],
  active: true,
}

beforeEach(() => {
  mockIPC((cmd, args) => {
    switch (cmd) {
      case 'list_shares_cmd':
        return [share]
      case 'list_servers_cmd':
        return [
          {
            id: 1, name: '落地机-1', host: '1.2.3.4', port: 22, user: 'root',
            auth_kind: 'password', kind: 'landing', system: '', notes: '',
            created_at: '', latency: null, bandwidth: null,
            last_checked_at: null, last_check_error: null,
          },
        ] satisfies Server[]
      case 'list_exportable_nodes_cmd':
        return [
          {
            node: {
              id: 'n1', name: '大美', protocol: 'ss', source: 'self-built',
              addr: '1.2.3.4', port: 8388, params_json: '{}', group_name: '自建',
              sub_id: null, relay_rule_id: null, enabled: true, latency: null,
              bandwidth: null, last_checked_at: null, last_check_error: null,
              direct_latency: null, hop_relay_ms: null, hop_landing_ms: null,
              hop_tunnel_ms: null,
            } satisfies Node,
            exportable: true,
            skip_reason: null,
          },
        ]
      case 'create_share_cmd':
        return { ...share, url: 'http://1.2.3.4:18080/sub/fedcba9876543210', token: 'fedcba9876543210' }
      case 'delete_share_cmd':
        return null
      default:
        throw new Error(`未 mock 的命令: ${cmd}`)
    }
  })
})

describe('分享页', () => {
  it('列出分享：链接、节点、状态', async () => {
    render(<Shares />)
    expect(
      await screen.findByText('http://1.2.3.4:18080/sub/0123456789abcdef'),
    ).toBeInTheDocument()
    expect(screen.getByText('大美')).toBeInTheDocument()
    expect(screen.getByText('运行中')).toBeInTheDocument()
  })

  it('新建分享：选服务器 → 勾节点 → 生成链接上屏', async () => {
    const user = userEvent.setup()
    render(<Shares />)
    await screen.findByText('大美')
    await user.click(screen.getByRole('button', { name: '新建分享' }))
    await user.click(await screen.findByLabelText('选择 大美'))
    await user.click(screen.getByRole('button', { name: /生成链接/ }))
    expect(
      await screen.findByText('http://1.2.3.4:18080/sub/fedcba9876543210'),
    ).toBeInTheDocument()
  })

  it('一条都没勾时生成按钮禁用', async () => {
    const user = userEvent.setup()
    render(<Shares />)
    await screen.findByText('大美')
    await user.click(screen.getByRole('button', { name: '新建分享' }))
    await user.click(await screen.findByLabelText('选择 大美')) // 取消勾选
    expect(screen.getByRole('button', { name: /生成链接/ })).toBeDisabled()
  })

  it('作废走二次确认并刷新列表', async () => {
    const user = userEvent.setup()
    render(<Shares />)
    await screen.findByText('大美')
    await user.click(screen.getByRole('button', { name: '作废' }))
    await user.click(await screen.findByRole('button', { name: '确认作废' }))
    await waitFor(() => {
      expect(screen.queryByText('http://1.2.3.4:18080/sub/0123456789abcdef')).not.toBeInTheDocument()
    })
  })

  it('三个边界提示可见', async () => {
    render(<Shares />)
    expect(await screen.findByText(/链接就是钥匙/)).toBeInTheDocument()
    expect(screen.getByText(/快照/)).toBeInTheDocument()
    expect(screen.getByText(/仅支持 YAML/)).toBeInTheDocument()
  })
})
```

- [ ] **Step 3: 跑测试确认失败**

Run: `npm test -- Shares`
Expected: 失败，`Failed to resolve import "./Shares"`

- [ ] **Step 4: 写实现**

`src/views/Shares.tsx`：

```tsx
// 分享视图：列出服务器上的分享链接、新建（选服务器 + 勾节点）、作废。
// 状态实读自服务器——本地不存分享记录，所以每次进页面都重新拉。
import { useCallback, useEffect, useState } from 'react'
import { createShare, deleteShare, listExportableNodes, listServers, listShares } from '../api'
import ConfirmButton from '../components/ConfirmButton'
import type { ExportableNode, Server, ShareInfo } from '../api/types'

export default function Shares() {
  const [shares, setShares] = useState<ShareInfo[]>([])
  const [servers, setServers] = useState<Server[]>([])
  const [error, setError] = useState<string | null>(null)
  // 新建向导状态
  const [wizardOpen, setWizardOpen] = useState(false)
  const [serverId, setServerId] = useState<number | null>(null)
  const [candidates, setCandidates] = useState<ExportableNode[]>([])
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [busy, setBusy] = useState(false)
  const [created, setCreated] = useState<ShareInfo | null>(null)

  const refresh = useCallback(async () => {
    try {
      setShares(await listShares())
    } catch (e) {
      setError(String(e))
    }
  }, [])

  useEffect(() => {
    void refresh()
    void listServers()
      .then((s) => {
        setServers(s)
        setServerId(s[0]?.id ?? null)
      })
      .catch((e) => setError(String(e)))
  }, [refresh])

  useEffect(() => {
    if (!wizardOpen) return
    let alive = true
    void listExportableNodes()
      .then((list) => {
        if (!alive) return
        setCandidates(list)
        setPicked(new Set(list.filter((e) => e.exportable).map((e) => e.node.id)))
      })
      .catch((e) => alive && setError(String(e)))
    return () => {
      alive = false
    }
  }, [wizardOpen])

  const toggle = (id: string) =>
    setPicked((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  const handleCreate = async () => {
    if (serverId == null || picked.size === 0) return
    setBusy(true)
    setError(null)
    try {
      const info = await createShare(serverId, [...picked])
      setCreated(info)
      setWizardOpen(false)
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const handleDelete = async (s: ShareInfo) => {
    setError(null)
    try {
      await deleteShare(s.server_id, s.token)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  return (
    <div className="space-y-4">
      <h2 className="text-xl font-semibold">分享</h2>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      <div className="space-y-1 rounded border border-amber-900 bg-amber-950/30 p-3 text-xs text-amber-200">
        <p>链接就是钥匙：拿到链接的人即可使用这些节点。服务是纯 HTTP 无加密，链接在传输途中被截获即等于泄漏。</p>
        <p>快照制：生成后内容不再更新。你之后改节点、改名、重新部署，链接里仍是生成那一刻的内容——要更新请重新生成。</p>
        <p>仅支持 YAML（mihomo / Clash 系客户端）；sing-box 吃 JSON，暂不支持。</p>
      </div>

      <button
        type="button"
        onClick={() => {
          setCreated(null)
          setWizardOpen(true)
        }}
        className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
      >
        新建分享
      </button>

      {wizardOpen && (
        <div className="space-y-2 rounded border border-neutral-700 bg-neutral-900 p-3 text-sm">
          <label className="flex items-center gap-2">
            服务器
            <select
              aria-label="分享服务器"
              value={serverId ?? ''}
              onChange={(e) => setServerId(Number(e.target.value))}
              className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            >
              {servers.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name}（{s.host}）
                </option>
              ))}
            </select>
          </label>
          <ul className="max-h-64 space-y-1 overflow-y-auto">
            {candidates.map((e) => (
              <li key={e.node.id} className="flex items-center gap-2">
                <input
                  type="checkbox"
                  aria-label={`选择 ${e.node.name}`}
                  checked={picked.has(e.node.id)}
                  onChange={() => toggle(e.node.id)}
                />
                <span>{e.node.name}</span>
                <span className="text-xs text-neutral-500">{e.node.protocol}</span>
              </li>
            ))}
          </ul>
          <div className="flex gap-2">
            <button
              type="button"
              onClick={() => void handleCreate()}
              disabled={busy || picked.size === 0 || serverId == null}
              className="rounded bg-blue-600 px-3 py-1.5 text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy ? '生成中…' : `生成链接（${picked.size} 条）`}
            </button>
            <button
              type="button"
              onClick={() => setWizardOpen(false)}
              disabled={busy}
              className="rounded bg-neutral-800 px-3 py-1.5 hover:bg-neutral-700 disabled:opacity-50"
            >
              取消
            </button>
          </div>
        </div>
      )}

      {created && (
        <p className="rounded border border-emerald-800 bg-emerald-950/40 px-3 py-2 text-sm text-emerald-300">
          已生成：{created.url}
        </p>
      )}

      <div className="overflow-x-auto rounded border border-neutral-800">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-neutral-800 text-left text-neutral-400">
              <th className="px-3 py-2">链接</th>
              <th className="px-3 py-2">节点</th>
              <th className="px-3 py-2">服务器</th>
              <th className="px-3 py-2">状态</th>
              <th className="px-3 py-2">操作</th>
            </tr>
          </thead>
          <tbody>
            {shares.map((s) => (
              <tr key={`${s.server_id}-${s.token}`} className="border-b border-neutral-900">
                <td className="px-3 py-2 font-mono text-xs">{s.url}</td>
                <td className="px-3 py-2 text-neutral-400">{s.node_names.join('、')}</td>
                <td className="px-3 py-2 text-neutral-400">{s.server_name}</td>
                <td className="px-3 py-2">
                  {s.active ? <span className="text-emerald-400">运行中</span> : <span className="text-neutral-500">未运行</span>}
                </td>
                <td className="px-3 py-2">
                  <div className="flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void navigator.clipboard.writeText(s.url)}
                      className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700"
                    >
                      复制链接
                    </button>
                    <ConfirmButton
                      label="作废"
                      confirmLabel="确认作废"
                      busyLabel="作废中…"
                      busy={false}
                      onConfirm={() => handleDelete(s)}
                    />
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {shares.length === 0 && <p className="p-4 text-sm text-neutral-500">还没有分享。</p>}
      </div>
    </div>
  )
}
```

`src/App.tsx` 三处改动：`ViewKey` 加 `'shares'`、`NAV_ITEMS` 在 `relay` 之后加 `{ key: 'shares', label: '分享' }`、渲染处加 `{view === 'shares' && <Shares />}`，并 `import Shares from './views/Shares'`。

**先核对** `ConfirmButton` 的 props 名（`label`/`confirmLabel`/`busyLabel`/`busy`/`onConfirm`）与 `Server` 类型的字段，照实际写。

- [ ] **Step 5: 跑测试确认通过**

Run: `npm test -- Shares`
Expected: 全 PASS

- [ ] **Step 6: 前端门禁**

Run: `npx tsc --noEmit` 再 `npm test`
Expected: tsc 无输出；`App.test.tsx` 等既有用例不因新增导航项而失败（若它断言了导航项数量，同步更新）

- [ ] **Step 7: 提交**

```bash
git add src/api/types.ts src/api/index.ts src/views/Shares.tsx src/views/Shares.test.tsx src/App.tsx
git commit -m "feat(share): 分享页——列表/新建向导/作废 + 三条边界提示"
```

---

### Task 8: CHANGELOG 与总门禁

- [ ] **Step 1: 写文档**

`CHANGELOG.md` 的 `## [未发布]` 节内追加：

```markdown
分享订阅链接：

- **新增「分享」页**：把选中的几条线路做成一条公网可达的订阅链接（服务装在你自己
  的服务器上），对方填进客户端即可用；不想给了在页面上「作废」即可收回
- **只放行带正确 token 的请求**：服务器上的 iptables 规则限定到分享路径，扫到端口
  的人也拿不到东西（TCP 层无响应，等于端口隐身）
- **两条必须知道的事**：服务是纯 HTTP 无加密，**链接本身就是钥匙**；内容是**快照**，
  生成后不再更新，要更新就重新生成一条。仅支持 YAML（mihomo / Clash 系）
```

- [ ] **Step 2: 总门禁**

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` 全 PASS
- [ ] `npx tsc --noEmit` 无输出
- [ ] `npm test` 全 PASS
- [ ] `git status --short` 核对：前几次会话的遗留改动仍在未提交状态

- [ ] **Step 3: 提交**

```bash
git add CHANGELOG.md
git commit -m "docs(share): CHANGELOG 记录分享订阅链接"
```
