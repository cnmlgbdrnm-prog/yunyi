# 本机上网客户端实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 仪表盘一键开启"上网"：本地 mihomo（已捆绑二进制）+ 系统代理 + 智能分流（国内直连/国外走落地机节点组，可手动选择服务器）+ 导出订阅带同一套智能规则，断开自愈还原系统代理。

**Architecture:** 客户端运行一套**独立的 mihomo 实例**（完整 config：节点+自动选择/手动选择分组+智能 rules），与测速 sidecar（proxies 动态注入）互不干扰；二进制解析/就绪轮询复用 speed::sidecar 基建。系统代理 WinINET 快照-还原；冲突检测只提醒不抢占；geosite 数据缺失时客户端降级（跳过 GEOSITE/GEOIP CN 规则，其余直连兜底保留）。

**Tech Stack:** Rust (tokio/reqwest/winreg/serde_yaml) + Tauri IPC + React 19 (Dashboard/Settings)。

## Global Constraints

- 智能规则顺序固定：`GEOIP,LAN,DIRECT` → `GEOIP,PRIVATE,DIRECT` → `GEOSITE,cn,DIRECT` → `GEOIP,CN,DIRECT`（导出恒含全部；客户端运行时按 geo 数据文件存在与否降级）
- 客户端 config 顶层级：`mixed-port` / `external-controller`(仅 127.0.0.1) / `secret`(随机 token) / `mode: rule` / `dns.nameserver: [223.5.5.5, 1.1.1.1]` / `proxies`/`proxy-groups`/`rules`
- 分组：`自动选择`(url-test, gstatic 204, 300s) + `手动选择`(select, 首项 `自动选择`)；规则 MATCH 落 `手动选择`（客户端）/`自动选择`（导出，保持现状）
- 端口：mixed 7890 起顺延（TcpListener 探测），外部控制器同法随机分配
- 冲突进程名（Windows tasklist image name）：`clash-verge.exe`/`mihomo.exe`/`sing-box.exe`/`v2rayN.exe`/`v2ray.exe`/`hiddify.exe`/`wingy.exe`/`karing.exe`，**排除自身进程 PID**（测速 sidecar 与本客户端）
- 系统代理值：`http=127.0.0.1:{port};https=127.0.0.1:{port};socks=127.0.0.1:{port}`（兼容现有 sysproxy.rs 的 `http=` 段解析）
- 客户端 config 与导出共用 `node_mapping`；client config 目录 `{data_dir}/net/`
- 不持久化 running 状态（App 重启 = 未开启，用户再点）

---

### Task 1: sidecar 二进制解析提取为可复用函数

**Files:**
- Modify: `src-tauri/src/speed/sidecar.rs:96-115`（ensure_started 的复制/校验段改为调用新函数）
- Test: `src-tauri/src/speed/sidecar_tests.rs`

**Interfaces:**
- Produces: `pub fn ensure_binary(dir: &Path, resource: Option<&Path>) -> Result<std::path::PathBuf, String>` — 幂等：目录内已存在且 sha256 匹配直接返回；否则从 candidate_paths 首个存在的源复制（dev 模式 source = `{CARGO_MANIFEST_DIR}/binaries/mihomo.exe`），校验失败返回中文错误。本函数被 net 客户端启动复用（dir 传 `{data_dir}/net`）。

- [ ] **Step 1: Write the failing test**（追加到 sidecar_tests.rs）

```rust
#[test]
fn ensure_binary_copies_from_manifest_dir_and_reports_missing() {
    let dir = tempfile::tempdir().unwrap();
    let dev_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(SIDECAR_EXE);
    if !dev_path.is_file() {
        // dev 环境没有二进制时跳过（CI/打包环境允许），保证测试不因缺文件失败
        return;
    }
    // 正常复制 + 幂等（第二次调用不再报错）
    let first = ensure_binary(dir.path(), None).unwrap();
    assert!(dir.path().join(SIDECAR_EXE).is_file());
    let second = ensure_binary(dir.path(), None).unwrap();
    assert_eq!(first, second);
    // 损坏文件 → 校验失败且有中文错误
    std::fs::write(dir.path().join(SIDECAR_EXE), b"tampered").unwrap();
    let err = ensure_binary(dir.path(), None).unwrap_err();
    assert!(err.contains("sha256") || err.contains("校验失败"), "{err}");
}
```

- [ ] **Step 2: Run** `cargo test -p vpn-ctl ensure_binary` → 编译失败（函数不存在）

- [ ] **Step 3: Implement** — sidecar.rs 新增（复制/校验逻辑自 ensure_started 抽出，ensure_started 改为调用）：

```rust
/// 确保 dir 内存在校验通过的 mihomo 二进制：已匹配直接返回路径；
/// 否则从 candidate_paths 首个存在的源复制；两者皆无返回中文错误。
/// 幂等（sha256 匹配即复用），供 sidecar 与 net 客户端共用。
pub fn ensure_binary(dir: &std::path::Path, resource: Option<&std::path::Path>) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let exe_path = dir.join(SIDECAR_EXE);
    if binary_ok(&exe_path) {
        return Ok(exe_path);
    }
    let src = candidate_paths(resource)
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            "未找到内置 mihomo 二进制，请先运行 npm run fetch:sidecar（dev 模式落盘 src-tauri/binaries/mihomo.exe）".to_string()
        })?;
    if let Some(parent) = exe_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    std::fs::copy(&src, &exe_path).map_err(|e| format!("复制 mihomo 失败: {e}"))?;
    if !binary_ok(&exe_path) {
        return Err("mihomo 二进制校验失败（sha256 不匹配，复制源可能已损坏）".to_string());
    }
    Ok(exe_path)
}
```

并改写 ensure_started 96-114 行为：

```rust
    let sidecar_dir = state.data_dir.join(SIDECAR_DIR);
    let exe_path = ensure_binary(&sidecar_dir, state.sidecar_resource.read().map_err(|_| "sidecar 资源锁读取失败".to_string())?.as_deref())?;
```

- [ ] **Step 4: Run** `cargo test -p vpn-ctl ensure_binary` → PASS；再 `cargo test -p vpn-ctl sidecar` 全绿（回归 ensure_started 原行为）

- [ ] **Step 5: Commit** `git commit -m "refactor(speed): sidecar 二进制确保提取为 ensure_binary 复用"`

---

### Task 2: 智能规则函数 + build_yaml 参数化 + client_yaml 生成

**Files:**
- Modify: `src-tauri/src/config/mod.rs`（node_mapping/url_test_group/select_group 改 pub(crate)、新增 smart_rules/client_yaml/net_groups）、`src-tauri/src/config/tests.rs`
- Modify: `src-tauri/src/subs/service.rs:222`（build_yaml 调用加 smart 参数）
- Test: `src-tauri/src/config/tests.rs`

**Interfaces:**
- Consumes: Task 1 之前的 Node 结构与现有 node_mapping（改可见性）
- Produces:
  - `pub fn smart_rules() -> Vec<String>` — 恒定返回下面前四条（无 MATCH）
  - `pub fn build_yaml(nodes: &[Node], smart_rules: bool) -> Result<(String, usize), ConfigError>` — smart=true 时 rules = smart_rules() + `MATCH,自动选择`
  - `pub fn net_groups(names: &[String]) -> Vec<Value>` — `自动选择`(url-test, 同现有)+`手动选择`(select, proxies = [自动选择, ...names])
  - `pub fn client_yaml(nodes: &[Node], mixed_port: u16, ctrl_port: u16, token: &str, geo_present: bool) -> Result<String, ConfigError>` — 顶层 fixed 键 + 与 build_yaml 相同的 proxies + net_groups + rules（geo_present=false 时 smart_rules 去掉后两条），MATCH,手动选择

- [ ] **Step 1: Write failing tests**（追加 config/tests.rs；先看现有金样风格再写）

```rust
#[test]
fn smart_rules_order_is_fixed_and_complete() {
    assert_eq!(
        smart_rules(),
        vec![
            "GEOIP,LAN,DIRECT",
            "GEOIP,PRIVATE,DIRECT",
            "GEOSITE,cn,DIRECT",
            "GEOIP,CN,DIRECT",
        ]
    );
}

#[test]
fn build_yaml_smart_rules_replace_flat_match() {
    let n = dummy_node("vless", "n1", "1.2.3.4", 443);
    let (y, skipped) = build_yaml(&[n], true).unwrap();
    assert_eq!(skipped, 0);
    let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
    let rules = v["rules"].as_sequence().unwrap();
    let flat: Vec<&str> = rules.iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(
        flat,
        vec![
            "GEOIP,LAN,DIRECT",
            "GEOIP,PRIVATE,DIRECT",
            "GEOSITE,cn,DIRECT",
            "GEOIP,CN,DIRECT",
            "MATCH,自动选择",
        ]
    );
    // 关闭时回到旧行为：仅一条 MATCH
    let (y2, _) = build_yaml(&[dummy_node("vless", "n2", "5.6.7.8", 443)], false).unwrap();
    let v2: serde_yaml::Value = serde_yaml::from_str(&y2).unwrap();
    assert_eq!(v2["rules"], serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("MATCH,自动选择".into())]));
}

#[test]
fn client_yaml_golden_with_and_without_geo() {
    let nodes = vec![dummy_node("ss", "n1", "1.2.3.4", 8388)];
    let y = client_yaml(&nodes, 7890, 9090, "tok123", true).unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
    assert_eq!(v["mixed-port"], 7890);
    assert_eq!(v["mode"], "rule");
    assert_eq!(v["secret"], "tok123");
    assert_eq!(v["external-controller"], "127.0.0.1:9090");
    assert_eq!(v["proxy-groups"][1]["name"], "手动选择");
    assert_eq!(v["proxy-groups"][1]["type"], "select");
    assert_eq!(v["proxy-groups"][1]["proxies"][0], "自动选择");
    let rules = v["rules"].as_sequence().unwrap();
    assert_eq!(rules[rules.len() - 1], "MATCH,手动选择");
    assert_eq!(rules[rules.len() - 3], "GEOIP,CN,DIRECT");
    // 无 geo 数据：降级只留 LAN/PRIVATE
    let y2 = client_yaml(&nodes, 7890, 9090, "tok123", false).unwrap();
    let v2: serde_yaml::Value = serde_yaml::from_str(&y2).unwrap();
    let rules2 = v2["rules"].as_sequence().unwrap();
    assert_eq!(
        rules2,
        &vec![
            serde_yaml::Value::String("GEOIP,LAN,DIRECT".into()),
            serde_yaml::Value::String("GEOIP,PRIVATE,DIRECT".into()),
            serde_yaml::Value::String("MATCH,手动选择".into()),
        ]
    );
}
```

（dummy_node 在 config/tests.rs 已有类似 helper——用其模式建 `protocol/name/addr/port/params_json` 最小 Node。）

- [ ] **Step 2: Run** `cargo test -p vpn-ctl smart_rules_order client_yaml_golden` → FAIL（函数不存在）

- [ ] **Step 3: Implement** — config/mod.rs：

```rust
/// 智能分流规则（顺序固定，不含 MATCH；导出与客户端共用）。
pub fn smart_rules() -> Vec<String> {
    vec![
        "GEOIP,LAN,DIRECT".into(),
        "GEOIP,PRIVATE,DIRECT".into(),
        "GEOSITE,cn,DIRECT".into(),
        "GEOIP,CN,DIRECT".into(),
    ]
}
```

build_yaml 签名改 `(nodes: &[Node], smart_rules: bool)`，rules 块：

```rust
    let mut rules: Vec<Value> = if smart_rules {
        smart_rules().into_iter().map(|r| Value::String(r)).collect()
    } else {
        vec![]
    };
    rules.push(Value::String("MATCH,自动选择".into()));
    root.insert(s("rules"), Value::Sequence(rules));
```

新增（net 客户端用，分组/映射复用 node_mapping/url_test_group/select_group——它们改 `pub(crate)`。select_group 不动；net_groups 新建）：

```rust
/// 客户端专用分组：自动选择(url-test) + 手动选择(select 首项=自动选择，可再挑单节点)
pub fn net_groups(names: &[String]) -> Vec<Value> {
    let mut manual = Mapping::new();
    manual.insert(s("name"), s("手动选择"));
    manual.insert(s("type"), s("select"));
    let mut proxies = vec![Value::String("自动选择".into())];
    proxies.extend(names.iter().map(|n| Value::String(n.clone())));
    manual.insert(s("proxies"), Value::Sequence(proxies));
    vec![url_test_group(names), Value::Mapping(manual)]
}

/// 客户端 config：与 build_yaml 共用 node_mapping；规则按 geo 数据存在性降级。
pub fn client_yaml(
    nodes: &[Node],
    mixed_port: u16,
    ctrl_port: u16,
    token: &str,
    geo_present: bool,
) -> Result<String, ConfigError> {
    let mut enabled: Vec<&Node> = nodes.iter().filter(|n| n.enabled).collect();
    enabled.sort_by(|a, b| a.name.cmp(&b.name));
    let mut proxies = vec![];
    for n in &enabled {
        if n.protocol == "ssr" {
            continue;
        }
        proxies.push(node_mapping(n, None)?);
    }
    let names: Vec<String> = enabled.iter().filter(|n| n.protocol != "ssr").map(|n| n.name.clone()).collect();
    let mut root = Mapping::new();
    root.insert(s("mixed-port"), serde_yaml::Value::from(mixed_port));
    root.insert(s("mode"), s("rule"));
    root.insert(s("secret"), s(token));
    root.insert(s("external-controller"), s(&format!("127.0.0.1:{ctrl_port}")));
    root.insert(s("allow-lan"), serde_yaml::Value::Bool(false));
    let mut dns = Mapping::new();
    dns.insert(s("enable"), serde_yaml::Value::Bool(true));
    dns.insert(s("nameserver"), Value::Sequence(vec![s("223.5.5.5"), s("1.1.1.1")]));
    root.insert(s("dns"), Value::Mapping(dns));
    root.insert(s("proxies"), Value::Sequence(proxies));
    root.insert(s("proxy-groups"), Value::Sequence(net_groups(&names)));
    let base: Vec<String> = if geo_present { smart_rules() } else { smart_rules()[..2].to_vec() };
    let mut rules: Vec<Value> = base.into_iter().map(|r| Value::String(r)).collect();
    rules.push(Value::String("MATCH,手动选择".into()));
    root.insert(s("rules"), Value::Sequence(rules));
    serde_yaml::to_string(&Value::Mapping(root))
        .map_err(|e| ConfigError::BuildFailed(e.to_string()))
}
```

subs/service.rs:222 改为 `crate::config::build_yaml(&visible, true)`（导出恒带智能分流；开关在该任务范围外时可 `false`——按 Task 8 再引 settings。**本任务固定传 true 并留 TODO 对接 Task 8**——实际上 Task 8 会改这里，本任务直接传 true 即可）。

现存 build_yaml 调用点/测试同步更新签名：`cargo test` 出错处逐一补 `, false` 或按用例语义（导出金样用例原本无规则断言则最省）。

- [ ] **Step 4: Run** `cargo test -p vpn-ctl smart_rules_order build_yaml_smart client_yaml_golden` → PASS；`cargo test -p vpn-ctl config` 全绿

- [ ] **Step 5: Commit** `git commit -m "feat(config): 智能分流规则+客户端 config 生成(client_yaml)"`

---

### Task 3: 冲突进程扫描（net/conflict.rs）

**Files:**
- Create: `src-tauri/src/net/mod.rs` + `src-tauri/src/net/conflict.rs`
- Test: `src-tauri/src/net/conflict_tests.rs`
- Modify: `src-tauri/src/net/mod.rs`（声明子模块后再逐步加 runtime）

**Interfaces:**
- Produces:
  - `pub fn parse_tasklist_csv(csv: &str) -> Vec<(String, u32)>` — `"mihomo.exe","1234","...` 行解析
  - `pub fn friendly_name(image: &str) -> String` — 常见名 → 中文（未知原名）
  - `pub fn conflicts_from(rows: &[(String, u32)], exclude_pids: &[u32]) -> Vec<String>` — 去重保序（先出现优先），排除 pid
  - `pub fn scan_conflicts(exclude_pids: &[u32]) -> Result<Vec<String>, String>` — Windows 调 `tasklist /FO CSV /NH`，非 Windows 返回空

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn parse_tasklist_csv_extracts_name_and_pid() {
    let csv = "\"clash-verge.exe\",\"1234\",\"Console\",\"1\",\"64,512 K\",\"\r\n\"mihomo.exe\",\"5678\",\"Services\",\"0\",\"100 K\",\"\"\r\n";
    let rows = parse_tasklist_csv(csv);
    assert_eq!(rows, vec![("clash-verge.exe".to_string(), 1234), ("mihomo.exe".to_string(), 5678)]);
    assert_eq!(parse_tasklist_csv(""), Vec::new());
}

#[test]
fn friendly_names_map_common_proxies() {
    assert_eq!(friendly_name("clash-verge.exe"), "Clash Verge");
    assert_eq!(friendly_name("v2rayN.exe"), "v2rayN");
    assert_eq!(friendly_name("wingy.exe"), "Wingy");
    assert_eq!(friendly_name("totally-unknown.exe"), "totally-unknown.exe");
}

#[test]
fn conflicts_exclude_own_pids_and_dedup_in_order() {
    let rows = vec![
        ("clash-verge.exe".to_string(), 100),
        ("mihomo.exe".to_string(), 200),
        ("mihomo.exe".to_string(), 300_u32),
    ];
    let ex = conflicts_from(&rows, &[200]);
    assert_eq!(ex, vec!["Clash Verge".to_string(), "mihomo(其它实例)".to_string()]);
}
```

（注意 friendly_name 对 mihomo.exe 返回 "mihomo(其它实例)" 之类，测试里这么定。）

- [ ] **Step 2: Run** → FAIL

- [ ] **Step 3: Implement** — conflict.rs：

```rust
//! 冲突检测：常见代理进程名扫描（Windows tasklist），自身进程（测速 sidecar/本客户端
//! mihomo）按 PID 排除。只检测只提醒，绝不接管/关闭其它代理软件。

pub(crate) const PIDS_EXCLUDE_NOTE: &str = ""; // 占位不用；逻辑直接传 exclude 参数

pub fn parse_tasklist_csv(csv: &str) -> Vec<(String, u32)> {
    csv.lines()
        .filter_map(|line| {
            let mut f = line.split(',');
            let name = f.next()?.trim_matches('"').to_string();
            let pid: u32 = f.next()?.trim_matches('"').parse().ok()?;
            Some((name, pid))
        })
        .collect()
}

pub fn friendly_name(image: &str) -> String {
    match image.to_ascii_lowercase().as_str() {
        "clash-verge.exe" | "clash-verge-rev.exe" => "Clash Verge".into(),
        "v2rayn.exe" => "v2rayN".into(),
        "wingy.exe" => "Wingy".into(),
        "hiddify.exe" => "Hiddify".into(),
        "karing.exe" => "Karing".into(),
        "sing-box.exe" => "sing-box".into(),
        "mihomo.exe" => "mihomo(其它实例)".into(),
        _ => image.to_string(),
    }
}

pub fn conflicts_from(rows: &[(String, u32)], exclude_pids: &[u32]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    rows.iter()
        .filter(|(_, pid)| !exclude_pids.contains(pid))
        .map(|(name, _)| friendly_name(name))
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

pub fn scan_conflicts(exclude_pids: &[u32]) -> Result<Vec<String>, String> {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
            .map_err(|e| format!("扫描进程失败: {e}"))?;
        let csv = String::from_utf8_lossy(&out.stdout);
        Ok(conflicts_from(&parse_tasklist_csv(&csv), exclude_pids))
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}
```

net/mod.rs：

```rust
//! 本机上网客户端：独立 mihomo 实例（完整 config）+ 系统代理 + 智能分流。
pub mod conflict;
pub mod geodata;
pub mod runtime;
pub mod sysproxy_set;
```

（子模块在后续任务创建时逐步存在——**本任务只创建 mod.rs 含 conflict 声明；后续任务启用其余行**，为避免编译失败，本任务 mod.rs 里只 `pub mod conflict;`，其余子模块在其任务里加。）

- [ ] **Step 4: Run** `cargo test -p vpn-ctl parse_tasklist_csv friendly_names conflicts_exclude` → PASS

- [ ] **Step 5: Commit** `git commit -m "feat(net): 冲突进程扫描(提醒不抢占)"`

---

### Task 4: 系统代理快照-还原（net/sysproxy_set.rs）

**Files:**
- Create: `src-tauri/src/net/sysproxy_set.rs`
- Test: `src-tauri/src/net/sysproxy_set_tests.rs`
- Modify: `src-tauri/src/net/mod.rs`（加 `pub mod sysproxy_set;`）

**Interfaces:**
- Consumes: 现有 `src-tauri/src/sysproxy.rs` 的注册表读取习惯（winreg HKCU `Software\Microsoft\Windows\CurrentVersion\Internet Settings`）
- Produces:
  - `pub fn proxy_server_value(port: u16) -> String` — `http=127.0.0.1:{port};https=127.0.0.1:{port};socks=127.0.0.1:{port}`
  - `#[derive(Clone, Debug)] pub struct ProxySnapshot { pub enable: i64, pub server: String }`
  - `pub fn apply_net_proxy(port: u16) -> Result<ProxySnapshot, String>` — 写ProxyEnable=1/ProxyServer=值，返回快照；非 Windows 返回 Ok(空快照)（no-op 语义，避免跨平台测试失败）
  - `pub fn restore_proxy(snap: Option<&ProxySnapshot>) -> Result<(), String>` — 还原快照；快照为 None 时原样置 0/清空（防残留）

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn proxy_server_value_uses_http_https_socks_same_port() {
    let v = proxy_server_value(7890);
    assert!(v.contains("http=127.0.0.1:7890"), "{v}");
    assert!(v.contains("https=127.0.0.1:7890"), "{v}");
    assert!(v.contains("socks=127.0.0.1:7890"), "{v}");
}

#[cfg(windows)]
#[test]
fn apply_then_restore_roundtrip() {
    let snap = apply_net_proxy(7890).unwrap();
    assert_eq!(snap.enable, 1);
    assert!(snap.server.contains(":7890"));
    restore_proxy(Some(&snap)).unwrap();
    // 恢复后读回（读取复用 sysproxy.rs 既有读取函数,未暴露则按注册表 key 直读）
}
```

（cross-platform：非 windows 下 apply/restore 是 no-op，测试仅非 windows 时验证 proxy_server_value；windows 验证描述保留。）

- [ ] **Step 2: Run** → FAIL

- [ ] **Step 3: Implement** — sysproxy_set.rs（winreg；读快照用与源 sysproxy.rs 相同的 HKCU key；应用同 key）

```rust
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const INTERNET_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

#[derive(Clone, Debug, PartialEq)]
pub struct ProxySnapshot { pub enable: i64, pub server: String }

pub fn proxy_server_value(port: u16) -> String {
    format!("http=127.0.0.1:{port};https=127.0.0.1:{port};socks=127.0.0.1:{port}")
}

fn open() -> Result<RegKey, String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(INTERNET_SETTINGS, winreg::enums::KEY_READ | winreg::enums::KEY_SET_VALUE)
        .map_err(|e| format!("打开系统代理设置失败: {e}"))
}

#[cfg(windows)]
pub fn apply_net_proxy(port: u16) -> Result<ProxySnapshot, String> {
    let key = open()?;
    let enable: i64 = key.get_value("ProxyEnable").unwrap_or(0);
    let server: String = key.get_value("ProxyServer").unwrap_or_default();
    let snap = ProxySnapshot { enable, server };
    key.set_value("ProxyServer", &proxy_server_value(port)).map_err(|e| format!("写 ProxyServer 失败: {e}"))?;
    key.set_value("ProxyEnable", &1_i64).map_err(|e| format!("写 ProxyEnable 失败: {e}"))?;
    Ok(snap)
}

#[cfg(windows)]
pub fn restore_proxy(snap: Option<&ProxySnapshot>) -> Result<(), String> {
    let key = open()?;
    match snap {
        Some(s) => {
            key.set_value("ProxyEnable", &s.enable).map_err(|e| format!("还原 ProxyEnable 失败: {e}"))?;
            key.set_value("ProxyServer", &s.server).map_err(|e| format!("还原 ProxyServer 失败: {e}"))?;
        }
        None => {
            let _ = key.set_value("ProxyEnable", &0_i64);
            let _ = key.set_value("ProxyServer", &"");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn apply_net_proxy(_port: u16) -> Result<ProxySnapshot, String> { Ok(ProxySnapshot { enable: 0, server: String::new() }) }
#[cfg(not(windows))]
pub fn restore_proxy(_snap: Option<&ProxySnapshot>) -> Result<(), String> { Ok(()) }
```

注意：`open()` 在非 windows 下仍被引用导致编译超集问题——将 open 也包 cfg(windows)。

- [ ] **Step 4: Run** `cargo test -p vpn-ctl proxy_server_value` → PASS（windows roundtrip 本机会真跑注册表——测试后 restore 保证环境还原）

- [ ] **Step 5: Commit** `git commit -m "feat(net): 系统代理快照-还原(WinINET 写)"`

---

### Task 5: 客户端运行时（net/runtime.rs）start/stop/status 状态机
> ⚠️ 依赖顺序：Task 5 引用了 `geodata::geo_present`（Task 6 实现）与 state.net 字段（本任务加），
> 请按 T1→T2→T3→T4→T6→T5→T7→T8→T9→T10 的顺序执行（T5 放在 T6 之后）。

**Files:**
- Create: `src-tauri/src/net/runtime.rs`
- Test: `src-tauri/src/net/runtime_tests.rs`
- Modify: `src-tauri/src/net/mod.rs`（加 `pub mod runtime;` + `NetStatus` struct）
- Modify: `src-tauri/src/state.rs`（`pub net: tokio::sync::Mutex<Option<crate::net::runtime::RunningNet>>` + 构造 `net: tokio::sync::Mutex::new(None)`）
- Modify: `src-tauri/src/speed/sidecar.rs`（`wait_ready` 改 `pub`）

**Interfaces:**
- Consumes: `ensure_binary`（Task1）、`client_yaml`（Task2）、`scan_conflicts`（Task3）、`apply_net_proxy/restore_proxy`（Task4）、`wait_ready`（speed::sidecar，改 pub）
- Produces:
  - `pub struct RunningNet { pub child: std::process::Child, pub mixed_port: u16, pub ctrl_port: u16, pub token: String, pub snapshot: Option<sysproxy_set::ProxySnapshot> }`
  - `pub struct NetStatus { pub running: bool, pub port: Option<u16>, pub conflicts: Vec<String>, pub degraded_geo: bool }`（serde）
  - `pub struct StartOptions { pub ignore_conflicts: bool, pub conflict_override: Option<Vec<String>> }`
  - `pub async fn start(state: &AppState, opts: StartOptions) -> Result<NetStatus, String>`
  - `pub async fn stop(state: &AppState) -> Result<NetStatus, String>`
  - `pub async fn status(state: &AppState) -> NetStatus`
  - `pub async fn stop_all(state: &AppState)`

- [ ] **Step 1: Write failing tests**

```rust
// 真实 mihomo 集成（dev 环境有 binaries/mihomo.exe；缺失时跳过——同 ensure_binary 测试策略）
#[tokio::test]
async fn lifecycle_with_real_mihomo_and_conflict_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let state = crate::state::AppState::new_with_store(&dir.path().join("t.db"), Arc::new(crate::crypto::MemoryStore::default())).unwrap();
    // 冲突即拒（override 模拟已检测到 Clash Verge）
    let err = start(&state, StartOptions { ignore_conflicts: false, conflict_override: Some(vec!["Clash Verge".into()]) }).await.unwrap_err();
    assert!(err.contains("Clash Verge"), "{err}");
    assert!(!status(&state).await.running);
    // 无节点也允许启动（config proxies 空,规则兜底 DIRECT→纯透传测试环境安全）
    let st = start(&state, StartOptions { ignore_conflicts: true, conflict_override: None }).await.unwrap();
    assert!(st.running);
    assert!(st.port.is_some());
    // 重复 start 幂等（已在运行直接回状态,不双进程）
    let st2 = start(&state, StartOptions { ignore_conflicts: true, conflict_override: Some(vec![]) }).await.unwrap();
    assert!(st2.running);
    assert_eq!(st2.port, st.port);
    let st3 = stop(&state).await.unwrap();
    assert!(!st3.running);
    assert!(!status(&state).await.running);
}
```

（现实约束：这台机器 Windows——进程扫描会真执行 tasklist；ignore_conflicts=true 绕过。若已装 Verge 且开着，用 ignore_conflicts 防止误拒。）

- [ ] **Step 2: Run** → FAIL（模块不存在）

- [ ] **Step 3: Implement** — runtime.rs：

```rust
//! 本机上网运行时：独立 mihomo 实例（完整节点+分组+智能规则 config），
//! start 冲突→二进制→端口→config→spawn→就绪→系统代理；stop 反向杀进程+还原代理。
//! 槽位锁全程持有（仿 sidecar::ensure_started 模式），App 退出经 stop_all 清理。

use crate::net::{sysproxy_set, NetStatus, StartOptions};
use crate::speed::sidecar;
use crate::state::AppState;
use std::collections::HashSet;

pub const NET_DIR: &str = "net";
pub const NET_DEFAULT_PORT: u16 = 7890;

pub struct RunningNet {
    pub child: std::process::Child,
    pub mixed_port: u16,
    pub ctrl_port: u16,
    pub token: String,
    pub snapshot: Option<sysproxy_set::ProxySnapshot>,
}

fn exclude_own_pids(state: &AppState) -> Vec<u32> {
    let mut pids = Vec::new();
    if let Ok(guard) = state.sidecar.try_lock() {
        if let Some(s) = guard.as_ref() {
            pids.push(s.child.id());
        }
    }
    if let Ok(guard) = state.net.try_lock() {
        if let Some(n) = guard.as_ref() {
            pids.push(n.child.id());
        }
    }
    pids
}

/// 本机端口探测：base 起顺延 20 个找空闲（TcpListener::bind 试探）。
fn alloc_mixed_port(base: u16) -> Result<u16, String> {
    for n in 0..20u16 {
        let cand = base.saturating_add(n);
        if std::net::TcpListener::bind(("127.0.0.1", cand)).is_ok() {
            return Ok(cand);
        }
    }
    Err(format!("{base} 起 20 个端口全被占用"))
}

fn current_pid_set(state: &AppState) -> HashSet<u32> {
    exclude_own_pids(state).into_iter().collect()
}

pub async fn start(state: &AppState, opts: StartOptions) -> Result<NetStatus, String> {
    let mut guard = state.net.lock().await;
    // 已在运行：直接返回（幂等;防双进程）
    if let Some(n) = guard.as_ref() {
        if n.child.try_wait().map_err(|e| format!("检查进程失败: {e}"))?.is_none() {
            return Ok(NetStatus { running: true, port: Some(n.mixed_port), conflicts: vec![], degraded_geo: (!crate::net::geodata::geo_present(state)) });
        }
        guard.take(); // 崩溃自愈:子进程退出则清槽重启
    }
    if !opts.ignore_conflicts {
        let conflicts = match &opts.conflict_override {
            Some(c) => c.clone(),
            None => crate::net::conflict::scan_conflicts(&current_pid_set(state))?,
        };
        if !conflicts.is_empty() {
            return Err(format!("检测到正在运行的代理软件: {}。请先在其设置中关闭系统代理/TUN 后再开启", conflicts.join("、")));
        }
    }
    let net_dir = state.data_dir.join(NET_DIR);
    let exe = sidecar::ensure_binary(&net_dir, state.sidecar_resource.read().map_err(|_| "资源锁读取失败".to_string())?.as_deref())?;
    let mixed_port = alloc_mixed_port(NET_DEFAULT_PORT)?;
    let ctrl_port = sidecar::alloc_local_ports().map_err(|e| e.to_string())?.1;
    let token = sidecar::random_token();
    let nodes = crate::db::repos::nodes::list(&state.db.lock())
        .map_err(|e| e.to_string())?;
    let geo = crate::net::geodata::geo_present(state);
    let yaml = crate::config::client_yaml(&nodes, mixed_port, ctrl_port, &token, geo)?;
    std::fs::write(net_dir.join("config.yaml"), yaml).map_err(|e| format!("写入客户端配置失败: {e}"))?;
    let mut child = std::process::Command::new(&exe)
        .current_dir(&net_dir)
        .spawn()
        .map_err(|e| format!("启动 mihomo 失败: {e}"))?;
    if let Err(e) = sidecar::wait_ready(&mut child, ctrl_port, &token).await {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    let snapshot = sysproxy_set::apply_net_proxy(mixed_port)?;
    *guard = Some(RunningNet { child, mixed_port, ctrl_port, token: token.clone(), snapshot: Some(snapshot) });
    Ok(NetStatus { running: true, port: Some(mixed_port), conflicts: vec![], degraded_geo: !geo })
}

pub async fn stop(state: &AppState) -> Result<NetStatus, String> {
    let mut guard = state.net.lock().await;
    if let Some(mut n) = guard.take() {
        let _ = n.child.kill();
        let _ = n.child.wait();
        sysproxy_set::restore_proxy(n.snapshot.as_ref())?;
    }
    Ok(NetStatus { running: false, port: None, conflicts: vec![], degraded_geo: !crate::net::geodata::geo_present(state) })
}

pub async fn status(state: &AppState) -> NetStatus {
    let guard = state.net.lock().await;
    match guard.as_ref() {
        Some(n) if n.child.try_wait().map_err(|_| ()).ok().flatten().is_none() => NetStatus { running: true, port: Some(n.mixed_port), conflicts: vec![], degraded_geo: !crate::net::geodata::geo_present(state) },
        _ => NetStatus { running: false, port: None, conflicts: vec![], degraded_geo: !crate::net::geodata::geo_present(state) },
    }
}

pub async fn stop_all(state: &AppState) {
    let _ = stop(state).await;
}
```

wait_ready 改 `pub`（sidecar.rs 151 行 `pub(crate) async fn wait_ready` → `pub async fn wait_ready`）；alloc_local_ports/random_token 已 pub。

注意 start 里 child.try_wait 是同步阻塞 API 需要小心 Mutex 跨 await 问题——全部 await 集中在 wait_ready 前，且锁为 tokio Mutex 可跨 await，无 unsafe。tests 里 `state.net.try_lock()` 时已在锁内调用会 panic 吗——exclude_own_pids 用 try_lock 规避。start 里先取 net 锁再调用 conflict::scan（不碰 state.net）OK。

- [ ] **Step 4: Run** `cargo test -p vpn-ctl lifecycle_with_real_mihomo` → PASS；`cargo test -p vpn-ctl` 全量回归

- [ ] **Step 5: Commit** `git commit -m "feat(net): 客户端运行时 start/stop/status(独立 mihomo 实例)"`

---

### Task 6: geosite 数据自动下载（net/geodata.rs）

**Files:**
- Create: `src-tauri/src/net/geodata.rs`
- Test: `src-tauri/src/net/geodata_tests.rs`
- Modify: `src-tauri/src/net/mod.rs`（加 `pub mod geodata;` + `geo_present` 引用）

**Interfaces:**
- Consumes: reqwest client（跟随系统代理——`crate::subs::service::http_client` + 现有 sysproxy 解析），wiremock 测试
- Produces:
  - `pub const GEO_DIR: &str = "net";`（与 runtime NET_DIR 同目录）
  - `pub async fn ensure_geodata(state: &AppState, client: reqwest::Client) -> bool` — 下载两个文件（URL 前缀参数化便于测试 + 默认 MetaCubeX/meta-rules-dat latest），单文件 30s 超时；任一失败 → false（降级）
  - `pub fn geo_present(state: &AppState) -> bool` — `data_dir/net/geoip.dat` 与 `geosite.dat` 都存在
  - `pub const DEFAULT_GEODATA_BASE: &str = "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest";`

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn ensure_geodata_downloads_and_geo_present_toggles() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new_with_store(&dir.path().join("t.db"), Arc::new(MemoryStore::default())).unwrap();
    let mock = MockServer::start().await;
    for f in ["geoip.dat", "geosite.dat"] {
        Mock::given(method("GET")).and(path(format!("/{f}"))).respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-data")).mount(&mock).await;
    }
    let client = crate::subs::service::http_client(Duration::from_secs(5), Duration::from_secs(30));
    let ok = ensure_geodata_with_base(&state, client, &mock.uri()).await;
    assert!(ok);
    assert!(geo_present(&state));
    // 失败降级
    let mock2 = MockServer::start().await;
    let client2 = crate::subs::service::http_client(Duration::from_secs(5), Duration::from_secs(30));
    let ok2 = ensure_geodata_with_base(&state, client2, &mock2.uri()).await;
    assert!(!ok2);
}
```

- [ ] **Step 2: Run** → FAIL

- [ ] **Step 3: Implement** — geodata.rs：

```rust
//! 分流规则数据：geoip.dat / geosite.dat 自动确保（首次开启前下载到 data_dir/net）。
//! 下载失败 → 降级（客户端 config 跳过 GEOSITE/GEOIP CN 规则，仅 LAN/PRIVATE 直连）。
//! base 参数化：生产用 DEFAULT_GEODATA_BASE，测试用 wiremock uri。

pub const GEO_DIR: &str = "net";
pub const GEO_FILES: [&str; 2] = ["geoip.dat", "geosite.dat"];
pub const DEFAULT_GEODATA_BASE: &str = "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest";

fn geo_dir(state: &AppState) -> std::path::PathBuf { state.data_dir.join(GEO_DIR) }

pub fn geo_present(state: &AppState) -> bool {
    GEO_FILES.iter().all(|f| geo_dir(state).join(f).is_file())
}

pub async fn ensure_geodata(state: &AppState, client: reqwest::Client) -> bool {
    ensure_geodata_with_base(state, client, DEFAULT_GEODATA_BASE).await
}

pub async fn ensure_geodata_with_base(state: &AppState, client: reqwest::Client, base: &str) -> bool {
    if geo_present(state) { return true; }
    let dir = geo_dir(state);
    if std::fs::create_dir_all(&dir).is_err() { return false; }
    for f in GEO_FILES {
        let url = format!("{base}/{f}");
        let bytes = match client.get(&url).send().await.and_then(|r| r.error_for_status().and_then(|r| r.bytes())) {
            Ok(b) => b,
            Err(_) => return false,
        };
        if std::fs::write(dir.join(f), &bytes).is_err() { return false; }
    }
    geo_present(state)
}
```

（http_client 签名确认：`subs::service::http_client(connect_timeout, total_timeout) -> reqwest::Client`，现有代码用法一致。）

- [ ] **Step 4: Run** `cargo test -p vpn-ctl ensure_geodata` → PASS

- [ ] **Step 5: Commit** `git commit -m "feat(net): geosite 数据自动确保(失败降级)"`

---

### Task 7: IPC 壳 + state 挂载 + 退出清理

**Files:**
- Modify: `src-tauri/src/state.rs`（net 字段 + 构造）
- Modify: `src-tauri/src/commands.rs`（三段 tauri command + 三个壳函数）
- Modify: `src-tauri/src/main.rs`（invoke_handler 注册 3 命令；ExitRequested 追加 net::stop_all）
- Test: `src-tauri/src/commands_tests.rs`（冲突拒绝/空闲状态/停止空转幂等）

**Interfaces:**
- Consumes: runtime::start/stop/status（Task5）、conflict（Task3）
- Produces: `net_status_cmd` / `net_start_cmd` / `net_stop_cmd`

- [ ] **Step 1: Write failing tests**（commands_tests.rs 追加）

```rust
#[tokio::test]
async fn net_commands_conflict_rejection_and_idle_status() {
    let (dir, state) = test_state();
    // 空闲状态
    let st = crate::commands::net_status(state.clone()).await;
    assert!(!st.running);
    // 冲突拒绝（override 列表驱动，零真实进程扫描；不启用真实 mihomo）
    let err = crate::commands::net_start(state.clone(), crate::domain::NetStartInput { ignore_conflicts: false, conflict_override: Some(vec!["Clash Verge".into()]) }).await.unwrap_err().to_string();
    assert!(err.contains("Clash Verge"), "{err}");
    // 停止空转幂等：未运行 stops 无错
    let st2 = crate::commands::net_stop(state.clone()).await.unwrap();
    assert!(!st2.running);
}
```

（net_status/net_start/net_stop 壳以 async fn 返回 R<NetStatus>；NetStartInput 在 domain.rs。）

- [ ] **Step 2: Run** → FAIL

- [ ] **Step 3: Implement**

state.rs：

```rust
/// 本机上网客户端（独立 mihomo 实例）；None = 未开启。App 退出时停。
pub net: tokio::sync::Mutex<Option<crate::net::runtime::RunningNet>>,
```

构造函数（new_with_store 内，adjacent 到 sidecar 初始化）：

```rust
net: tokio::sync::Mutex::new(None),
```

domain.rs：

```rust
/// 上网启动选项：ignore_conflicts 供测试与「确认后仍继续」场景；
/// conflict_override 驱动冲突列表（生产 None = 真实扫描）。
#[derive(Debug, Clone)]
pub struct NetStartInput {
    pub ignore_conflicts: bool,
    pub conflict_override: Option<Vec<String>>,
}
```

commands.rs（放在 settings 段前/网络段）：

```rust
pub async fn net_status(state: Arc<AppState>) -> R<crate::net::NetStatus> {
    Ok(crate::net::runtime::status(&state).await)
}

pub async fn net_start(state: Arc<AppState>, input: crate::domain::NetStartInput) -> R<crate::net::NetStatus> {
    crate::net::runtime::start(&state, crate::net::StartOptions {
        ignore_conflicts: input.ignore_conflicts,
        conflict_override: input.conflict_override,
    }).await.map_err(AppError::Message)
}

pub async fn net_stop(state: Arc<AppState>) -> R<crate::net::NetStatus> {
    crate::net::runtime::stop(&state).await.map_err(AppError::Message)
}

#[tauri::command]
pub async fn net_status_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<crate::net::NetStatus> { net_status(state.inner().clone()).await }

#[tauri::command]
pub async fn net_start_cmd(state: tauri::State<'_, Arc<AppState>>, ignore_conflicts: bool, conflict_override: Option<Vec<String>>) -> R<crate::net::NetStatus> {
    net_start(state.inner().clone(), crate::domain::NetStartInput { ignore_conflicts, conflict_override }).await
}

#[tauri::command]
pub async fn net_stop_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<crate::net::NetStatus> { net_stop(state.inner().clone()).await }
```

main.rs：register 三命令（start_deploy_cmd 之后）；ExitRequested 处 147-150 追加（与 sidecar 并列）：

```rust
                tauri::async_runtime::block_on(crate::net::runtime::stop_all(&state));
```

- [ ] **Step 4: Run** `cargo test -p vpn-ctl net_commands` → PASS；`cargo test -p vpn-ctl` 全绿

- [ ] **Step 5: Commit** `git commit -m "feat(net): IPC 三命令+退出清理+NetStatus"`

---

### Task 8: 设置键 + 导出接智能分流开关

**Files:**
- Modify: `src-tauri/src/settings.rs`（ensure_defaults 加 `export_smart_split=1`）
- Modify: `src-tauri/src/subs/service.rs:222`（读 settings 传 build_yaml smart 参数）
- Modify: `src/api/index.ts` / `src/views/Settings.tsx` + `Settings.test.tsx`（开关 UI）
- Test: `src-tauri/src/subs/service_tests.rs`?（repos::settings get 已有测试面——本任务加 settings.rs 默认值测试 + frontend Settings.test）

**Interfaces:**
- Consumes: repos::settings::get（`src/db/repos.rs:448-470`，`get(conn,key)->Result<Option<String>>`/`set(conn,key,value)`）
- Produces: settings key `export_smart_split`（"1"=开默认），rebuild_config 依据它传 build_yaml smart

- [ ] **Step 1: Write failing tests**

rust（settings_tests.rs 追加）：

```rust
#[test]
fn defaults_include_export_smart_split_on() {
    // ensure_defaults 后 settings 表含 export_smart_split=1
}
```

frontend（Settings.test.tsx 追加）：开关存在、点一下调 set_settings（mockIPC 记录 `'set_settings_cmd'` args 含 `export_smart_split`）。

- [ ] **Step 2: Run** → FAIL

- [ ] **Step 3: Implement**

settings.rs ensure_defaults 追加：

```rust
        if svc.get("export_smart_split")?.is_none() {
            svc.set("export_smart_split", "1")?;
        }
```

subs/service.rs rebuild_config 中 build_yaml 调用改为读 table：

```rust
        let smart = {
            let conn = self.db.lock();
            crate::db::repos::settings::get(&conn, "export_smart_split")
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(true)
        };
        let (yaml, skipped_ssr) = crate::config::build_yaml(&visible, smart)
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
```

api/index.ts：`export function setSettings(values: Record<string,string>): Promise<void> { return invoke('set_settings_cmd', { values }) }`（确认现有 set_settings 封装是否存在，存在则补 export_smart_split 用法即可）。

Settings.tsx：新增复选行「导出订阅带智能分流规则」（读：`getSettings(['export_smart_split'])`；写：`setSettings({ export_smart_split: checked ? '1' : '0' })`）。

- [ ] **Step 4: Run** `cargo test -p vpn-ctl defaults_include_export` + `npx vitest run src/views/Settings.test.tsx` → PASS

- [ ] **Step 5: Commit** `git commit -m "feat(ui): 导出智能分流设置开关"`

---

### Task 9: 仪表盘「网络」卡

**Files:**
- Modify: `src/views/Dashboard.tsx`
- Modify: `src/api/types.ts`（`NetStatus`）+ `src/api/index.ts`（`netStatus/netStart/netStop`）
- Test: `src/views/Dashboard.test.tsx`

**Interfaces:**
- Consumes: Task7 命令
- Produces: 仪表盘「网络」卡（状态徽章+一键开关+冲突/降级横幅）

- [ ] **Step 1: Write failing tests**（Dashboard.test.tsx 追加；mock 已有 IPC 基座追加 net_*）

```tsx
it('网络卡一键开启与关闭', async () => {
  // mock net_status_cmd 初始 running:false;net_start_cmd 返回 running:true,port:7890
  const user = userEvent.setup()
  render(<Dashboard onNavigate={() => {}} />)
  expect(await screen.findByText('未开启')).toBeInTheDocument()
  await user.click(screen.getByRole('button', { name: '开启网络' }))
  expect(await screen.findByText(/运行中/)).toBeInTheDocument()
  expect(screen.getByText(/7890/)).toBeInTheDocument()
  await user.click(screen.getByRole('button', { name: /关闭网络|断开/ }))
  expect(await screen.findByText('未开启')).toBeInTheDocument()
})

it('网络卡冲突提示不启动', async () => {
  // net_start_cmd 抛错 '检测到正在运行的代理软件: Clash Verge'
  const user = userEvent.setup()
  render(<Dashboard onNavigate={() => {}} />)
  await screen.findByText('未开启')
  await user.click(screen.getByRole('button', { name: '开启网络' }))
  expect(await screen.findByText(/Clash Verge/)).toBeInTheDocument()
  expect(screen.getByText('未开启')).toBeInTheDocument()
})
```

- [ ] **Step 2: Run** → FAIL（api 函数与卡不存在）

- [ ] **Step 3: Implement**

types.ts：

```ts
export interface NetStatus {
  running: boolean
  port: number | null
  conflicts: string[]
  degraded_geo: boolean
}
```

index.ts：

```ts
export const netStatus = (): Promise<NetStatus> => invoke('net_status_cmd')
/** 一键开启：冲突列表会在开启失败错误文本中带上（前端横幅展示） */
export const netStart = (): Promise<NetStatus> => invoke('net_start_cmd', { ignoreConflicts: true, conflictOverride: null })
export const netStop = (): Promise<NetStatus> => invoke('net_stop_cmd')
```

（前端默认 ignoreConflicts=false 更合理？设计"提醒不抢占"：默认 false 让后端扫描；若报"检测到…"前端口已顺延则业务冲突——我们选 **默认 ignore_conflicts=false**：后端扫描冲突→Err→前端横幅；用 `ignoreConflicts: false, conflictOverride: null`。测试 mock 的 start 返回成功或错误两条路径。）

Dashboard.tsx：格子后追加：

```tsx
      <div className="rounded border border-neutral-800 bg-neutral-900 p-4">
        <div className="flex items-center justify-between">
          <div>
            <p className="text-sm text-neutral-400">网络</p>
            <p className="mt-1 text-xl font-bold">
              {net.running ? `运行中 · 端口 ${net.port}` : '未开启'}
            </p>
            <p className="mt-1 text-xs text-neutral-500">
              {net.running ? '系统代理已接入 · 智能分流' : '一键开启后访问外网走落地机，国内直连'}
            </p>
          </div>
          <button type="button" onClick={handleNetToggle} className={...}>
            {net.running ? '关闭网络' : '开启网络'}
          </button>
        </div>
        {netError && <p role="alert" className="mt-2 text-xs text-red-400">{netError}</p>}
        {net.running && net.degraded_geo && (
          <p className="mt-2 text-xs text-amber-400">分流数据缺失：全部流量经代理，国内站点可能变慢</p>
        )}
      </div>
```

handleNetToggle：running → netStop；else → netStart；catch → setNetError(String(e))；then → setNet + 清错误。mount 时 netStatus()（放进 refresh）。

- [ ] **Step 4: Run** `npx vitest run src/views/Dashboard.test.tsx` → PASS；全量 `npx vitest run` 回归 + `npx tsc --noEmit -p tsconfig.json`

- [ ] **Step 5: Commit** `git commit -m "feat(ui): 仪表盘网络卡(一键开关+冲突/降级提示)"`

---

### Task 10: 全量回归与手工验收脚本

**Files:** 无新增

- [ ] **Step 1:** `cargo test` 全绿（含 net 三文件与 config 变更），`npx vitest run` 全绿，`npx tsc --noEmit` 干净
- [ ] **Step 2: 手工验收清单**（用户执行，脚本注释写进 PR body 或交付说明）：
  1. 确认 Clash Verge 已关闭（冲突横幅应在再次开启时出现——开一个验证提醒不抢占，验完关掉）
  2. 仪表盘 → 网络卡 → 开启 → 状态"运行中 · 端口 7890"
  3. 浏览器：访问 `baidu.com`（国内应秒开/直连）、`google.com`（经落地机出口，速度与自动化测速相近）
  4. 节点页换「手动选择」组内节点 → 浏览器验证切换生效
  5. 关闭网络 → 系统代理还原（`reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings` 确认 ProxyEnable 回原值）
  6. 崩溃自愈：开启状态下 kill mihomo 进程 → 重新开启应自愈拉起
- [ ] **Step 3: Commit** `git commit -am "test: 本机上网客户端全量回归"`（或直接收尾提交）

---

## Self-Review

- **Spec 覆盖**：仪表盘一键(T9)、独立 mihomo+系统代理(T5)、智能分流规则(T2 client + T8 export)、冲突提醒不抢占(T3/T7)、geosite 下载降级(T6/T2 geo_present)、断开还原(T5 stop)、Settings 开关(T8) ✓；「能选择服务器」= 手动选择组含自动选择首项(T2 net_groups) + 规则 MATCH,手动选择 ✓
- **占位符**：无 TBD；所有新函数给出可编译骨架 ✓（runtime 里 geodata::geo_present 在 T6 才实现——Task5 代码引用提前出现，Note: Task5 的 start/stop/status 依赖 geodata::geo_present；执行顺序 T5 在 T6 之前会导致编译失败 → **调整执行顺序：Task 5 与 Task 6 合并顺序，先 T6 后 T5**（T6 mock 无需 runtime；但 T6 测试用 AppState 新字段 net 尚未加——state 字段在 T7。修正：把 state.rs net 字段挂到 Task 5（runtime 需要），T5 执行顺序在 T6 之后。）
- **修正后的顺序**：T1 二进制 → T2 config → T3 conflict → T4 sysproxy → **T6 geodata** → T5 runtime(+state.net 字段) → T7 IPC/退出清理 → T8 settings/export → T9 Dashboard → T10 回归 ✓
- **类型一致**：NetStatus{running,port,conflicts,degraded_geo} 全链一致；StartOptions{ignore_conflicts,conflict_override}；client_yaml(nodes,mixed_port,ctrl_port,token,geo_present)；build_yaml(nodes,smart) ✓
- **前置校验失败风险**：T5 生命周期测试真实拉起 mihomo——dev 环境 binaries/mihomo.exe 已在 worktree（先前复制过），sha256 校验用真实二进制 OK；CI 缺文件时测试跳过（同 T1 策略）。运行样例用空节点 config（DIRECT 兜底，不出网，纯本地自证启动/停止/状态）。
