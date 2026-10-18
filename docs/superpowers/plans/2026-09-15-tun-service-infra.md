# TUN 提权服务基础设施 实现计划（P1）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让「开启网络」经由一个按需启动的提权 Windows 服务来 spawn/kill mihomo，为后续 TUN 全局模式（P2）备好提权通道；本阶段行为与现状等价（不开 TUN）。

**Architecture:** 服务宿主就**复用同一个 exe**（SCM 注册 `vpn-ctl.exe --service`），不新开 crate——服务因此能直接复用 `config` 等现有代码，且「调用方必须是同一个 exe 路径」这一身份校验天然成立。主程序与服务经命名管道 IPC。**只有 spawn/kill mihomo 交给服务**，其余（端口选择、配置生成、就绪探测、系统代理、local_proxy 登记）全部留在主程序——因为系统代理写的是 `HKEY_CURRENT_USER`（`net/sysproxy_set.rs:29`），SYSTEM 服务写的是它自己的 hive，不是用户的。服务未安装/未运行时主程序**回退到现行的直接 spawn**，故本阶段不改变任何既有失败模式。

**Tech Stack:** Rust 2021 / Tauri 2 / `windows-service`（SCM 宿主）/ `windows-sys`（命名管道，已在依赖树中）/ `serde_json` / `tokio`（服务侧已有）。

## Global Constraints

- 平台：仅 Windows。全部服务代码用 `#[cfg(windows)]` 隔离，非 Windows 保持可编译（照 `net/sysproxy_set.rs` 的既有写法）。
- 服务启动类型必须是 **OnDemand（手动）**，**绝不注册开机自启**（`ServiceStartType::OnDemand`）。
- 服务跟应用生命周期走：应用启动 `sc start`，应用退出 `sc stop`。
- **mihomo 路径绝不由客户端指定**：服务从自身 exe 所在目录解析。
- **配置内容由主程序生成**后经 IPC 传给服务；服务落盘前必须过一遍清洗（任务 2）。
- 系统代理（HKCU）、`local_proxy` 登记、端口选择、`wait_ready` 探测**一律留在主程序侧**，不下沉到服务。
- 提交信息用中文 conventional commits（`feat(service): …`），与仓库既有风格一致。
- 每个模块文件头部要有模块说明注释（输入输出、依赖），照 `net/runtime.rs`、`state.rs` 的既有风格。

---

### Task 1: IPC 协议类型与编解码

协议层是纯函数，没有任何 Windows 依赖——先做它，后面的服务端/客户端都建立在它之上。

**Files:**
- Create: `src-tauri/src/service/mod.rs`
- Create: `src-tauri/src/service/protocol.rs`
- Create: `src-tauri/src/service/protocol_tests.rs`
- Modify: `src-tauri/src/main.rs`（加 `mod service;`）

**Interfaces:**
- Consumes: 无（本任务是最底层）
- Produces:
  - `service::protocol::Request` — 枚举，变体 `StartNet { config_yaml: String, mixed_port: u16, ctrl_port: u16, token: String }`、`StopNet`、`Status`
  - `service::protocol::Response` — 枚举，变体 `Started`、`Stopped { mixed_port: Option<u16> }`、`Failed { message: String }`
  - `service::protocol::encode(&Request) -> String`
  - `service::protocol::decode(&str) -> Result<Request, String>`
  - `service::protocol::encode_response(&Response) -> String`
  - `service::protocol::decode_response(&str) -> Result<Response, String>`

- [ ] **Step 1: 写失败的测试**

Create `src-tauri/src/service/protocol_tests.rs`:

```rust
//! 协议层测试：编解码往返 + 坏输入必须报错而非静默降级。
//! 全部是纯函数，不碰管道/SCM，故可在任何平台跑。
use super::protocol::{decode, decode_response, encode, encode_response, Request, Response};

#[test]
fn start_net_roundtrip() {
    let req = Request::StartNet {
        config_yaml: "mixed-port: 7890\n".into(),
        mixed_port: 7890,
        ctrl_port: 9090,
        token: "t0ken".into(),
    };
    assert_eq!(decode(&encode(&req)).expect("往返"), req);
}

#[test]
fn stop_net_and_status_roundtrip() {
    assert_eq!(decode(&encode(&Request::StopNet)).unwrap(), Request::StopNet);
    assert_eq!(decode(&encode(&Request::Status)).unwrap(), Request::Status);
}

#[test]
fn yaml_survives_roundtrip_byte_for_byte() {
    // 配置是主程序生成的原文，服务只清洗不重排；往返必须逐字节相同，
    // 否则「空选集产物与缓存逐字节相同」那类断言会在下游失效
    let yaml = "proxies:\n  - name: 香港A\n    type: ss\nrules:\n  - MATCH,DIRECT\n";
    let req = Request::StartNet {
        config_yaml: yaml.into(),
        mixed_port: 7890,
        ctrl_port: 9090,
        token: "t".into(),
    };
    match decode(&encode(&req)).unwrap() {
        Request::StartNet { config_yaml, .. } => assert_eq!(config_yaml, yaml),
        other => panic!("变体错了: {other:?}"),
    }
}

#[test]
fn response_roundtrip() {
    for r in [
        Response::Started,
        Response::Stopped { mixed_port: Some(7890) },
        Response::Stopped { mixed_port: None },
        Response::Failed { message: "端口被占".into() },
    ] {
        assert_eq!(decode_response(&encode_response(&r)).unwrap(), r);
    }
}

#[test]
fn unknown_command_is_rejected_not_defaulted() {
    // 未知命令必须报错。静默当成默认值是提权服务的典型漏洞形态：
    // 攻击者只要构造一个「解析成 StopNet」的畸形载荷就能停掉用户的代理
    let err = decode(r#"{"cmd":"rm_rf","path":"/"}"#).expect_err("未知命令必须拒绝");
    assert!(err.contains("未知命令"), "{err}");
}

#[test]
fn malformed_json_is_rejected() {
    assert!(decode("not json at all").is_err());
    assert!(decode("").is_err());
    assert!(decode_response("{").is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test service::protocol_tests`
Expected: 编译失败 —— `unresolved import super::protocol`（模块还不存在）

- [ ] **Step 3: 写最小实现**

Create `src-tauri/src/service/mod.rs`:

```rust
//! 提权服务：以 SYSTEM 身份 spawn/kill mihomo，供 TUN 全局模式使用。
//!
//! 只有 mihomo 的进程生命周期在服务侧；端口选择、配置生成、就绪探测、系统代理
//! （HKCU，见 net/sysproxy_set.rs）与 local_proxy 登记全部留在主程序——SYSTEM
//! 服务写的是它自己的用户 hive，写不到用户的 WinINET 设置。
pub mod protocol;

#[cfg(test)]
mod protocol_tests;
```

Create `src-tauri/src/service/protocol.rs`:

```rust
//! 主程序 ↔ 服务 的 IPC 载荷。
//!
//! 线格式是 JSON，用 `tag` 做内嵌判别式（`{"cmd":"start_net",…}` /
//! `{"kind":"started"}`），便于人肉排查管道流量。
//!
//! 输入输出：本模块全是纯函数，不碰管道、不碰 SCM，故可跨平台测试。
//! 依赖：serde / serde_json。

use serde::{Deserialize, Serialize};

/// 服务认得的命令。**只有这三个**——白名单越短越好。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// 起 mihomo：配置原文 + 端口 + 控制器 token（服务只清洗与落盘，不改语义）
    StartNet {
        config_yaml: String,
        mixed_port: u16,
        ctrl_port: u16,
        token: String,
    },
    /// 杀 mihomo（幂等：未运行也算成功）
    StopNet,
    /// 查询服务与 mihomo 的存活状态
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Started,
    /// mixed_port 回显服务实际使用的端口，供主程序核对
    Stopped { mixed_port: Option<u16> },
    Failed { message: String },
}

pub fn encode(req: &Request) -> String {
    serde_json::to_string(req).expect("Request 序列化不会失败")
}

pub fn decode(raw: &str) -> Result<Request, String> {
    // serde 的错误对未知 tag 报的是「unknown variant `rm_rf`」，直接透出即可读；
    // 但补一句中文前缀，免得日志里突然冒英文
    serde_json::from_str(raw).map_err(|e| format!("未知命令或载荷格式错误: {e}"))
}

pub fn encode_response(resp: &Response) -> String {
    serde_json::to_string(resp).expect("Response 序列化不会失败")
}

pub fn decode_response(raw: &str) -> Result<Response, String> {
    serde_json::from_str(raw).map_err(|e| format!("响应格式错误: {e}"))
}
```

Modify `src-tauri/src/main.rs` —— 在 `mod serve;` 之后按字母序插入：

```rust
mod service;
```

同时更新 `main.rs` 顶部模块说明注释，把 `service` 加进模块清单，并说明它是提权服务宿主。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test service::protocol_tests`
Expected: 6 passed

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/service/ src-tauri/src/main.rs
git commit -m "feat(service): IPC 协议类型与编解码（纯函数，白名单三命令）"
```

---

### Task 2: 配置危险字段清洗

服务会以 SYSTEM 身份让 mihomo 读一份**由非提权主程序提供**的配置。mihomo 的 `log-file` 能把日志写到任意路径——那就是最直接的「SYSTEM 任意文件写」，必须先堵死。

**Files:**
- Create: `src-tauri/src/service/config_guard.rs`
- Create: `src-tauri/src/service/config_guard_tests.rs`
- Modify: `src-tauri/src/service/mod.rs`（加 `pub mod config_guard;` 与 `mod config_guard_tests;`）

**Interfaces:**
- Consumes: 无
- Produces: `service::config_guard::sanitize(config_yaml: &str, allowed_geo_root: &std::path::Path) -> Result<String, String>`

- [ ] **Step 1: 写失败的测试**

Create `src-tauri/src/service/config_guard_tests.rs`:

```rust
//! 配置清洗测试。核心断言：能写文件的字段必须消失，越界的 geodata-dir 必须
//! 报错而不是被静默改写（静默改写会让「配置和用户看到的不一样」无法排查）。
use super::config_guard::sanitize;
use std::path::Path;

const ROOT: &str = r"C:\ProgramData\yunyi";

#[test]
fn log_file_is_stripped() {
    let yaml = "mixed-port: 7890\nlog-file: C:\\Windows\\System32\\evil.dll\n";
    let out = sanitize(yaml, Path::new(ROOT)).expect("清洗");
    assert!(!out.contains("log-file"), "log-file 必须被删除: {out}");
    assert!(out.contains("7890"), "其余字段不得丢失: {out}");
}

#[test]
fn external_ui_is_stripped() {
    let yaml = "mixed-port: 7890\nexternal-ui: /some/dir\n";
    let out = sanitize(yaml, Path::new(ROOT)).expect("清洗");
    assert!(!out.contains("external-ui"), "external-ui 必须被删除: {out}");
}

#[test]
fn geodata_dir_inside_root_is_kept() {
    let yaml = format!("mixed-port: 7890\ngeodata-dir: '{ROOT}\\net'\n");
    let out = sanitize(&yaml, Path::new(ROOT)).expect("清洗");
    assert!(out.contains("geodata-dir"), "根内的 geodata-dir 应保留: {out}");
}

#[test]
fn geodata_dir_outside_root_is_rejected() {
    // 拒绝而不是改写：静默改写会让 mihomo 的分流行为和用户配置不一致
    let yaml = "mixed-port: 7890\ngeodata-dir: 'C:\\Users\\public'\n";
    let err = sanitize(yaml, Path::new(ROOT)).expect_err("根外必须拒绝");
    assert!(err.contains("geodata-dir"), "{err}");
}

#[test]
fn normal_fields_survive_intact() {
    let yaml = "mixed-port: 7890\nmode: rule\nproxies:\n  - name: 香港A\n    type: ss\nrules:\n  - MATCH,DIRECT\n";
    let out = sanitize(yaml, Path::new(ROOT)).expect("清洗");
    let v: serde_yaml::Value = serde_yaml::from_str(&out).expect("产物仍是合法 YAML");
    assert_eq!(v["mode"], serde_yaml::Value::from("rule"));
    assert_eq!(v["proxies"][0]["name"], serde_yaml::Value::from("香港A"));
    assert_eq!(v["rules"][0], serde_yaml::Value::from("MATCH,DIRECT"));
}

#[test]
fn non_mapping_document_is_rejected() {
    assert!(sanitize("- just\n- a list\n", Path::new(ROOT)).is_err());
    assert!(sanitize("not: [valid: yaml", Path::new(ROOT)).is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test service::config_guard_tests`
Expected: 编译失败 —— `unresolved import super::config_guard`

- [ ] **Step 3: 写最小实现**

Create `src-tauri/src/service/config_guard.rs`:

```rust
//! 落盘前的配置清洗。
//!
//! 威胁模型：配置内容由**非提权**的主程序提供，而 mihomo 以 SYSTEM 运行。
//! mihomo 配置里能写文件、能挂外部资源的字段就是提权路径。本模块只做减法，
//! 不改写语义——越界一律报错，不静默纠正。
//!
//! 已知局限（照实写在这，别当成已解决）：这是**黑名单**，对 mihomo 未来新增的
//! 「能写文件」字段天然失效。要根治得让服务自己生成配置，而节点数据来自订阅
//! （且手工节点可带 clash_raw 片段直插配置，见 config/mod.rs 的 node_mapping），
//! 源头同样不可信，故 Blacklist 是当前取舍下的最优解。
//!
//! 输入输出：YAML 文本 → 清洗后的 YAML 文本；纯函数，可跨平台测试。
//! 依赖：serde_yaml。

use std::path::Path;

/// 必须删除的顶层字段：它们能让 SYSTEM 身份的 mihomo 写任意路径。
const STRIPPED_KEYS: [&str; 2] = ["log-file", "external-ui"];

/// 清洗配置：删危险字段 + 校验 geodata-dir 落在 `allowed_geo_root` 之内。
///
/// 越界的 geodata-dir **报错而非改写**——静默改写会让 mihomo 的实际行为与
/// 用户看到的配置不一致，事后无法排查。
pub fn sanitize(config_yaml: &str, allowed_geo_root: &Path) -> Result<String, String> {
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(config_yaml).map_err(|e| format!("配置不是合法 YAML: {e}"))?;
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| "配置顶层必须是映射（key: value），实际不是".to_string())?;

    for key in STRIPPED_KEYS {
        map.remove(serde_yaml::Value::from(key));
    }

    if let Some(geo) = map.get(serde_yaml::Value::from("geodata-dir")) {
        let dir = geo
            .as_str()
            .ok_or_else(|| "geodata-dir 必须是字符串".to_string())?;
        if !Path::new(dir).starts_with(allowed_geo_root) {
            return Err(format!(
                "geodata-dir 越界：{dir} 不在允许的根 {} 之内",
                allowed_geo_root.display()
            ));
        }
    }

    serde_yaml::to_string(&root).map_err(|e| format!("清洗后序列化失败: {e}"))
}
```

Modify `src-tauri/src/service/mod.rs`:

```rust
pub mod config_guard;
pub mod protocol;

#[cfg(test)]
mod config_guard_tests;
#[cfg(test)]
mod protocol_tests;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test service::config_guard_tests`
Expected: 6 passed

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/service/
git commit -m "feat(service): 配置清洗——删除可写任意路径的字段，越界 geodata-dir 报错"
```

---

### Task 3: 服务固定路径与 `--service` 入口分流

服务需要一组**固定的、与用户数据目录无关**的路径（它跑在 SYSTEM 下，用户目录可能不可访问），以及一个入口分流：带 `--service` 的进程走 SCM 宿主，不带的老老实实起 Tauri。

**Files:**
- Create: `src-tauri/src/service/paths.rs`
- Create: `src-tauri/src/service/paths_tests.rs`
- Modify: `src-tauri/src/service/mod.rs`
- Modify: `src-tauri/src/main.rs`（`fn main` 开头加分流）

**Interfaces:**
- Consumes: 无
- Produces:
  - `service::paths::root() -> std::path::PathBuf` —— `%ProgramData%\yunyi`
  - `service::paths::net_dir() -> std::path::PathBuf` —— `<root>\net`
  - `service::paths::config_path() -> std::path::PathBuf` —— `<root>\net\config.yaml`
  - `service::paths::SERVICE_NAME: &str` = `"yunyi-service"`
  - `service::paths::PIPE_NAME: &str` = `r"\\.\pipe\yunyi-service"`
  - `service::paths::is_service_invocation(args: &[String]) -> bool`

- [ ] **Step 1: 写失败的测试**

Create `src-tauri/src/service/paths_tests.rs`:

```rust
//! 路径与入口分流测试。路径本身依赖环境变量，故只断言「拼在 root 之下」
//! 这种不依赖具体机器的性质；`is_service_invocation` 是纯函数，全量覆盖。
use super::paths::{config_path, is_service_invocation, net_dir, root};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn service_flag_detected_anywhere_in_argv() {
    assert!(is_service_invocation(&s(&["vpn-ctl.exe", "--service"])));
    // SCM 会带自己的参数过来，标志不在最后也是合法的
    assert!(is_service_invocation(&s(&["vpn-ctl.exe", "--service", "run"])));
    assert!(!is_service_invocation(&s(&["vpn-ctl.exe"])));
    // 前缀相似的不得误命中：--service-name 不是服务模式
    assert!(!is_service_invocation(&s(&["vpn-ctl.exe", "--service-name"])));
    assert!(!is_service_invocation(&[]));
}

#[test]
fn paths_are_all_under_root() {
    // 三个路径必须同根：服务以 SYSTEM 跑，散落在别处会带来额外的 ACL 面
    assert!(net_dir().starts_with(root()));
    assert!(config_path().starts_with(root()));
}

#[test]
fn config_lives_in_net_dir() {
    assert_eq!(config_path().parent().unwrap(), net_dir());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test service::paths_tests`
Expected: 编译失败 —— `unresolved import super::paths`

- [ ] **Step 3: 写最小实现**

Create `src-tauri/src/service/paths.rs`:

```rust
//! 服务的固定路径与入口标志。
//!
//! 服务以 SYSTEM 运行，用户数据目录未必可达，故配置与网卡数据一律放在
//! `%ProgramData%\yunyi`（该目录默认只允许管理员写，正好与「配置由服务落盘」
//! 的设计吻合）。
//!
//! 输入输出：纯函数/常量，无 IO。
//! 依赖：std::env。

use std::path::PathBuf;

/// SCM 里的服务名（`sc query yunyi-service` 用的就是它）。
pub const SERVICE_NAME: &str = "yunyi-service";

/// 命名管道全名。
pub const PIPE_NAME: &str = r"\\.\pipe\yunyi-service";

/// 命令行标志：带它 = 本次进程是被 SCM 拉起来的服务宿主，不是 GUI。
pub const SERVICE_FLAG: &str = "--service";

/// 服务固定根：`%ProgramData%\yunyi`。
pub fn root() -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
    PathBuf::from(base).join("yunyi")
}

/// mihomo 的工作目录（配置、geo 数据都在这）。
pub fn net_dir() -> PathBuf {
    root().join("net")
}

/// 服务落盘的 mihomo 配置路径。
pub fn config_path() -> PathBuf {
    net_dir().join("config.yaml")
}

/// 本次进程是否以服务模式启动。
///
/// 用**精确相等**而非 `contains`/`starts_with`——后者会让 `--service-name` 这类
/// 前缀相似的参数误命中，把一次普通启动变成服务宿主，GUI 直接不出来。
pub fn is_service_invocation(args: &[String]) -> bool {
    args.iter().any(|a| a == SERVICE_FLAG)
}
```

Modify `src-tauri/src/service/mod.rs`:

```rust
pub mod config_guard;
pub mod paths;
pub mod protocol;

#[cfg(test)]
mod config_guard_tests;
#[cfg(test)]
mod paths_tests;
#[cfg(test)]
mod protocol_tests;
```

Modify `src-tauri/src/main.rs` 的 `fn main()` —— 在 `tauri::Builder` 之前插入分流：

```rust
fn main() {
    // 服务宿主分流必须在 Tauri 之前：SCM 拉起进程时期望它立刻进入
    // StartServiceCtrlDispatcher，晚一秒就会报「服务未及时响应启动请求」。
    if service::paths::is_service_invocation(&std::env::args().collect::<Vec<_>>()) {
        service::host::run();
        return;
    }

    tauri::Builder::default()
        // …以下原样不动…
```

**注意**：本步引用的 `service::host::run()` 在任务 4 才创建。为让本任务自成可编译单元，先在 `service/mod.rs` 里放一个占位：

```rust
#[cfg(windows)]
pub mod host;
```

并在 `src-tauri/src/service/host.rs` 先写最小可编译版本：

```rust
//! 服务宿主（占位）：任务 4 填充 SCM 派发与管道服务端。
pub fn run() {
    eprintln!("[yunyi-service] 宿主尚未实现");
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test service::paths_tests`
Expected: 3 passed

- [ ] **Step 5: 手工确认分流生效**

Run: `cd src-tauri && cargo run -- --service`
Expected: 打印 `[yunyi-service] 宿主尚未实现` 后退出，**不弹 GUI 窗口**

Run: `cd src-tauri && cargo run`
Expected: 正常启动 GUI

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/service/ src-tauri/src/main.rs
git commit -m "feat(service): 固定路径与 --service 入口分流"
```

---

### Task 4: SCM 服务宿主

把 `service::host::run()` 从占位换成真正的服务：向 SCM 注册派发器、报告状态、在后台线程跑命令循环。

**Files:**
- Modify: `src-tauri/Cargo.toml`（加 `windows-service`）
- Modify: `src-tauri/src/service/host.rs`

**Interfaces:**
- Consumes: `service::paths::{SERVICE_NAME, is_service_invocation}`
- Produces: `service::host::run()` —— 阻塞直到服务收到停止请求

- [ ] **Step 1: 加依赖**

```bash
cd src-tauri && cargo add windows-service --target 'cfg(windows)'
```

在 `Cargo.toml` 该行上方补注释，说明用途（照 `winreg`、`windows-sys` 的既有注释风格）：

```toml
# windows-service：提权服务的 SCM 宿主（TUN 全局模式需要装 wintun 内核驱动，
# 必须管理员令牌；服务方案换到的是「装的时候一次 UAC，之后开关 TUN 全程不弹」）
windows-service = "0.7"
```

- [ ] **Step 2: 写实现**

Replace `src-tauri/src/service/host.rs`:

```rust
//! 提权服务宿主：向 SCM 注册派发器，在后台线程跑命令循环。
//!
//! 为什么服务与应用是**同一个 exe**：见 service/mod.rs 的说明。SCM 注册的
//! 命令行是 `<exe> --service`，由 main.rs 的入口分流接住。
//!
//! 输入输出：`run()` 无参、无返回，阻塞到服务停止。
//! 依赖：windows-service（SCM）、本模块的 pipe 服务端（任务 5）。

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::paths::SERVICE_NAME;

define_windows_service!(ffi_service_main, service_main);

/// 服务入口（被 SCM 在后台线程调用）。
fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        eprintln!("[yunyi-service] 运行失败: {e}");
    }
}

fn run_service() -> windows_service::Result<()> {
    // 停止信号经 channel 传给命令循环；SCM 的 Stop 与内部错误都走这条
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // 不支持的控件必须报 NotSupported，否则 SCM 会认为服务接受了它
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // 命令循环：任务 5 在此接受命名管道连接
    super::pipe::serve_until(shutdown_rx);

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

/// 进程以服务模式启动时的入口（main.rs 分流到这里）。
pub fn run() {
    // 第三参数是 SCM 传进来的参数数，0 = 不取
    if let Err(e) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        eprintln!("[yunyi-service] 无法启动服务派发器: {e}");
        eprintln!("[提示] 本进程必须由 SCM 拉起（sc start {SERVICE_NAME}），不能手工运行");
    }
}
```

**注意**：`super::pipe::serve_until` 在任务 5 才存在。为让本任务可编译，先在 `service/pipe.rs` 放最小版本：

```rust
//! 命名管道服务端（占位）：任务 5 填充调用方校验与请求分发。
use std::sync::mpsc::Receiver;

pub fn serve_until(shutdown: Receiver<()>) {
    let _ = shutdown.recv();
}
```

并在 `service/mod.rs` 加 `#[cfg(windows)] pub mod pipe;`

- [ ] **Step 3: 编译确认**

Run: `cd src-tauri && cargo build`
Expected: 编译通过，无 warning

- [ ] **Step 4: 手工确认拒绝非 SCM 启动**

Run: `cd src-tauri && cargo run -- --service`
Expected: 打印「无法启动服务派发器: …」与提示行后退出（错误码非 0）。这是**正确行为**——服务必须由 SCM 拉起。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/service/
git commit -m "feat(service): SCM 宿主（windows-service 派发器 + 停止信号）"
```

---

### Task 5: 命名管道服务端与调用方校验

管道的威胁模型：**任何本地进程都能连**。服务以 SYSTEM 跑，所以必须校验调用方是我们自己的 exe，否则任意本地进程都能指挥 SYSTEM 起进程。

**Files:**
- Modify: `src-tauri/src/service/pipe.rs`
- Create: `src-tauri/src/service/pipe_tests.rs`（只测可纯函数化的部分）
- Modify: `src-tauri/src/service/mod.rs`

**Interfaces:**
- Consumes: `service::protocol::{decode, encode_response, Request, Response}`、`service::paths::PIPE_NAME`
- Produces:
  - `service::pipe::serve_until(shutdown: std::sync::mpsc::Receiver<()>)`
  - `service::pipe::same_executable(client_pid: u32) -> Result<bool, String>` —— 调用方 exe 路径是否与服务自身相同

- [ ] **Step 1: 写失败的测试**

Create `src-tauri/src/service/pipe_tests.rs`:

```rust
//! 管道测试：只覆盖「当前进程的 exe 路径」这一条可自测的逻辑，
//! 以及请求分发的纯逻辑。跨进程的 ACL/句柄行为在任务 9 手工验证。
use super::pipe::current_exe_matches;

#[test]
fn own_pid_matches_self() {
    // 拿自己的 PID 去问，必然匹配——若这条失败，说明路径归一化（大小写、
    // 短路径名、符号链接）写错了，真实校验会一律拒绝，服务直接不可用
    let pid = std::process::id();
    assert!(current_exe_matches(pid).expect("查询自身 exe"), "自己的 PID 必须匹配");
}

#[test]
fn bogus_pid_errors_not_silently_false() {
    // 查不到的 PID 必须报错：静默返回 false 会把「查询失败」伪装成
    // 「身份不符」，排查时分不清是权限问题还是真被拒绝
    assert!(current_exe_matches(u32::MAX - 1).is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test service::pipe_tests`
Expected: 编译失败 —— `cannot find function current_exe_matches`

- [ ] **Step 3: 给 windows-sys 补 features**

`Cargo.toml` 的 `windows-sys` features 数组里追加（照既有注释风格写清用途）：

```toml
    # 命名管道 IPC（提权服务）：CreateNamedPipeW/ConnectNamedPipe/GetNamedPipeClientProcessId
    "Win32_Storage_FileSystem",
    "Win32_System_Pipes",
    # QueryFullProcessImageNameW：取调用方 exe 路径做身份校验
    "Win32_System_ProcessStatus",
    "Win32_Security",
```

- [ ] **Step 4: 写实现**

Replace `src-tauri/src/service/pipe.rs`:

```rust
//! 命名管道服务端。
//!
//! 威胁模型：命名管道**任何本地进程都能连**，而本服务以 SYSTEM 运行。若不校验
//! 调用方，任意本地进程都能指挥 SYSTEM 起进程——这是教科书级的本地提权。
//! 故每次连接先取客户端 PID（GetNamedPipeClientProcessId），比对其 exe 路径
//! 是否与服务自身相同。
//!
//! 为什么「同一个 exe」是足够的判据：服务与应用是同一个二进制（见 service/mod.rs），
//! 攻击者要伪造得像，就得在本机放一份路径一致的副本——那需要管理员权限才能写到
//! 安装目录，而有了管理员权限本来就不需要提权。
//!
//! 输入输出：`serve_until` 阻塞接受连接直到收到 shutdown。
//! 依赖：windows-sys（管道 + 进程镜像名）、本模块 protocol。

use std::sync::mpsc::Receiver;

use super::protocol::{decode, encode_response, Request, Response};

/// 单条请求的最大字节数。配置 YAML 可能上百 KB，留足余量但必须有上限，
/// 否则一个不出数据的客户端能把服务内存吃光。
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// 连接轮询间隔：`ConnectNamedPipe` 是阻塞的，靠每次连接前检查一次
/// shutdown 信号来响应停止请求。200ms 是「停止要跟手」与「别空转烧 CPU」的折中。
const POLL_MS: u32 = 200;

#[cfg(windows)]
pub use imp::{current_exe_matches, serve_until};

#[cfg(not(windows))]
pub fn serve_until(shutdown: Receiver<()>) {
    let _ = shutdown.recv();
}

#[cfg(not(windows))]
pub fn current_exe_matches(_pid: u32) -> Result<bool, String> {
    Err("命名管道仅 Windows 可用".to_string())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// `ERROR_PIPE_CONNECTED`：客户端在 `ConnectNamedPipe` 之前就连上了。
    /// 这是**成功**而非失败——当成失败会让服务随机漏掉连接。
    const ERROR_PIPE_CONNECTED: u32 = 535;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// 管道句柄的 RAII 包装：任何提前 return 都要断开并关闭，否则实例泄漏，
    /// 泄漏够多之后 `CreateNamedPipeW` 会直接失败。
    struct PipeHandle(HANDLE);

    impl Drop for PipeHandle {
        fn drop(&mut self) {
            unsafe {
                DisconnectNamedPipe(self.0);
                CloseHandle(self.0);
            }
        }
    }

    /// 取 pid 对应进程的 exe 全路径（小写归一，Windows 路径大小写不敏感）。
    fn exe_path_of(pid: u32) -> Result<String, String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return Err(format!("打不开进程 {pid}: 错误码 {}", GetLastError()));
            }
            let mut buf = vec![0u16; 32768];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(handle);
            if ok == 0 {
                return Err(format!("取进程 {pid} 镜像名失败: 错误码 {}", GetLastError()));
            }
            let s = OsString::from_wide(&buf[..len as usize]);
            Ok(s.to_string_lossy().to_lowercase())
        }
    }

    /// 调用方 exe 路径是否与服务自身一致。
    ///
    /// 查询失败**返回 Err 而不是 false**：静默的 false 会把「查不到」伪装成
    /// 「身份不符」，排查时分不清是权限问题还是真被拒绝。
    pub fn current_exe_matches(client_pid: u32) -> Result<bool, String> {
        let mine = std::env::current_exe()
            .map_err(|e| format!("取服务自身路径失败: {e}"))?
            .to_string_lossy()
            .to_lowercase();
        Ok(exe_path_of(client_pid)? == mine)
    }

    fn connect(handle: HANDLE) -> bool {
        unsafe {
            if ConnectNamedPipe(handle, std::ptr::null_mut()) != 0 {
                return true;
            }
            GetLastError() == ERROR_PIPE_CONNECTED
        }
    }

    fn read_request(handle: HANDLE) -> Result<String, String> {
        let mut buf = vec![0u8; MAX_REQUEST_BYTES];
        let mut read = 0u32;
        unsafe {
            let ok = ReadFile(
                handle,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!("读管道失败: 错误码 {}", GetLastError()));
            }
        }
        String::from_utf8(buf[..read as usize].to_vec()).map_err(|e| format!("请求不是 UTF-8: {e}"))
    }

    fn write_response(handle: HANDLE, body: &str) -> Result<(), String> {
        let mut written = 0u32;
        unsafe {
            let ok = WriteFile(
                handle,
                body.as_ptr() as *const _,
                body.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!("写管道失败: 错误码 {}", GetLastError()));
            }
        }
        Ok(())
    }

    pub fn serve_until(shutdown: Receiver<()>) {
        loop {
            // 停止信号只在两次连接之间检查：ConnectNamedPipe 一旦阻塞就只能在
            // 客户端连上或进程被杀时返回
            if shutdown.try_recv().is_ok() {
                return;
            }
            unsafe {
                // 消息模式（PIPE_TYPE_MESSAGE + PIPE_READMODE_MESSAGE）：每次
                // ReadFile 正好返回一条消息，省掉自己做长度前缀分帧；字节模式下
                // 必须自己分帧，否则上百 KB 的配置会被截断
                let handle = CreateNamedPipeW(
                    wide(super::super::paths::PIPE_NAME).as_ptr(),
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    MAX_REQUEST_BYTES as u32,
                    MAX_REQUEST_BYTES as u32,
                    POLL_MS,
                    std::ptr::null(),
                );
                if handle == INVALID_HANDLE_VALUE {
                    eprintln!("[yunyi-service] 创建管道失败: {}", GetLastError());
                    return;
                }
                let handle = PipeHandle(handle);

                if !connect(handle.0) {
                    continue; // 没人连上（超时/被打断），下一轮
                }

                // 先校验身份、再读载荷：反过来的话攻击者只要不发货就能占住线程
                let mut pid = 0u32;
                if GetNamedPipeClientProcessId(handle.0, &mut pid) == 0 {
                    continue;
                }
                match current_exe_matches(pid) {
                    Ok(true) => {}
                    // 身份不符：直接断开、**不回任何响应**，不给探测者反馈
                    Ok(false) => continue,
                    Err(e) => {
                        eprintln!("[yunyi-service] 身份校验失败(pid={pid}): {e}");
                        continue;
                    }
                }

                let raw = match read_request(handle.0) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[yunyi-service] {e}");
                        continue;
                    }
                };
                let resp = match decode(&raw) {
                    Ok(req) => super::super::mihomo::handle(&req),
                    Err(e) => Response::Failed { message: e },
                };
                let _ = write_response(handle.0, &encode_response(&resp));
                // PipeHandle 的 Drop 负责 Disconnect + Close
            }
        }
    }
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test service::pipe_tests`
Expected: 2 passed

- [ ] **Step 6: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/src/service/
git commit -m "feat(service): 命名管道服务端 + 调用方 exe 身份校验"
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test service::pipe_tests`
Expected: 2 passed

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/service/
git commit -m "feat(service): 命名管道服务端 + 调用方 exe 身份校验"
```

---

### Task 6: 命名管道客户端

主程序侧连管道、发命令、读响应。

**Files:**
- Create: `src-tauri/src/service/client.rs`
- Modify: `src-tauri/src/service/mod.rs`

**Interfaces:**
- Consumes: `service::protocol::{decode, decode_response, encode, Request, Response}`、`service::paths::PIPE_NAME`
- Produces: `service::client::call(req: &Request) -> Result<Response, String>`

- [ ] **Step 1: 写失败的测试**

Create `src-tauri/src/service/client_tests.rs`:

```rust
//! 客户端测试。管道不可用时必须是**可识别的**降级信号，不能是普通错误——
//! net::runtime 靠它决定要不要回退直接 spawn，分错了会让「开启网络」在服务
//! 缺失时直接失败而不是降级。
use super::client::{call_named, is_unavailable, ERR_SERVICE_UNAVAILABLE};
use super::protocol::Request;

#[test]
fn unavailable_error_is_recognizable() {
    assert!(is_unavailable(&format!("{ERR_SERVICE_UNAVAILABLE}: 管道不存在")));
    assert!(!is_unavailable("服务启动 mihomo 失败: 端口被占"));
}

#[test]
fn calling_without_service_reports_unavailable_not_panic() {
    // 管道名必须可注入：若直接打真实管道名，这条测试就会依赖「本机此刻没装
    // 服务」——任务 8 装完服务后它必挂。用一个不可能存在的名字才是确定性的。
    let err = call_named(r"\\.\pipe\yunyi-service-does-not-exist", &Request::Status)
        .expect_err("管道不存在时应报错");
    assert!(is_unavailable(&err), "应可识别为服务不可用: {err}");
}

#[test]
fn decode_error_is_not_treated_as_unavailable() {
    // 「服务活着但响应看不懂」是错误，不是降级信号——静默降级会掩盖服务端 bug
    let err = decode_failure_message_for_test();
    assert!(!is_unavailable(&err), "{err}");
}

/// 构造一条「服务回了个坏响应」的错误文案，复用 client 的格式化路径
fn decode_failure_message_for_test() -> String {
    super::protocol::decode_response("{ 坏响应")
        .expect_err("坏响应必须报错")
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test service::client_tests`
Expected: 编译失败 —— `unresolved import super::client`

- [ ] **Step 3: 写实现**

Create `src-tauri/src/service/client.rs`:

```rust
//! 命名管道客户端（主程序侧）。
//!
//! 每次调用新建一条连接、发一条请求、读一条响应、关闭——服务是无状态的命令
//! 循环，长连接只会多出一份闲置句柄要维护。
//!
//! **降级信号与错误的区分是本模块的核心契约**：服务未装/未启动（管道不存在）
//! 返回带 [`ERR_SERVICE_UNAVAILABLE`] 前缀的错误，调用方据此回退直接 spawn；
//! 其余错误（服务活着但启动失败、响应看不懂）一律透出，不得降级——否则会把
//! 「服务有 bug」伪装成「没装服务」，永远查不出来。
//!
//! 输入输出：Request → Response；无状态。
//! 依赖：windows-sys、本模块 protocol。

use super::protocol::{decode_response, encode, Request, Response};

/// 连不上管道（服务未装/未启动）——调用方据此走降级路径。
pub const ERR_SERVICE_UNAVAILABLE: &str = "服务未运行";

/// 该错误是否代表「服务不可用、可降级」。
pub fn is_unavailable(msg: &str) -> bool {
    msg.starts_with(ERR_SERVICE_UNAVAILABLE)
}

pub fn call(req: &Request) -> Result<Response, String> {
    call_named(super::paths::PIPE_NAME, req)
}

/// 指定管道名的调用入口。存在的唯一理由是**可测**：测试要打一个不可能存在的
/// 管道名来验证降级路径，而直接打真实管道名会让测试依赖机器状态（装没装服务）。
pub fn call_named(pipe: &str, req: &Request) -> Result<Response, String> {
    #[cfg(windows)]
    {
        imp::call(pipe, req)
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe, req);
        Err(format!("{ERR_SERVICE_UNAVAILABLE}: 仅 Windows 支持"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, GENERIC_READ, GENERIC_WRITE, OPEN_EXISTING,
    };

    /// 服务未启动时管道不存在；服务正忙时管道全被占用。两者都是**可降级**的。
    /// 用字面量而非 windows-sys 常量，免得为一个数字再开一个 feature。
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_PIPE_BUSY: u32 = 231;

    /// 响应上限：状态类响应只有几百字节，1 MB 是极大的余量。
    const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

    /// 撞上没有空闲实例时重试几次再放弃——服务是单线程循环，并发调用会撞上。
    const BUSY_RETRIES: u32 = 3;
    const BUSY_RETRY_MS: u64 = 100;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn open(pipe: &str) -> Result<Handle, String> {
        let name = wide(pipe);
        let mut last = 0u32;
        for _ in 0..BUSY_RETRIES {
            unsafe {
                let h = CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if h != INVALID_HANDLE_VALUE {
                    return Ok(Handle(h));
                }
                last = GetLastError();
            }
            if last != ERROR_PIPE_BUSY {
                break; // 只有「忙」值得重试，其余错误重试没用
            }
            std::thread::sleep(std::time::Duration::from_millis(BUSY_RETRY_MS));
        }
        if last == ERROR_FILE_NOT_FOUND || last == ERROR_PIPE_BUSY {
            Err(format!("{ERR_SERVICE_UNAVAILABLE}: 管道不可达（错误码 {last}）"))
        } else {
            Err(format!("连接服务管道失败: 错误码 {last}"))
        }
    }

    pub fn call(pipe: &str, req: &Request) -> Result<Response, String> {
        let handle = open(pipe)?;
        let body = encode(req);
        let mut written = 0u32;
        unsafe {
            if WriteFile(
                handle.0,
                body.as_ptr() as *const _,
                body.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(format!("写管道失败: 错误码 {}", GetLastError()));
            }
        }

        // 必须先读完响应再关句柄：消息模式管道会丢弃未读数据
        let mut buf = vec![0u8; MAX_RESPONSE_BYTES];
        let mut read = 0u32;
        unsafe {
            if ReadFile(
                handle.0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
            {
                // 这里刻意**不**翻成 unavailable：服务明明连着却不回话，
                // 是服务端故障，降级会把它掩盖掉
                return Err(format!("读管道失败: 错误码 {}", GetLastError()));
            }
        }
        let text = String::from_utf8(buf[..read as usize].to_vec())
            .map_err(|e| format!("响应不是 UTF-8: {e}"))?;
        decode_response(&text)
    }
}
```

Modify `src-tauri/src/service/mod.rs` 加 `pub mod client;` 与 `#[cfg(test)] mod client_tests;`

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test service::client_tests`
Expected: 3 passed

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/service/
git commit -m "feat(service): 命名管道客户端（区分可降级的不可用与真错误）"
```

---

### Task 7: mihomo 生命周期（服务侧）

服务收到 `StartNet` 后：清洗配置 → 落盘到固定路径 → spawn mihomo → 记录子进程。收到 `StopNet` 杀进程（幂等）。

**Files:**
- Create: `src-tauri/src/service/mihomo.rs`
- Modify: `src-tauri/src/service/pipe.rs`（接上请求分发）
- Modify: `src-tauri/src/service/mod.rs`

**Interfaces:**
- Consumes: `service::{config_guard::sanitize, paths::{config_path, net_dir}}`、`service::protocol::{Request, Response}`
- Produces:
  - `service::mihomo::handle(req: &Request) -> Response` —— 纯分发，无 IO 副作用以外的逻辑
  - `service::mihomo::current_pid() -> Option<u32>`

- [ ] **Step 1: 写实现**

Create `src-tauri/src/service/mihomo.rs`：

```rust
//! 服务侧 mihomo 生命周期。
//!
//! 只有「起进程 / 杀进程」在这——端口选择、就绪探测、系统代理、local_proxy
//! 登记全在主程序侧。理由见 service/mod.rs。
//!
//! mihomo 路径**从服务自身 exe 所在目录解析**，绝不接受客户端指定：那等于让
//! 任何本地进程指挥 SYSTEM 执行任意程序。
//!
//! 输入输出：Request → Response。
//! 依赖：std::process（mihomo 是外部进程，不需要 tokio）。

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;

use super::config_guard::sanitize;
use super::paths;
use super::protocol::{Request, Response};

/// 当前 mihomo 子进程。服务单实例运行，故用全局 Mutex 即可。
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// 服务自身 exe 所在目录下的 mihomo.exe。
fn mihomo_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("取服务自身路径失败: {e}"))?;
    let dir = exe.parent().ok_or("服务 exe 没有父目录")?;
    let path = dir.join("mihomo.exe");
    if !path.is_file() {
        return Err(format!("找不到 mihomo.exe：{}", path.display()));
    }
    Ok(path)
}

pub fn handle(req: &Request) -> Response {
    match req {
        Request::StartNet { config_yaml, .. } => start(config_yaml),
        Request::StopNet => stop(),
        Request::Status => status(),
    }
}

fn start(config_yaml: &str) -> Response {
    let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    // 幂等重入：已有一个活着的就先杀掉换新的，避免留下两份抢同一个 mixed-port
    if let Some(mut old) = guard.take() {
        let _ = old.kill();
        let _ = old.wait();
    }

    let cleaned = match sanitize(config_yaml, &paths::root()) {
        Ok(c) => c,
        Err(e) => return Response::Failed { message: e },
    };
    if let Err(e) = std::fs::create_dir_all(paths::net_dir()) {
        return Response::Failed { message: format!("创建 {} 失败: {e}", paths::net_dir().display()) };
    }
    if let Err(e) = std::fs::write(paths::config_path(), &cleaned) {
        return Response::Failed { message: format!("写入配置失败: {e}") };
    }

    let exe = match mihomo_path() {
        Ok(p) => p,
        Err(e) => return Response::Failed { message: e },
    };
    // 必须传 -d：该 mihomo 版本不读 cwd 的 config.yaml（与 net/runtime.rs:217 同因）
    let child = Command::new(&exe)
        .args(["-d", &paths::net_dir().to_string_lossy()])
        .current_dir(paths::net_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(c) => {
            *guard = Some(c);
            Response::Started
        }
        Err(e) => Response::Failed { message: format!("启动 mihomo 失败: {e}") },
    }
}

fn stop() -> Response {
    let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    let port = guard.take().map(|mut c| {
        let _ = c.kill();
        let _ = c.wait();
    });
    let _ = port;
    Response::Stopped { mixed_port: None }
}

fn status() -> Response {
    let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    let alive = match guard.as_mut() {
        Some(c) => matches!(c.try_wait(), Ok(None)),
        None => false,
    };
    if !alive {
        // 崩溃自愈：槽里留着死进程就清掉，免得 Status 一直报活着
        let _ = guard.take();
    }
    Response::Stopped { mixed_port: None }
}
```

**注意**：`Response::Stopped { mixed_port }` 现在承载「mihomo 是否在跑」的语义。把 `Status` 的响应改成带 `running` 字段更贴切——**回改 `protocol.rs`**：给 `Response` 加变体

```rust
/// 服务与 mihomo 的存活状态（`Status` 的响应）
Status { running: bool },
```

并让 `status()` 返回 `Response::Status { running: alive }`。同步更新任务 1 的 `response_roundtrip` 测试，加一条 `Response::Status { running: false }`。

- [ ] **Step 2: 接上分发**

Modify `service/pipe.rs` 的命令循环：读到 `Request` 后

```rust
let resp = super::mihomo::handle(&req);
let _ = write_message(handle, &encode_response(&resp));
```

- [ ] **Step 3: 编译与单测**

Run: `cd src-tauri && cargo test service::`
Expected: 全绿（协议 7 条 + 清洗 6 条 + 路径 3 条 + 管道 2 条）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/service/
git commit -m "feat(service): mihomo 生命周期（清洗落盘 + spawn/kill + 崩溃自愈）"
```

---

### Task 8: 安装 / 卸载 / 状态查询

装服务要用管理员权限（一次 UAC），之后 `sc start` 不再需要——靠的是安装时把服务 DACL 改成允许当前用户启停。

**Files:**
- Create: `src-tauri/src/service/install.rs`
- Modify: `src-tauri/src/service/mod.rs`
- Modify: `src-tauri/src/main.rs`（`--install-service` / `--uninstall-service` 分流）

**Interfaces:**
- Consumes: `service::paths::SERVICE_NAME`
- Produces:
  - `service::install::install() -> Result<(), String>`
  - `service::install::uninstall() -> Result<(), String>`
  - `service::install::is_installed() -> bool`

- [ ] **Step 1: 加入口分流**

在 `main.rs` 的 `--service` 分流之后、Tauri 之前：

```rust
    // 安装/卸载由主程序用 ShellExecute(runas) 拉起**本 exe** 的这两个标志，
    // 故这里必须在 Tauri 之前接住
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--install-service") {
        match service::install::install() {
            Ok(()) => println!("服务安装完成"),
            Err(e) => {
                eprintln!("服务安装失败: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.iter().any(|a| a == "--uninstall-service") {
        // …
    }
```

- [ ] **Step 2: 写实现**

`service/install.rs` 的三个函数：

- `install()`：
  1. `ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)`
  2. `ServiceInfo { name: SERVICE_NAME, display_name: "云驿网络服务", service_type: OWN_PROCESS, start_type: ServiceStartType::OnDemand, error_control: Normal, executable_path: 自身 exe, launch_arguments: vec![OsString::from("--service")], dependencies: vec![], account_name: None, account_password: None }`
     —— `account_name: None` = 以 LocalSystem 运行；`OnDemand` = **不开机自启**
  3. 已存在（`ERROR_SERVICE_EXISTS` 1073）→ 视为成功（幂等）
  4. 改 DACL：`sc.exe sdset yunyi-service <SDDL>`，SDDL 里给当前用户 SID 授 `SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS`。**这是「之后永不弹 UAC」的唯一来源**，不能省。
- `uninstall()`：先 `sc stop`，再 `Service::delete()`；未安装视为成功（幂等）
- `is_installed()`：`ServiceManager::local_computer(None, ServiceManagerAccess::CONNECT)` + `open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)` 成功即已装

- [ ] **Step 3: 编译确认**

Run: `cd src-tauri && cargo build`
Expected: 通过

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/service/ src-tauri/src/main.rs
git commit -m "feat(service): 安装/卸载（OnDemand 启动 + DACL 授予当前用户启停）"
```

---

### Task 9: 主程序接线（走服务 + 未装时降级）

把 `net::runtime` 的 spawn/kill 换成经服务；服务不可用时回退现行路径。

**Files:**
- Modify: `src-tauri/src/net/runtime.rs`
- Modify: `src-tauri/src/net/mod.rs`（`NetStatus` 加 `via_service: bool`）
- Create: `src-tauri/src/net/runtime_service_tests.rs`

**Interfaces:**
- Consumes: `service::client::{call, ERR_SERVICE_UNAVAILABLE}`、`service::protocol::{Request, Response}`
- Produces: `net::runtime::{start, stop, status}` 行为不变，`NetStatus` 多一个 `via_service: bool`

- [ ] **Step 1: 写失败的测试**

```rust
//! 服务不可用时必须回退直接 spawn——这是本阶段的**主要安全性保证**：
//! 服务有 bug 或没装，都不能让「开启网络」从可用变成不可用。
#[tokio::test]
async fn falls_back_to_direct_spawn_when_service_unavailable() {
    // 用一个不可能存在的管道名模拟「服务未装」
    // …起真实客户端、断言 running 且 via_service == false
}
```

**注意**：这条测试要起真实 mihomo，照 `runtime_tests.rs` 既有 smoke 用例的写法（含断言后立刻 `local_proxy::set(None)` 的收尾，见 `runtime_tests.rs:129-133` 的注释）。

- [ ] **Step 2: 改 `start()`**

把 `net/runtime.rs:219-223` 的 `Command::new(&exe).spawn()` 换成：

```rust
        // 优先经提权服务起进程（P2 的 TUN 需要它持有管理员令牌）；服务未装/
        // 未启动则回退直接 spawn——服务是增益不是依赖，不能让「开启网络」
        // 因为服务缺失而不可用。
        let via_service = match crate::service::client::call(&Request::StartNet {
            config_yaml: yaml.clone(),
            mixed_port,
            ctrl_port,
            token: token.clone(),
        }) {
            Ok(Response::Started) => true,
            Ok(Response::Failed { message }) => {
                last_err = format!("服务启动 mihomo 失败: {message}");
                continue;
            }
            // 服务不可用是预期情况（未安装），静默降级
            Err(_) => false,
        };
        let child = if via_service {
            None
        } else {
            Some(crate::util::quiet(std::process::Command::new(&exe))
                .args(["-d", net_dir.to_str().unwrap_or(".")])
                .current_dir(&net_dir)
                .spawn()
                .map_err(|e| format!("启动 mihomo 失败: {e}"))?)
        };
```

`RunningNet` 的 `child: Child` 改成 `child: Option<Child>`（`None` = 进程归服务管），`stop()` 与 `status()` 相应分支：`child` 为 `None` 时改发 `Request::StopNet` / `Request::Status`。

`wait_ready` 仍按原样用 `ctrl_port` + `token` 探测——它在主程序侧跑，与谁持有子进程无关。

- [ ] **Step 3: 跑测试**

Run: `cd src-tauri && cargo test net::`
Expected: 全绿

- [ ] **Step 4: 跑全量回归**

Run: `cd src-tauri && cargo test`
Expected: 全绿（基线 485 passed / 0 failed）

- [ ] **Step 5: 服务跟应用生命周期走**

在 `service/install.rs` 加两个函数：

```rust
/// 拉起服务（应用启动时调用）。未安装 → Err（调用方静默忽略，走降级）。
pub fn start_service() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("连接 SCM 失败: {e}"))?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::START)
        .map_err(|e| format!("打开服务失败（可能未安装）: {e}"))?;
    match service.start(&[] as &[&std::ffi::OsStr]) {
        Ok(()) => Ok(()),
        // 1073 = 已在运行：幂等成功，不是错误
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1056) => Ok(()),
        Err(e) => Err(format!("启动服务失败: {e}")),
    }
}

/// 停服务（应用退出时调用）。幂等。
pub fn stop_service() -> Result<(), String> { /* open_service(STOP).stop() */ }
```

在 `main.rs` 的 `.setup()` 末尾加上：

```rust
            // 服务未装/未启动都无所谓——net::runtime 会回退直接 spawn
            if let Err(e) = crate::service::install::start_service() {
                eprintln!("[vpn-ctl] 提权服务未就绪（将回退直接起 mihomo）: {e}");
            }
```

在既有的 `RunEvent::ExitRequested` handler（`main.rs` 里停 sidecar 的那处）**之前**加：

```rust
                // 先停服务：它会连带杀掉 mihomo。顺序反了的话，服务还活着、
                // mihomo 却已被停，下轮 start 会撞上「已在运行」的假象
                if let Err(e) = crate::service::install::stop_service() {
                    eprintln!("[vpn-ctl] 停止提权服务失败: {e}");
                }
```

- [ ] **Step 6: 手工验证生命周期**

Run: `cd src-tauri && cargo run`

Expected（服务已在任务 8 装好）：
1. 应用起来后 `sc query yunyi-service` → `STATE: RUNNING`
2. 关掉应用 → `sc query yunyi-service` → `STATE: STOPPED`

再验降级：`sc stop yunyi-service` 后**卸载**服务，重开应用并点「开启网络」——应照常可用（走了直接 spawn），日志里有「提权服务未就绪」但不影响功能。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/net/ src-tauri/src/service/ src-tauri/src/main.rs
git commit -m "feat(net): 开启网络优先经提权服务起 mihomo，服务缺失时回退直接 spawn"
```

---

### Task 10: 本机手工验证（P1 收口）

> **先做 Task 11。** 本任务的 Step 4（端到端开网络）在 Task 11 完成前必定失败——
> 那条路当时是死的，且失败方式不降级。Task 11 是执行本任务时实测暴露出来的。

SCM、UAC、管道 ACL 都无法在单测里跑，这一节就是它们的验证清单。**全部要管理员权限**。

- [x] **Step 1: 装服务**

在应用里（或直接）拉起 `vpn-ctl.exe --install-service`，过一次 UAC。

Expected: `sc query yunyi-service` 显示 `STATE: STOPPED`，`START_TYPE: DEMAND_START`

```powershell
sc.exe qc yunyi-service
```

必须看到 `START_TYPE : 3   DEMAND_START`——**若是 `2 AUTO_START` 就是错的**，违反「不开机自启」。

- [x] **Step 2: 无 UAC 启动服务**

```powershell
sc.exe start yunyi-service
```

Expected: 成功，**且不弹 UAC**。若弹了 UAC，说明任务 8 的 DACL 那步没生效。

- [x] **Step 3: 非管理员验证身份校验**

用记事本另存一份 `vpn-ctl.exe` 到桌面（路径变了），从它发起一次 `Status`。Expected: 连接被关、无响应（服务不回应身份不符者）。

- [x] **Step 4: 端到端开网络**

在应用里点「开启网络」。

Expected: 正常开启；`tasklist | findstr mihomo` 能看到 mihomo 由服务拉起；网络可用。

- [x] **Step 5: 退出即清**

关掉应用。

Expected: `sc query yunyi-service` → `STOPPED`；`tasklist | findstr mihomo` → 无 mihomo；系统代理已还原。

- [ ] **Step 6: 卸载**

```powershell
vpn-ctl.exe --uninstall-service
sc.exe query yunyi-service
```

Expected: `[SC] EnumQueryServicesStatus:OpenService 失败 1060: 指定的服务未安装`

> **未做，且是有意不做。** P2（TUN 开关）的开发全程需要服务保持安装状态；
> 卸载会连带把后续所有验证的前置条件拆掉。留到 P2 收口之后单独做一次。

**实测记录（2026-09-15 本机，Step 1~5）：**

| Step | 实测 |
|---|---|
| 1 装服务 | `sc qc yunyi-service` → `START_TYPE : 3 DEMAND_START`，**不是** `AUTO_START` |
| 2 无 UAC 启动 | 从**非提权** shell 直接 `sc.exe start yunyi-service` 成功、没弹 UAC（服务 DACL 授 IU 启停那步确有效） |
| 3 身份校验 | 调用方 `powershell.exe`（路径与服务自身 `target\debug\vpn-ctl.exe` 不同）：管道**连得上**（DACL 放行 IU，这是必需的），随即**被断开**——`Write` 抛「管道已中断」，拿不到任何响应。安全边界成立 |
| 4 端到端开网络 | 见 `2026-09-15-tun-switch.md` 的 Task 7 记录（更完整，含 TUN） |
| 5 退出即清 | 无 `vpn-ctl`/`mihomo`；服务 STOPPED；`yunyi*` 网卡 0；路由只剩 WLAN；`ProxyEnable=0` |

Step 3 顺带说明了一件事：**管道 DACL 与身份校验是两层，缺一不可**。DACL 必须放行 IU，
否则主程序（非提权）自己都连不上；真正拦人的是身份校验——它不回应、直接拆连接，
所以路径不符的进程连错误信息都拿不到。

---

### Task 11: 服务自给自足（修 P1 的取源与 geodata-dir）

**背景（既有缺陷，非前十个任务引入的逻辑错误，而是两个任务各自正确、合起来不成立）**

执行 Task 10 前静态复核时发现：**装上服务后「开启网络」100% 失败**，且因主程序对
「服务活着却失败」的契约是不降级（`net/runtime.rs:241`），它会耗尽全部端口候选后
整体报错，行为上完全不可用。两处独立成因：

**① `geodata-dir` 的实现与 spec §4.2 第 3 条不符。** spec 原文是「`geodata-dir`
**钉死到固定目录**」，Task 2 实现成了「越界**报错**」。而 `config/mod.rs:164` 生成配置时
**无条件**插入 `geodata-dir: <主程序数据目录>`（实测产物：
`C:\Users\asus\AppData\Roaming\com.vpnctl.app\net`），与服务的 `%ProgramData%\yunyi`
永不相等 → `sanitize` 恒 `Err` → 恒 `Response::Failed`。

Task 2 当时写「越界报错而非静默改写」的理由（避免 mihomo 实际行为与用户所见配置不一致）
对**用户数据**成立，但 `geodata-dir` 不是用户数据：geo 数据集是服务自有的只读资产，
必须落在管理员可写位置，否则非提权进程能换掉 SYSTEM mihomo 要解析的数据文件。故按 spec
改为无条件覆写。

**② 没有任何环节把 mihomo.exe 与 geo 数据集放进「服务 exe 同目录」。**
`service/mihomo.rs` 按 `<服务 exe 同目录>/mihomo.exe` 解析（符合 spec §4.2 第 1 条
「服务自己从固定安装目录解析」），但：

- dev：exe 在 `target/debug/`，那儿没有 mihomo.exe
- 正式安装：`tauri.conf.json` 只声明了 `binaries/mihomo.exe`，Tauri 保留相对路径 →
  落在 `<安装目录>/binaries/mihomo.exe`，不是 `<安装目录>/mihomo.exe`
- `%ProgramData%\yunyi` 至今不存在，除服务自己写 config.yaml 外无任何代码碰它
- `binaries/geo/` 有 geo 三件套，但没进 resources，且 `geodata.rs` 的
  `candidate_geo_paths()` 用的是**编译期** `CARGO_MANIFEST_DIR`（指向构建机），
  所以正式包里 geo 只能联网下——服务在 SYSTEM 下更不该走这条

**修法（用户已裁定）**：服务自给自足——它要执行/加载的一切都取自「服务 exe 同目录」
（管理员可写，与「服务二进制本身就在那儿」同一信任级）。

- 主程序非管理员，**写不进** `%ProgramData%\yunyi`，所以「让主程序去铺」这条路不成立
- geo 不从主程序的 APPDATA 取：那是非提权可写位置，换得掉 SYSTEM 要解析的数据文件

**③（补打包链路时发现的第三个洞）`binaries/geo/` 从来不是构建产物。**
`tauri.conf.json` 声明资源后才发现：`scripts/fetch-sidecar.mjs` 只拉 mihomo.exe，
`binaries/` 整个在 `.gitignore` 里。而 `geodata.rs::candidate_geo_paths()` 的注释和
本脚本头部都写着「npm 打包时并入资源」——**这个并入从来没实现过**。所以本机
`binaries/geo/` 的三个文件来历不明，全新构建机上是空的：只要把 geo 声明成资源，
`tauri build` 就会因资源缺失直接失败。

必须一并补上，否则「服务自带 geo」这个前提在干净环境里不成立。

**Files:**
- Modify: `src-tauri/src/service/config_guard.rs`（`sanitize` 签名与语义）
- Modify: `src-tauri/src/service/mihomo.rs`（资产目录解析 + geo 铺设）
- Modify: `src-tauri/tauri.conf.json`（geo 打进资源）
- Modify: `src-tauri/src/service/config_guard_tests.rs`
- Modify: `scripts/fetch-sidecar.mjs`（geo 落盘，此前只拉 mihomo）
- Create: `src-tauri/src/service/mihomo_tests.rs`

**Interfaces:**
- Produces: `sanitize(yaml: &str, pinned_geo_dir: &Path) -> Result<String, String>`——
  第二个参数从「允许的根」变成「要写进去的值」
- Produces: `mihomo::assets_dir_from(beside: Option<PathBuf>, dev: Option<PathBuf>) -> Option<PathBuf>`、
  `mihomo::provision_geo(src_geo: &Path, net_dir: &Path) -> Vec<String>`（均 `pub(super)`，供测试）

- [x] **Step 1: 改 `sanitize` 为钉死**

```rust
// 无条件覆写（含调用方压根没给这个字段的情况）：服务自有的数据集归服务管
map.insert(
    serde_yaml::Value::from("geodata-dir"),
    serde_yaml::Value::from(pinned_geo_dir.to_string_lossy().to_string()),
);
```

- [x] **Step 2: 改测试**

删 `geodata_dir_outside_root_is_rejected`、`geodata_dir_inside_root_is_kept`；新增
`geodata_dir_is_overwritten_to_the_pinned_dir`（用真实形态的 APPDATA 路径，断言产物里
不残留 `com.vpnctl.app`）与 `geodata_dir_is_added_even_when_caller_omitted_it`。

- [x] **Step 3: 资产目录解析**

`assets_dir()` = `<服务 exe 同目录>/binaries`，dev 回退 `CARGO_MANIFEST_DIR/binaries`。
抽出 `assets_dir_from(beside, dev)` 纯函数以便确定性测候选顺序。

- [x] **Step 4: geo 铺设**

`provision_geo(src_geo, net_dir)`：逐个复制 `src_geo` 下的**文件**（跳过子目录），
目标已存在的跳过（geoip.dat 十几 MB，不该每次开网络重写）。缺源**不拒绝启动**——
配置未必用到 geo 规则，为此打掉「开启网络」是因小失大，只记一行 stderr。

- [x] **Step 5: `start()` 接线**

顺序：`assets_dir` → `create_dir_all(net_dir)` → `provision_geo` → `sanitize(yaml, &net_dir)`
→ 写 config.yaml → `mihomo_path(&assets)` → spawn。geo 必须在配置落盘前铺，因为
`geodata-dir` 指向那里。

- [x] **Step 6: 打包声明 geo**

`tauri.conf.json` 的 `resources` 补三件套，与 mihomo 同为 `binaries/` 前缀（Tauri 保留
相对路径，正落在服务会去找的位置）。

- [x] **Step 7: 补 geo 的落盘**

`scripts/fetch-sidecar.mjs` 拆成 `fetchMihomo` + `fetchGeo`，两者互不跳过（mihomo 就绪
不代表 geo 也在）。geo 用 `GEODATA_BASE`（默认 `MetaCubeX/meta-rules-dat` 的 `latest`，
与 `geodata.rs::DEFAULT_GEODATA_BASE` 同源）逐个下载到 `binaries/geo/`。

geo **不做哈希校验**：上游 `latest` 是滚动 tag，钉死哈希会天天失败。数据集是公开只读的
分流表，不在信任边界上。

Run: `node --check scripts/fetch-sidecar.mjs && node scripts/fetch-sidecar.mjs`
Expected: 幂等退出 0（文件齐全时四项全跳过）

> **未实测**：本机 github.com 整站不可达（curl 超时，非 URL 问题），下载分支没跑过。
> 能拿到的证据只有「URL 与文件名清单和 `geodata.rs` 里生产在用的常量逐字符一致」。

- [x] **Step 8: 回归**

Run: `cargo test` ；`cargo clippy --all-targets`
Expected: 512 passed（较 Task 9 的 506 增 6 条）；clippy 无新增警告

- [x] **Step 9: 提交**

```bash
git add src-tauri/src/service src-tauri/tauri.conf.json scripts/fetch-sidecar.mjs docs/superpowers/plans/2026-09-15-tun-service-infra.md
git commit -m "fix(service): 服务自给自足——mihomo/geo 取自 exe 同目录，geodata-dir 按 spec 钉死"
```

**未覆盖（留给 Task 10 实测）**：本任务只做了静态复核能确认的部分。`<安装目录>/binaries`
的实际落盘位置、`%ProgramData%\yunyi` 的创建权限、SYSTEM 下 geo 复制是否被拦，都要靠
Task 10 的 Step 4 真正跑一遍才算数。

---

### Task 12: 实测暴露的两处管道缺陷（Task 10 Step 3/5 拦下）

前十个任务里两处**静态复核看不出来、一跑就现形**的错误。记录于此，因为它们的形态
（「盘上写着 A，实际是 B」）正是手工验收不可替代的理由。

**① 管道没有安全描述符 → 主程序自己连不上。**

`CreateNamedPipeW` 传的是 `std::ptr::null()` 安全属性，管道于是继承创建者（SYSTEM）的
默认 DACL——只有 SY 与 BA。实测（Task 10 Step 3）非提权进程连接直接 `Access is denied`，
**在 `Connect()` 就失败，根本走不到身份校验**。

后果命中主路径：主程序也是非特权的，它自己同样连不上。而 `client.rs` 把错误码 5
（`ERROR_ACCESS_DENIED`）归入 `连接服务管道失败`，不是 `ERR_SERVICE_UNAVAILABLE`
前缀，`is_unavailable` 返回 false → `net/runtime.rs` 视为真错误**不降级** →
「开启网络」整体失败。spec §4.2 设计的身份校验成了永远不会执行的死代码。

修法：显式给 DACL，授 `IU` 读写（`D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)`），
与安装时给 IU 授服务启停同一个取舍——DACL 只负责放行到身份校验那一步。
SDDL 转换失败时**明确停住**而不是退回 NULL：退回会建出一条谁都连不上的管道，
故障以「莫名其妙失败」的形态出现，极难定位。

**② `ConnectNamedPipe` 无限期阻塞 → 服务停不下来。**

`pipe.rs` 原设计是「在管道循环里每轮 `try_recv` 一次停止信号，靠 `nDefaultTimeOut`
（200ms）把 `ConnectNamedPipe` 叫醒」。**那个值只对客户端的 `WaitNamedPipe` 生效**，
对服务端的 `ConnectNamedPipe` 无效——后者在阻塞模式下无限期等待。

实测（Task 10 Step 5 前置）：`sc stop` 返回成功、服务却永远 RUNNING；再 stop 报
**1061**（无法接受控制消息）；`taskkill` 非提权被拒，最后只能提权强杀。

修法：管道循环移到独立线程，主线程 `recv()` 等停止信号，收到后**先杀 mihomo**
（静态量不析构、`Child` 的 Drop 也不杀进程，不显式清理就留一个 SYSTEM 孤儿）
再上报 STOPPED 让进程退出，管道句柄由内核回收。

**Files:**
- Modify: `src-tauri/src/service/pipe.rs`
- Modify: `src-tauri/src/service/host.rs`
- Modify: `src-tauri/Cargo.toml`（补 `Win32_Security_Authorization`）

- [x] **Step 1: 管道 DACL** — `PIPE_SDDL` + `ConvertStringSecurityDescriptorToSecurityDescriptorW`，
      `PipeSecurity` 的 Drop 负责 `LocalFree`
- [x] **Step 2: 停止走进程退出** — `serve_until()` 去掉 shutdown 参数（它本就没有可观察的退出点），
      host.rs 起线程 + 主线程等信号 + 杀 mihomo + 上报 STOPPED
- [x] **Step 3: 回归** — `cargo test` 512 passed；`cargo fmt --check` 0

**实测结论（2026-09-15，本机）**：

| 验证 | 结果 |
|---|---|
| Step 1 装服务 | ✅ `START_TYPE : 3 DEMAND_START`、`LocalSystem`、`STOPPED` |
| Step 2 非管理员启动 | ✅ 退出码 0、`RUNNING`、**全程无 UAC** |
| Step 3 身份校验 | ✅ 连接成功（DACL 放行）→ 服务在读载荷前即断开、无响应；服务未崩 |
| Step 5 停止 | ✅ `sc stop` 0.5s 内 `STOPPED`，PID 归零、管道消失、无孤儿 mihomo |

**仍未实测**：Step 4 端到端开网络（要 GUI 点「开启网络」）、Step 6 卸载。

---

## 本计划不覆盖（留给 P2）

- 配置生成加 `tun` 段（`config/mod.rs` 的 `client_yaml`）
- `SetTun` 协议命令与热重载切换
- 前端 TUN 开关与设置页的安装入口
- 应用启动时的残留网卡自检
