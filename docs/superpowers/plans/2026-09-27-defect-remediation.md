# 全仓缺陷修复 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修掉评审确认的 27 条缺陷，重点是三类静默破坏：读失败被当成"没有"而覆盖用户数据、凭据在不该轮换时轮换导致节点变砖、切页后仍在跑的远端任务失去跟踪与取消入口。

**Architecture:** 不引入新范式。本批缺陷的共同形态是"已知的正确做法没铺全"（`privacy.rs` 的 `|| true` vs 同仓 `deploy.rs:290` 判 `exit_code`；`need_singbox` 轮换 vs 33 行之前 `need_ss` 复用），因此修法是把已存在的正确做法铺到它没铺到的地方，并把易忘的判断收敛到**无法忘记**的位置（按路径定权限、按 jobId 分桶、单一注册表）。

**Tech Stack:** Rust（Tauri 2、rusqlite、tokio、thiserror）、React 19 + TypeScript 5.8（Vitest、@testing-library/react）、Python 3（远端分享服务）。

**规格：** `docs/superpowers/specs/2026-09-27-defect-remediation-design.md`

## Global Constraints

- 平台：Windows 11；命令一律在 `D:\新建文件夹\vpn控制` 下执行。
- **每阶段独立提交、独立验证**，顺序 ①→②→③→④→⑤，不跳阶段。
- 门禁命令（与 CI 同源，任一红不可提交）：
  `cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..`
  然后 `npm test && npx tsc --noEmit`。
- **基线（2026-09-27 实测，不得低于）**：clippy 干净 · `cargo test` **811 passed / 0 failed / 3 ignored** · `vitest` **279 passed**。
- **RED 必须先实际看到红**。不接受"纸面上这个断言应该会失败"。本仓已有两次「计划声称既有测试已覆盖、实测被推翻」的记录。
- **禁止并发跑两个 `cargo test`**：本仓 sysproxy 用例抢 HKCU，并发会让它假红。
- **禁止两个 agent 改同一批文件**：先看 `git status` 与文件 mtime。
- 远端 SSH 目标（真机验证用，本机免密）：`203.0.113.127`、`203.0.113.17`。
- 注释写"为什么"，含实测证据；本仓风格是中文注释、引用 `file:line`。

---

## 阶段①（P1）远端破坏性路径

修 #2 #3 #4 #12 #27。根因统一：**读失败被当成文件不存在**。

### Task 1.1: 远端读取原语 `read_remote_file`

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`（在 `write_remote_file` 旁新增，约 :770 一带）
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Produces: `pub(crate) enum RemoteFile { Absent, Content(String) }`；`pub(crate) async fn read_remote_file(sess: &mut SshSession, cancel: &AtomicBool, step: &str, path: &str, timeout: Duration) -> Result<RemoteFile, String>`

- [ ] **Step 1: 写失败测试**

在 `deploy_tests.rs` 末尾追加。注意本仓夹具陷阱：`FakeSshServer`（`ssh/tests.rs:229-231`）对**未命中**命令回落 `exit 0 + 空 stdout`，所以 `(0,"","")` 条目是装饰性的——必须用**显式 exit code**。

```rust
#[tokio::test]
async fn read_remote_absent_is_distinct_from_read_failure() {
    // 42 = 文件不存在（正常）；1 = 读取失败（异常）
    let mut s = FakeSshServer::scripted(vec![
        ("if [ -e '/etc/x' ]; then cat '/etc/x'; else exit 42; fi", 42, "", ""),
        ("if [ -e '/etc/y' ]; then cat '/etc/y'; else exit 42; fi", 1, "", "Permission denied"),
    ]);
    let mut sess = s.connect().await;

    let absent = read_remote_file(&mut sess, &AtomicBool::new(false), "读", "/etc/x", CONFIG_TIMEOUT)
        .await.expect("absent 不该是 Err");
    assert!(matches!(absent, RemoteFile::Absent), "文件不存在应落 Absent，不是错误");

    let failed = read_remote_file(&mut sess, &AtomicBool::new(false), "读", "/etc/y", CONFIG_TIMEOUT).await;
    let msg = failed.expect_err("读取失败必须是 Err，不能退化成 Absent");
    assert!(msg.contains("Permission denied"), "错误文本要带远端 stderr，实际: {msg}");
}

#[tokio::test]
async fn read_remote_content_returns_body_verbatim() {
    let mut s = FakeSshServer::scripted(vec![
        ("if [ -e '/etc/z' ]; then cat '/etc/z'; else exit 42; fi", 0, "line1\nline2\n", ""),
    ]);
    let mut sess = s.connect().await;
    let got = read_remote_file(&mut sess, &AtomicBool::new(false), "读", "/etc/z", CONFIG_TIMEOUT)
        .await.expect("ok");
    match got { RemoteFile::Content(c) => assert_eq!(c, "line1\nline2\n"), _ => panic!("应为 Content") }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test read_remote_ -- --nocapture`
Expected: 编译失败（`read_remote_file` / `RemoteFile` 未定义）。

- [ ] **Step 3: 实现**

在 `servers/deploy.rs` 的 `write_remote_file` 之前插入：

```rust
/// 远端文件读取结果：区分「文件不存在」（正常）与「读取失败」（异常）。
///
/// 为什么需要这个类型：本仓有多处把「读失败」与「文件不存在」当成同一件事
/// （`relay/build.rs` 的 read_conf、`privacy.rs` 的 `cat … 2>/dev/null || true`），
/// 后果是读失败时按「首次部署」处理——覆盖用户既有配置、轮换本不该动的密钥。
pub(crate) enum RemoteFile {
    Absent,
    Content(String),
}

/// 读远端文件：存在则 cat（exit 0），不存在则 exit 42，其它非零 = 读失败。
///
/// **单次往返**同时取回内容与存在性，不需要「先 test 再 cat」两次往返。
/// `[ -e ]` 为真但 cat 失败（权限/IO）→ cat 非零 → 落 Err，正是我们要的语义。
pub(crate) async fn read_remote_file(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    path: &str,
    timeout: Duration,
) -> Result<RemoteFile, String> {
    let script = format!("if [ -e '{path}' ]; then cat '{path}'; else exit 42; fi");
    let out = run_cmd(sess, cancel, step, &script, timeout).await?;
    match out.exit_code {
        0 => Ok(RemoteFile::Content(out.stdout)),
        42 => Ok(RemoteFile::Absent),
        _ => Err(format!("读取 {path} 失败：{}", stderr_tail(&out.stderr))),
    }
}
```

`stderr_tail` 目前只有 `relay/build.rs:1317` 一份私有实现。本次提取到 `servers/deploy.rs` 作 `pub(crate) fn stderr_tail`，`relay/build.rs` 改为 `use`。若 `deploy.rs` 里已有同名函数则复用，不要新建第二份。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test read_remote_`
Expected: 2 passed。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(deploy): 远端读取原语，区分「文件不存在」与「读取失败」"
```

### Task 1.2: `read_conf` 用新原语，读失败即中止建链（#3）

**Files:**
- Modify: `src-tauri/src/relay/build.rs:932-946`（`read_conf`），及调用点 `:291,292,539`
- Test: `src-tauri/src/relay/build_tests.rs`

**Interfaces:**
- Consumes: Task 1.1 的 `RemoteFile` / `read_remote_file`
- Produces: `relay::build::read_conf` 返回 `Result<RemoteFile, String>`（原来是 `String`）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn read_failure_aborts_build_instead_of_rotating_keys() {
    // 中转机 wg0.conf 读取失败（权限/瞬时故障）——绝不能当成「首次建链」
    let mut s = FakeSshServer::scripted(vec![
        ("modprobe wireguard", 0, "", ""),
        ("which wg", 0, "/usr/bin/wg\n", ""),
        ("ip route get 8.8.8.8", 0, "8.8.8.8 dev eth0 src 10.0.0.1\n", ""),
        // 关键：cat wg0.conf 明确失败，不是「文件不存在」
        (&format!("if [ -e '{WG_CONF_PATH}' ]; then cat '{WG_CONF_PATH}'; else exit 42; fi"),
         1, "", "cat: Permission denied"),
    ]);
    let err = run_build(s).await.expect_err("读取失败必须中止建链");
    assert!(err.contains("Permission denied"), "应带上远端原因: {err}");
    // 核心断言：没有发出任何写 wg0.conf 的命令
    assert!(!s.saw_command("wg0.conf.new"), "读失败时绝不能写盘（会把既有链路冲掉）");
    assert!(!s.saw_command("wg genkey"), "读失败时绝不能轮换接口私钥");
}
```

**两处前置**（本任务里一并做掉，`ssh/tests.rs`）：
- `FakeSshServer` 补 `fn saw_command(&self, needle: &str) -> bool` 与 `fn last_command(&self) -> String`，
  记录收到的全部命令文本。Task 1.4 也会用到。
- `run_build(s)` 是本测试文件里的薄封装（把 `FakeSshServer` 的两端接进既有的 build 入口并跑完），
  照 `relay/build_tests.rs` 里既有用例的搭法写；**不要**新造一套启动流程。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test read_failure_aborts`
Expected: FAIL —— 当前实现会走「首次建链」分支，发出 `wg genkey` 与写盘。

- [ ] **Step 3: 实现**

`read_conf` 改为：

```rust
/// 读回远端 conf。**读失败必须上报**：早先这里把「读失败」与「无文件」都归成空串，
/// 后果是读失败 → interface_private_key 取不到 → 重新 genkey（作废该机上**所有**
/// 已建链路与设备 peer）→ merge 走整文件渲染分支（抹掉其它链路的 [Peer] 与 DNAT）。
/// 一次瞬时 SSH 抖动即可触发，且表面看不出异常。
async fn read_conf(sess: &mut SshSession, cancel: &AtomicBool) -> Result<RemoteFile, String> {
    read_remote_file(sess, cancel, "配置", WG_CONF_PATH, CONFIG_TIMEOUT).await
}
```

调用点 `:291` / `:292`（`old_relay` / `old_landing`）与 `:539`（`old_conf`）改为：

```rust
let old_relay = read_conf(&mut relay, cancel).await?;   // ? 而非忽略
```

随后把原先「`old_conf.trim().is_empty()` 即首次」的判据改为 `matches!(old, RemoteFile::Absent)`；
`Content(c)` 走合并分支（`c` 原样传入，即使内容是空文件 —— 空文件是**存在但为空**，与不存在不同）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test read_failure_aborts && cargo test relay::`
Expected: 全绿；`relay::build_tests` 既有用例不回归（首次建链走 `exit 42` 夹具）。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(relay): wg0.conf 读失败不再当成首次建链（防钥匙轮换与整文件覆盖）"
```

### Task 1.3: `privacy.rs` 去掉 `|| true`，并给拆除路径补回滚（#2）

**Files:**
- Modify: `src-tauri/src/servers/privacy.rs:630-638`（部署）、`850-858`（拆除）
- Test: `src-tauri/src/servers/privacy_tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn torrc_read_failure_does_not_overwrite_whole_file() {
    // /etc/tor/torrc 读取失败：必须中止，不能把用户手工配的 bridges/ExitNodes 冲掉
    let mut s = FakeSshServer::scripted(vec![
        ("systemctl is-active tor", 0, "active\n", ""),
        (&format!("if [ -e '{TOR_CONFIG_PATH}' ]; then cat '{TOR_CONFIG_PATH}'; else exit 42; fi"),
         1, "", "cat: Input/output error"),
    ]);
    let err = run_privacy_deploy(s).await.expect_err("读取失败必须中止");
    assert!(err.contains("Input/output error") || err.contains("读取"), "实际: {err}");
    assert!(!s.saw_command("torrc.new"), "读失败时绝不能写 torrc");
}

#[tokio::test]
async fn torrc_absent_still_proceeds_as_first_write() {
    let mut s = FakeSshServer::scripted(vec![
        ("systemctl is-active tor", 0, "active\n", ""),
        (&format!("if [ -e '{TOR_CONFIG_PATH}' ]; then cat '{TOR_CONFIG_PATH}'; else exit 42; fi"),
         42, "", ""),
        // 后续写盘等步骤按现有夹具补齐
    ]);
    run_privacy_deploy(s).await.expect("文件不存在是正常首建，必须继续");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test torrc_read_failure`
Expected: FAIL —— 当前 `|| true` 让读失败退化成空串，用例会在 `assert!(!s.saw_command(...))` 上红。

- [ ] **Step 3: 实现**

部署路径（`:630`）与拆除路径（`:850`）都把

```rust
let existing = run_cmd(sess, cancel, "配置",
    &format!("cat {TOR_CONFIG_PATH} 2>/dev/null || true"), CONFIG_TIMEOUT).await?.stdout;
```

换成

```rust
let existing = match read_remote_file(sess, cancel, "配置", TOR_CONFIG_PATH, CONFIG_TIMEOUT).await? {
    RemoteFile::Content(c) => c,
    RemoteFile::Absent => String::new(),   // 真·首次：没有任何既有内容可保留
};
```

拆除路径另需补备份与回滚：`run_privacy_job` 有（`privacy.rs:250-348`），`run_privacy_remove_job` 没有。
拆除前 `cp` 一份 torrc 到 `{TOR_CONFIG_PATH}.bak`；失败时用备份还原并重载 tor。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test privacy::`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(privacy): torrc 读失败不再覆盖整份文件；拆除路径补备份回滚"
```

### Task 1.4: 原子写 + 按路径定权限（#12 #27）

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs:770-786`（`write_remote_file`）
- Test: `src-tauri/src/servers/deploy_tests.rs`

**设计要点（照抄，别改）**：权限**按路径判定**而不是加参数。理由：本批缺陷的形态就是"在某一条路径上忘了应用正确做法"——加一个可选参数等于给未来的调用点留了同一个坑；按 `WG_CONF_PATH` 判定则**无法被遗忘**。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn write_is_atomic_and_sets_mode_by_path() {
    // 普通配置：644，且经临时文件 + mv（不能直接 > 目标）
    let mut s = FakeSshServer::scripted(vec![("mkdir -p '/etc/xray'", 0, "", "")]);
    write_remote_file(&mut sess, &cancel, "写", "/etc/xray/config.json", "{}", CONFIG_TIMEOUT).await.unwrap();
    let cmd = s.last_command();
    assert!(cmd.contains("> '/etc/xray/config.json.new'"), "必须写临时文件: {cmd}");
    assert!(cmd.contains("mv -f"), "必须用 mv 原子替换: {cmd}");
    assert!(cmd.contains("chmod 644"), "普通配置 644: {cmd}");
    assert!(!cmd.contains("chmod 700"), "普通配置不该动父目录: {cmd}");
}

#[tokio::test]
async fn wireguard_conf_is_600_and_dir_700() {
    // wg0.conf 含 PrivateKey：必须 600，父目录 700
    let mut s = FakeSshServer::scripted(vec![("chmod 700 '/etc/wireguard'", 0, "", "")]);
    write_remote_file(&mut sess, &cancel, "写", WG_CONF_PATH, "[Interface]\nPrivateKey = xxx\n", CONFIG_TIMEOUT).await.unwrap();
    let cmd = s.last_command();
    assert!(cmd.contains("chmod 700 '/etc/wireguard'"), "父目录必须显式 700: {cmd}");
    assert!(cmd.contains("chmod 600"), "wg0.conf 必须 600: {cmd}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test write_is_atomic && cargo test wireguard_conf_is_600`
Expected: FAIL —— 当前是 `echo b64 | base64 -d > path`，无临时文件、无 chmod。

- [ ] **Step 3: 实现**

```rust
/// 远端写文件：mkdir -p 父目录 + 临时文件 + chmod + mv 原子替换。
/// 路径为常量白名单，故无注入面（内容整体 base64 后单引号包裹）。
///
/// **原子性**：原实现 `> '{path}'` 是 truncate-then-stream，取消/超时/断连
/// （都是常态）会留下截断的 config.json，服务起不来而不报原因。
///
/// **权限**：按路径判定，不加参数——本批缺陷的形态就是「某条路径上忘了应用」，
/// 可选参数等于给未来的调用点留同一个坑。`wg0.conf` 内含 PrivateKey，
/// 首次写入按 umask 会落 644（`mkdir -p` 还会把 /etc/wireguard 建成 755），
/// 而 `wg-quick` 自己就会警告 world accessible。
pub(crate) async fn write_remote_file(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    remote_path: &str,
    content: &str,
    timeout: Duration,
) -> Result<(), String> {
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, content.as_bytes());
    let parent = remote_path.rsplit_once('/').map(|(p, _)| p).unwrap_or(".");
    let (mode, dir_mode) = remote_perms(remote_path);
    let mut cmd = String::new();
    if let Some(dm) = dir_mode {
        // 显式 chmod（幂等）：`mkdir -p -m` 只对新建目录生效，
        // 已存在却是 755 的 /etc/wireguard 它修不了——而恰恰是这种情况最需要修
        cmd.push_str(&format!("chmod {dm:o} '{parent}' && "));
    }
    cmd.push_str(&format!(
        "mkdir -p '{parent}' && echo '{b64}' | base64 -d > '{remote_path}.new' \
         && chmod {mode:o} '{remote_path}.new' && mv -f '{remote_path}.new' '{remote_path}'"
    ));
    run_cmd_ok(sess, cancel, step, &cmd, timeout).await
}

/// 远端文件权限策略：路径 → (文件 mode, 父目录 mode)。
/// 新增敏感文件时**必须**在这里登记，否则落 umask 默认（通常 644）。
fn remote_perms(path: &str) -> (u32, Option<u32>) {
    if path == crate::relay::WG_CONF_PATH || path == crate::relay::WG_BAK_PATH {
        (0o600, Some(0o700))   // 含 PrivateKey；wg-quick 自己都会警告 world accessible
    } else {
        (0o644, None)
    }
}
```

`sh` 里的顺序必须是 `mkdir -p '{parent}' && chmod {dm:o} '{parent}' && …`：`chmod` 放在
`mkdir` **之前**会在目录尚不存在时失败，整个命令短路。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test write_is_atomic && cargo test wireguard_conf_is_600 && cargo test`
Expected: 全绿；既有 `deploy_tests` / `relay::build_tests` / `share::tests` 里断言写盘命令文本的用例
可能需要同步更新（命令文本变了）。**逐个看，别批量改**——断言"命令里有 chmod 600"是有价值的，
断言"命令完全等于旧文本"的才需要更新。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(deploy): 远端写盘改原子替换；wg0.conf 600、父目录 700"
```

### Task 1.5: sing-box 凭据复用（#4）

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs:1193-1196` 及附近
- Test: `src-tauri/src/servers/deploy_tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn redeploy_reuses_singbox_credentials() {
    // 第二次部署必须复用既有凭据：轮换 = 服务端新凭据 + 客户端旧凭据 = 静默变砖
    let existing = r#"{"inbounds":[{"type":"hysteria2","users":[{"password":"PINNED"}]}]}"#;
    let cfgs = deploy_twice_and_capture_singbox_config(existing).await;
    assert!(cfgs[1].contains("PINNED"), "重复部署不该换 hy2 密码: {}", cfgs[1]);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test redeploy_reuses_singbox`
Expected: FAIL —— 当前无条件 `Uuid::new_v4()`。

- [ ] **Step 3: 实现**

照 `parse_singbox_ports`（`deploy.rs:318-332`）加 `parse_singbox_creds`，从既有 config.json 读回
hy2 password / tuic uuid+password。`need_singbox` 分支改为：

```rust
// 同 xray/ss 两段：既有凭据原样复用，重复部署不轮换（轮换 = 两侧短暂不一致，
// 中途失败就把节点变砖）。只有首次部署或读不回配置时才新生成。
let hy2_pw   = existing.singbox_hy2_pw.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
let tuic_uuid = existing.singbox_tuic_uuid.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
let tuic_pw   = existing.singbox_tuic_pw.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
```

`existing` 的读取同样走 Task 1.1 的原语（读失败要中止，不是当成首次）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test redeploy_reuses_singbox && cargo test`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(deploy): sing-box 凭据重复部署不再轮换（与 xray/ss 同构）"
```

### Task 1.6: 阶段① 收口

- [ ] **Step 1: 全量门禁**

Run: `cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: fmt 干净 · clippy 干净 · **≥811 passed / 0 failed**。

- [ ] **Step 2: 真机验证**

SSH 到 `203.0.113.127`：手工制造一次 `cat` 失败（`chmod 000` 目标文件）后跑一次建链/部署，
确认**中止并报可读原因**，且远端配置**未被修改**（比对 md5）。恢复权限。

- [ ] **Step 3: 提交**

```bash
git commit --allow-empty -m "test(p1): 真机验证读失败中止（产物已存档）"
```

---

## 阶段②（P2）分享 token

修 #1 #25。

### Task 2.1: 换掉 `python3 -m http.server`，禁目录列举（#1）

**Files:**
- Modify: `src-tauri/src/share/mod.rs:91-104`（`unit_content`）、新增脚本内容常量
- Test: `src-tauri/src/share/tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn share_unit_serves_listing_disabled_script() {
    let unit = unit_content("abc123", 8080);
    assert!(!unit.contains("-m http.server"), "不能再依赖会列目录的 http.server: {unit}");
    assert!(unit.contains("share_server.py"), "ExecStart 应指向禁列举脚本: {unit}");
}

#[test]
fn share_server_script_never_lists_directories() {
    let s = share_server_script();
    assert!(s.contains("def list_directory"), "必须覆盖 list_directory");
    assert!(s.contains("send_error(404)"), "列举必须回 404");
    assert!(!s.contains("directory="), "不用 directory= 参数，避免依赖 python 3.7+");
    assert!(s.contains("os.chdir"), "改用 os.chdir 定位文档根");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test share_unit_serves`
Expected: FAIL —— 当前 unit 里是 `-m http.server`，且无脚本。

- [ ] **Step 3: 实现**

新增 `pub fn share_server_script() -> String`：

```python
import http.server, socketserver, os, sys
os.chdir(sys.argv[2])
class H(http.server.SimpleHTTPRequestHandler):
    def list_directory(self, path):
        self.send_error(404)
        return None
    def log_message(self, *a):
        pass
socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("0.0.0.0", int(sys.argv[1])), H) as s:
    s.serve_forever()
```

`unit_content` 的 `ExecStart` 改为
`/usr/bin/env python3 {SHARE_ROOT}/share_server.py {port} {dir} --bind` 无关（脚本内已绑 0.0.0.0）。

**脚本落盘**：在 `create_share` 里用 `write_remote_file` 写到 `{SHARE_ROOT}/share_server.py`
（base64 传输，天然免疫转义）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test share::`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(share): 分享服务不再列目录（token 无法被端口扫描枚举）"
```

### Task 2.2: 已存在的分享自动升级

**Files:**
- Modify: `src-tauri/src/share/mod.rs`（`create_share` / `list_shares`）
- Test: `src-tauri/src/share/tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn listing_upgrades_legacy_unit_to_listing_disabled() {
    // 升级前建的分享仍跑着 -m http.server：list_shares 应把它换成新脚本并重启
    let s = FakeSshServer::scripted(vec![
        ("systemctl cat", 0, LEGACY_UNIT_WITH_HTTP_SERVER, ""),
    ]);
    let _ = list_shares(state_with_one_server()).await;
    assert!(s.saw_command("share_server.py"), "应重写 unit 指向新脚本");
    assert!(s.saw_command("daemon-reload"), "改 unit 后必须 daemon-reload");
    assert!(s.saw_command("systemctl restart"), "必须重启服务");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test listing_upgrades_legacy`
Expected: FAIL。

- [ ] **Step 3: 实现**

`create_share` 与 `list_shares` 各自在拿到 unit 内容后比对：`ExecStart` 不含 `share_server.py`
即为旧形态 → 重写 unit + 脚本 + `daemon-reload` + `restart`（幂等，已是新形态则跳过）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test share::`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(share): 既有分享自动升级为禁列举形态"
```

### Task 2.3: 供应链校验（#25）

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs:1011`（Xray 安装）、`:1020-1038`（ss / sing-box 下载）
- Test: `src-tauri/src/servers/deploy_tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn ss_download_is_checksum_pinned() {
    let cmd = ss_install_cmd("1.2.3");
    assert!(cmd.contains("sha256sum -c"), "二进制下载必须校验和: {cmd}");
}

#[test]
fn xray_install_uses_pinned_version_not_moving_branch() {
    let cmd = xray_install_cmd();
    assert!(!cmd.contains("/main/"), "安装脚本不得指向可变分支: {cmd}");
    assert!(cmd.contains("v1."), "应指向固定版本: {cmd}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test ss_download_is_checksum && cargo test xray_install_uses`
Expected: FAIL。

- [ ] **Step 3: 实现**

ss / sing-box 下载后加 `sha256sum -c`（哈希常量照 `tor/mod.rs:22-24` 的做法钉在源码里，
脚本头部注明"换版本需同步哈希"）；Xray 安装脚本 URL 从 `main` 改为固定 tag。
若上游不提供稳定哈希，则改为**下载到临时文件后先 `sha256sum` 打印并记录**，并在验收时人工比对——
至少让漂移可见。

- [ ] **Step 4: 跑测试确认通过 + 收口**

Run: `cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: ≥811 passed。

- [ ] **Step 5: 真机验证 + 提交**

SSH 到 `203.0.113.127` 建一个分享，然后**以攻击者视角**确认：
`curl http://<host>:<port>/` 与 `curl http://<host>:<port>/sub/` **都回 404 且不列出目录**，
而原链接 `curl http://<host>:<port>/sub/<token>` 仍能取到配置。

```bash
git add -A && git commit -m "fix(deploy): ss/sing-box 下载补校验和；xray 安装脚本固定版本"
```

---

## 阶段③（P3）前端任务跟踪

修 #5 #6 #7 #15 #17 与 #9 的终态部分。

### Task 3.1: 模块级 job 注册表

**Files:**
- Create: `src/jobs/registry.ts`
- Test: `src/jobs/registry.test.ts`

**Interfaces:**
- Produces:
  - `type JobKind = 'deploy' | 'upstream' | 'privacy' | 'link' | 'peer' | 'speed' | 'server_speed' | 'server_throughput'`
    （末项是**前端专有**：后端把 ServerSpeed 与 Throughput 合并成同一个 `server_speed`
    发出（`jobs/mod.rs:109`），前端无从反推，只能由发起方在 reserve 时记录）
  - `type TrackedJob = { id: number; kind: JobKind; progress: number; error: string | null; targets: string[] }`
    （`targets` 统一承载 node id / server id 的字符串形式；`done` 不进状态——终态即从表中移除）
  - `subscribe(fn: () => void): () => void`
  - `getSnapshot(): ReadonlyMap<number, TrackedJob>`
  - **`reserveJob(kind: JobKind, targets: string[]): number`** — **IPC 前**调用，返回**负数占位号**，
    乐观占位以堵住 await 窗口
  - **`bindJob(placeholder: number, jobId: number): void`** — IPC 返回真实 jobId 后绑定
  - `release(jobId: number): void`
  - `useTrackedJobs(kind?: JobKind): Map<number, TrackedJob>`
  - `installJobListener(listen: ListenFn): Promise<() => void>` — **全进程只调一次**
  - `__ingest(p: JobProgress): void` — 测试与监听器入口

> **原计划写的是 `startTracking(jobId, …)`「IPC 前调用」——那在物理上不可能成立**：jobId 由后端在
> IPC 返回时才分配。改为「占位号 → 绑定真实 id」两步，守卫照样覆盖 await 窗口。
> 不要为了对齐旧计划把它改回单函数。

- [ ] **Step 1: 写失败测试**

```ts
import { renderHook, act } from '@testing-library/react'
import { useTrackedJobs, reserveJob, bindJob, __ingest } from './registry'

/// 测试辅助：占位 → 绑定真实 id，模拟一次完整的「IPC 前占位、IPC 后绑定」。
const track = (jobId: number, kind: JobKind, targets: string[]) =>
  bindJob(reserveJob(kind, targets), jobId)

it('切走再切回后，进行中的 job 仍在跟踪表里', () => {
  track(42, 'speed', ['n1'])
  act(() => { __ingest({ job_id: 42, kind: 'speed', progress: 30, error: null, step: '测速', message: '' }) })
  const first = renderHook(() => useTrackedJobs('speed'))
  expect(first.result.current.get(42)?.progress).toBe(30)
  first.unmount()                       // 等价于视图切换（组件卸载）
  const second = renderHook(() => useTrackedJobs('speed'))
  expect(second.result.current.has(42)).toBe(true)   // 关键断言：没丢
})

it('终态事件后 job 从跟踪表移除', () => {
  track(7, 'deploy', ['s1'])
  act(() => { __ingest({ job_id: 7, kind: 'deploy', progress: 100, error: null, step: '完成', message: '' }) })
  const { result } = renderHook(() => useTrackedJobs('deploy'))
  expect(result.current.has(7)).toBe(false)
})

it('失败事件按 error 判终态（不依赖 step 文案）', () => {
  track(8, 'deploy', ['s1'])
  act(() => { __ingest({ job_id: 8, kind: 'deploy', progress: 40, error: '磁盘满', step: '结束', message: '' }) })
  const { result } = renderHook(() => useTrackedJobs('deploy'))
  expect(result.current.has(8)).toBe(false)
})

it('逐目标失败事件不判终态——一台机器失败不该终结整个批量任务', () => {
  track(9, 'speed', ['n1', 'n2'])
  // message 命中已登记目标 = 逐目标事件；error 是这一台的失败原因
  act(() => { __ingest({ job_id: 9, kind: 'speed', progress: 50, error: '超时', step: '测速', message: 'n1' }) })
  const { result } = renderHook(() => useTrackedJobs('speed'))
  expect(result.current.has(9)).toBe(true)   // 任务仍在跑，n2 还没上屏
})

it('reserveJob 立即占位——IPC 还没回来时忙碌态就已生效', () => {
  const ph = reserveJob('speed', ['n9'])
  const { result } = renderHook(() => useTrackedJobs('speed'))
  expect(result.current.get(ph)).toBeDefined()   // 乐观占位，堵住 await 窗口
})
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npm test -- registry`
Expected: FAIL —— 模块不存在。

- [ ] **Step 3: 实现**

用 `useSyncExternalStore`（React 官方，避免自定义订阅在并发渲染下的撕裂）：

```ts
// src/jobs/registry.ts
// 模块级任务注册表：任务状态不再挂在组件上。
// 为什么必须在模块级：视图是条件挂载的（App.tsx:56-63），组件本地状态切页即丢，
// 而后端任务仍在跑——表现为「任务在跑但界面没有忙碌态、没有取消入口、可以重复提交」。
// 终态判定只在这里做一处：progress >= 100 || error != null。
// 依据：jobs/mod.rs:253-276 的补发事件失败必带 error、成功必带 progress=100，
// 故前端不必依赖 '完成'/'结束' 这类文案（原先两套约定并存，改个中文词就会静默失效）。
import { useSyncExternalStore } from 'react'

const jobs = new Map<number, TrackedJob>()
const listeners = new Set<() => void>()
let snapshot: ReadonlyMap<number, TrackedJob> = jobs   // 引用稳定，供 useSyncExternalStore 比对

function emit() { snapshot = new Map(jobs); listeners.forEach((l) => l()) }

/// IPC **之前**调用：拿不到 jobId（后端在返回时才分配），故先用负数占位号占住目标，
/// 让忙碌守卫覆盖 await 窗口——这正是本阶段要堵的那个洞。
let nextPlaceholder = -1
export function reserveJob(kind: JobKind, targets: string[]): number {
  const id = nextPlaceholder--
  jobs.set(id, { id, kind, progress: 0, error: null, targets })
  emit()
  return id
}

/// IPC 返回后把占位号换成真实 jobId。
export function bindJob(placeholder: number, jobId: number): void {
  const cur = jobs.get(placeholder)
  if (!cur) return
  jobs.delete(placeholder)
  jobs.set(jobId, { ...cur, id: jobId })
  emit()
}

export function __ingest(p: JobProgress): void {
  const cur = jobs.get(p.job_id)
  if (!cur) return                       // 不是本次会话发起的任务，不接管
  // 逐目标事件 vs 终态事件，必须分开判（原计划的 progress>=100||error!=null 是错的）：
  // speed/server_speed 的 error 承载的是**单台**失败原因（jobs/mod.rs:219-227），
  // 拿它判终态会「一台机器失败就把整个批量任务提前判死」，后面的机器再也不上屏。
  // 判据：message 命中本任务已登记的目标 id = 逐目标事件，只记结果不判终态；
  //       message 为空（终态补发恒为空，jobs/mod.rs:253-275）才是终态。
  const isPerTarget = p.message !== '' && cur.targets.includes(p.message)
  if (isPerTarget) {
    resultsFor(p.job_id).set(p.message, { error: p.error })   // 行内结果按 jobId 分桶（#7）
    emit(); return
  }
  const done = p.progress >= 100 || p.error != null
  if (done) { jobs.delete(p.job_id); emit(); return }
  jobs.set(p.job_id, { ...cur, progress: p.progress, error: p.error })
  emit()
}

export function useTrackedJobs(kind?: JobKind) {
  const snap = useSyncExternalStore(subscribe, getSnapshot)
  return useMemo(() => {
    const out = new Map<number, TrackedJob>()
    snap.forEach((v, k) => { if (!kind || v.kind === kind) out.set(k, v) })
    return out
  }, [snap, kind])
}
```

`installJobListener` 在 `App.tsx` 的 `useEffect` 里调一次（`listen('job://progress', …)` 后接
`__ingest`），并在卸载时 `unlisten`。

对账兜底收进同一处：注册表每 5s 调一次 `listJobs()`，把库里已是终态但注册表里还在的 job 清掉
（覆盖"终态事件丢失"）。**全进程只有一个定时器**，各视图不再各自起。

- [ ] **Step 4: 跑测试确认通过**

Run: `npm test -- registry`
Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
git add src/jobs/ && git commit -m "feat(ui): 模块级任务注册表，切页不再丢进行中的任务"
```

### Task 3.2: 视图迁移到注册表

**Files:**
- Modify: `src/views/Servers.tsx`、`src/views/Nodes.tsx`、`src/views/Relay.tsx`、`src/components/UpstreamPanel.tsx`、`src/components/DeployWizard.tsx`、`src/components/DevicePeersPanel.tsx`、`src/components/LinkWizard.tsx`
- Test: 各视图同名 `.test.tsx`

**做法**：逐视图替换，**一次一个视图、一次一次提交**。每个视图：删掉自己的 `listen` 订阅、
`progressHandlers`/`pushProgress` 脚手架、本地 `speedJobs`/`privacyJobs` 等 Map 与 5s 对账循环，
改为 `useTrackedJobs(kind)` + `startTracking`/`release`。

- [ ] **Step 1: 先写 Servers 的回归测试（必须看到红）**

```tsx
it('切到下个视图再切回：进行中的批量测速仍显示忙碌且可取消', async () => {
  const { unmount } = render(<Servers />)
  await user.click(screen.getByRole('button', { name: '批量测速' }))
  unmount()                                  // 等价于切到仪表盘
  render(<Servers />)                        // 切回来
  expect(screen.getByRole('button', { name: '取消测速' })).toBeInTheDocument()
})
```

Run: `npm test -- Servers`
Expected: **FAIL** —— 当前实现切回后忙碌态消失。**这条用例就是本阶段的核心价值，必须亲眼看到它红。**

- [ ] **Step 2: 迁移 Servers.tsx**

- [ ] **Step 3: 跑测试确认通过**

Run: `npm test -- Servers`
Expected: PASS，且其余 Servers 用例不回归。

- [ ] **Step 4: 对每个视图重复 Step 1–3 并单独提交**

顺序：`Nodes` → `Relay` → `UpstreamPanel` → `DeployWizard` → `DevicePeersPanel` → `LinkWizard`。
`DevicePeersPanel` / `LinkWizard` 原先**没有**对账兜底，迁移后自动获得。

```bash
git add -A && git commit -m "refactor(ui): <视图> 任务跟踪迁到注册表"
```

### Task 3.3: 结果按 jobId 分桶（#7）

**Files:**
- Modify: `src/views/Servers.tsx`、`src/views/Nodes.tsx`
- Test: 同名 `.test.tsx`

- [ ] **Step 1: 写失败测试**

```tsx
it('job A 完成不清空 job B 仍在跑的行内结果', async () => {
  // 启动两个并发测速任务，B 先返回一行结果，再让 A 完成
  // 断言 B 的那行结果还在
  expect(screen.getByText(/节点B.*12\.3 MB\/s/)).toBeInTheDocument()
})
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npm test -- Servers`
Expected: FAIL —— 当前终态走 `setLiveResults({})`，把 B 的结果一起清了。

- [ ] **Step 3: 实现**

行内结果改为 `Map<jobId, Map<targetId, Result>>`，终态只删该 job 的桶：

```ts
setLiveResults((prev) => { const next = new Map(prev); next.delete(jobId); return next })
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npm test`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(ui): 单个任务终结不再清空其它任务的行内结果"
```

### Task 3.4: `deployedProtos` 改用后端真相（#17）

- [ ] **Step 1: 写失败测试**

```tsx
it('切页回来「已部署协议」徽章仍在（从后端读，不靠本地镜像）', async () => {
  const { unmount } = render(<Servers />)
  await waitFor(() => expect(screen.getByText('已部署：vless')).toBeInTheDocument())
  unmount(); render(<Servers />)
  expect(await screen.findByText('已部署：vless')).toBeInTheDocument()
})
```

- [ ] **Step 2: 跑测试确认失败** → Run: `npm test -- Servers` · Expected: FAIL

- [ ] **Step 3: 实现**

`Servers.tsx:72` 的本地 `deployedProtos` 改为挂载时调 `listDeployedProtocols`（`api/index.ts:398`，
当前是**死代码**，从未被调用）填充。

- [ ] **Step 4: 跑测试确认通过 + 收口**

Run: `npm test && npx tsc --noEmit && cd src-tauri && cargo test`
Expected: vitest ≥279 + 新增用例全绿；tsc 干净。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(ui): 已部署协议改读后端，切页不再消失（启用死代码 listDeployedProtocols）"
```

---

## 阶段④（P4）Rust 一致性

修 #8 #9 #14 #23。

### Task 4.1: 锁毒化统一（#8）

**Files:**
- Create: `src-tauri/src/util/lock.rs`（或并入既有 `util`）
- Modify: `src-tauri/src/db/mod.rs:31-33`、`src-tauri/src/jobs/mod.rs`(9 处)、`src-tauri/src/net/connlog.rs`(9 处)、`src-tauri/src/net/mmdb.rs`(2 处)
- Test: 各模块既有测试 + 新增反毒化用例

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn db_lock_survives_poisoned_mutex() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    // 另起线程持锁 panic，把锁毒化
    let d2 = db.clone();
    let _ = std::thread::spawn(move || { let _g = d2.lock(); panic!("毒化它"); }).join();
    // 关键：毒化后仍能用，而不是整个应用连锁 panic
    assert!(db.lock().query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test db_lock_survives`
Expected: FAIL —— 当前 `expect("db mutex poisoned")` 会 panic。测试进程里 panic 需要 `catch_unwind`
或让 join 吞掉（上面已用 `let _ =`）。

- [ ] **Step 3: 实现**

```rust
// src-tauri/src/util/lock.rs
/// 取锁并忽略毒化：持锁线程 panic 不该让整个应用连锁失效。
///
/// 仓内已有 19 处这个写法（service/mihomo.rs、net/local_proxy.rs、service/session.rs），
/// 另一半却用 `.lock().unwrap()`（jobs、connlog、mmdb、db）。本项目已有
/// 「async 中 panic 无法 unwind → 进程 abort」的实测记录（jobs/mod.rs:280-288），
/// 一旦持锁 panic，之后每个碰该锁的命令都会连锁 panic，应用等于慢慢死掉。
pub(crate) fn lock_ignore_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// RwLock 的读侧同上（`net/mmdb.rs` 的 `state.read().unwrap()`）。
pub(crate) fn read_ignore_poison<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

/// RwLock 的写侧同上（`net/mmdb.rs` 的 `state.write().unwrap()`）。
pub(crate) fn write_ignore_poison<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}
```

`Db::lock()` 改为 `lock_ignore_poison(&self.conn)`；`jobs`/`connlog` 的 `.lock().unwrap()` 与
`mmdb` 的 `.read().unwrap()` / `.write().unwrap()` 逐处换成本组函数。**本任务只动这 4 个模块**，
`service/mihomo.rs` 等已经正确的 19 处不动（它们的写法本身就对，顺手改只是噪音）。

> **禁区：`jobs/mod.rs:219-227` 的 error 重载不要动。** 那里 `if p.kind != "speed" &&
> p.kind != "server_speed" { p.error = None; }` 让 error 字段对这两类 kind 承载**单台**失败原因，
> 前端注册表已按此契约实现完毕（见阶段③）。它不是死代码、不是冗余，**也不得给 `JobProgress`
> 加字段**——那会改动线上 IPC 契约并让前端已通过的用例失效。本任务在该文件里只碰锁。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test`
Expected: ≥812 passed（含新用例）。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix: 统一忽略锁毒化（DB 锁毒化不再连锁 panic）"
```

### Task 4.2: `AppError` 可分类，线上形状不变（#9）

**Files:**
- Modify: `src-tauri/src/commands.rs:24-41`
- Test: `src-tauri/src/commands_tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn app_error_serializes_to_plain_string_but_keeps_kind() {
    let e = AppError::Validation("订阅名不能为空".into());
    assert_eq!(serde_json::to_value(&e).unwrap(), json!("订阅名不能为空"),
               "线上契约必须仍是字符串——前端 44 条命令都按字符串处理");
    assert!(matches!(e, AppError::Validation(_)), "Rust 侧要能分类");
}
```

- [ ] **Step 2: 跑测试确认失败** → Run: `cd src-tauri && cargo test app_error_serializes` · Expected: 编译失败

- [ ] **Step 3: 实现**

```rust
/// 前端可见的统一错误。**线上形状保持字符串**（`Serialize` 输出 `to_string()`）：
/// 前端 100% 经 api/index.ts 拿到它并当字符串用，改成 `{code,message}` 是 44 条命令的
/// 破坏性变更，而当前没有任何一处 UI 需要按类别分支——属于无需求驱动的改动。
/// Rust 侧保留分类，供将来需要时在 api 层单点归一化。
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum AppError {
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Remote(String),
    #[error("{0}")]
    Message(String),
}
```

`Serialize` 实现不变（仍 `serialize_str(self.to_string())`）。`err()` 默认映射到 `Message`。
本次**不**逐处重分类（那会是一次无收益的大 diff）；只在明显的位置把已有的 `AppError::Message("…不能为空")`
换成 `AppError::Validation`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test && cd .. && npm test`
Expected: 全绿；前端 **一行都不用改**（这正是本设计的目的）。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "refactor(ipc): AppError 可分类，线上仍序列化为字符串（前端零改动）"
```

### Task 4.3: 取消检查点（#14）

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs:1362-1376,1380-1446`、`src-tauri/src/servers/privacy.rs`（回滚后）
- Test: `src-tauri/src/servers/deploy_tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn cancel_before_bbr_skips_bbr_and_lands_cancelled() {
    // 回写段前取消：应落 cancelled，且不该再改内核
    let out = run_deploy_with_cancel_at(Stage::BeforeBbr).await;
    assert!(!out.saw_command("tcp_congestion_control=bbr"), "已取消不该再开 BBR");
    assert_eq!(out.status, "cancelled");
}
```

- [ ] **Step 2: 跑测试确认失败** → Run: `cd src-tauri && cargo test cancel_before_bbr` · Expected: FAIL

- [ ] **Step 3: 实现**

在 BBR 段与回写段**之前**各加：

```rust
if cancel.load(Ordering::SeqCst) {
    return Err("已取消".to_string());   // 步骤边界检查：不承诺打断执行中的命令
}
```

`privacy.rs` 回滚完成后同样重新检查取消位，再决定是否继续 `set_onion` / `sync_local_private`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(deploy): 取消检查点补到 BBR 段与回写段"
```

### Task 4.4: 迁移降级护栏（#23）

**Files:**
- Modify: `src-tauri/src/db/migrations.rs:262-269`
- Test: `src-tauri/src/db/tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn newer_schema_is_rejected_with_a_readable_error() {
    let mut conn = Connection::open_in_memory().unwrap();
    run(&mut conn).unwrap();
    conn.pragma_update(None, "user_version", latest_version() + 5).unwrap();
    let err = run(&mut conn).expect_err("库比二进制新必须报错，不能静默继续");
    assert!(err.to_string().contains("比本程序新"), "要给人话: {err}");
}
```

- [ ] **Step 2: 跑测试确认失败** → Run: `cd src-tauri && cargo test newer_schema_is_rejected` · Expected: FAIL

- [ ] **Step 3: 实现**

```rust
pub fn run(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // 降级护栏：库比二进制新（换回旧版本、或跨分支共用数据目录）时，
    // 下面的 skip 会**静默什么都不做**，旧二进制继续对着新 schema 跑——
    // 表现是莫名其妙的查询错误。这个形态本仓已踩过一次，直接报人话。
    if current > MIGRATIONS.len() as i64 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "数据目录比本程序新（库 v{current}，本程序支持到 v{}）：请升级到较新版本，\
                 或换一个数据目录。", MIGRATIONS.len()
            )),
        ));
    }
    for (version, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        apply_migration(conn, version, sql)?;
    }
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过 + 收口**

Run: `cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: ≥815 passed。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(db): 库比程序新时报可读错误，不再静默跳过迁移"
```

---

## 阶段⑤（P5）清理

修 #10 #11 #13 #16 #18–#22 #24 #26（#26 记录不修）。

### Task 5.1: 注入面（#10）

**Files:**
- Create: `src-tauri/src/servers/shell.rs`（`shell_quote` + `stderr_tail` 共用）
- Modify: `src-tauri/src/relay/build.rs:256-266`（`egress_if`）、`:1290-1313`（iptables 重放）、`src-tauri/src/server_speed/iperf.rs:36-38`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn iface_name_with_metacharacters_is_rejected() {
    assert!(valid_iface_name("eth0"));
    assert!(!valid_iface_name("eth0;curl${IFS}evil"));
    assert!(!valid_iface_name("eth0 -j ACCEPT"));
}

#[test]
fn iptables_rule_with_comment_metachars_is_quoted_or_skipped() {
    let line = r#"-A PREROUTING -m comment --comment "x; id" -j DNAT --to-destination 10.0.0.1:443"#;
    let out = dnat_undo_command(line);
    assert!(!out.contains("; id") || out.contains(r#"'; id'"#), "注释里的元字符必须被引号包住: {out}");
}
```

- [ ] **Step 2: 跑测试确认失败** → Expected: FAIL

- [ ] **Step 3: 实现**

1. `egress_if`：取回后过 `valid_iface_name`（已存在于 `relay/build.rs:744`，当前**只在一处调用**），
   不合法即 `Err`。
2. `remove_dnat_rules`：不再整行重放。改为解析出规则字段（dport / to-destination）后**重建**一条
   命令，而不是 `line.replace("-A", …)`。顺带修掉 `replace("-A", …)` 会把注释里的 `-A` 一并替换的
   正确性 bug。
3. `shell_quote` 从 `iperf.rs:36` 提到 `servers/shell.rs`，`iperf.rs` 改为引用。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test`

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(relay): 远端输出不再整行重放为 root 命令；接口名白名单收敛"
```

### Task 5.2: SSH 主机密钥 TOFU（#11）

**Files:**
- Modify: `src-tauri/src/ssh/mod.rs:56-62`
- Test: `src-tauri/src/ssh/tests.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn changed_host_key_is_rejected() {
    // 首次记录指纹；第二次换一个主机密钥 → 必须拒绝
    let err = connect_twice_with_different_keys().await.expect_err("主机密钥变更必须拒绝");
    assert!(err.contains("主机密钥"), "{err}");
}
```

- [ ] **Step 2: 跑测试确认失败** → Expected: FAIL（当前恒 `Ok(true)`）

- [ ] **Step 3: 实现**

`check_server_key` 首次连接记录 `host:port → 公钥指纹` 到数据目录 `known_hosts.json`；之后比对，
不匹配即 `Ok(false)` 并在上层映射为可读错误"主机密钥已变更（可能是中间人，或服务器重装）"。

- [ ] **Step 4: 跑测试确认通过 + 提交**

```bash
git add -A && git commit -m "feat(ssh): 主机密钥 TOFU，变更即拒绝"
```

### Task 5.3: 凭据降级可见（#13）

- [ ] **Step 1-4**: 写测试断言 `seal_param` 失败时**留下可见痕迹**（设置表标记 + 日志），
  再改 `crypto/mod.rs:158-168` 落标记。Run: `cd src-tauri && cargo test seal_param`

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "fix(crypto): 加密降级为明文时留下可见告警"
```

### Task 5.4: 组件拆分（#16）

- [ ] **Step 1**: 从 `Servers.tsx`（845 行）抽出 `ServerForm` / `ServerSpeedPanel` / `ServerThroughputPanel`，
  **先跑 `npm test -- Servers` 确认 40 个用例全绿作为安全网**。
- [ ] **Step 2**: 每抽一个就跑一次测试，红了立刻回滚该次抽取。
- [ ] **Step 3**: 同样处理 `Nodes.tsx`（750 行）与 `Dashboard.tsx`（540 行，18 个 `useState`）。
- [ ] **Step 4**: Run: `npm test && npx tsc --noEmit` · Expected: 全绿
- [ ] **Step 5**: `git commit -m "refactor(ui): 拆分 Servers/Nodes/Dashboard 巨型组件"`

### Task 5.5: 小修合集（#18 #19 #20 #21 #22 #24）

每项**各自一次提交**，每项都先写红：

| # | 修法 |
|---|---|
| #18 | `Relay.tsx:86-98` 依赖去掉 `rules`（body 已读 `rulesRef.current`），定时器不再每次刷新重建 |
| #19 | `Shares.tsx:217` 加 `.catch` + `document.execCommand` 兜底，照 `PeerConfigDialog.tsx:58-79` |
| #20 | `Settings.tsx:116-138` UAC 轮询加 `alive` 标志与取消，照 `Shares.tsx:49` |
| #21 | `relayStatusCache` 删除链路时同步清条目（`Relay.tsx:161,262`） |
| #22 | `ConnLogPanel.tsx:197` 改用稳定 key（`pid+proto+resolved_ip+dest_port+本地端口`） |
| #24 | `server_speed/mod_test.rs` → `server_speed/mod_tests.rs`，同步 `mod.rs` 的 `#[cfg(test)] mod` 声明 |

**#26（`serve/mod.rs:66` 的 token 非常量时间比较）按设计记录不修**：token 是 UUIDv4（122 bit）
且服务仅回环绑定，不构成实际攻击面。若日后该服务改为对外监听，这条必须重新评估。

```bash
git add -A && git commit -m "fix(ui): <逐条>"
```

### Task 5.6: 发布

- [ ] **Step 1**: CHANGELOG「未发布」整理成 `## [0.3.0] - 2026-09-27`（本批含行为变更：#1 分享链接
  形态变了、#23 库比程序新会报错，按语义化版本升 minor）。
- [ ] **Step 2**: `src-tauri/tauri.conf.json` 版本号改 `0.3.0`，`package.json` 同步。
- [ ] **Step 3**: Run: `cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cd .. && npm test && npx tsc --noEmit && npm run check:package && npm run check:updater-pubkey`
- [ ] **Step 4**: `git commit -m "chore(release): 0.3.0"`

---

## 附：验证清单（每阶段收口时逐条核对）

- [ ] `cargo fmt --check` 干净
- [ ] `cargo clippy --all-targets -- -D warnings` 干净
- [ ] `cargo test` ≥ 基线且 0 failed
- [ ] `npm test` ≥ 279 passed
- [ ] `npx tsc --noEmit` 干净
- [ ] 本阶段每条修复都有**曾经红过**的用例
- [ ] 没有把"断言命令文本完全等于旧文本"的测试硬改成新文本（那是掩盖回归）
