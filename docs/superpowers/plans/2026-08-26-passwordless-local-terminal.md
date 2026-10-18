# 一键免密覆盖本机终端 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 点击「一键免密」后，除 App 自持密钥外，本机 `~/.ssh` 已有密钥（或自动生成的密钥）一并登记到服务器，使 PowerShell 里 `ssh user@host` 同样免密。

**Architecture:** `setup_passwordless`（`src-tauri/src/servers/mod.rs:371`）在 App 钥验证通过后、凭据切换前，调用新函数 `prep_local_keys`：扫描本机 `~/.ssh` 标准私钥（有 `.pub` 读文件、只有私钥用 russh 派生公钥），一个都没有则生成 ed25519 写入 `~/.ssh/id_ed25519(+.pub)` 并设置 `ssh-keygen` 同款权限（Win icacls / unix chmod）；每行公钥经既有 `grep -qF || echo` 幂等追加到服务器。App 密钥链路、数据库密封、凭据切换一字不改。

**Tech Stack:** Rust + russh 0.53（`keys::PrivateKey::random` / `decode_secret_key` / `to_openssh`），rand 0.8，thiserror；测试用既有 `FakeSshServer`（`ssh/tests.rs`，未匹配命令回落 exit 0）+ tempdir。

> **执行修订（2026-08-26，以提交 06ca8b9/c657090 为准）：** icacls 改为三个独立 `/grant:r`（逗号合并或一个 grant 多身份在当前 icacls 报非法参数/退出 87）；**仅锁私钥文件、不锁 `.ssh` 目录**——目录 icacls 会剥掉受限令牌（沙箱/CI）的继承遍历权导致自锁，新目录继承 `%USERPROFILE%` ACL 已等同 ssh-keygen 产物；`CREATOR OWNER` 参数被当前 icacls 静默忽略故未使用。

## Global Constraints

- 不加新依赖（russh/rand 已有）。
- 错误信息用中文，错误类型用 thiserror 变体（现有 `ServersError`）。
- 公钥行在服务器侧统一规范为「type base64」两段（去掉注释）——保证 `grep -qF` 幂等匹配串稳定，也避免用户密钥注释中的单引号破坏远程命令。
- 现有 App 钥链路（每服务器独立钥、私钥密封入库、`vpn-ctl-admin` 注释）**不得改动**。
- 本机准备任何一步失败 → 整个免密 `Err` 并中止，数据库事务不执行（设计：失败即中止）。
- 测试运行平台：本机 Windows + CI `windows-latest`。icacls 路径 `#[cfg(windows)]`；`#[cfg(not(windows))]` 走 chmod 兜底（测试可移植性）。
- 判断标准：`cargo fmt` → `cargo clippy --all-targets -- -D warnings` → `cargo test`（在 `src-tauri/` 下运行）。

---

### Task 1: 本机密钥准备 `prep_local_keys` + `ServersError::LocalKey` + 单测

**Files:**
- Modify: `src-tauri/src/servers/mod.rs`（`ServersError` 加变体；文件尾部加 `prep_local_keys` / `generate_local_key` / `normalize_pub_line` / `default_ssh_dir` / `lock_key_permissions`）
- Test: `src-tauri/src/servers/tests.rs`

**Interfaces:**
- Produces:
  - `pub fn prep_local_keys(ssh_dir: &std::path::Path) -> Result<Vec<String>, ServersError>` — 返回待登记公钥行（每行已规范化「type base64」，无注释，非空）；扫描（`.pub` 优先 → 私钥派生 → 全无则生成）必然至少返回 1 行。
  - `fn default_ssh_dir() -> Result<std::path::PathBuf, ServersError>` — `%USERPROFILE%\.ssh`（unix `$HOME/.ssh`）。
  - `ServersError::LocalKey(String)`，错误文案形如「本机免密准备失败: {0}」。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/servers/tests.rs` 追加（文件顶部 `use` 已含 `tempfile`；无需新 import）：

```rust
#[test]
fn prep_local_keys_generates_when_none_exist() {
    let dir = tempfile::tempdir().unwrap();
    let lines = crate::servers::prep_local_keys(dir.path()).unwrap();
    assert_eq!(lines.len(), 1, "无本机密钥时应生成一把: {lines:?}");
    let line = &lines[0];
    let tokens: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(tokens.len(), 2, "规范行应只有 type+base64: {line}");
    assert_eq!(tokens[0], "ssh-ed25519");
    // 私钥文件写盘且为 OpenSSH PEM
    let pem = std::fs::read_to_string(dir.path().join("id_ed25519")).unwrap();
    assert!(pem.contains("OPENSSH PRIVATE KEY"), "{pem}");
    // .pub 文件含固定注释 vpn-ctl-local（溯源线）
    let pub_file = std::fs::read_to_string(dir.path().join("id_ed25519.pub")).unwrap();
    assert!(pub_file.contains("vpn-ctl-local"), "{pub_file}");
    // .pub 文件令牌与返回行一致（服务器行 = 规范化后的文件行）
    assert_eq!(
        pub_file.split_whitespace().take(2).collect::<Vec<_>>().join(" "),
        *line
    );
    // 生成的私钥可被 russh 解码（与 App 解密链同一解析器）
    russh::keys::decode_secret_key(&pem, None).unwrap();
}

#[test]
fn prep_local_keys_reads_existing_pub_and_derives_missing_pub() {
    let dir = tempfile::tempdir().unwrap();
    // 有 .pub：直接读文件（注释被剥掉）
    std::fs::write(
        dir.path().join("id_rsa.pub"),
        "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDtest 老板的钥匙@laptop\n",
    )
    .unwrap();
    // 只有私钥、无 .pub：从私钥派生公钥
    let kp = russh::keys::PrivateKey::random(
        &mut rand::rngs::OsRng,
        russh::keys::ssh_key::Algorithm::Ed25519,
    )
    .unwrap();
    let pem = kp
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .unwrap()
        .to_string();
    std::fs::write(dir.path().join("id_ed25519"), pem).unwrap();

    let lines = crate::servers::prep_local_keys(dir.path()).unwrap();
    // 两个公钥行都必须存在（顺序：id_ed25519 派生在前，id_rsa 读取在后）
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1], "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDtest"); // 注释已剥除
    assert!(lines[0].starts_with("ssh-ed25519 "));
}

#[test]
fn prep_local_keys_generated_key_reuses_existing_pub_line_on_second_call() {
    let dir = tempfile::tempdir().unwrap();
    let first = crate::servers::prep_local_keys(dir.path()).unwrap();
    // 幂等：第二次应读回同一把（.pub 先行分支）
    let second = crate::servers::prep_local_keys(dir.path()).unwrap();
    assert_eq!(first, second);
}
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test prep_local_keys`（在 `src-tauri/` 下）
Expected: 编译失败或 FAIL —— `prep_local_keys` 尚未定义（测试引用不存在的函数）即可表明失败态。

- [ ] **Step 3: 实现 `prep_local_keys` 族**

在 `src-tauri/src/servers/mod.rs` 的 `ServersError` 枚举中追加变体：

```rust
    #[error("本机免密准备失败: {0}")]
    LocalKey(String),
```

文件尾部追加（放在 `parse_pretty_name` 之后、`setup_passwordless` 之前）：

```rust
/// 标准 OpenSSH 私钥名（客户端默认尝试集的常用子集）。
const LOCAL_KEY_NAMES: [&str; 3] = ["id_ed25519", "id_rsa", "id_ecdsa"];

/// 本机 OpenSSH 密钥目录：Windows `%USERPROFILE%\.ssh`（unix `$HOME/.ssh`）。
fn default_ssh_dir() -> Result<std::path::PathBuf, ServersError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| {
            ServersError::LocalKey("无法确定用户主目录（USERPROFILE/HOME 未设置）".into())
        })?;
    Ok(std::path::PathBuf::from(home).join(".ssh"))
}

/// 规范化为「type base64」两段：剥掉注释（避免密钥注释中的引号破坏远程命令），
/// 同时让 grep -qF 幂等匹配串稳定。解析失败返回 None（不是合法的公钥行）。
fn normalize_pub_line(line: &str) -> Option<String> {
    let tokens: Vec<&str> = line.split_whitespace().take(2).collect();
    (tokens.len() == 2).then(|| tokens.join(" "))
}

/// 收集本机免密的公钥行（双钥并行：与 App 的每服务器独立钥互不影响）。
/// 规则：`.pub` 存在 → 直接读；仅私钥存在 → russh 派生公钥（口令加密的私钥
/// 无法派生，跳过——其余已登记钥仍保障本机免密）；一个都没有 → 生成写入。
/// 必然返回至少 1 行。
pub fn prep_local_keys(ssh_dir: &std::path::Path) -> Result<Vec<String>, ServersError> {
    let mut lines = Vec::new();
    for name in LOCAL_KEY_NAMES {
        let pub_path = ssh_dir.join(format!("{name}.pub"));
        let priv_path = ssh_dir.join(name);
        if pub_path.is_file() {
            let raw = std::fs::read_to_string(&pub_path)
                .map_err(|e| ServersError::LocalKey(format!("读取 {pub_path:?} 失败: {e}")))?;
            if let Some(norm) = normalize_pub_line(&raw) {
                lines.push(norm);
            }
            continue;
        }
        if priv_path.is_file() {
            let pem = std::fs::read_to_string(&priv_path)
                .map_err(|e| ServersError::LocalKey(format!("读取 {priv_path:?} 失败: {e}")))?;
            if let Ok(key) = russh::keys::decode_secret_key(&pem, None) {
                if let Ok(line) = key.public_key().to_openssh() {
                    if let Some(norm) = normalize_pub_line(&line) {
                        lines.push(norm);
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push(generate_local_key(ssh_dir)?);
    }
    Ok(lines)
}

/// 生成 ed25519 密钥对写入 ssh_dir（无钥兜底），返回公钥行。
/// 注释 vpn-ctl-local（与 App 钥 vpn-ctl-admin 同风格，authorized_keys 可溯源）。
fn generate_local_key(ssh_dir: &std::path::Path) -> Result<String, ServersError> {
    let keypair = russh::keys::PrivateKey::random(
        &mut rand::rngs::OsRng,
        russh::keys::ssh_key::Algorithm::Ed25519,
    )
    .map_err(|e| ServersError::LocalKey(format!("生成密钥失败: {e}")))?;
    let mut pub_key = keypair.public_key().clone();
    pub_key.set_comment("vpn-ctl-local");
    let pub_line = pub_key
        .to_openssh()
        .map_err(|e| ServersError::LocalKey(format!("公钥序列化失败: {e}")))?;
    let pem = keypair
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map_err(|e| ServersError::LocalKey(format!("私钥序列化失败: {e}")))?
        .to_string();
    std::fs::create_dir_all(ssh_dir)
        .map_err(|e| ServersError::LocalKey(format!("创建 {ssh_dir:?} 失败: {e}")))?;
    let priv_path = ssh_dir.join("id_ed25519");
    std::fs::write(&priv_path, pem.as_bytes())
        .map_err(|e| ServersError::LocalKey(format!("写入 {priv_path:?} 失败: {e}")))?;
    std::fs::write(ssh_dir.join("id_ed25519.pub"), format!("{pub_line}\n"))
        .map_err(|e| ServersError::LocalKey(format!("写入 id_ed25519.pub 失败: {e}")))?;
    lock_key_permissions(ssh_dir, &priv_path)?;
    Ok(normalize_pub_line(&pub_line).unwrap_or(pub_line))
}

/// 权限对齐 ssh-keygen 行为（Win OpenSSH 拒绝「权限过宽」的私钥文件与目录）：
/// Windows — icacls 关继承，授予当前用户/SYSTEM/Administrators
/// （授权清单一次传入，/grant:r 的规范的 sid:perm[,sid:perm...] 语法）；
/// unix — chmod 600 私钥 / 700 .ssh（测试与本地交叉编译可移植）。
fn lock_key_permissions(
    ssh_dir: &std::path::Path,
    priv_path: &std::path::Path,
) -> Result<(), ServersError> {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME")
            .map_err(|_| ServersError::LocalKey("USERNAME 未设置".into()))?;
        for path in [priv_path, ssh_dir] {
            let status = std::process::Command::new("icacls")
                .arg(path)
                .args(["/inheritance:r", "/grant:r"])
                .arg(format!("{user}:F,SYSTEM:F,Administrators:F"))
                .status()
                .map_err(|e| ServersError::LocalKey(format!("icacls {path:?} 执行失败: {e}")))?;
            if !status.success() {
                return Err(ServersError::LocalKey(format!(
                    "icacls {path:?} 返回非零: {status}"
                )));
            }
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(priv_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ServersError::LocalKey(format!("chmod 600 {priv_path:?} 失败: {e}")))?;
        std::fs::set_permissions(ssh_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ServersError::LocalKey(format!("chmod 700 {ssh_dir:?} 失败: {e}")))?;
    }
    Ok(())
}
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cargo test prep_local_keys`
Expected: 3 项全部 PASS。若测试代码里有 `russh` 笔误，先改正为 `russh` 再跑。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/servers/mod.rs src-tauri/src/servers/tests.rs
git commit -m "feat(servers): prep_local_keys 本机密钥扫描/派生/空钥自动生成（icacls 对齐 ssh-keygen）"
```

---

### Task 2: `setup_passwordless` 接入本机免密 + 现有测试回归

**Files:**
- Modify: `src-tauri/src/servers/mod.rs:370-435`（`setup_passwordless`）
- Test: `src-tauri/src/servers/tests.rs:346`（现有 `setup_passwordless_switches_to_key_and_deletes_password`）

**Interfaces:**
- Consumes: `prep_local_keys(Path)`、`default_ssh_dir()`（Task 1，签名见上）、既有 `SshSession::run_ok`、`PROBE_TIMEOUT`。
- Produces: `setup_passwordless` 行为变化——免密后服务器 `authorized_keys` 额外包含本机公钥行；签名与错误类型不变。

- [ ] **Step 1: 写失败测试（先证明现有行为缺少本机行）**

在 `src-tauri/src/servers/tests.rs` 的 `setup_passwordless_switches_to_key_and_deletes_password` 之后追加：

```rust
#[tokio::test]
async fn setup_passwordless_also_registers_local_terminal_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir);
    let srv = FakeSshServer::start().await;
    srv.push_script("mkdir -p ~/.ssh", 0, "", "");
    srv.push_script("mkdir -p ~/.ssh", 0, "", "");
    srv.push_script("grep -qF", 0, "", "");
    srv.push_script("grep -qF", 0, "", "");
    let mut i = valid_input();
    i.host = "127.0.0.1".into();
    i.port = srv.port as i64;
    let s = create_server(&state, i).unwrap();
    setup_passwordless(&state, s.id).await.unwrap();
    // App 钥 + 本机钥都必须下发（App 钥带 vpn-ctl-admin；本机钥为编辑行，
    // 本机 ~/.ssh 无论有无密钥，prep_local_keys 都保证至少一行）
    let cmds = srv.recorded();
    let auth_cmds: Vec<&String> = cmds.iter().filter(|c| c.contains("authorized_keys")).collect();
    assert!(auth_cmds.len() >= 2, "应有 App 钥与本机钥两条下发: {cmds:?}");
    assert!(
        cmds.iter().any(|c| c.contains("vpn-ctl-admin")),
        "App 钥注释线不变: {cmds:?}"
    );
}
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test setup_passwordless_also_registers_local_terminal_key`
Expected: FAIL —— `assert!(auth_cmds.len() >= 2)` 处失败（当前仅 1 条 authorized_keys 下发，`setup_passwordless` 尚未接入本机钥）。注意 FakeSshServer：`push_script` 按包含匹配按序消费，未匹配命令默认退出码 0。

- [ ] **Step 3: 实现接入**

修改 `src-tauri/src/servers/mod.rs` 的 `setup_passwordless`：

1. 在“验证公钥登录可用后（`verify.run_ok("免密验证", "true", PROBE_TIMEOUT).await?;` 之后、`let (cred_id, b64) = seal_to_b64(...)` 之前）插入：

```rust
    // 双钥并行：App 钥验证通过后、凭据切换前，同步把本机终端钥匙也登记到服务器——
    // prep_local_keys 保证至少一行（已有私钥则扫描/派生，无钥则自动生成写盘）
    let local_lines = prep_local_keys(&default_ssh_dir()?)?;
    for line in &local_lines {
        sess.run_ok("免密-本机", &append_pub_line(line), PROBE_TIMEOUT)
            .await?;
    }
```

2. 把现有内联的追加命令（`mod.rs:403-406`）抽出为复用函数，并让 App 钥下发改用它（DRY，行为不变）：

```rust
/// 幂等追加单行公钥到 authorized_keys（grep -qF 命中则跳过），末行收紧权限。
fn append_pub_line(pub_line: &str) -> String {
    format!(
        "grep -qF '{}' ~/.ssh/authorized_keys 2>/dev/null || echo '{}' >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys",
        pub_line, pub_line
    )
}
```

App 钥段替换为：

```rust
    let append = append_pub_line(&pub_line);
```

（原代码 `let append = format!("grep -qF '{}' '...{}'...; chmod 600 ...");` 整体逻辑保持。）

- [ ] **Step 4: 运行测试验证通过**

Run: `cargo test setup_passwordless`
Expected: 新增测试与既有 `setup_passwordless_switches_to_key_and_deletes_password` 全部 PASS。
已知真机差异（不视为失败）：测试运行于本机时读到真实 `~/.ssh`（只读，不写）；CI runner 无密钥 → 走生成分支（写 runner 家目录 + icacls，windows-latest 可用）。两环境断言一致（`>= 2` 条下发）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/servers/mod.rs src-tauri/src/servers/tests.rs
git commit -m "feat(servers): 免密同步登记本机终端密钥（prep_local_keys 接入，失败即中止）"
```

---

### Task 3: README 更新 + 全量门禁 + 提交

**Files:**
- Modify: `README.md:70`（服务器管理能力描述行）、`README.md:82`（`setup_passwordless_cmd` 表格行）

**Interfaces:**
- 无（纯文档 + 全量验证）。

- [ ] **Step 1: 更新 README**

- `README.md:70` 的「一键免密（ed25519 自动下发）」改为「一键免密（ed25519 自动下发；本机 `~/.ssh` 密钥一并登记，无钥自动生成）」。
- `README.md:82` 的 `setup_passwordless_cmd` 描述追加同一说明：「生成 ed25519 密钥对并下发公钥（含本机 `~/.ssh` 登记或自动生成），切换凭据」。

- [ ] **Step 2: 全量门禁**

Run（在 `src-tauri/` 下）:
- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
Expected: 全绿。

- [ ] **Step 3: 提交**

```bash
git add README.md
git commit -m "docs(README): 一键免密说明——本机 ~/.ssh 同步登记，无钥自动生成"
```

- [ ] **Step 4: 真机验收（可选，需构建 GUI）**

退出运行中的 App 后 `npm run tauri build`（或 `npm run tauri dev`），在「服务器」页对一台服务器点「一键免密」应成功；随后本机终端验证：

```powershell
ssh -o BatchMode=yes root@<host> echo ok
```

期望：打印 `ok` 且无密码提示（BatchMode 禁止交互）。

---

## Self-Review 记录（写计划时完成）

- **Spec 覆盖**：范围 1 本机登记/生成 ✓（Task 1）、决策表双钥并行+无钥生成+失败中止 ✓（Task 2 接入 + Global Constraints）、幂等 ✓（append_pub_line + normalize）、测试计划 ✓（Task 1 三测 + Task 2 集成测）、README ✓（Task 3）。
- **占位符扫描**：无 TBD/TODO；每个代码步骤含完整代码。
- **类型一致性**：`prep_local_keys(ssh_dir: &Path) -> Result<Vec<String>, ServersError>` 在 Task 1 定义、Task 2 消费，签名一致；`append_pub_line(&str) -> String` 同为 Task 2 内定义并复用。
- **风险记录**：icacls 权限命令在 zh-CN Windows 上 `Administrators`/`SYSTEM`/`USERNAME` 均为本地可解析名；若解析失败 icacls 返回非零 → 免密中止且报错可见（设计内失败路径）。用户密钥注释含引号的情况被 normalize 剥注释规避。
