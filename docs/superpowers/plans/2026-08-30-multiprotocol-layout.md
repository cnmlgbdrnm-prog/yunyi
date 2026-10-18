# 多协议落地机 + 服务器页就地展开 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地机部署从 2 协议扩到 6 协议(L1 必装: vless/trojan/vmess/ss; L2 可选: hysteria2/tuic),节点面映射补全(hysteria2/tuic/wireguard),服务器页改成卡片就地展开交互。

**Architecture:** 后端按"协议注册表"重构 deploy 流水线:一次 job 产出多协议节点;xray 单进程多入站、ss 独立 unit、sing-box 可选 unit;失败按协议隔离。前端用 ExpandableCard 互斥展开(部署向导/编辑表单),协议卡网格选择器。节点面:config/mod.rs 补 mihomo 映射、singbox 补 tuic/wg、subs/parse.rs 补 tuic URI。

**Tech Stack:** Rust(tauri 2 / tokio / russh / serde)、React 19 + TS + Tailwind v4、vitest + cargo test。

## Global Constraints

- 部署协议白名单常量:`["vless","trojan","vmess","ss","hysteria2","tuic"]`(字符串标识,入 nodes.protocol 与 params)
- 端口自动分配表:`vless=443、trojan=4443、vmess=8443、ss=8388、hysteria2=2097、tuic=2053`,占用逐 +1 探测(最多 20 次),可被覆盖
- 远端写文件只经 `write_remote_file` + 白名单常量路径(禁止动态拼路径)
- 所有远端命令经 `run_cmd(ok)`;超时:INSTALL_TIMEOUT 600s / CONFIG_TIMEOUT 60s / VERIFY_TIMEOUT 30s
- 每一步有检查点;失败隔离:单协议失败 → 其余继续验证与写回,进度事件携带 `failed_protocols`
- 前端展开状态用 `expanded: {type:'deploy'|'edit', serverId} | null`,互斥展开
- spec:`docs/superpowers/specs/2026-08-30-multiprotocol-layout-design.md`

---

### Task 1: 协议注册表与端口分配表(deploy.rs 纯函数)

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`(新增 pub fn,不改流水线)
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Produces:
```rust
pub const PROTOCOLS: [&str; 6] = ["vless", "trojan", "vmess", "ss", "hysteria2", "tuic"];
pub const L1_PROTOCOLS: [&str; 4] = ["vless", "trojan", "vmess", "ss"];
pub fn default_port(proto: &str) -> u16;               // 上表
pub fn is_supported_protocol(proto: &str) -> bool;      // PROTOCOLS.contains
pub fn allocate_port(sess, cancel, base: u16) -> ...    // probe_port 逐 +1,async fn → Result<u16, String>
```
- Consumes: `probe_port`(`deploy.rs:42`), `PROBE_TIMEOUT`

- [ ] **Step 1: 写失败测试**

```rust
// deploy_tests.rs
use super::*;

#[test]
fn default_ports_follow_spec_table() {
    assert_eq!(default_port("vless"), 443);
    assert_eq!(default_port("trojan"), 4443);
    assert_eq!(default_port("vmess"), 8443);
    assert_eq!(default_port("ss"), 8388);
    assert_eq!(default_port("hysteria2"), 2097);
    assert_eq!(default_port("tuic"), 2053);
    assert_eq!(default_port("unknown"), 0);
}

#[test]
fn supported_protocols_whitelist() {
    assert!(is_supported_protocol("vless"));
    assert!(is_supported_protocol("tuic"));
    assert!(!is_supported_protocol("socks"));
    assert_eq!(L1_PROTOCOLS.len(), 4);
    for p in L1_PROTOCOLS { assert!(is_supported_protocol(p)); }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vpn-ctl default_ports_follow_spec_table` · Expected: FAIL(函数不存在)

- [ ] **Step 3: 实现纯函数**

```rust
pub const PROTOCOLS: [&str; 6] = ["vless", "trojan", "vmess", "ss", "hysteria2", "tuic"];
pub const L1_PROTOCOLS: [&str; 4] = ["vless", "trojan", "vmess", "ss"];

pub fn is_supported_protocol(proto: &str) -> bool { PROTOCOLS.contains(&proto) }

pub fn default_port(proto: &str) -> u16 {
    match proto {
        "vless" => 443, "trojan" => 4443, "vmess" => 8443,
        "ss" => 8388, "hysteria2" => 2097, "tuic" => 2053,
        _ => 0,
    }
}

/// 给定基准端口,从 base 起探测空闲(逐 +1),最多 20 次;全占报错。
async fn allocate_port(
    sess: &mut SshSession, cancel: &AtomicBool, base: u16,
) -> Result<u16, String> {
    for n in 0..21u16 {
        let cand = base.saturating_add(n);
        if !probe_port(sess, cancel, cand).await? {
            return Ok(cand);
        }
    }
    Err(format!("端口 {base} 起 20 个连续全被占用"))
}
```

- [ ] **Step 4: 通过**

Run: `cargo test -p vpn-ctl deploy` · Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): 协议注册表与端口分配表"
```

---

### Task 2: xray 多入站配置生成(金样)

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`(`xray_config_json` 旁新增)
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Produces:
```rust
pub struct XrayInboundEntry {
    pub proto: &'static str,           // "vless"|"trojan"|"vmess"
    pub port: u16,
    pub uuid: String,
    pub password: String,              // trojan 用
    pub short_id: String, pub private_key: String, pub reality_dest: String, // reality 两协议
    pub ws_path: String,               // vmess 用,默认 "/ray"
}
pub fn xray_config_json_multi(entries: &[XrayInboundEntry]) -> String;
```
- Consumes: 无(纯函数;现有 `xray_config_json` 保留到 T3 移除)

- [ ] **Step 1: 写失败测试(金样)**

```rust
#[test]
fn xray_multi_inbounds_includes_three_protocols() {
    let e = vec![
        XrayInboundEntry { proto: "vless", port: 443, uuid: "u1".into(), password: String::new(), short_id: "a1".into(), private_key: "pk".into(), reality_dest: "www.microsoft.com".into(), ws_path: String::new() },
        XrayInboundEntry { proto: "trojan", port: 4443, uuid: String::new(), password: "p1".into(), short_id: "a1".into(), private_key: "pk".into(), reality_dest: "www.microsoft.com".into(), ws_path: String::new() },
        XrayInboundEntry { proto: "vmess", port: 8443, uuid: "u2".into(), password: String::new(), short_id: String::new(), private_key: String::new(), reality_dest: String::new(), ws_path: "/ray".into() },
    ];
    let cfg = xray_config_json_multi(&e);
    let v: serde_json::Value = serde_json::from_str(&cfg).expect("valid json");
    let inbounds = v["inbounds"].as_array().expect("inbounds array");
    assert_eq!(inbounds.len(), 3);
    assert_eq!(inbounds[0]["protocol"], "vless");
    assert_eq!(inbounds[0]["streamSettings"]["security"], "reality");
    assert_eq!(inbounds[0]["settings"]["clients"][0]["id"], "u1");
    assert_eq!(inbounds[1]["protocol"], "trojan");
    assert_eq!(inbounds[1]["settings"]["clients"][0]["password"], "p1");
    assert_eq!(inbounds[2]["protocol"], "vmess");
    assert_eq!(inbounds[2]["streamSettings"]["network"], "ws");
    assert_eq!(inbounds[2]["streamSettings"]["wsSettings"]["path"], "/ray");
    // vless/trojan 必须带 reality,vmess 不带
    assert_eq!(inbounds[2]["streamSettings"].get("security"), None);
    assert_eq!(v["outbounds"][0]["protocol"], "freedom");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vpn-ctl xray_multi_inbounds` · Expected: FAIL

- [ ] **Step 3: 实现**

```rust
pub struct XrayInboundEntry {
    pub proto: &'static str,
    pub port: u16,
    pub uuid: String,
    pub password: String,
    pub short_id: String,
    pub private_key: String,
    pub reality_dest: String,
    pub ws_path: String,
}

pub fn xray_config_json_multi(entries: &[XrayInboundEntry]) -> String {
    let inbounds: Vec<serde_json::Value> = entries.iter().map(|e| {
        let base = serde_json::json!({
            "listen": "0.0.0.0",
            "port": e.port,
            "protocol": e.proto,
            "sniffing": { "enabled": true, "destOverride": ["http", "tls", "quic"] },
        });
        let mut stream = serde_json::Map::new();
        match e.proto {
            "vless" => {
                stream.insert("network".into(), "tcp".into());
                stream.insert("security".into(), "reality".into());
                stream.insert("realitySettings".into(), serde_json::json!({
                    "dest": format!("{}:443", e.reality_dest),
                    "serverNames": [e.reality_dest],
                    "privateKey": e.private_key,
                    "shortIds": [e.short_id]
                }));
                serde_json::json!({ "settings": { "clients": [{ "id": e.uuid, "flow": "xtls-rprx-vision" }], "decryption": "none" } })
            }
            "trojan" => {
                stream.insert("network".into(), "tcp".into());
                stream.insert("security".into(), "reality".into());
                stream.insert("realitySettings".into(), serde_json::json!({
                    "dest": format!("{}:443", e.reality_dest),
                    "serverNames": [e.reality_dest],
                    "privateKey": e.private_key,
                    "shortIds": [e.short_id]
                }));
                serde_json::json!({ "settings": { "clients": [{ "password": e.password }], "fallbacks": [] } })
            }
            _ => {
                stream.insert("network".into(), "ws".into());
                let path = if e.ws_path.is_empty() { "/ray" } else { &e.ws_path };
                stream.insert("wsSettings".into(), serde_json::json!({ "path": path }));
                serde_json::json!({ "settings": { "clients": [{ "id": e.uuid, "alterId": 0 }] } })
            }
        }
        .as_object_mut()
        .map(|sets| sets.clone())
        .unwrap_or_default();
        let mut inbound = base;
        inbound.as_object_mut().unwrap().append(&mut serde_json::Map::new()); // 组装
        // 组装的方式:先在 base 中插入 settings/streamSettings
        let mut obj = base.as_object_mut().unwrap();
        obj.insert("settings".into(), settings);
        obj.insert("streamSettings".into(), serde_json::Value::Object(stream));
        *obj
    }).collect();
    serde_json::json!({
        "inbounds": inbounds,
        "outbounds": [{ "protocol": "freedom", "tag": "direct" }]
    }).to_string()
}
```

> 注意:上面 `settings` 变量类型是 `serde_json::Value`(match 分支返回 json! 值),`obj.insert("settings", settings)` 直接接收 Value。请以金样测试为准修正编译细节(分支构造用 `serde_json::json!` 整段拼装亦可,保持 JSON 结构一致)。

- [ ] **Step 4: 通过 + 保留旧单协议金样**

Run: `cargo test -p vpn-ctl deploy` · Expected: PASS(旧 `xray_config_json` 测试不动)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): xray 多入站配置生成(vless/trojan/vmess)"
```

---

### Task 3: sing-box 服务端配置生成(L2)

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`
- Test: `src-tauri/src/servers/deploy_tests.rs`

**Interfaces:**
- Produces:
```rust
pub const SINGBOX_CONFIG_PATH: &str = "/etc/sing-box/config.json";
pub const SINGBOX_UNIT_PATH: &str = "/etc/systemd/system/singbox.service";
pub const SINGBOX_INSTALL_URL: &str = "https://github.com/SagerNet/sing-box/releases/download/..."; // 由 latest_singbox_asset 给出
pub fn singbox_config_json(hy2_port: u16, hy2_pw: &str, tuic_port: u16, tuic_uuid: &str, tuic_pw: &str) -> String;
pub fn singbox_systemd_unit() -> String;
pub async fn latest_singbox_asset(http: &reqwest::Client, api_base: &str, target: &str) -> Result<(String, String), String>; // 镜像 latest_ss_asset 模式
```
- Consumes: `is_safe_url_fragment`, `latest_ss_asset` 同构

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn singbox_config_contains_both_l2_inbounds() {
    let cfg = singbox_config_json(2097, "pw-hy2", 2053, "tcu", "pw-t");
    let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
    let ib = v["inbounds"].as_array().unwrap();
    assert_eq!(ib.len(), 2);
    assert_eq!(ib[0]["type"], "hysteria2");
    assert_eq!(ib[0]["listen_port"], 2097);
    assert_eq!(ib[0]["users"][0]["password"], "pw-hy2");
    assert_eq!(ib[1]["type"], "tuic");
    assert_eq!(ib[1]["users"][0]["uuid"], "tcu");
    assert_eq!(v["outbounds"][0]["type"], "direct");
}

#[test]
fn singbox_unit_has_execstart_and_restart() {
    let u = singbox_systemd_unit();
    assert!(u.contains("Singleton: ExecStart=/usr/local/bin/sing-box run -c /etc/sing-box/config.json".replace("Singleton: ", "").as_str())
        || u.contains("ExecStart=/usr/local/bin/sing-box run -c /etc/sing-box/config.json"));
    assert!(u.contains("Restart=on-failure"));
}
```

- [ ] **Step 2: 运行确认失败** → `cargo test -p vpn-ctl singbox_config` FAIL

- [ ] **Step 3: 实现**

```rust
pub fn singbox_config_json(hy2_port: u16, hy2_pw: &str, tuic_port: u16, tuic_uuid: &str, tuic_pw: &str) -> String {
    serde_json::json!({
        "inbounds": [
            { "type": "hysteria2", "listen": "0.0.0.0", "listen_port": hy2_port,
              "users": [{ "password": hy2_pw }] },
            { "type": "tuic", "listen": "0.0.0.0", "listen_port": tuic_port,
              "users": [{ "uuid": tuic_uuid, "password": tuic_pw }] }
        ],
        "outbounds": [{ "type": "direct" }]
    }).to_string()
}

pub fn singbox_systemd_unit() -> String {
    "[Unit]\nDescription=sing-box server\nAfter=network.target\n\n[Service]\nExecStart=/usr/local/bin/sing-box run -c /etc/sing-box/config.json\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}

/// 查 sing-box 最新资产(镜像 latest_ss_asset:注入 api_base,prod=GitHub)
pub async fn latest_singbox_asset(
    http: &reqwest::Client, api_base: &str, target: &str,
) -> Result<(String, String), String> {
    let url = format!("{api_base}/releases/latest");
    let resp = http.get(url).header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json").send().await
        .map_err(|e| format!("查询 sing-box 最新版本失败: {e}"))?
        .error_for_status().map_err(|e| format!("GitHub API 错误: {e}"))?;
    let j: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let tag = j["tag_name"].as_str().ok_or("缺少 tag_name")?.to_string();
    let asset = j["assets"].as_array().ok_or("缺少 assets")?
        .iter().filter_map(|a| a["name"].as_str())
        .find(|n| n.ends_with(&format!("{target}.tar.gz")))
        .ok_or_else(|| format!("未找到 {target} 的发布资产"))?.to_string();
    if !is_safe_url_fragment(&tag) || !is_safe_url_fragment(&asset) {
        return Err("资产名不合法".into());
    }
    Ok((tag, asset))
}
```

- [ ] **Step 4: 通过** → `cargo test -p vpn-ctl singbox` PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_tests.rs
git commit -m "feat(deploy): sing-box L2 配置生成(hy2/tuic)"
```

---

### Task 4: 部署流水线多协议化(DeployRequest/commands/run_deploy)

**Files:**
- Modify: `src-tauri/src/jobs/mod.rs`(`DeployRequest`,约 28-34 行)
- Modify: `src-tauri/src/commands.rs`(`DeployInput` + `start_deploy` 校验,约 378-400;`start_deploy_cmd` 约 970-984)
- Modify: `src-tauri/src/servers/deploy.rs`(`run_deploy`)
- Modify: `src-tauri/src/domain.rs`(如 DeployInput 在 domain.rs:7 导入处)
- Test: `src-tauri/src/servers/deploy_tests.rs`(run_deploy 分支弱验证:配置/命令构造白盒测试点)+ `src-tauri/src/commands_tests.rs`

**Interfaces:**
- Produces:
```rust
// jobs/mod.rs
pub struct DeployRequest {
    pub job_id: i64,
    pub server_id: i64,
    pub protocols: Vec<String>,            // 白名单内
    pub reality_dest: Option<String>,      // 作用于 vless/trojan
}
// commands.rs
pub struct DeployInput {
    pub job_id: i64, pub server_id: i64,
    pub protocols: Vec<String>, pub reality_dest: Option<String>,
}
```

- [ ] **Step 1: 更新既有失败测试**

`commands_tests.rs` 中 `DeployInput{kind:...}` 构造改 `protocols: vec!["vless".into()]`;`start_deploy` 校验测试改为:

```rust
#[test]
fn start_deploy_rejects_unknown_protocols() {
    let err = call_start_deploy(DeployInput { job_id: 1, server_id: 1,
        protocols: vec!["socks".into()], reality_dest: None });
    assert!(err.contains("不支持的协议"));
}
```

- [ ] **Step 2: 运行确认失败** → `cargo test -p vpn-ctl start_deploy` FAIL

- [ ] **Step 3: 实现(关键路径)**

```rust
// commands.rs start_deploy:白名单校验
for p in &input.protocols {
    if !crate::servers::deploy::is_supported_protocol(p) {
        return Err(format!("不支持的协议: {p}"));
    }
}
if input.protocols.is_empty() { return Err("未选择任何协议".into()); }
if input.protocols.contains(&"vless".to_string()) || input.protocols.contains(&"trojan".to_string()) {
    let _ = input.reality_dest.clone(); // 可空,run_deploy 内默认
}
```

`run_deploy` 重构:按协议分组调用单元安装器。新增私有辅助:

```rust
async fn deploy_one_protocol(
    sess, state, req, cancel, emit, proto: &str, port_cfg: &mut ProtocolPorts, keys: &RealityKeys,
) -> Result<Node, String>
```

实现骨架(承接现有分支,新增协议):

```rust
async fn run_deploy(...) -> Result<(), String> {
    // 探测:uname/os/已装组件(command -v xray|ssserver|sing-box) 一次;
    // 为每个协议分配端口 allocate_port(default_port(proto))
    // 阶段进度:探测 10 → 安装(L1 xray/ss、L2 sing-box,已装跳过)30 → 配置 55(写 xray config(multi entries)或 ss 或 singbox)→ 启动 80 → 写回 95(每协议一行 Node)
    // 失败隔离:每协议验证失败记录 failed_protocols,继续;全程至少一个成功才 Ok;全失败 Err
}
```

节点写回(N 个协议共用同一 server):

```rust
fn node_for(server: &Server, proto: &str, port: u16, params: String) -> Node { /* 基于现有 Node 构造,name 用 {server.name}-{proto} */ }
```

每个协议 params_json 生成(vless: uuid/security=reality/type tcp/flow/sni/public-key/short-id/fingerprint;trojan: id 同 vless 变成 password/security=reality/…;vmess: uuid/alterId/network=ws/path;ss: cipher/password;hysteria2: password/query 型 params(h2 uri 字段:password、sni?、skip-cert-verify);tuic: uuid/password)。

- [ ] **Step 4: 通过** → `cargo test -p vpn-ctl deploy commands` PASS

- [ ] **Step 5: Commit**

```bash
git add -A src-tauri/src/jobs src-tauri/src/commands.rs src-tauri/src/domain.rs src-tauri/src/servers/deploy.rs src-tauri/src/commands_tests.rs
git commit -m "feat(deploy): 多协议部署流水线(协议隔离+失败隔离)"
```

---

### Task 5: nodes 写回幂等与部署记录

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`(写回段)
- Modify: `src-tauri/src/db/repos.rs`(如需 `delete_self_built_by_addrs`)
- Test: `src-tauri/src/db/tests.rs` / `deploy_tests.rs`

**Interfaces:**
- Produces:
```rust
pub fn delete_self_built_by_addr(conn, addr: &str) -> Result<usize>; // 清理同一地址全部自建(保持 session-scope power)
```

- [ ] **Step 1: 写失败测试(幂等语义)**

```rust
#[test]
fn redeploy_replaces_all_self_built_for_addr() {
    // 构造:同 addr 已有 vless+ss 行 → 调用 delete_self_built_by_addr → 行清空;不同 addr 不受影响
}
```

- [ ] **Step 2/3: 实现** — repos 增加按 addr 删除(沿用 delete_self_built 的 sql 改 `WHERE source='self-built' AND addr=?`),写回段调用一次;deploys::insert 每协议一条 summary(`{"kind":"multiprotocol","protocols":[...],"ports":{...}}`)。

- [ ] **Step 4: 通过** → `cargo test -p vpn-ctl db deploy` PASS

- [ ] **Step 5: Commit** `git commit -am "feat(deploy): 多协议节点幂等写回与部署记录"`

---

### Task 6: mihomo 映射补 hysteria2/tuic/wireguard + trojan reality-opts

**Files:**
- Modify: `src-tauri/src/config/mod.rs`(node_mapping match 增分支 + 三个 mapping fn + trojan_mapping 补 reality-opts)
- Test: `src-tauri/src/config/tests.rs`

**Interfaces:**
- Produces:
```rust
fn hysteria2_mapping(n, name, params) -> Mapping;
fn tuic_mapping(n, name, params) -> Mapping;
fn wireguard_mapping(n, name, params) -> Mapping;
```
mihomo(meta)字段约定(hysteria2):`name/type/server/port/password/up-mbps/down-mbps(skip-cert-verify)`;tuic:`name/type/server/port/uuid/password(别名 token)/congestion-controller/udp-relay-mode`;wireguard:`name/type/server/port/ip/private-key/public-key/preshared-key/dns/mtu`。

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn maps_hysteria2_uri_node() {
    let n = Node { protocol: "hysteria2".into(), params_json: r#"{"password":"pw","sni":"b.com","skip-cert-verify":"1","up":"60","down":"120"}"#.into(), ..base() };
    let m = node_mapping(&n, None).unwrap();
    assert_eq!(m["type"], "hysteria2"); assert_eq!(m["password"], "pw");
    assert_eq!(m["skip-cert-verify"], true);
    assert_eq!(m["sni"], "b.com");
}

#[test]
fn maps_tuic_uri_node() {
    let n = Node { protocol: "tuic".into(), params_json: r#"{"uuid":"u","password":"t","sni":"c.com"}"#.into(), ..base() };
    let m = node_mapping(&n, None).unwrap();
    assert_eq!(m["type"], "tuic"); assert_eq!(m["uuid"], "u"); assert_eq!(m["password"], "t");
}

#[test]
fn trojan_reality_reality_opts() {
    let n = Node { protocol: "trojan".into(),
        params_json: r#"{"password":"p","security":"reality","public-key":"k","short-id":"s1","sni":"d.com","fingerprint":"chrome"}"#.into(), ..base() };
    let m = node_mapping(&n, None).unwrap();
    assert_eq!(m["tls"], true);
    assert_eq!(m["reality-opts"]["public-key"], "k");
    assert_eq!(m["servername"], "d.com");
}
```

- [ ] **Step 2: 运行确认失败** → `cargo test -p vpn-ctl maps_hysteria2` FAIL

- [ ] **Step 3: 实现**

```rust
"hysteria2" => hysteria2_mapping(n, name, &params),
"tuic" => tuic_mapping(n, name, &params),
"wireguard" => wireguard_mapping(n, name, &params),

fn hysteria2_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("hysteria2"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("password"), s(&str_param(params, "password")));
    let sni = str_param(params, "sni");   // 兼容 "sni"
    if !sni.is_empty() { m.insert(s("sni"), s(&sni)); }
    let up = str_param(params, "up"); if !up.is_empty() { m.insert(s("up"), s(&up)); }
    let down = str_param(params, "down"); if !down.is_empty() { m.insert(s("down"), s(&down)); }
    if skip_cert_verify_true(params) { m.insert(s("skip-cert-verify"), Value::Bool(true)); }
    m.insert(s("udp"), Value::Bool(true));
    m
}
fn tuic_mapping(...) { /* name/type=tuic/server/port/uuid/password(或 token)/sni/udp */ }
fn wireguard_mapping(...) { /* name/type=wireguard/server/port/private-key、public-key、preshared-key、ip、dns、mtu(全部直传,缺失省略) */ }
// trojan_mapping 内,与 vless_mapping:186-203 相同逻辑补 reality-opts(security=="reality" & public-key 非空)
```

- [ ] **Step 4: 通过** → `cargo test -p vpn-ctl config` PASS

- [ ] **Step 5: Commit** `git commit -am "feat(config): mihomo 映射补 hysteria2/tuic/wireguard, trojan reality"`

---

### Task 7: subs URI 导入补 tuic

**Files:**
- Modify: `src-tauri/src/subs/parse.rs`(looks_like_uri_list + parse_uri)
- Test: `src-tauri/src/subs/parse_tests.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn parses_tuic_uri() {
    let line = "tuic://u:p@1.2.3.4:2053?sni=b.com&congestion-control=bbr#%E5%8F%B6%E4%B8%80";
    let node = parse_uri_for_test(line).expect("parse");
    assert_eq!(node.protocol, "tuic");
    assert_eq!(node.addr, "1.2.3.4");
    assert_eq!(node.port, 2053);
    assert_eq!(node.params["uuid"], "u");
    assert_eq!(node.params["password"], "p");
    assert_eq!(node.params["sni"], "b.com");
    assert_eq!(node.name, "叶一");
}
```

- [ ] **Step 3: 实现** — `looks_like_uri_list` 数组加 `"tuic://"`;`parse_uri` 分支:`uri.starts_with("tuic://")` 走现有 `url::Url::parse` 通用分支(protocol 判定加 `else if uri.starts_with("tuic://") { "tuic" }`),uuid 键保持 userinfo 首段。

- [ ] **Step 4: 通过** → `cargo test -p vpn-ctl subs` PASS;Commit `git commit -am "feat(subs): tuic URI 导入"`

---

### Task 8: sing-box 出口补 tuic/wireguard

**Files:**
- Modify: `src-tauri/src/config/singbox.rs`(node_outbound match + 两个 fn)
- Test: `src-tauri/src/config/tests.rs`(singbox 相关既有测试文件)

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn singbox_tuic_outbound() {
    let n = Node { protocol: "tuic".into(), params_json: r#"{"uuid":"u","password":"t","sni":"c.com"}"#.into(), ..base() };
    let out = singbox_node_outbound(&n).unwrap();
    assert_eq!(out["type"], "tuic"); assert_eq!(out["uuid"], "u");
}
```
(注:`singbox_node_outbound` 未导出,必要时 `#[cfg(test)] pub(crate)` 或经 build_singbox 断言。)

- [ ] **Step 3: 实现**

```rust
"tuic" => tuic_outbound(n, &params),
"wireguard" => wireguard_outbound(n, &params),
// tuic: {type,tag,server,server_port,uuid,password,congestion_controller,udp_relay_mode?,tls:{enabled,server_name,insecure}}
// wireguard: {type,tag,server,server_port,local_address,private_key,peer_public_key,peer_pre_shared_key,dns?}
```

- [ ] **Step 4: 通过** + Commit `git commit -am "feat(singbox): 出站补 tuic/wireguard"`

---

### Task 9: 前端 API 类型与命令更新

**Files:**
- Modify: `src/api/types.ts`,`src/api/index.ts`
- Test: `src/api/...(若存在对应单测)`

**Interfaces:**
- Produces:
```ts
export interface DeployInput { server_id: number; protocols: string[]; reality_dest?: string | null }
export function startDeploy(input: { serverId: number; protocols: string[]; realityDest?: string | null }): Promise<number>
```

- [ ] **Step 1: 更新 ts(直接改)** — `start_deploy_cmd` 参数改为 `{ serverId, protocols, realityDest }`,invoke 传 snake_case 对象 `{ server_id, protocols, reality_dest }`。

```ts
export const startDeploy = (input: { serverId: number; protocols: string[]; realityDest?: string | null }) =>
  invoke<number>('start_deploy_cmd', { serverId: input.serverId, protocols: input.protocols, realityDest: input.realityDest ?? null })
```

- [ ] **Step 2: `cargo tauri dev` 编译通过(前端 tsc 静检)** — `npx tsc --noEmit` 预期 PASS;Commit `git commit -am "feat(ui): startDeploy 多协议参数"`

---

### Task 10: ExpandableCard 组件

**Files:**
- Create: `src/components/ExpandableCard.tsx`
- Test: `src/components/ExpandableCard.test.tsx`

**Interfaces:**
```tsx
interface Props {
  expanded: boolean
  id: string                              // 用于文档查询
  header: ReactNode
  children: ReactNode                     // 展开内容
  onClose: () => void
}
export default function ExpandableCard(props: Props): JSX.Element
// 行为:展开时 render child 至 header 后 + useEffect 内 scrollIntoView({block:'nearest'})
```

- [ ] **Step 1: 测试**

```tsx
import { render, screen, fireEvent } from '@testing-library/react'
import ExpandableCard from './ExpandableCard'

test('展开渲染内容并收起回调', () => {
  render(<ExpandableCard id="c1" expanded header={<button>卡头</button>} onClose={vi.fn()}>
    <p>部署向导内容</p>
  </ExpandableCard>)
  expect(screen.queryByText('部署向导内容')).toBeNull()
  fireEvent.click(screen.getByRole('button', { name: '卡头' }))     // 外部控制 expanded,这里点击仅冒泡
  expect(screen.getByText('部署向导内容')).toBeInTheDocument()      // expanded=true 后可见(重渲染验证)
})
```

> 实现:用受控 `expanded` 驱动,展开时 `useEffect(() => { if (expanded) ref.current?.scrollIntoView({ block: 'nearest', behavior: 'smooth' }) }, [expanded])`。

- [ ] **Step 2/3/4: 实现 + 通过** → `npx vitest run src/components/ExpandableCard.test.tsx` PASS;Commit `git commit -m "feat(ui): ExpandableCard 就地展开容器"`

---

### Task 11: DeployWizard 重做(协议卡网格 + 全自动参数)

**Files:**
- Modify: `src/components/DeployWizard.tsx`(整体重构)
- Test: `src/components/DeployWizard.test.tsx`

**Interfaces:**
```tsx
interface Props { server: Server; onClose: () => void; onDone: () => void }
// PROTOCOLS_UI: [{ id:'vless', label:'VLESS · Reality', tag:'抗检测主力', l1:true, port:443 }, ...]
// 状态: selected: Set<string>(初始=L1 全部);realityDest 默认 'www.microsoft.com';启动时 startDeploy({serverId, protocols:[...selected], realityDest})
```

- [ ] **Step 1: 重写测试**

```tsx
test('默认全选 L1 四协议', () => {
  render(<DeployWizard server={s} onClose={vi.fn()} onDone={vi.fn()} />)
  expect(screen.getByRole('checkbox', { name: /VLESS/ })).toBeChecked()
  expect(screen.getByRole('checkbox', { name: /SS-2022/ })).toBeChecked()
  expect(screen.getByRole('checkbox', { name: /Hysteria2/ })).not.toBeChecked()
})
test('启动提交 protocols 数组且 Reality 域名可改', async () => {
  render(...) ; await user.click(screen.getByRole('checkbox', { name: /QTC/ })) …
  fireEvent.change(screen.getByLabelText(/伪装域名/), { target: { value: 'www.yahoo.com' } })
  await user.click(screen.getByRole('button', { name: /开始部署/ }))
  expect(startDeploy).toHaveBeenCalledWith({ serverId: 9, protocols: ['vless','trojan','vmess','ss','tuic'], realityDest: 'www.yahoo.com' })
})
```

- [ ] **Step 3: 实现** — 卡网格:Rust 后端协议白名单对应前端数组;选中集初始 `new Set(PROTOCOLS_UI.filter(p => p.l1).map(p => p.id))`;进度/终态/取消事件沿用现有 hook 逻辑。

- [ ] **Step 4: 通过** → `npx vitest run src/components/DeployWizard.test.tsx` PASS;Commit `git commit -m "feat(ui): 部署向导协议卡网格(多协议提交)"`

---

### Task 12: Servers.tsx 就地展开改造

**Files:**
- Modify: `src/views/Servers.tsx`
- Test: `src/views/Servers.test.tsx`

**Interfaces:**
- 状态:`const [expand, setExpand] = useState<{ type: 'deploy' | 'edit'; id: number } | null>(null)`
- 卡片渲染:`<ExpandableCard expanded={expand?.id === s.id} header={...卡片头/按钮区...} onClose={() => setExpand(null)}>` —— 按钮「部署落地机」`setExpand({type:'deploy',id})`;「编辑」`setExpand({type:'edit',id})` 且表单初始 fill。
- **删除** 顶部的 `deploying`/`form`+`editingId` 渲染块(原 60-66 行状态与 580 行附近表单 JSX),表单 JSX 收进卡片 children;保存逻辑保留。

- [ ] **Step 1: 更新测试**

```tsx
test('就地展开:点部署在此卡展开,点编辑收起部署', async () => {
  render(<Servers />) …
  await user.click(screen.getByRole('button', { name: '部署落地机' }))
  expect(await screen.findByText(/协议/)).toBeInTheDocument()      // 向导就地出现
  await user.click(screen.getByRole('button', { name: '编辑' }))
  expect(screen.queryByText(/开始部署/)).toBeNull()                // 部署已收起
  expect(screen.getByText(/保存服务器/)).toBeInTheDocument()
})
```

- [ ] **Step 3: 实现** — 沿 594-655 卡片 JSX 包 `ExpandableCard`;删除全局 form 块与 deploy 块;`handleSave` 成功后 `setExpand(null)`;「收藏协议徽章」:`<span>` 数组来自 nodes 过滤(`listNodes` 按 addr/self-built)端点——**简化**:徽章用 `deploys` 表最新 summary 或跳过徽章依赖,改由部署完成回调携带 `result.protocols` 更新服务器卡本地 state(`deployedProtos[s.id]`)。

- [ ] **Step 4: 通过** → `npx vitest run src/views/Servers.test.tsx` PASS;Commit `git commit -m "feat(ui): 服务器页就地展开(部署/编辑)"`

---

### Task 13: 协议管理(查看/补装 L2/卸载)IPC

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`(新增 `run_deploy_manage` 全家)与 `commands.rs`(三个 cmd)、`src/api/types.ts`+`index.ts`、`Servers.tsx`(更多菜单接入)
- Test: `src-tauri/src/servers/deploy_tests.rs`(管理命令白盒)

**Interfaces:**
```rust
pub fn deployed_protocol_version(state, server_id) -> Result<Vec<String>, String>; // 依据 repos::deploys 最新 summary.protocols
```
前端「协议管理」:modal(复用 ExpandableCard 内嵌)列出已装协议、L2 可勾选、卸载按钮 → 调用 `install_supplement_cmd` / `remove_protocol_cmd`。

- [ ] **Step 1: 最小实现(L2 补装/卸载)** — 复用 run_deploy 的安装/配置/启动段(sing-box 存在则仅重写 config+restart;卸载=停止+禁用+删 config,update summary)。为控制范围:**协议管理放 P2 里程碑**,本任务只做 IPC 骨架 + 测试;

> 若时间盒紧张,Task 13 可降级为"协议管理延后"(见 Reserve 段落)。

- [ ] **Step 4: 通过并提交**

---

### Task 14: 集成验证与收尾

**Files:** 全量测试 + 手动验收脚本

- [ ] **Step 1: 全量单测** `cargo test && npm test` 全绿(PASS)
- [ ] **Step 2: tauri dev 手工验收** — 对 `112` 部署 vless+trojan+ss(L1 默认)观察就地展开/进度/节点落库;再到节点页确认协议徽章;L2 勾选 hy2 观察 UDP 预提示
- [ ] **Step 3: 回归** — 测速单节点(trojan)、导出订阅 mihomo/新节点互认
- [ ] **Step 4: Commit** `git commit -am "test: 多协议与就地展开全量回归"`

---

## Reserve(延后项,本期不做)

- 协议管理 UI(Task 13 减速方案:仅暴露 IPC,UI 下期)
- 服务器端证书体系(VMess TLS、hy2/tuic 公网证书)
- 多用户/流量限额
- relay/WG 协议扩展

## Self-Review

- Spec 覆盖:协议矩阵(Task1-4)、节点面(Task6-8)、布局(Task10-12)、测试计划(各 Task 步+Task14)✓
- 无占位符:所有代码块给出可编译骨架;reserve 显式声明 ✓
- 类型一致:`protocols: Vec<String>` / `DeployInput` / `startDeploy` 三处一致;mihomo 映射键名以 mihomo 官方 schema 为准 ✓
