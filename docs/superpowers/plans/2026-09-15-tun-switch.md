# TUN 开关（P2）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在「开启网络」之后随时点开关切换 TUN 全局模式，全程不弹 UAC。

**Architecture:** 配置生成侧给 `client_yaml` 加 `tun_enabled` 参数（开启时同时插入 `tun` 与 `dns` 两块）；IPC 侧新增 `SetTun` 命令，由已提权的服务改写配置并打 `PUT /configs` 热重载；前端在仪表盘网络卡片加开关，服务未装时禁用并给安装入口。

**Tech Stack:** Rust（Tauri 2 后端）、React + TypeScript（前端）、windows-sys（IPC）、serde_yaml、mihomo RESTful API。

## Global Constraints

- 服务以 SYSTEM 运行，**只认四条命令**：`StartNet` / `StopNet` / `SetTun` / `Status`（spec §4.1）。白名单不得扩张。
- mihomo 路径**绝不由客户端指定**（spec §4.2.1）。
- 配置写盘前必须清洗：删 `log-file` / `external-ui`，`geodata-dir` 钉死到服务目录（spec §4.2.3，已在 P1 实现）。
- 服务**不开机自启**（`start=demand`）。
- 服务侧不引入新依赖：HTTP 用 std 的 `TcpStream` 手写，不拖 reqwest/tokio 进 SYSTEM 服务。
- 中文注释；注释解释「为什么」，不复述代码。

---

## Phase 0' 实测结论（2026-09-15 本机，服务已在跑）

**这些结论直接决定下文设计，先列。**

| 实验 | 结论 |
|---|---|
| 热重载翻 TUN | `PUT /configs` **HTTP 204，1.4s**；网卡瞬时建/销，无需重启 mihomo |
| 路由接管 | 只**新增**一条 metric 0 默认路由指向 `yunyi-tun0`，原 WLAN 路由保留（与 spec 0c 一致） |
| 真节点分流 | 直连（不走任何代理）出网从「失败」变为 `203.0.113.17` = `ashura` 节点 IP；国内目标仍走直连（baidu 0.13s） |
| **DNS（必须有）** | 不配 `dns` 块时，国外域名解析被污染 → `GEOIP,CN,DIRECT` **误判**（实测 `c.pki.goog` 被判成 CN 走 DIRECT）→ google 超时 |
| **`fallback` 必须走代理** | 只配 `fallback` + `fallback-filter` **没用**——fallback 的查询自己也是直连发出的、同样被污染。加 `#手动选择` 让查询经节点后：google 200/1.66s、github 200/2.42s、baidu 200/0.13s |
| 还原 | 卸掉后网卡消失、默认路由只剩 WLAN、零残留 |
| 与系统代理共存 | **保留**。两者同时开，国内外均正常（spec 倾向保留，实测支持） |

**仍未验证**：TUN 开启时 mihomo 崩溃/被强杀后的恢复（spec 0c 已证零残留，但那是 P0 在无节点配置下测的）。

---

## 文件结构

| 文件 | 职责 |
|---|---|
| `src-tauri/src/config/mod.rs` | `client_yaml` 生成配置；新增 `tun_and_dns_block()` 作为 tun/dns 两块的**唯一**来源 |
| `src-tauri/src/service/protocol.rs` | IPC 载荷；新增 `SetTun` |
| `src-tauri/src/service/http.rs`（新建） | 最小 HTTP/1.1 客户端（仅回环、仅明文） |
| `src-tauri/src/service/mihomo.rs` | 服务侧 mihomo 生命周期；新增记忆 ctrl/token/配置 + `SetTun` 处理 |
| `src-tauri/src/net/mod.rs` `runtime.rs` | `NetStatus` 加 `tun`；暴露 `set_tun()` |
| `src-tauri/src/commands.rs` | 新增 `set_tun_cmd` |
| `src/api/types.ts` `src/api/*.ts` | 前端类型与 IPC 封装 |
| `src/views/Dashboard.tsx` | 网络卡片加 TUN 开关 |
| `src/views/Settings.tsx` | 安装/卸载服务入口 |

---

### Task 1: `tun_and_dns_block()` + `client_yaml` 加参数

**Files:**
- Modify: `src-tauri/src/config/mod.rs`
- Test: `src-tauri/src/config/tests.rs`

**Interfaces:**
- Produces: `pub fn tun_and_dns_block(enabled: bool, has_nodes: bool) -> Vec<(&'static str, serde_yaml::Value)>`
  返回要插入/覆写的 `(key, value)` 对（`enabled=false` 时为空 vec）
- Produces: `client_yaml(..., tun_enabled: bool)` —— **加在 `servers` 之后**，避免打乱既有调用点

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn tun_block_present_when_enabled() {
    let y = client_yaml(&nodes, 7890, 9090, "tok", true, "C:/net", &[], &[], true).unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
    assert_eq!(v["tun"]["enable"], serde_yaml::Value::from(true));
    assert_eq!(v["tun"]["stack"], serde_yaml::Value::from("mixed"));
    assert_eq!(v["tun"]["auto-route"], serde_yaml::Value::from(true));
}

#[test]
fn tun_and_dns_absent_when_disabled() {
    let y = client_yaml(&nodes, 7890, 9090, "tok", true, "C:/net", &[], &[], false).unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
    assert!(v["tun"].is_null());
    assert!(v["dns"].is_null());
}

#[test]
fn dns_fallback_goes_through_proxy_group_only_when_nodes_exist() {
    // 零节点时没有 proxy-groups，`#手动选择` 会指向不存在的组 → mihomo 拒绝加载
    let with = tun_and_dns_block(true, true);
    let dns_with = &with.iter().find(|(k, _)| *k == "dns").unwrap().1;
    let fb = dns_with["fallback"].as_sequence().unwrap();
    assert!(fb[0].as_str().unwrap().contains("#手动选择"), "有节点时必须走代理");

    let without = tun_and_dns_block(true, false);
    let dns_without = &without.iter().find(|(k, _)| *k == "dns").unwrap().1;
    let fb2 = dns_without["fallback"].as_sequence().unwrap();
    assert!(!fb2[0].as_str().unwrap().contains('#'), "无节点时不得带组名");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test config::tests::tun -- --nocapture`
Expected: 编译失败（`client_yaml` 少一个参数 / `tun_and_dns_block` 不存在）

- [x] **Step 3: 实现**

```rust
/// tun 与 dns 两块配置的**唯一**来源：主程序生成配置、服务热重载翻开关，都调这里。
///
/// 两块绑在一起不是偷懒：实测（见计划 Phase 0'）在 TUN 下不配 dns 块，
/// 国外域名解析会被污染，`GEOIP,CN,DIRECT` 于是把 google 判成国内直连而超时。
///
/// `has_nodes` 决定 dns fallback 是否带 `#手动选择`：**这是整件事的关键**。
/// fallback 的查询如果不走代理，它自己也是直连发出的、同样被污染，
/// `fallback-filter` 就形同虚设。没有节点时不能带组名（组不存在会让 mihomo 拒绝加载）。
pub fn tun_and_dns_block(enabled: bool, has_nodes: bool) -> Vec<(&'static str, serde_yaml::Value)> {
    if !enabled {
        return vec![];
    }
    let tun: serde_yaml::Value = serde_yaml::from_str(
        "enable: true\nstack: mixed\ndevice: yunyi-tun0\nauto-route: true\nauto-detect-interface: true\n",
    )
    .expect("tun 块字面量合法");
    let fallback_suffix = if has_nodes { "#手动选择" } else { "" };
    let dns: serde_yaml::Value = serde_yaml::from_str(&format!(
        "enable: true\nenhanced-mode: fake-ip\nfake-ip-range: 198.18.0.1/16\n\
         nameserver:\n  - https://223.5.5.5/dns-query\n  - https://1.12.12.12/dns-query\n\
         fallback:\n  - https://1.1.1.1/dns-query{suffix}\n  - https://8.8.8.8/dns-query{suffix}\n\
         fallback-filter:\n  geoip: true\n  geoip-code: CN\n",
        suffix = fallback_suffix
    ))
    .expect("dns 块字面量合法");
    vec![("tun", tun), ("dns", dns)]
}
```

`client_yaml` 内，在现有 `root.insert` 序列之后：

```rust
    for (k, v) in tun_and_dns_block(tun_enabled, has_nodes) {
        root.insert(s(k), v);
    }
```

- [x] **Step 4: 更新两处既有调用点**

`src/net/runtime.rs:211` 与 `src/config/tests.rs:584,600` 补最后一个参数（先补 `false`，Task 5 再改成真实值）。

- [x] **Step 5: 跑测试**

Run: `cargo test config::`
Expected: PASS

- [x] **Step 6: 提交**

```bash
git add src-tauri/src/config src-tauri/src/net/runtime.rs
git commit -m "feat(config): client_yaml 支持 TUN——tun 与 dns 两块同源生成"
```

---

### Task 2: `SetTun` 协议

**Files:**
- Modify: `src-tauri/src/service/protocol.rs`
- Test: `src-tauri/src/service/protocol_tests.rs`

**Interfaces:**
- Produces: `Request::SetTun { enable: bool }`、`Response::TunSet { enabled: bool }`
- Produces: `Response::Status { running: bool, tun: bool }`（加字段）

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn set_tun_roundtrip() {
    let req = Request::SetTun { enable: true };
    assert_eq!(decode(&encode(&req)).unwrap(), req);
}

#[test]
fn status_carries_tun_flag() {
    // 前端的开关状态只能来自这里——主程序不自己记账，避免两处状态漂移
    let r = Response::Status { running: true, tun: true };
    assert_eq!(decode_response(&encode_response(&r)).unwrap(), r);
}
```

- [x] **Step 2: 跑测试确认失败** — Run: `cargo test service::protocol_tests`

- [x] **Step 3: 实现**

`Request` 加 `SetTun { enable: bool }`；`Response::Status` 加 `tun: bool`；新增 `Response::TunSet { enabled: bool }`。

- [x] **Step 4: 修既有构造点** — `mihomo.rs` 的 `status()`、`runtime.rs` 的匹配分支

- [x] **Step 5: 跑测试并提交**

```bash
git commit -am "feat(service): IPC 加 SetTun，Status 带 tun 状态"
```

---

### Task 3: 服务侧最小 HTTP 客户端

**Files:**
- Create: `src-tauri/src/service/http.rs`
- Test: `src-tauri/src/service/http_tests.rs`

**Interfaces:**
- Produces: `pub fn put_json(addr: &str, path: &str, token: &str, body: &serde_json::Value) -> Result<u16, String>`
  返回 HTTP 状态码

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn parses_status_line() {
    assert_eq!(parse_status("HTTP/1.1 204 No Content\r\n").unwrap(), 204);
    assert_eq!(parse_status("HTTP/1.1 400 Bad Request\r\n").unwrap(), 400);
}

#[test]
fn malformed_response_is_an_error_not_a_guess() {
    // 静默当成 200 会让「热重载失败」被伪装成成功，开关回弹但用户不知道为什么
    assert!(parse_status("garbage").is_err());
    assert!(parse_status("").is_err());
}
```

- [x] **Step 2: 跑测试确认失败** — Run: `cargo test service::http_tests`

- [x] **Step 3: 实现**

```rust
//! 最小 HTTP/1.1 客户端（仅回环、仅明文）。
//!
//! 为什么不用 reqwest：服务以 SYSTEM 运行，且运行在 Tauri 之外（main.rs 在
//! 拉起 Tauri 之前就分流到服务模式），进程里没有 tokio 运行时。为一次
//! `PUT /configs` 拖进 reqwest + 一个隐式运行时，给 SYSTEM 服务平添攻击面。
//! 控制器的流量是 127.0.0.1 明文 HTTP，手写三十行够用且完全可审。
//!
//! 输入输出：PUT 一个 JSON body → 状态码。失败一律 Err，**绝不猜**。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 只解析状态行。非 2xx 由调用方决定怎么报——这里不替它判断。
pub(super) fn parse_status(head: &str) -> Result<u16, String> {
    let line = head.lines().next().ok_or("响应为空")?;
    let mut parts = line.split_whitespace();
    let version = parts.next().ok_or("状态行缺协议版本")?;
    if !version.starts_with("HTTP/") {
        return Err(format!("响应不是 HTTP: {line}"));
    }
    parts
        .next()
        .ok_or_else(|| format!("状态行缺状态码: {line}"))?
        .parse::<u16>()
        .map_err(|e| format!("状态码不是数字: {e}"))
}

pub(super) fn put_json(addr: &str, path: &str, token: &str, body: &serde_json::Value) -> Result<u16, String> {
    let payload = serde_json::to_string(body).map_err(|e| format!("序列化失败: {e}"))?;
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("连接控制器 {addr} 失败: {e}"))?;
    // 控制器是本机进程，卡住只可能是它挂了；5s 足够，且不能让服务线程无限等
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let req = format!(
        "PUT {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{payload}",
        len = payload.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("发送请求失败: {e}"))?;

    let mut buf = String::new();
    // 只读状态行所在的首段即可；Connection: close 保证对端会关闭
    let mut chunk = [0u8; 1024];
    if let Ok(n) = stream.read(&mut chunk) {
        buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    parse_status(&buf)
}
```

- [x] **Step 4: 跑测试并提交**

```bash
git commit -am "feat(service): 最小 HTTP 客户端（回环明文，不拖 reqwest 进 SYSTEM 服务）"
```

---

### Task 4: 服务侧 `SetTun` —— 改写配置 + 热重载

**Files:**
- Modify: `src-tauri/src/service/mihomo.rs`
- Modify: `src-tauri/src/service/mod.rs`（挂 `http`）

**Interfaces:**
- Consumes: `config::tun_and_dns_block`、`service::http::put_json`
- Produces: `mihomo::handle` 支持 `SetTun`

- [x] **Step 1: 写失败测试**（纯函数部分）

```rust
#[test]
fn flipping_tun_rewrites_both_blocks() {
    // 从「TUN 关」的配置翻到开：tun 与 dns 两块都要出现
    let before = "mixed-port: 7890\nproxy-groups:\n- name: 手动选择\n  type: select\n";
    let after = super::mihomo::with_tun(before, true).unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
    assert_eq!(v["tun"]["enable"], serde_yaml::Value::from(true));
    assert_eq!(v["dns"]["enable"], serde_yaml::Value::from(true));
    assert_eq!(v["mixed-port"], serde_yaml::Value::from(7890), "其余字段不得丢");
}

#[test]
fn flipping_tun_off_removes_both_blocks() {
    let on = super::mihomo::with_tun("mixed-port: 7890\n", true).unwrap();
    let off = super::mihomo::with_tun(&on, false).unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&off).unwrap();
    assert!(v["tun"].is_null());
    assert!(v["dns"].is_null());
    assert_eq!(v["mixed-port"], serde_yaml::Value::from(7890));
}
```

- [x] **Step 2: 跑测试确认失败**

- [x] **Step 3: 实现**

新增 statics 记住 `StartNet` 带来的 `ctrl_port` / `token` / 清洗后的配置原文；`with_tun(yaml, enable)` 用 serde_yaml 解析、删旧 `tun`/`dns`、按 `config::tun_and_dns_block` 插新块、序列化。

`has_nodes` 的判据：解析后的配置里有没有非空 `proxy-groups`。

`SetTun` 处理：改配置 → 写 `paths::config_path()` → `http::put_json("127.0.0.1:{ctrl_port}", "/configs", &token, &json!({"path": ..., "payload": yaml, "force": true}))` → 2xx 则 `Response::TunSet { enabled }`，否则 `Response::Failed`。

- [x] **Step 4: 跑测试并提交**

```bash
git commit -am "feat(service): SetTun 改写 tun/dns 块并热重载"
```

---

### Task 5: 主程序接线

**Files:**
- Modify: `src-tauri/src/net/mod.rs`（`NetStatus` 加 `tun: bool`）
- Modify: `src-tauri/src/net/runtime.rs`（`status()` 带 tun；新增 `set_tun()`）
- Modify: `src-tauri/src/commands.rs`（`set_tun_cmd`）
- Modify: `src-tauri/src/main.rs`（注册命令）
- Modify: `src-tauri/src/net/runtime.rs:211`（`client_yaml` 传真实 tun 值）

**Interfaces:**
- Produces: `pub async fn set_tun(state: &AppState, enable: bool) -> Result<(), String>`
- Produces: IPC `set_tun_cmd(enabled: bool) -> Result<(), String>`

- [x] **Step 1: 写失败测试**

覆盖两条：`set_tun` 在服务不可用时**必须报错而不是静默成功**（TUN 没有降级路径——非提权进程建不了网卡）；`status()` 的 `tun` 在服务不可用时为 `false`。

- [x] **Step 2~4: 实现 + 跑测试**

`set_tun` 直接 `svc::call(&SvcRequest::SetTun { enable })`，`Err(e) if is_unavailable` → 返回「需要先安装网络服务」的明确提示；其余错误原样透出。

- [x] **Step 5: 提交**

```bash
git commit -am "feat(net): set_tun 接线 + NetStatus.tun"
```

---

### Task 6: 前端开关

**Files:**
- Modify: `src/api/types.ts`、`src/api/*.ts`、`src/views/Dashboard.tsx`、`src/views/Settings.tsx`
- Test: `src/views/Dashboard.test.tsx`

- [x] **Step 1: 写失败测试**

三种形态各一条：服务可用 → 开关可点；`via_service === false` → 开关禁用 + 文案说明；TUN 开启中 → 显示已开启且按钮文字为「关闭 TUN」。

- [x] **Step 2~4: 实现**

`NetStatus` 的 `tun` 与 `via_service` 决定开关形态。失败时（热重载非 2xx）开关回弹 + 展示服务返回的错误。

- [x] **Step 5: 跑前端门禁**

Run: `npx tsc --noEmit && npm test`
Expected: 全绿

- [x] **Step 6: 提交**

```bash
git commit -am "feat(ui): 仪表盘 TUN 开关（服务未装时禁用并给安装入口）"
```

---

### Task 7: 本机手工验收

**全部要管理员权限（仅装/卸服务那两步）。**

- [x] 开网络 → 点 TUN 开关 → `netsh interface show interface` 出现 `yunyi-tun0`，默认路由多一条
- [x] **不走系统代理**直连 `https://www.google.com` → 200（这是 TUN 生效的判据）
- [x] `https://www.baidu.com` → 仍走直连（<0.5s）
- [x] 再点一次 → 网卡消失、路由还原、直连 google 恢复失败
- [x] 关应用 → 服务 STOPPED、无 mihomo、无 yunyi-tun0、系统代理还原

**实测记录（2026-09-15 本机）：**

| 步骤 | 实测 |
|---|---|
| 基线（开 TUN 前） | 网卡只有 WLAN + Tailscale；默认路由只有 WLAN→192.168.0.1；直连 google `code=000` |
| 点 TUN | `yunyi-tun0` Up（Meta Tunnel）；默认路由**新增**一条 198.18.0.2，**WLAN 那条保留**（与 spec 0c 一致） |
| 直连 google | `200`，1.83s |
| 出网 IP | `203.0.113.17` —— 在应用自己生成的 config.yaml 里正是 `ashura-vmess`（vmess，8444） |
| 直连 baidu | `200`，0.297s |
| 再点一次 | `yunyi-tun0` 消失；路由只剩 WLAN；google 回到 `code=000`；**mihomo 仍在跑**（只翻 TUN，没重启网络，正确） |
| 残留 | `yunyi*` 网卡（含隐藏）0 个；`198.18.0.0/15` 路由 0 条 |
| 关应用 | 无 `vpn-ctl`/`mihomo` 进程；服务 STOPPED；网卡 0；路由只剩 WLAN；`ProxyEnable=0` |
| 设置页 | 「已安装 · 运行中」+ 卸载按钮，`net_service_state_cmd` 查到的状态与实际一致 |

**一条未解释的观察**：TUN 开着时 baidu 0.297s，关掉后头两次 2.82s / 2.86s，之后稳定在
0.80s（其中系统 DNS 解析占 0.71s）。开 TUN 前没有单独测过 baidu，所以**无法排除也无法
确认**是翻开关留下的退化。能确定的是与 TUN 拆除无关：网卡与路由零残留，google 精确回到
失败态。若后续在意，需要的是「从没开过 TUN 时」的 baidu 基线，而不是再翻一次开关。

---

## 本计划不覆盖

- **启动期残留网卡自检**（spec §6 的兜底、§8 的 P3）：删网卡需要管理员权限，主程序做不到，得走服务命令。P0 已证正常路径不会有残留，留到 P3。
- **Authenticode 签名校验**（spec §4.2.2）：需先买证书，独立一事。
- **TUN 下 mihomo 被强杀后的恢复**：spec 0c 已证零残留，但当时是无节点配置，P2 落地后需复验。

---

## 执行中发现的问题（2026-09-15）

### 服务在跑时 clippy 必然失败——锁的是 `target/debug/binaries/mihomo.exe`（**已修**）

**症状**：`cargo clippy`（带不带 `--all-targets` 都一样）报

```
error: failed to run custom build command for `vpn-ctl v0.2.3`
  --- stdout
  cargo:rerun-if-changed=binaries\mihomo.exe
  另一个程序正在使用此文件，进程无法访问。 (os error 32)
```

**这条报错不点名文件**，而最后打印的那行 `binaries\mihomo.exe` 是**干扰项**——源目录那个文件没被锁（独占读写打开都成功，实测）。

**真正的锁**：`target/debug/binaries/mihomo.exe`。链路是：

1. `assets_dir()` 取 `<exe 同目录>/binaries`——debug 构建下服务 exe 在 `target/debug/`，于是解析到 `target/debug/binaries/`。
2. 该目录**存在**（正是 tauri-build 的 `copy_resources` 建出来的），所以 dev 回退分支轮不到，服务就从这儿跑 mihomo。
3. tauri-build 每次重跑 build script 都要 `fs::copy(binaries/mihomo.exe → target/debug/binaries/mihomo.exe)`（`tauri-build-2.6.3/src/lib.rs:88-103`），目的地被跑着的 mihomo 占着 → os error 32。

**为什么 `cargo build` 看着没事**：build script 的指纹只跟 `tauri.conf.json` / `capabilities` / `binaries/` 内容走，改 `.rs` 不触发它重跑。clippy 换了 `RUSTC_WORKSPACE_WRAPPER`，指纹变化 → 必然重跑 → 必然撞锁。所以这不是偶发，是**开着应用就没法跑 lint**，实测两种调用方式都复现。

**已修**：服务不再从资产目录直接跑 mihomo，改为先铺进 `%ProgramData%\yunyi\bin`
（服务自有、默认只有管理员可写），从那儿执行——和 `provision_geo` 早就对 geo 做的事
一模一样。铺的时候比 `(长度, mtime)`，一致就跳过，省掉每次 31MB 的拷贝，也顺带
躲开「上一份刚 kill、镜像句柄还没完全释放」的窄窗口。

这一改同时补上了同源的另一条：本来 dev 下服务以 SYSTEM 跑的是 `target/debug/binaries`
里那份 mihomo，而 `target/` 普通用户可写——等于一个「换掉 mihomo.exe 换 SYSTEM 代码
执行」的口子。生产环境（资源在 Program Files）本来就没这两个问题，但统一成一条规则
更省事。

`mtime` 那条假设（`fs::copy` 保留 mtime，Windows 走 `CopyFileExW`）由
`provision_mihomo_skips_the_copy_when_nothing_changed` 钉着——它用独占句柄
（`share_mode(0)`）验证源没变时不碰目标文件。若不成立，该测试会以「重拷撞锁」
的形式失败，而不是让代码静默跑旧二进制。

### 铺进 `%ProgramData%\yunyi` 还不够——那个目录本身就没锁（**已修**）

上面那条改完之后，我写了一句「服务自有的目录只有管理员可写，这条规则坐实了」。
**实测把这句话推翻了。** `C:\ProgramData\yunyi` 的实测 ACL：

```
Owner: BUILTIN\Administrators
  NT AUTHORITY\SYSTEM    | FullControl      | inherited=True
  BUILTIN\Administrators | FullControl      | inherited=True
  CREATOR OWNER          | 268435456        | inherited=True
  BUILTIN\Users          | ReadAndExecute   | inherited=True
  BUILTIN\Users          | Write            | inherited=True      ← 问题在这
```

`AreAccessRulesProtected = False`，也就是全部靠继承，而继承链从 `C:\ProgramData`
带下来了一条 `Users: Write`。实测（非提权账户 `ASHURAFL\asus`）**确实能在该目录下
建出子目录**。

于是有预创建攻击：抢在服务之前把 `bin` 建出来，创建者按 `CREATOR OWNER` 规则拿到
该目录的 FullControl，服务随后把 `mihomo.exe` 写进这个用户说了算的目录——换掉它
照样是 SYSTEM 代码执行。**等于把口子从 `target/` 搬了个家，没堵上。**

修法：`service/secure_dir.rs`。服务自有的目录不再靠继承，而是自带 DACL
（`D:PAI(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)`，受保护、只授 SYSTEM 与管理员），
`start()` 里按 父→子 的顺序对 `root` / `net` / `bin` 各做一次。

两个实现上的坑，都是实测撞出来的：

1. **不能只换 DACL，得连所有者一起夺。** Windows 给对象所有者保留隐式的
   `WRITE_DAC`，只换 DACL 的话，抢先建目录的人随时能改回来。所以「目录已存在」
   那一路走 `OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION`。
   夺所有者要 `SeRestorePrivilege`——SYSTEM 持有，但持有不等于默认启用，所以
   失败时降级为只设 DACL 并 `eprintln!` 出声（主路已堵，残留的只是那条窄路）。
2. **创建那一路不能硬钉 `O:BA`。** 第一版写了 `O:BAD:PAI(...)`，测试进程直接拿到
   `ERROR_INVALID_OWNER`(1307)——非 SYSTEM 调用者没这个特权。改成创建时不指定
   所有者（落成创建者，服务建就是 SYSTEM），只在「已存在」那一路夺。

验证（非提权 shell，独立于测试代码复核）：

```
Protected(不继承): True
  ACE: NT AUTHORITY\SYSTEM    | FullControl | inherited=False
  ACE: BUILTIN\Administrators | FullControl | inherited=False
写测试: 被拒 -> UnauthorizedAccessException
```

测试 +4（`secure_dir_tests.rs`）。其中 `non_admin_cannot_write_into_a_secured_dir`
是**真往受保护目录写一次**并断言被拒——光比 DACL 字符串证不了这件事，加固失效的
典型形态恰恰是指纹正确但权限被继承悄悄带回来。

**攻击路径到底有多宽——实测把 ACE 的继承标志摊开看**：

```
BUILTIN\Users | ReadAndExecute, Synchronize | ContainerInherit, ObjectInherit
BUILTIN\Users | Write                       | ContainerInherit          ← 只进子目录
CREATOR OWNER | GENERIC_ALL                 | ContainerInherit, ObjectInherit, InheritOnly
```

`Users: Write` **只作用于容器**，所以直接落在 `yunyi` 或 `net` 里的文件本来就只拿到
`ReadAndExecute`（实测 `net\config.yaml`、`geoip.dat`、`country.mmdb` 都是这个结果），
普通用户写不进去。真正能利用的是 `CREATOR OWNER` 那条——它是 `ObjectInherit` 的，
所以**用户抢建出来的目录**，其 FullControl 会一路传给服务之后写进去的文件。

结论：漏洞面完全落在「用户能抢先建出子目录」这一点上。把 `yunyi` 保护起来（别人建不了
子目录）就同时堵死了两条：

1. 抢建 `bin` → 换掉 `mihomo.exe`（SYSTEM 代码执行）；
2. 抢建 `net` 并预先放一份 `geosite.dat` ——`provision_geo` 对已存在的文件是跳过的，
   服务永远不会覆盖它，而 SYSTEM 的 mihomo 会去读（路由操纵，取决于 geosite 解析器
   还有没有更深的洞）。

已存在的旧文件不需要迁移：实测它们本来就没拿到 `Users: Write`。

### 仍未修：服务跑着时 `cargo build` 换不掉 `target/debug/vpn-ctl.exe`

服务是通过 SCM 注册、以 `target/debug/vpn-ctl.exe` 这个路径运行的，它自己就锁着这个
文件。**这条不打算修**，因为它和身份校验绑在一起：管道的「调用方 exe 路径必须与服务
自身一致」正是安全边界，而它要求 GUI 与服务**是同一个文件**。把服务改成跑自己的副本
就破坏了这个不变量。

生产下这不是问题（应用装在 Program Files，服务注册到同一个路径，cargo 不参与）。
dev 下的代价是：装卸服务那类要重建 exe 的操作得先把应用关掉。可接受。
（实机复测记在下面。）
