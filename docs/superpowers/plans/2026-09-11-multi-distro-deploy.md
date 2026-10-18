# 多发行版部署 + BBR 自动开启 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让协议部署在任意 systemd 系 Linux 发行版上可用（取代只认 ubuntu/debian 的名字白名单），并把 BBR 一键开启接进部署流水线。

**Architecture:** 部署流水线 `servers/deploy.rs` 里三处发行版依赖被逐个拆掉——名字白名单换成 `/run/systemd/system` 能力探测；`apt-get install openssl` 换成包管理器探测脚本；`server_bbr.rs` 的现成 `check_bbr`/`enable_bbr` 被流水线按需调用（插在端口验证之后、写回之前，失败不上抛）。前端在部署向导加一个默认勾选的复选框，经 `DeployInput` → `DeployRequest` 一路透传。

**Tech Stack:** Rust（Tauri 2 backend）、React 18 + TypeScript + Vitest（frontend）、`russh` 0.53、`FakeSshServer` 测试替身（`crate::ssh::tests`）。

**Spec:** `docs/superpowers/specs/2026-09-11-multi-distro-deploy-design.md`

## Global Constraints

- **只覆盖 systemd 系发行版**。无 systemd（Alpine/OpenRC、Devuan/sysvinit、Void/runit）必须**明确报错**，不得静默继续。
- **不改 `src-tauri/src/server_bbr.rs`**。直接复用现有 `check_bbr` / `enable_bbr` 及其幂等脚本。
- **不改 relay 链路**（`src-tauri/src/relay/build.rs` 的 `apt-get` 硬编码不在本次范围）。
- **BBR 的任何失败都不上抛**，只记录进 deploys 摘要与完成文案。
- **暂存文件必须逐个列路径，禁止 `git add -A` / `git add .`**。工作区有上一次会话遗留的 25 个未提交改动（见 `git status`），全量暂存会把它们一起卷进本次提交。
- 常量/函数注释、错误文案、提交信息一律**中文**，与仓库现有风格一致。
- 测试命令一律带 manifest：`cargo test --manifest-path src-tauri/Cargo.toml <filter>`（bin crate，`--lib` 不可用）。
- 前端门禁：`npx tsc --noEmit` + `npm test`。

## 关键既有事实（实现前必读，均已核对源码）

1. **`Option<T>` 的 Tauri 参数真的可选**。`tauri-2.11.5/src/ipc/command.rs:134-145` 的 `deserialize_option` 在 key 缺失时走 `visitor.visit_none()`，不报错。所以 `enable_bbr: Option<bool>` 可以不传。
2. **`FakeSshServer::push_script(contains, exit, stdout, stderr)` 按 FIFO 消费**：每次 exec 取**队首第一个** `cmd.contains(contains)` 命中的条目并移除（`ssh/tests.rs:206-221`）。压入顺序即匹配顺序，已消费的条目不会二次命中。
3. **现有测试 `deploy_tests.rs:975-1011` `unsupported_os_stops_before_any_config` 断言 `ID=centos` 被拒**（`:999` 断言 err 含「暂不支持」）。本改动会推翻该断言，必须重写（Task 3）。
4. **现有进度断言 `deploy_tests.rs:582-594`** 只查步骤集合含 探测/安装/配置/启动/写回 + 首「探测」末「写回」；新增「优化」步骤位于「写回」之前，不会破坏它。
5. **`deploy_tests.rs:504/607/675/730/803/919/982/1020/1053`** 共 9 处压了 `push_script("cat /etc/os-release", 0, "ID=ubuntu\n", "")`，探测命令一改全部失配，必须同步替换（Task 3）。
6. `deploys` 摘要由 `parse_deployed_protocols`（`deploy.rs:64`）读取，**只认 `protocols` 字段、容忍额外字段**，所以新增 `bbr` 字段安全。
7. **测试夹具签名**：`landing_state(&dir) -> Arc<AppState>`、`seeded_server(&state, &srv) -> i64`、`test_runner(&state, &mock)`（`deploy_tests.rs:469-500`）；IPC 侧是 `test_state()` / `server_input(host, port, kind)` / `mock_app(state)` / `invoke_ok(&wv, cmd, json!{…})`（`ipc_tests.rs:791-808`）。

---

### Task 1: 能力探测常量与解析函数（纯函数，不接线）

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`（在 `os_is_supported` 定义处 `:310-316` 之后新增）
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub const CAPABILITY_PROBE_SCRIPT: &str`
  - `pub fn parse_capability(stdout: &str) -> (String, bool)` —— 返回 `(OS 描述, 是否运行 systemd)`

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/servers/deploy_tests.rs` 的 `os_release_support_matrix` 用例（`:278-285`）之后插入：

```rust
// ---------- 能力探测（取代发行版名字白名单） ----------

/// 解析 CAPABILITY_PROBE_SCRIPT 输出。OS 行缺失/空 → 「未知」；SYSTEMD 行缺失 → false。
#[test]
fn parse_capability_reads_os_and_systemd() {
    assert_eq!(
        parse_capability("OS=ubuntu \nSYSTEMD=yes\n"),
        ("ubuntu".to_string(), true),
        "ID_LIKE 为空时 OS 行尾部留一个空格，须 trim"
    );
    assert_eq!(
        parse_capability("OS=alpine\nSYSTEMD=no\n"),
        ("alpine".to_string(), false)
    );
    // ID_LIKE 存在时拼接在 OS 行里（用于报错文案）
    assert_eq!(
        parse_capability("OS=centos rhel fedora\nSYSTEMD=yes\n"),
        ("centos rhel fedora".to_string(), true)
    );
    // 两个标记行都缺失 → 未知 + false（不 panic）
    assert_eq!(parse_capability(""), ("未知".to_string(), false));
    assert_eq!(parse_capability("乱码\n"), ("未知".to_string(), false));
    // SYSTEMD=yes 行不完整（如被截断）→ 不得误判为 true
    assert_eq!(parse_capability("OS=x\nSYSTEMD=\n"), ("x".to_string(), false));
}

/// 探测脚本本体：必须同时验 /run/systemd/system 目录与 systemctl 可执行。
/// 只看 command -v 不够——容器/WSL 里 systemctl 存在但 systemd 未运行，
/// 会走到「unit 写完 → enable 静默不生效 → 端口验证失败」的半截状态。
#[test]
fn capability_probe_script_checks_systemd_runtime_dir() {
    let s = CAPABILITY_PROBE_SCRIPT;
    assert!(
        s.contains("/run/systemd/system"),
        "必须以 runtime 目录判定 systemd 在跑: {s}"
    );
    assert!(
        s.contains("command -v systemctl"),
        "须同时要求 systemctl 可执行: {s}"
    );
    assert!(
        s.contains("SYSTEMD=yes") && s.contains("SYSTEMD=no"),
        "两个分支都要有输出标记: {s}"
    );
    assert!(s.contains(". /etc/os-release"), "POSIX 点号取 os-release: {s}");
    assert!(!s.contains("source "), "禁止 source（busybox ash 不认）: {s}");
}
```

同时把文件顶部 `:7-13` 的 `use crate::servers::deploy::{…}` 列表补上 `parse_capability` 与 `CAPABILITY_PROBE_SCRIPT`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml parse_capability`
Expected: 编译失败 —— `cannot find function parse_capability in module`

Run: `cargo test --manifest-path src-tauri/Cargo.toml capability_probe_script_checks`
Expected: 编译失败 —— `CAPABILITY_PROBE_SCRIPT` 未定义

- [ ] **Step 3: 实现**

在 `src-tauri/src/servers/deploy.rs` 的 `os_is_supported`（`:310-316`）之后插入：

```rust
/// 部署前置能力探测：一次 SSH 同时取系统标识与 systemd 判定。
/// 判据是 /run/systemd/system 目录（systemd 作为 PID 1 时创建）**加** systemctl 可执行，
/// 而不是发行版名字——覆盖所有 systemd 系及其衍生版（含国内云厂商发行版）。
/// 只看 command -v 不够：容器/WSL 里 systemctl 存在但 systemd 未运行，
/// 会走到「unit 写完 → enable 静默不生效 → 端口验证失败」的半截状态，失败点离原因太远。
/// `. ` 用 POSIX 点号而非 source（busybox ash 不认 source）。
pub const CAPABILITY_PROBE_SCRIPT: &str = concat!(
    ". /etc/os-release 2>/dev/null; echo \"OS=${ID:-unknown}${ID_LIKE:+ ${ID_LIKE}}\"\n",
    "if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then echo SYSTEMD=yes; else echo SYSTEMD=no; fi",
);

/// 解析 CAPABILITY_PROBE_SCRIPT 输出 → (OS 描述, 是否运行 systemd)。
/// OS 行缺失/为空 → 「未知」；SYSTEMD 行缺失或取值非 `yes` → false。
pub fn parse_capability(stdout: &str) -> (String, bool) {
    let os = stdout
        .lines()
        .find_map(|l| l.strip_prefix("OS="))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("未知")
        .to_string();
    let systemd = stdout.lines().any(|l| l.trim() == "SYSTEMD=yes");
    (os, systemd)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml parse_capability`
Expected: PASS（1 passed）

Run: `cargo test --manifest-path src-tauri/Cargo.toml capability_probe_script_checks`
Expected: PASS（1 passed）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): 能力探测常量与解析（systemd 判定取代发行版名字）"
```

---

### Task 2: DeployRequest / DeployInput 加 enable_bbr 字段

> 先做本任务：Task 3 的新用例构造 `DeployRequest` 时带 `enable_bbr`，字段不存在则无法编译。

**Files:**
- Modify: `src-tauri/src/jobs/mod.rs:29-34`、`src-tauri/src/domain.rs:200-204`、`src-tauri/src/commands.rs:429-434`、`:473-478`
- Modify（机械补全构造点）：`src-tauri/src/jobs/tests.rs:110`、`src-tauri/src/commands_tests.rs:232/259/267/277`、`src-tauri/src/net/runtime_tests.rs:248`、`src-tauri/src/servers/deploy_tests.rs:529/624/686/753/829/942/984/1024/1065/1115`

**Interfaces:**
- Consumes: 无
- Produces: `DeployRequest.enable_bbr: bool`、`DeployInput.enable_bbr: Option<bool>`

- [ ] **Step 1: 改两个结构体定义**

`src-tauri/src/jobs/mod.rs:29-34`：

```rust
#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub job_id: i64,
    pub server_id: i64,
    pub protocols: Vec<String>,
    pub reality_dest: Option<String>,
    /// 部署成功后是否顺手检测并开启 BBR（失败不影响部署结论）
    pub enable_bbr: bool,
}
```

`src-tauri/src/domain.rs:200-204`：

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct DeployInput {
    pub server_id: i64,
    pub protocols: Vec<String>,
    pub reality_dest: Option<String>,
    /// 缺省 false。Tauri 的 Option 参数在 key 缺失时走 visit_none()，不报错
    /// （tauri-2.11.5/src/ipc/command.rs:134-145 已核实）
    pub enable_bbr: Option<bool>,
}
```

- [ ] **Step 2: 改生产代码两个构造点**

`src-tauri/src/commands.rs:429-434`（`start_deploy`）：

```rust
            crate::jobs::DeployRequest {
                job_id,
                server_id: input.server_id,
                protocols: input.protocols,
                reality_dest: input.reality_dest,
                enable_bbr: input.enable_bbr.unwrap_or(false),
            },
```

`src-tauri/src/commands.rs:473-478`（`install_supplement`，补装不触发 BBR）：

```rust
        DeployInput {
            server_id,
            protocols: merged,
            reality_dest: None,
            enable_bbr: None,
        },
```

- [ ] **Step 3: 机械补全所有测试构造点**

规则：构造 `DeployRequest` 的地方加 `enable_bbr: false,`；构造 `DeployInput` 的地方加 `enable_bbr: None,`。逐个落到（行号为改动前的当前值）：

| 文件 | 行 | 加什么 |
|---|---|---|
| `jobs/tests.rs` | 110（`req()` 内） | `enable_bbr: false,` |
| `commands_tests.rs` | 232 / 259 / 267 / 277 | `enable_bbr: None,` |
| `net/runtime_tests.rs` | 248 | `enable_bbr: None,` |
| `servers/deploy_tests.rs` | 529 / 624 / 686 / 753 / 829 / 942 / 984 / 1024 / 1065 / 1115 | `enable_bbr: false,` |

- [ ] **Step 4: 编译确认**

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 无 `missing field` 错误。若有遗漏，按报错的文件与行号补齐（这正是本步骤的用途）

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- jobs::`
Expected: PASS（`DeployRequest` 相关既有用例不受影响）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/jobs/mod.rs src-tauri/src/domain.rs src-tauri/src/commands.rs \
        src-tauri/src/jobs/tests.rs src-tauri/src/commands_tests.rs \
        src-tauri/src/net/runtime_tests.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): DeployRequest/DeployInput 加 enable_bbr 字段

DeployInput 用 Option<bool>（Tauri 的 Option 参数在 key 缺失时走
visit_none()，已核实 tauri-2.11.5/src/ipc/command.rs:134-145），
start_deploy 处 unwrap_or(false)；install_supplement 传 None——
补装是「再加一个协议」，首装时已问过 BBR。"
```

---

### Task 3: 用能力探测替换 os_is_supported 闸门

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs:310-316`（删 `os_is_supported`）、`:698-715`（换探测与闸门）
- Modify: `src-tauri/src/servers/deploy_tests.rs`（删旧用例与 import、替换 9 处夹具、重写被推翻的用例、新增正例）

**Interfaces:**
- Consumes: Task 1 的 `CAPABILITY_PROBE_SCRIPT`、`parse_capability`
- Produces: 无新符号；`os_is_supported` 从模块中消失

- [ ] **Step 1: 写失败测试（重写被推翻的用例 + 新增正例）**

把 `src-tauri/src/servers/deploy_tests.rs:975-1011` 的 `unsupported_os_stops_before_any_config` **整体替换**为下面两个用例：

```rust
/// 无 systemd 的系统（Alpine/OpenRC 等）必须在任何配置写入前停下，
/// 报错要说清缺的是哪个能力，而不是笼统的「不支持的系统」。
#[tokio::test]
async fn non_systemd_host_stops_before_any_config() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    srv.push_script("uname -m", 0, "x86_64\n", "");
    srv.push_script("/etc/os-release", 0, "OS=alpine\nSYSTEMD=no\n", "");
    let server_id = seeded_server(&state, &srv).await;
    let req = crate::jobs::DeployRequest {
        job_id: 3,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: None,
        enable_bbr: false,
    };
    let runner = test_runner(&state, &mock).await;
    let err = runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("未运行 systemd"), "{err}");
    assert!(err.contains("alpine"), "报错须带上系统标识: {err}");
    assert!(
        srv.recorded()
            .iter()
            .all(|c| !c.contains("install-release.sh")),
        "无 systemd 的机器不得进入安装"
    );
    assert!(
        crate::db::repos::nodes::list(&state.db.lock(), &state.creds)
            .unwrap()
            .is_empty()
    );
}

/// 回归（本次改动的核心目标）：CentOS 这类非 Debian 系系统只要在跑 systemd
/// 就必须放行——旧实现按名字白名单把它挡在门外。
#[tokio::test]
async fn non_debian_host_with_systemd_deploys_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    srv.push_script("uname -m", 0, "x86_64\n", "");
    srv.push_script("/etc/os-release", 0, "OS=centos rhel fedora\nSYSTEMD=yes\n", "");
    srv.push_script("command -v xray", 1, "", ""); // 未安装
    srv.push_script("ss -tln", 1, "", ""); // 端口空闲
    srv.push_script("install-release.sh", 0, "installed\n", "");
    srv.push_script(
        "xray x25519",
        0,
        "PrivateKey: cHItdjE\nPassword (PublicKey): cHViMQ\nHash32: x\n",
        "",
    );
    srv.push_script("systemctl enable --now xray", 0, "", "");
    srv.push_script("systemctl is-active xray", 0, "active\n", "");
    srv.push_script("grep -q '^LISTEN'", 0, "LISTEN 0 0 0.0.0.0:443\n", "");
    let server_id = seeded_server(&state, &srv).await;
    let req = crate::jobs::DeployRequest {
        job_id: 4,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: Some("www.microsoft.com".into()),
        enable_bbr: false,
    };
    let runner = test_runner(&state, &mock).await;
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .expect("systemd 系发行版必须部署成功");
    let nodes = crate::db::repos::nodes::list(&state.db.lock(), &state.creds).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].protocol, "vless");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml non_systemd_host_stops_before_any_config`
Expected: FAIL —— 当前实现仍发 `cat /etc/os-release`，夹具压的 `/etc/os-release` 被它命中并返回 `OS=alpine\nSYSTEMD=no`（无 `ID=` 行）→ `os_is_supported` 返回 false → 报错文本是「暂不支持的系统」而非「未运行 systemd」，断言失败

Run: `cargo test --manifest-path src-tauri/Cargo.toml non_debian_host_with_systemd_deploys_successfully`
Expected: FAIL —— 被名字白名单拒绝（`unsupported_os_stops_before_any_config` 原本正断言这一点）

- [ ] **Step 3: 实现（四处改动）**

**3a. 删除 `os_is_supported`** —— 删掉 `src-tauri/src/servers/deploy.rs:310-316` 整段（含文档注释）：

```rust
/// 校验 OS 支持：ID 或 ID_LIKE 含 ubuntu/debian。
pub fn os_is_supported(os_release: &str) -> bool { … }
```

**3b. 替换探测与闸门** —— 把 `:698-715` 这一段：

```rust
        let os_rel = run_cmd(
            &mut sess,
            cancel,
            "探测",
            "cat /etc/os-release",
            PROBE_TIMEOUT,
        )
        .await?
        .stdout;
        if !os_is_supported(&os_rel) {
            return Err(format!(
                "暂不支持的系统（仅支持 Ubuntu/Debian 家族）: {}",
                os_rel
                    .lines()
                    .find(|l| l.starts_with("ID="))
                    .unwrap_or("未知")
            ));
        }
```

换成：

```rust
        let caps = run_cmd(
            &mut sess,
            cancel,
            "探测",
            CAPABILITY_PROBE_SCRIPT,
            PROBE_TIMEOUT,
        )
        .await?
        .stdout;
        let (os, has_systemd) = parse_capability(&caps);
        if !has_systemd {
            return Err(format!("该系统未运行 systemd（{os}），暂不支持自动部署"));
        }
```

**3c. 替换 9 处夹具 matcher** —— `src-tauri/src/servers/deploy_tests.rs:504/607/675/730/803/919/1020/1053` 各有一行：

```rust
    srv.push_script("cat /etc/os-release", 0, "ID=ubuntu\n", "");
```

改为（注意 `:675` 那处原文是 `"ID=debian\n"`，一并统一为 ubuntu —— 该用例测的是 ss 部署，与发行版无关）：

```rust
    srv.push_script("/etc/os-release", 0, "OS=ubuntu\nSYSTEMD=yes\n", "");
```

**3d. 删除旧用例与 import** —— 删掉 `deploy_tests.rs:278-285` 的 `os_release_support_matrix`，并从 `:7-13` 的 `use crate::servers::deploy::{…}` 列表中移除 `os_is_supported`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- servers::deploy`
Expected: 全绿（含被重写的两个用例、`xray_deploy_full_sequence_writes_config_and_node` 等依赖夹具的全部用例）

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): 能力探测取代发行版名字白名单

旧实现按 /etc/os-release 的 ID/ID_LIKE 只放行 ubuntu/debian 家族，
CentOS/Fedora/Arch 及国内云厂商衍生版一律被拒。而流水线本身几乎不依赖
发行版——Xray 走官方 install-release.sh（自带 apt/yum/dnf 探测），
shadowsocks-rust 与 sing-box 都是 GitHub Releases 二进制直落
/usr/local/bin + 工具自写 systemd unit。

改成探测 /run/systemd/system 目录 + systemctl 可执行：看能力不看名字。
两个判据都要——容器/WSL 里 systemctl 存在但 systemd 未运行，只看命令
存在会走到「unit 写完、enable 静默不生效、端口验证失败」的半截状态。

既有 unsupported_os_stops_before_any_config 用例断言 ID=centos 被拒，
正是本次要推翻的行为，重写为 non_systemd_host_stops_before_any_config
（Alpine 无 systemd → 明确报错）并补 CentOS 正例。"
```

---

### Task 4: sing-box 自签证书改包管理器探测

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`（新增 `OPENSSL_INSTALL_SCRIPT` 常量与 `singbox_cert_cmd()`；替换 `:918-919`）
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub const OPENSSL_INSTALL_SCRIPT: &str`
  - `pub fn singbox_cert_cmd() -> String`

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/servers/deploy_tests.rs` 的 Task 1 两个用例之后插入：

```rust
// ---------- sing-box 自签证书：openssl 按包管理器安装 ----------

/// 六个包管理器分支齐备 + 全无时的可识别标记。
/// 顺序与写法对齐 server_speed/iperf.rs 的 iperf_install_cmd（同仓既有先例）。
#[test]
fn openssl_install_script_covers_all_package_managers() {
    let s = OPENSSL_INSTALL_SCRIPT;
    for pm in [
        "apt-get install -y openssl",
        "dnf install -y openssl",
        "yum install -y openssl",
        "pacman -Sy --noconfirm openssl",
        "zypper -n install openssl",
        "apk add openssl",
    ] {
        assert!(s.contains(pm), "缺分支 {pm}: {s}");
    }
    assert!(s.contains("NO_OPENSSL"), "全无包管理器须有可识别标记: {s}");
    // 必须是 command -v 探测链，不能硬编码某一家
    for pm in ["apt-get", "dnf", "yum", "pacman", "zypper", "apk"] {
        assert!(s.contains(&format!("command -v {pm}")), "缺探测 {pm}: {s}");
    }
}

/// 证书命令保持幂等（已有证书跳过），且仅在缺 openssl 时才走安装分支。
#[test]
fn singbox_cert_cmd_is_idempotent_and_installs_openssl_on_demand() {
    let c = singbox_cert_cmd();
    assert!(c.contains("if ! command -v openssl"), "缺 openssl 才安装: {c}");
    assert!(c.contains(OPENSSL_INSTALL_SCRIPT), "须内联安装脚本: {c}");
    assert!(
        c.contains("[ ! -f /etc/sing-box/self.crt ]"),
        "已有证书必须跳过（幂等）: {c}"
    );
    assert!(c.contains("mkdir -p /etc/sing-box"), "{c}");
    assert!(c.contains("openssl req -x509"), "{c}");
    // 不得只剩 apt 一条路（回归防线：本次改动前正是写死 apt-get）
    assert!(c.contains("command -v dnf"), "须走包管理器探测而非写死 apt: {c}");
    assert!(c.contains("command -v apk"), "{c}");
}
```

并把 `singbox_cert_cmd`、`OPENSSL_INSTALL_SCRIPT` 加进 `deploy_tests.rs` 顶部的 import 列表。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml openssl_install_script_covers`
Expected: 编译失败 —— `cannot find value OPENSSL_INSTALL_SCRIPT`

Run: `cargo test --manifest-path src-tauri/Cargo.toml singbox_cert_cmd_is_idempotent`
Expected: 编译失败 —— `cannot find function singbox_cert_cmd`

- [ ] **Step 3: 实现**

在 `src-tauri/src/servers/deploy.rs` 的 `SINGBOX_UNIT_PATH`（`:445`）附近新增：

```rust
/// openssl 缺失时的安装命令：按包管理器探测，顺序与写法对齐
/// server_speed/iperf.rs 的 iperf_install_cmd。全无 → NO_OPENSSL + exit 1，
/// 调用方识别成可读错误。实际很少触发：RHEL 系 / Arch 上 openssl 是 curl 的依赖。
pub const OPENSSL_INSTALL_SCRIPT: &str = concat!(
    "if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && apt-get install -y openssl;\n",
    "elif command -v dnf >/dev/null 2>&1; then dnf install -y openssl;\n",
    "elif command -v yum >/dev/null 2>&1; then yum install -y openssl;\n",
    "elif command -v pacman >/dev/null 2>&1; then pacman -Sy --noconfirm openssl;\n",
    "elif command -v zypper >/dev/null 2>&1; then zypper -n install openssl;\n",
    "elif command -v apk >/dev/null 2>&1; then apk add openssl;\n",
    "else echo NO_OPENSSL; exit 1; fi",
);

/// sing-box 自签证书生成命令（幂等）：已有证书跳过；缺 openssl 时按包管理器安装。
/// sing-box 两个入站强制要求显式 tls 块，无证书直接 FATAL。
pub fn singbox_cert_cmd() -> String {
    format!(
        "mkdir -p /etc/sing-box; \
         if ! command -v openssl >/dev/null 2>&1; then {OPENSSL_INSTALL_SCRIPT}; fi; \
         if [ ! -f /etc/sing-box/self.crt ]; then \
         openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
         -keyout /etc/sing-box/self.key -out /etc/sing-box/self.crt -days 3650 \
         -subj /CN=vpn-ctl.singbox; fi"
    )
}
```

**替换 `:918-919`** —— 把 `let cert_cmd = "…";` 那行与紧随的 `run_cmd_ok(...)` 换成：

```rust
            // 自签证书（幂等：已存在跳过）。缺 openssl 时按包管理器安装；
            // 全无包管理器 → 脚本回 NO_OPENSSL，映射成可读错误（而非裸 exit code）
            let out = run_cmd(
                &mut sess,
                cancel,
                "配置",
                &singbox_cert_cmd(),
                CONFIG_TIMEOUT,
            )
            .await?;
            if out.stdout.contains("NO_OPENSSL") {
                return Err(
                    "该系统找不到 openssl，也没有可识别的包管理器（apt-get/dnf/yum/pacman/zypper/apk），无法生成 sing-box 自签证书"
                        .into(),
                );
            }
            if out.exit_code != 0 {
                return Err(format!(
                    "生成自签证书失败(exit={}): {}",
                    out.exit_code,
                    out.stderr.trim_end()
                ));
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml openssl_install_script_covers`
Expected: PASS（1 passed）

Run: `cargo test --manifest-path src-tauri/Cargo.toml singbox_cert_cmd_is_idempotent`
Expected: PASS（1 passed）

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- servers::deploy`
Expected: 全绿（sing-box 路径的既有用例走新命令；`run_cmd` 与 `run_cmd_ok` 对 FakeSshServer 的差异只在不匹配脚本条目时的分支，既有夹具不受影响）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): sing-box 自签证书改按包管理器探测安装 openssl

原来写死 apt-get install -y openssl，非 Debian 系发行版走到这里必然失败。
改成 apt-get→dnf→yum→pacman→zypper→apk 顺序探测，全无则回 NO_OPENSSL
标记映射成可读错误（而非裸 exit code）。写法对齐 server_speed/iperf.rs
既有的 iperf_install_cmd。"
```

---

### Task 5: BBR 编排接入部署流水线

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`（在 `:1052` 之后插入 BBR 块；`:1096-1100` 摘要加字段；`:1109-1111` 完成文案）
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Consumes: Task 2 的 `req.enable_bbr`；`crate::server_bbr::{check_bbr, enable_bbr}`
- Produces: `deploys.config_json` 新增可选 `"bbr"` 字符串字段

- [ ] **Step 1: 写失败测试**

在 `deploy_tests.rs` 中 Task 4 的用例之后插入。先加两个夹具：

```rust
// ---------- BBR 编排 ----------

/// xray 全序列 + BBR 检测/开启响应。检测与开启脚本都含 "sysctl -n"，
/// 但 check 先消费掉队首那条，开启脚本再匹配 "net.core.default_qdisc=fq"。
fn xray_script_with_bbr(
    srv: &FakeSshServer,
    check_stdout: &str,
    enable_exit: u32,
    enable_stdout: &str,
) {
    xray_script(srv);
    srv.push_script("sysctl -n", 0, check_stdout, "");
    srv.push_script("net.core.default_qdisc=fq", enable_exit, enable_stdout, "");
}

/// 读最新一次部署摘要。
fn latest_deploy_summary(state: &Arc<AppState>, server_id: i64) -> serde_json::Value {
    let (cfg,): (String,) = state
        .db
        .lock()
        .query_row(
            "SELECT config_json FROM deploys WHERE server_id=?1 ORDER BY id DESC LIMIT 1",
            rusqlite::params![server_id],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    serde_json::from_str(&cfg).unwrap()
}

/// 已经是 bbr → 不下发开启脚本（没必要多跑一次 sysctl 写），摘要记「已启用」。
#[tokio::test]
async fn bbr_already_enabled_skips_enable_script() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    xray_script_with_bbr(&srv, "bbr\n", 0, "bbr\n");
    let server_id = seeded_server(&state, &srv).await;
    let req = crate::jobs::DeployRequest {
        job_id: 5,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: Some("www.microsoft.com".into()),
        enable_bbr: true,
    };
    let runner = test_runner(&state, &mock).await;
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    assert!(
        srv.recorded().iter().all(|c| !c.contains("modprobe tcp_bbr")),
        "已是 bbr 不得下发开启脚本: {:?}",
        srv.recorded()
    );
    assert_eq!(latest_deploy_summary(&state, server_id)["bbr"], "BBR 已启用");
}

/// 非 bbr → 下发开启脚本，回读 bbr → 摘要记「已开启 bbr」（沿用 enable_bbr 的返回值）。
#[tokio::test]
async fn bbr_enable_runs_script_and_records_result() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    xray_script_with_bbr(&srv, "cubic\n", 0, "bbr\n");
    let server_id = seeded_server(&state, &srv).await;
    let req = crate::jobs::DeployRequest {
        job_id: 6,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: Some("www.microsoft.com".into()),
        enable_bbr: true,
    };
    let runner = test_runner(&state, &mock).await;
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    assert!(
        srv.recorded().iter().any(|c| c.contains("modprobe tcp_bbr")),
        "非 bbr 必须下发开启脚本"
    );
    assert_eq!(latest_deploy_summary(&state, server_id)["bbr"], "已开启 bbr");
}

/// 内核不支持 BBR（NO_BBR_MODULE）→ 部署仍成功，摘要记录失败原因。
/// 协议已经跑起来了，BBR 只是优化项，不能把成功的部署在最后一步变成 failed。
#[tokio::test]
async fn bbr_failure_does_not_fail_deploy() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    xray_script_with_bbr(&srv, "cubic\n", 1, "NO_BBR_MODULE\n");
    let server_id = seeded_server(&state, &srv).await;
    let req = crate::jobs::DeployRequest {
        job_id: 7,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: Some("www.microsoft.com".into()),
        enable_bbr: true,
    };
    let runner = test_runner(&state, &mock).await;
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .expect("BBR 失败不得让部署失败");
    // 协议节点照常写回
    assert_eq!(
        crate::db::repos::nodes::list(&state.db.lock(), &state.creds)
            .unwrap()
            .len(),
        1
    );
    let bbr = latest_deploy_summary(&state, server_id)["bbr"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(bbr.starts_with("开启失败"), "{bbr}");
    assert!(bbr.contains("内核不支持 BBR"), "{bbr}");
}

/// enable_bbr=false → 不发 BBR 任何命令、无「优化」步骤、摘要无 bbr 字段。
/// 防御性护栏（改前改后都过），盯住「不勾选就绝不碰内核参数」这条不变量。
#[tokio::test]
async fn bbr_disabled_leaves_pipeline_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    xray_script(&srv);
    let server_id = seeded_server(&state, &srv).await;
    let req = crate::jobs::DeployRequest {
        job_id: 8,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: Some("www.microsoft.com".into()),
        enable_bbr: false,
    };
    let events = Arc::new(std::sync::Mutex::new(vec![]));
    let events2 = events.clone();
    let emit: crate::jobs::EmitFn = Arc::new(move |p| events2.lock().unwrap().push(p));
    let runner = test_runner(&state, &mock).await;
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            emit,
        )
        .await
        .unwrap();
    assert!(
        srv.recorded()
            .iter()
            .all(|c| !c.contains("tcp_congestion_control")),
        "未勾选不得触 BBR: {:?}",
        srv.recorded()
    );
    assert!(
        events.lock().unwrap().iter().all(|e| e.step != "优化"),
        "未勾选不得有优化步骤"
    );
    assert!(latest_deploy_summary(&state, server_id).get("bbr").is_none());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml bbr_already_enabled_skips_enable_script`
Expected: FAIL —— 摘要里没有 `bbr` 字段，`summary["bbr"]` 是 `Value::Null`，断言 `== "BBR 已启用"` 失败

Run: `cargo test --manifest-path src-tauri/Cargo.toml bbr_disabled_leaves_pipeline_untouched`
Expected: PASS（该用例是护栏性质，改前改后都过 —— 见其文档注释）

- [ ] **Step 3: 实现（三处改动）**

**3a. 插入 BBR 块** —— 在 `src-tauri/src/servers/deploy.rs` 的 `:1052` `check_cancel(cancel)?;`（端口验证循环之后那条）与 `:1054` 的 `// 5 写回` 之间插入：

```rust
        // 4.5 BBR（可选）：插在端口验证之后、写回之前——协议服务确认起来了才动内核参数，
        // BBR 出问题不可能被误判成部署失败。任何失败都不上抛：协议已部署成功，
        // BBR 属优化项，加 `?` 会让一次成功的部署在最后一步变成 failed，所以要收集成字符串。
        let mut bbr_note: Option<String> = None;
        if req.enable_bbr {
            emit_progress("优化", "检测 BBR", 88);
            match crate::server_bbr::check_bbr(&mut sess).await {
                // 已是 bbr：不下发开启脚本（脚本虽幂等，但没必要多跑一次 sysctl 写）
                Ok(algo) if algo == "bbr" => bbr_note = Some("BBR 已启用".to_string()),
                Ok(algo) => {
                    emit_progress("优化", &format!("当前 {algo}，开启 BBR"), 90);
                    bbr_note = Some(match crate::server_bbr::enable_bbr(&mut sess).await {
                        Ok(m) => m,
                        Err(e) => format!("开启失败：{e}"),
                    });
                }
                Err(e) => bbr_note = Some(format!("检测失败：{e}")),
            }
        }
```

**3b. 摘要加字段** —— 把 `:1096-1100`：

```rust
            let summary = serde_json::json!({
                "protocols": ok_protocols.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(),
                "ports": serde_json::Value::Object(ports_map)
            })
            .to_string();
            repos::deploys::insert(&conn, req.server_id, "deploy", &summary, "ok")
```

换成：

```rust
            let mut summary = serde_json::json!({
                "protocols": ok_protocols.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(),
                "ports": serde_json::Value::Object(ports_map)
            });
            // BBR 结果（有才写）：parse_deployed_protocols 只读 protocols、容忍额外字段，
            // 不影响既有摘要解析
            if let Some(note) = &bbr_note {
                summary["bbr"] = serde_json::Value::from(note.clone());
            }
            let summary = summary.to_string();
            repos::deploys::insert(&conn, req.server_id, "deploy", &summary, "ok")
```

**3c. 完成文案带 BBR 结果** —— 把 `:1109-1111`：

```rust
        if failed_protocols.is_empty() {
            emit_progress("写回", "部署完成", 100);
            Ok(())
```

换成：

```rust
        if failed_protocols.is_empty() {
            // BBR 结果并进 100% 文案：用户不点开摘要也能看到
            let done = match &bbr_note {
                Some(n) => format!("部署完成（{n}）"),
                None => "部署完成".to_string(),
            };
            emit_progress("写回", done.as_str(), 100);
            Ok(())
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml bbr_`
Expected: PASS（4 passed）

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- servers::deploy`
Expected: 全绿 —— 特别确认 `xray_deploy_full_sequence_writes_config_and_node` 仍过（`:582-594` 的步骤断言未变）

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
Expected: 无告警

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): BBR 接入部署流水线（端口验证后、写回前）

插在端口验证之后、写回之前——协议服务确认起来了才动内核参数，BBR 出
问题不可能被误判成部署失败。任何失败都不上抛：协议已部署成功，BBR 属
优化项，加 ? 会让一次成功的部署在最后一步变成 failed，所以收集成字符串
落进 deploys 摘要的 bbr 字段，并带进 100% 完成文案。

已是 bbr 时不下发开启脚本（脚本虽幂等，但没必要多跑一次 sysctl 写）。
server_bbr.rs 一行未改，直接复用 check_bbr/enable_bbr。

摘要四种取值：已是 bbr→「BBR 已启用」；开启成功→「已开启 bbr」；
开启失败→「开启失败：{原因}」；检测失败→「检测失败：{原因}」。前两条
刻意不同：一个表示本来就是，一个表示本次改动生效了。"
```

---

### Task 6: IPC 参数与部署向导复选框

**Files:**
- Modify: `src-tauri/src/commands.rs:1085-1100`（`start_deploy_cmd`）
- Modify: `src/api/types.ts:132-136`
- Modify: `src/components/DeployWizard.tsx`
- Test: `src/components/DeployWizard.test.tsx`、`src-tauri/src/ipc_tests.rs`

**Interfaces:**
- Consumes: Task 2 的 `DeployInput.enable_bbr: Option<bool>`
- Produces: IPC 参数 `enableBbr?: boolean`（camelCase 线格式）

- [ ] **Step 1: 写失败测试**

**后端** —— 在 `src-tauri/src/ipc_tests.rs` 的 `enable_bbr_cmd_ipc_uses_server_id_casing`（`:810-827`）之后追加（夹具用法照抄该组既有用例）：

```rust
/// start_deploy_cmd 的 enableBbr 是可选参数：缺省与显式都要能解。
/// Tauri 的 Option 参数在 key 缺失时走 visit_none()，不报 InvalidArgs
/// （tauri-2.11.5/src/ipc/command.rs:134-145）。
#[tokio::test(flavor = "multi_thread")]
async fn start_deploy_cmd_accepts_optional_enable_bbr() {
    let (_d, state) = test_state();
    let id = crate::servers::create_server(&state, server_input("127.0.0.1", 1, "landing"))
        .unwrap()
        .id;
    let (app, wv) = mock_app(state.clone());
    // 不传 enableBbr → None → unwrap_or(false)
    let v = invoke_ok(
        &wv,
        "start_deploy_cmd",
        json!({ "serverId": id, "protocols": ["vless"] }),
    );
    assert!(v.as_i64().unwrap() > 0, "响应: {v}");
    // 显式 true 同样可解
    let v2 = invoke_ok(
        &wv,
        "start_deploy_cmd",
        json!({ "serverId": id, "protocols": ["vless"], "enableBbr": true }),
    );
    assert!(v2.as_i64().unwrap() > 0, "响应: {v2}");
    drop(wv);
    let _ = app;
}
```

**前端** —— 在 `src/components/DeployWizard.test.tsx` 的 `describe` 块内末尾追加：

```tsx
  it('默认勾选「同时开启 BBR」并随部署提交 true', async () => {
    const user = userEvent.setup()
    render(<DeployWizard server={server} onClose={() => {}} onDone={() => {}} />)
    expect(screen.getByRole('checkbox', { name: '同时开启 BBR' })).toBeChecked()
    await user.click(screen.getByRole('button', { name: /开始部署/ }))
    await waitFor(() => expect(deployArgs.length).toBe(1))
    expect((deployArgs[0] as { enableBbr: boolean }).enableBbr).toBe(true)
  })

  it('取消勾选后提交 enableBbr: false', async () => {
    const user = userEvent.setup()
    render(<DeployWizard server={server} onClose={() => {}} onDone={() => {}} />)
    await user.click(screen.getByRole('checkbox', { name: '同时开启 BBR' }))
    expect(screen.getByRole('checkbox', { name: '同时开启 BBR' })).not.toBeChecked()
    await user.click(screen.getByRole('button', { name: /开始部署/ }))
    await waitFor(() => expect(deployArgs.length).toBe(1))
    expect((deployArgs[0] as { enableBbr: boolean }).enableBbr).toBe(false)
  })
```

**同时**修改既有的「提交选中的 protocols 数组与 Reality 域名」用例（`:73-77`）：该断言是 `toEqual` 精确匹配，payload 多了 `enableBbr` 就会失败。改为：

```tsx
    expect(deployArgs[0]).toEqual({
      serverId: 1,
      protocols: ['vless', 'trojan', 'vmess', 'ss', 'hysteria2'],
      realityDest: 'www.yahoo.com',
      enableBbr: true,
    })
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npm test -- DeployWizard`
Expected: FAIL —— 找不到 `role=checkbox name='同时开启 BBR'`；且精确匹配那条用例因 payload 缺 `enableBbr` 失败

Run: `cargo test --manifest-path src-tauri/Cargo.toml start_deploy_cmd_accepts_optional_enable_bbr`
Expected: 后端此例改前也过（Tauri 忽略多余 key 且 Option 可缺省），属护栏性质；真正的红转绿在前端

- [ ] **Step 3: 实现**

**3a. `src-tauri/src/commands.rs:1085-1100`**：

```rust
#[tauri::command]
pub async fn start_deploy_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    protocols: Vec<String>,
    reality_dest: Option<String>,
    enable_bbr: Option<bool>,
) -> R<i64> {
    start_deploy(
        state.inner().clone(),
        DeployInput {
            server_id,
            protocols,
            reality_dest,
            enable_bbr,
        },
    )
}
```

**3b. `src/api/types.ts:132-136`**：

```ts
export interface DeployInput {
  serverId: number
  protocols: DeployProtocol[]
  realityDest?: string
  /** 部署成功后顺手开启 BBR（失败不影响部署结论） */
  enableBbr?: boolean
}
```

**3c. `src/components/DeployWizard.tsx`**：

在 `const [realityDest, setRealityDest] = useState('www.microsoft.com')`（`:51`）之后加：

```tsx
  // BBR 默认勾选、不持久化：BBR 幂等，已开启的机器再勾也只是检测后跳过
  const [enableBbr, setEnableBbr] = useState(true)
```

在协议网格的闭合并 `</div>`（`:228`）之后、`{hasReality && !running && (`（`:230`）之前插入：

```tsx
      <label className="flex cursor-pointer select-none items-center gap-2 text-sm text-neutral-300">
        <input
          type="checkbox"
          aria-label="同时开启 BBR"
          checked={enableBbr}
          disabled={running}
          onChange={(e) => setEnableBbr(e.target.checked)}
          className="accent-emerald-600"
        />
        同时开启 BBR（拥塞控制，已开启的机器自动跳过）
      </label>
```

在 `handleStart` 的 `startDeploy({…})`（`:122-126`）里加一行：

```tsx
      const id = await startDeploy({
        serverId: server.id,
        protocols,
        realityDest: hasReality ? realityDest : undefined,
        enableBbr,
      })
```

并把 `enableBbr` 加进该 `useCallback` 的依赖数组（`:152`）：

```tsx
  }, [server.id, selected, realityDest, hasReality, enableBbr])
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npm test -- DeployWizard`
Expected: PASS（9 passed —— 原 7 条 + 新 2 条）

Run: `npx tsc --noEmit`
Expected: 无错误

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全绿

- [ ] **Step 5: 全量门禁**

Run: `npm test`
Expected: 全部通过

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
Expected: 无告警

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/ipc_tests.rs src/api/types.ts \
        src/components/DeployWizard.tsx src/components/DeployWizard.test.tsx
git commit -m "feat(ui): 部署向导加「同时开启 BBR」复选框

默认勾选、不持久化（BBR 幂等，已开启的机器再勾也只是检测后跳过），
运行中禁用。经 DeployInput.enableBbr → start_deploy_cmd 的 enableBbr
可选参数透传。既有 payload 精确匹配用例同步补 enableBbr 字段。"
```

---

### Task 7: 修复「只选一个 L2 协议」的部署 panic

**背景（既有缺陷，非本计划前六个任务引入；由 `66ff052` 引入，基线 `0d7224e` 即存在）**

`deploy.rs` 生成 sing-box 配置时**无条件**读取两个 L2 端口：

```rust
let cfg = singbox_config_json(
    ports["hysteria2"],
    &hy2_pw,
    ports["tuic"],
    &tuic_uuid,
    &tuic_pw,
);
```

而 `ports` 只装 `req.protocols` 里的协议（`for proto in &req.protocols { … ports.insert(…) }`）。
`need_singbox` 的判据是「选中集 ∩ {hysteria2, tuic} ≠ ∅」，所以**只勾 hysteria2 或只勾 tuic 时必然走进这个块**，然后 `HashMap` 索引缺失键直接 panic，而不是返回 `Err`。

实测：`panicked at src\servers\deploy.rs:979:22: no entry found for key`。

- **可达性**：前端 hysteria2 / tuic 是两个独立复选框（`DeployWizard.tsx:22-24`，`l1: false`「可选装」）；后端只校验非空 + 白名单；前端自己的用例就提交 `['vless','trojan','vmess','ss','hysteria2']`（有 hysteria2 无 tuic）。
- **孤例证据**：紧邻的两处 `success_protocols.push` 都用了 `req.protocols.contains(...)` 守卫，`ports["ss"]` 也在 `if need_ss` 内 —— **只有这一处漏了**，不是系统性写法问题。
- **伤害时序**：panic 发生在端口分配、安装、写证书**之后**，远端留下半配置。
- Task 3 让更多发行版的主机能走到这条路径，所以本任务与本计划强相关。

**修法（用户已裁定）**：按选中协议生成 inbounds。不采用「单 L2 直接拒绝」——单 L2 是合法部署形态，且按选中集生成与紧邻守卫的意图一致，补装（先装 hy2 再补 tuic）时配置会按新的选中集重建，行为正确。

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`（`singbox_config_json` 定义 + **唯一**生产调用点）
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Produces: `pub fn singbox_config_json(hy2: Option<(u16, &str)>, tuic: Option<(u16, &str, &str)>) -> String`
- 影响面已核实：全仓 `singbox_config_json` 只有 4 处出现 —— 定义（`deploy.rs`）、唯一生产调用点（`deploy.rs` 的 `if need_singbox` 块内）、import（`deploy_tests.rs:11`）、唯一测试调用点（`deploy_tests.rs` 的 `singbox_config_contains_both_l2_inbounds`）
- `read_existing_ports` 用 `parse_singbox_ports` 解析既有配置，天然容忍缺失的 inbound，**不需要改**

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn singbox_config_omits_unselected_l2_inbound() {
    // 只选 hysteria2：只出一个 inbound，且不含 tuic
    let cfg = singbox_config_json(Some((2097, "pw-hy2")), None);
    assert!(cfg.contains("\"hysteria2\""));
    assert!(
        !cfg.contains("\"tuic\""),
        "未选中的 L2 不得出现在配置里: {cfg}"
    );

    // 只选 tuic：镜像断言
    let cfg = singbox_config_json(None, Some((2053, "tcu", "pw-t")));
    assert!(cfg.contains("\"tuic\""));
    assert!(
        !cfg.contains("\"hysteria2\""),
        "未选中的 L2 不得出现在配置里: {cfg}"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml singbox_config_omits_unselected_l2_inbound`
Expected: 编译失败（现签名是五个必填参数，`None` 传不进去）。这就是 RED。

- [ ] **Step 3: 改签名与实现**

```rust
pub fn singbox_config_json(
    hy2: Option<(u16, &str)>,
    tuic: Option<(u16, &str, &str)>,
) -> String {
    let mut inbounds = Vec::new();
    if let Some((port, pw)) = hy2 {
        inbounds.push(serde_json::json!({
            "type": "hysteria2", "listen": "0.0.0.0", "listen_port": port,
            "tls": {
                "enabled": true,
                "certificate_path": "/etc/sing-box/self.crt",
                "key_path": "/etc/sing-box/self.key"
            },
            "users": [{ "password": pw }]
        }));
    }
    if let Some((port, uuid, pw)) = tuic {
        inbounds.push(serde_json::json!({
            "type": "tuic", "listen": "0.0.0.0", "listen_port": port,
            "tls": {
                "enabled": true,
                "certificate_path": "/etc/sing-box/self.crt",
                "key_path": "/etc/sing-box/self.key"
            },
            "users": [{ "uuid": uuid, "password": pw }]
        }));
    }
    serde_json::json!({
        "inbounds": inbounds,
        "outbounds": [{ "type": "direct" }]
    })
    .to_string()
}
```

- [ ] **Step 4: 改调用点**

在 `if need_singbox { … }` 块内，把原来那段无条件索引换成：

```rust
            let hy2_selected = req.protocols.iter().any(|p| p == "hysteria2");
            let tuic_selected = req.protocols.iter().any(|p| p == "tuic");
            let cfg = singbox_config_json(
                hy2_selected.then(|| (ports["hysteria2"], hy2_pw.as_str())),
                tuic_selected.then(|| (ports["tuic"], tuic_uuid.as_str(), tuic_pw.as_str())),
            );
```

**必须用 `then(|| …)` 闭包形式，绝不能写 `then_some((ports["hysteria2"], …))`** —— `then_some` 的参数是**急切求值**的，`ports["hysteria2"]` 在 `then_some` 被调用前就已经索引了，panic 原封不动地留着。这是本任务唯一容易「改了个寂寞」的地方，请务必自查这一条。

（`hy2_pw` / `tuic_uuid` / `tuic_pw` 三个随机值仍可无条件生成，无副作用，不必改。）

- [ ] **Step 5: 更新既有测试调用点**

`deploy_tests.rs` 的 `singbox_config_contains_both_l2_inbounds` 改成传两个 `Some(...)`，断言（两个 inbound 都在）不变。

- [ ] **Step 6: 补端到端回归（真正盯住 panic 的那条）**

Step 1 的单元测试只证明配置生成正确，**证明不了调用点不再 panic** —— panic 在调用点。照 `singbox_cert_nonzero_exit_surfaces_code_and_stderr` 的夹具写法各加一个 e2e：

1. **只选 hysteria2**（`protocols: vec!["hysteria2".into()]`）
2. **只选 tuic**（`protocols: vec!["tuic".into()]`）

两者都断言 `run(...)` 返回 `Ok(())` 且节点写回一行、端口正确。

**这两个用例在修复前必须 panic**（先写、先跑，确认 RED 是 panic 而不是断言失败）。

- [ ] **Step 7: 变异验证**

把 Step 4 的 `hy2_selected.then(|| …)` 改回 `Some((ports["hysteria2"], …))`（即恢复无条件索引），重跑 Step 6 的两个 e2e → **必须 FAILED（panic）**；还原 → **必须 PASS**。两个方向的输出都留档。

- [ ] **Step 8: 门禁三连 + 提交**

Run（逐个跑，**不要用管道接 tail/head**，会吞掉退出码）:
- `cargo fmt --check --manifest-path src-tauri/Cargo.toml`
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`

写完代码先跑一次 `cargo fmt`（不带 `--check`）让它自己改写，再跑 `--check` 确认干净 —— **本计划已三次栽在这上面**（Task 1 三条 assert 超宽、Task 3 fixture 101 字符、Task 4 新 assert 该收成一行）。接受 rustfmt 的改写结果。

用例数：Task 6 完成后应 ≥ 434，本任务再加 3 个（1 单元 + 2 e2e）。

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "fix(deploy): 只选单个 L2 协议时按选中集生成 sing-box 配置（修 panic）"
```

**暂存必须逐个列路径，禁止 `git add -A` / `git add .`。**

---

## 验收

全部任务完成后逐条勾：

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` 全绿
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings` 无告警
- [ ] `npx tsc --noEmit` 无错误
- [ ] `npm test` 全绿
- [ ] `grep -rn "os_is_supported" src-tauri/src` 无结果（白名单已彻底移除）
- [ ] `git status --short` 里本次涉及的 7 个文件均已提交，**其余 25 个上次会话遗留改动仍处于未跟踪/未暂存状态**（未被误提交）
- [ ] `grep -n 'ports\["' src-tauri/src/servers/deploy.rs` 的**每一处**都在对应的 `need_*` / `*_selected` 守卫内（Task 7）
- [ ] 手工验证（可选，需真实服务器）：对一台 CentOS/Arch 机器点部署 → 不再报「暂不支持的系统」；勾选「同时开启 BBR」→ 进度出现「优化」步骤、完成后摘要含 `bbr` 字段
- [ ] 手工验证（可选）：只勾 **hysteria2** 或只勾 **tuic** 点部署 → 不 panic，正常完成并写回一行节点（Task 7）
