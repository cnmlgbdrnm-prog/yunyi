# 部署端口随机化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. 执行中回写「执行状态」段。

**Goal Contract**

- **Original Goal:**
  - 「现在端口部署的时候，现在端口都是硬编码？不是随机的？」
  - 「改成随机的吧！」
- **Hard Constraints:**
  - 范围：主协议部署（vless / trojan / vmess / ss / hysteria2 / tuic）+ 分享端口（订阅口 / 公网数据口 / 回环内部口 / CDN 自动口）。不含上游出口专用实例（`upstream.rs::pick_listen_port`）与中转扩展端口（`relay/extend.rs::allocate_forward_port`）。
  - 主协议随机池：10000–65535，排除分享端口段 18080–18799（用户选定）。
  - 分享端口段保持不变：18080–18180 / 18200–18499 / 18500–18799。
  - 分配算法：方案 A——区间内随机起点 + 段内顺序探测，到段尾回绕。
  - 存量服务器与存量分享不迁移；重部署/编辑继续复用远端现有端口。
  - CDN 自动口只能从 Cloudflare 支持的 `HTTPS_PORTS` 中选择，随机只改变尝试顺序。
  - 测试源码位于 `tests/`（`.gitignore` 中 `/tests/`，不随仓库上传）；提交只包含 `src-tauri/src`、`CHANGELOG.md`、计划文件。
  - 不得把真实 IP、凭据写进文档或代码（仓库 `check-sensitive` 规则）。
  - Rust 检查统一在 `src-tauri` 下执行：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`。
- **Acceptance Criteria:**
  - A1: 新协议首次部署的端口落在 10000–65535，且不在 18080–18799；可用 `cargo test --features local-tests servers::deploy` 与本计划 T2 的断言验证。
  - A2: 同一台服务器一次部署多个协议，端口互不重复，且不与远端既有协议端口重复；由 T1 的 `skip_taken` 单测 + T2 的多协议用例验证。
  - A3: 重部署不改动已存在协议的端口；由 T2 的 `deployed_node_is_auto_attached_to_a_matching_relay_link`（第二次部署复用端口）与既有 reuse 用例验证。
  - A4: 新分享的订阅口/数据口/内部口分别落在各自区间，互不重复，且不与已有分享重复；由 T3 纯函数单测 + T4 的 `allocate_ports_skips_ports_allocated_in_same_call` 验证。
  - A5: CDN 自动模式只从 Cloudflare 支持的 6 个端口中选；由 T5 的 `auto_candidates` 单测与 `cdn_tests` 验证。
  - A6: `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、完整本地测试通过（输出贴进执行证据）。

**Architecture:** 新增两个纯函数模块承载随机池/区间的数学（`servers/port_pool.rs`、`share/ports.rs`），部署与分享编排只负责「拿起点 → 探测 → 去重」。主协议在 `run_deploy_with` 里维护 `taken` 集合并把随机端口写回节点；分享在 `allocate_share_ports_with_starts` 里显式接收起点，生产入口用线程 RNG 生成起点后不再跨 await 持有 RNG。

**Tech Stack:** Rust 2021、`rand 0.8`（已在依赖中）、Tokio、本地 FakeSshServer 测试夹具。

## 模块地图（ASCII）

**层级（包含关系）**

```
服务端部署
├─ deploy.rs（部署流水线：探测→安装→配置→启动→写回）
│  └─ port_pool.rs（主协议随机端口池：常量 + 随机起点 + 回绕候选）
└─ deploy_spec.rs（协议配置生成；移除 default_port）

分享
├─ install.rs（创建/编辑编排 + SSH 端口探测）
│  └─ ports.rs（分享端口区间：随机起点 + 回绕候选）
├─ cdn.rs（Cloudflare HTTPS 端口表 + 自动候选轮转）
└─ naming.rs（三段端口范围常量）
```

**调用（运行时触发）**

```
deploy.rs::allocate_port ──▶ port_pool::random_pool_start / pool_candidates / skip_taken
share/install.rs::allocate_share_ports ──▶ ports::random_start_in / range_candidates
share/install.rs::allocate_cdn_port ──▶ cdn::auto_candidates
```

**依赖（数据 / 契约来源）**

```
port_pool ⇠── share/naming.rs 的 PORT_RANGE_START / INTERNAL_PORT_END（排除段单一来源）
install   ⇠── share/naming.rs 的三段范围常量
```

## 执行状态（进度指针）

- 当前：T6（pending）
- 下一动作：派发 T6 实施者（BASE = 935fece），按 task-6-brief.md 做全量验证与 CHANGELOG
- 阻塞：无
- 已完成：T1（c9fccc8..46ffbdf）、T2（5e6c1e3）、T3（4094cba）、T4（2756977）、T5（2756977..935fece），均 review clean
- 最后更新：2026-11-01

---

### T1: 主协议随机端口池纯函数模块

- **Depends on:** 无

**Files:**
- Create: `src-tauri/src/servers/port_pool.rs`
- Modify: `src-tauri/src/servers/mod.rs`（在 `pub mod ipv6_detect;` 之前加 `pub mod port_pool;`）

**Interfaces:**
- Consumes: `crate::share::naming::{PORT_RANGE_START, INTERNAL_PORT_END}`、`rand::Rng`
- Produces:
  - `pub const MAIN_PORT_MIN: u16 = 10000`
  - `pub const MAIN_PORT_MAX: u16 = 65535`
  - `pub const MAX_PROBES: usize = 64`
  - `pub const SHARE_PORT_MIN: u16 = PORT_RANGE_START`
  - `pub const SHARE_PORT_MAX: u16 = INTERNAL_PORT_END`
  - `pub fn random_pool_start(rng: &mut impl Rng) -> u16`
  - `pub fn pool_candidates(start: u16, limit: usize) -> Vec<u16>`
  - `pub fn skip_taken(candidates: Vec<u16>, taken: &HashSet<u16>) -> Vec<u16>`

- **验收:** `cargo test --features local-tests port_pool -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；输出含 5 个 `servers::port_pool::tests::*` 用例。

- **状态:** completed ｜ **证据:** commits `c9fccc8` + `46ffbdf`；`cargo test --features local-tests port_pool ...` → 6 passed（含 5 个 port_pool 用例）；`cargo fmt --check` exit 0；`cargo clippy --all-targets -- -D warnings` exit 0。审查：spec ✅ / quality Approved（6 条 Minor 已记入 ledger 延后处理，其中 `#[allow(dead_code)]` 由 T2 移除）。

- [ ] **Step 1: 写失败测试与函数桩**

创建 `src-tauri/src/servers/port_pool.rs`，内容为以下完整文件（函数先返回桩值，测试必须失败）：

```rust
//! 主协议随机端口池：10000–65535，排除分享端口段 18080–18799。
//!
//! 纯函数模块：不建连、不探测；SSH 探测与分配编排在 `deploy`。
//! 池按「逻辑索引」分成两段连续端口，随机起点与回绕都在逻辑索引上做，
//! 这样排除段不会引入重试或空号。

use crate::share::naming::{INTERNAL_PORT_END, PORT_RANGE_START};
use rand::Rng;

/// 主协议端口池下界（避开 1–9999 的系统/面板常用端口）。
pub const MAIN_PORT_MIN: u16 = 10000;
/// 主协议端口池上界。
pub const MAIN_PORT_MAX: u16 = 65535;
/// 单次分配最多探测的候选数。池内占用率极低，64 个足够；
/// 有上限才能保证 SSH 往返次数可控。
pub const MAX_PROBES: usize = 64;

/// 分享端口段（订阅/公网数据/内部三段合并），主协议整体避开。
pub const SHARE_PORT_MIN: u16 = PORT_RANGE_START;
pub const SHARE_PORT_MAX: u16 = INTERNAL_PORT_END;

/// 段 1 的长度：10000..=18079。
const SEG1_LEN: u32 = (SHARE_PORT_MIN - MAIN_PORT_MIN) as u32; // 8080
/// 逻辑池总长度：8080 + 46736 = 54816。
const TOTAL: u32 = SEG1_LEN + (MAIN_PORT_MAX - SHARE_PORT_MAX) as u32;

/// 逻辑索引 → 实际端口；`index` 必须 < TOTAL。
fn port_at(index: u32) -> u16 {
    if index < SEG1_LEN {
        MAIN_PORT_MIN + index as u16
    } else {
        SHARE_PORT_MAX + 1 + (index - SEG1_LEN) as u16
    }
}

/// 池内随机起点（均匀覆盖 54816 个端口）。
pub fn random_pool_start(rng: &mut impl Rng) -> u16 {
    let _ = rng;
    MAIN_PORT_MIN // 桩：实现前所有断言失败
}

/// 从 `start` 起、回绕、最多 `limit` 个候选；永不返回分享段端口。
pub fn pool_candidates(start: u16, limit: usize) -> Vec<u16> {
    let _ = (start, limit);
    Vec::new() // 桩
}

/// 过滤掉已分配/已占用的端口；保持候选顺序。
pub fn skip_taken(candidates: Vec<u16>, taken: &std::collections::HashSet<u16>) -> Vec<u16> {
    let _ = taken;
    candidates // 桩：不跳过任何端口
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashSet;

    #[test]
    fn pool_size_matches_segments() {
        assert_eq!(SEG1_LEN, 8080);
        assert_eq!(TOTAL, 54816);
    }

    #[test]
    fn random_start_never_lands_in_share_range() {
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..10_000 {
            let p = random_pool_start(&mut rng);
            assert!((MAIN_PORT_MIN..=MAIN_PORT_MAX).contains(&p), "{p}");
            assert!(!(SHARE_PORT_MIN..=SHARE_PORT_MAX).contains(&p), "{p}");
        }
    }

    #[test]
    fn candidates_wrap_and_skip_share_range() {
        let v = pool_candidates(65500, 64);
        assert_eq!(v.len(), 64);
        assert_eq!(v[0], 65500);
        assert_eq!(v[35], 65535);
        assert_eq!(v[36], MAIN_PORT_MIN, "段尾必须回绕到池首");
        assert!(
            v.iter()
                .all(|p| !(SHARE_PORT_MIN..=SHARE_PORT_MAX).contains(p)),
            "候选不得落入分享段: {v:?}"
        );
        let set: HashSet<u16> = v.iter().copied().collect();
        assert_eq!(set.len(), 64, "候选不得重复");
    }

    #[test]
    fn start_at_share_boundary_maps_to_next_segment() {
        assert_eq!(pool_candidates(18079, 1), vec![18079]);
        assert_eq!(pool_candidates(18800, 1), vec![18800]);
    }

    #[test]
    fn skip_taken_keeps_order() {
        let taken: HashSet<u16> = [10000, 10002].into_iter().collect();
        assert_eq!(
            skip_taken(vec![10000, 10001, 10002, 10003], &taken),
            vec![10001, 10003]
        );
    }
}
```

在 `src-tauri/src/servers/mod.rs` 的模块列表里加一行（放在 `pub mod ipv6_detect;` 前）：

```rust
pub mod port_pool;
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri; cargo test --features local-tests port_pool -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: FAIL；`random_start_never_lands_in_share_range` 在 `p = 10000` 之外的样本上失败，`candidates_wrap_and_skip_share_range` 因空 vec 失败。

- [ ] **Step 3: 实现三个函数**

把 `random_pool_start`、`pool_candidates`、`skip_taken` 替换为：

```rust
/// 池内随机起点（均匀覆盖 54816 个端口）。
pub fn random_pool_start(rng: &mut impl Rng) -> u16 {
    port_at(rng.gen_range(0..TOTAL))
}

/// 从 `start` 起、回绕、最多 `limit` 个候选；永不返回分享段端口。
///
/// `start` 必须是池内端口（生产只有 [`random_pool_start`] 或池内端口传入）。
pub fn pool_candidates(start: u16, limit: usize) -> Vec<u16> {
    debug_assert!(
        start >= MAIN_PORT_MIN
            && start <= MAIN_PORT_MAX
            && !(SHARE_PORT_MIN..=SHARE_PORT_MAX).contains(&start),
        "起点必须在主协议端口池内: {start}"
    );
    let start_index = if start < SHARE_PORT_MIN {
        u32::from(start - MAIN_PORT_MIN)
    } else {
        SEG1_LEN + u32::from(start - (SHARE_PORT_MAX + 1))
    };
    (0..limit as u32)
        .map(|n| port_at((start_index + n) % TOTAL))
        .collect()
}

/// 过滤掉已分配/已占用的端口；保持候选顺序。
pub fn skip_taken(candidates: Vec<u16>, taken: &std::collections::HashSet<u16>) -> Vec<u16> {
    candidates
        .into_iter()
        .filter(|p| !taken.contains(p))
        .collect()
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri; cargo test --features local-tests port_pool -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: PASS，5 个用例全绿。

- [ ] **Step 5: 提交（只提交源码，测试源码不入库）**

```bash
git add src-tauri/src/servers/port_pool.rs src-tauri/src/servers/mod.rs
git commit -m "feat(deploy): 新增主协议随机端口池

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### T2: 主协议部署接入随机端口

- **Depends on:** T1

**Files:**
- Modify: `src-tauri/src/servers/deploy.rs`（导入、`allocate_port`、`run_deploy_with` 分配循环）
- Modify: `src-tauri/src/servers/deploy_spec.rs`（删除 `default_port`）
- Modify: `src-tauri/src/jobs/mod.rs`（第 24–25 行注释）
- Modify（本地测试，不入库）: `tests/rust/servers/deploy_tests.rs`

**Interfaces:**
- Consumes: T1 的 `random_pool_start`、`pool_candidates`、`skip_taken`、`MAX_PROBES`、`MAIN_PORT_MIN`、`MAIN_PORT_MAX`
- Produces: `async fn allocate_port(sess, cancel, protocol, taken: &mut HashSet<u16>) -> Result<u16, String>`；`run_deploy_with` 内 `taken` 集合

- **验收:** `cargo test --features local-tests servers::deploy -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；`grep -rn "default_port" src-tauri/src` 无输出；`cargo clippy --all-targets -- -D warnings` 通过。

- **状态:** completed ｜ **证据:** commit `5e6c1e3`；`cargo test ... servers::deploy` → 118 passed, 0 failed；`grep -rn default_port src` 无输出（exit 1）；`cargo fmt --check` exit 0；`cargo clippy --all-targets -- -D warnings` exit 0。审查：spec ✅ / quality Approved（2 条 Minor 已记入 ledger 延后）。

- [ ] **Step 1: 先改本地测试，制造 RED**

打开 `tests/rust/servers/deploy_tests.rs`，做以下修改（该目录被 `.gitignore` 忽略，只本地生效）：

**(a)** 删除导入列表里的 `default_port,`，并删除整个 `default_ports_follow_spec_table` 测试（约 68–76 行）。端口池的纯函数测试在 T1 的源码内联测试里。

**(b)** 把以下 6 处精确端口断言替换为池范围断言：

1. `xray_deploy_full_sequence_writes_config_and_node`（约 1533 行）：
   - 原：`assert_eq!(n.port, 443);`
   - 新：
     ```rust
     assert!(
         (10000..=65535).contains(&n.port) && !(18080..=18799).contains(&n.port),
         "新部署端口必须来自主协议随机池: {}",
         n.port
     );
     ```
2. `xray_deploy_skips_install_when_already_installed`（约 1736 行）：
   - 原：`assert_eq!(nodes[0].port, 443);`
   - 新：
     ```rust
     assert!(
         (10000..=65535).contains(&nodes[0].port)
             && !(18080..=18799).contains(&nodes[0].port),
         "新部署端口必须来自主协议随机池: {}",
         nodes[0].port
     );
     ```
3. `ss_deploy_full_sequence`（约 1796 行）：
   - 原：`assert_eq!(nodes[0].port, 8388);`
   - 新：
     ```rust
     assert!(
         (10000..=65535).contains(&nodes[0].port)
             && !(18080..=18799).contains(&nodes[0].port),
         "新部署端口必须来自主协议随机池: {}",
         nodes[0].port
     );
     ```
4. `singbox_only_hysteria2_deploy_completes`（约 2046 行）：
   - 原：`assert_eq!(nodes[0].port, 2097);`
   - 新：同第 3 条的断言。
5. `singbox_only_tuic_deploy_completes`（约 2111 行）：
   - 原：`assert_eq!(nodes[0].port, 2053);`
   - 新：同第 3 条的断言。
6. `udp_protocols_are_verified_on_udp_sockets`（约 2181 行）：
   - 原：`assert_eq!(nodes[0].port, 2097);`
   - 新：同第 3 条的断言。

**(c)** `multi_protocol_deploy_writes_one_node_per_protocol`（约 2604 行）：

把 2620–2622 行的精确端口脚本：

```rust
// 端口分配:vless=443 空闲、ss=8388 空闲(逐协议 probe)
srv.push_script("sport = :443", 1, "", "");
srv.push_script("sport = :8388", 1, "", "");
```

替换为：

```rust
// 端口分配：随机起点；FakeSshServer 未命中回落 exit 0 =「已占用」，
// 每个协议消费一条「空闲」脚本即可（首个探针命中后立即返回）。
srv.push_script("ss -tln", 1, "", "");
srv.push_script("ss -tln", 1, "", "");
```

把 `assert_eq!(vless.port, 443);` 与 `assert_eq!(ss.port, 8388);` 替换为：

```rust
assert!(
    (10000..=65535).contains(&vless.port) && !(18080..=18799).contains(&vless.port),
    "vless 端口必须来自主协议随机池: {}",
    vless.port
);
assert!(
    (10000..=65535).contains(&ss.port) && !(18080..=18799).contains(&ss.port),
    "ss 端口必须来自主协议随机池: {}",
    ss.port
);
assert_ne!(vless.port, ss.port, "同次部署的协议端口不得重复");
```

**(d)** `multi_xray_deploy_uses_per_inbound_reality_keypair`（约 2685 行）：

把 2693–2696 行：

```rust
// 端口分配：分别探测 443/4443/8443（FIFO 按序消费）
srv.push_script("sport = :443", 1, "", "");
srv.push_script("sport = :4443", 1, "", "");
srv.push_script("sport = :8443", 1, "", "");
```

替换为：

```rust
// 端口分配：三个协议各消费一条「空闲」脚本；随机起点由部署器决定。
for _ in 0..3 {
    srv.push_script("ss -tln", 1, "", "");
}
```

**(e)** 重写 `deployed_node_is_auto_attached_to_a_matching_relay_link`（约 2516–2568 行）为「两次部署」：

```rust
#[tokio::test]
async fn deployed_node_is_auto_attached_to_a_matching_relay_link() {
    let dir = tempfile::tempdir().unwrap();
    let state = landing_state(&dir);
    let mock = MockServer::start().await;
    let srv = FakeSshServer::start().await;
    xray_script(&srv);
    let server_id = seeded_server(&state, &srv).await;
    let runner = test_runner(&state, &mock).await;
    let req = crate::jobs::DeployRequest {
        job_id: 11,
        server_id,
        protocols: vec!["vless".into()],
        reality_dest: Some("www.apple.com".into()),
        enable_bbr: false,
    };

    // 第一次部署：端口随机，此时还没有链路
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req.clone()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let nodes = crate::db::repos::nodes::list(&state.db.lock(), &state.creds).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].relay_rule_id, None);
    let deployed_port = nodes[0].port;

    // 用实际分配的端口建链：匹配规则必须按「落地机地址 + 端口」命中
    let (_relay_id, rule_id) = seeded_relay_and_rule(&state, server_id, deployed_port, 4443);

    // 第二次部署：复用第一次写下的配置与端口，并自动补挂
    let written = srv
        .files()
        .get("/usr/local/etc/xray/config.json")
        .cloned()
        .expect("第一次部署应写入 xray 配置");
    let written = String::from_utf8(written).unwrap();
    script_remote_conf(
        &srv,
        "/usr/local/etc/xray/config.json",
        Some(written.as_str()),
    );
    xray_redeploy_script(&srv);
    runner
        .run(
            crate::jobs::JobRequest::Deploy(req),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    let nodes = crate::db::repos::nodes::list(&state.db.lock(), &state.creds).unwrap();
    assert_eq!(nodes.len(), 1);
    let n = &nodes[0];
    assert_eq!(
        n.relay_rule_id,
        Some(rule_id),
        "部署完必须自动挂上匹配的中转链路：{n:?}"
    );
    // **库里存的是直连地址**（不变量：数据始终存直连地址，换向只在生成侧一处发生）。
    assert_eq!(n.addr, "127.0.0.1", "库里必须是落地机直连地址");
    assert_eq!(n.port, deployed_port, "重部署必须复用第一次分配的端口");
    // 生效地址（生成侧算出来的）才指向中转机
    let rules = crate::db::repos::relay::list(&state.db.lock()).unwrap();
    let servers = crate::db::repos::servers::list(&state.db.lock()).unwrap();
    let eff = crate::config::effective_node(n, &rules, &servers);
    assert_eq!(eff.addr, "10.0.0.9", "生效地址应指向中转机");
    assert_eq!(eff.port, 4443, "生效端口应是链路的转发入口");
    let summary = latest_deploy_summary_text(&state, server_id);
    assert!(summary.contains("\"attached\":true"), "{summary}");
}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `cd src-tauri; cargo test --features local-tests servers::deploy -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: FAIL；池范围断言在旧端口（443/8388/…)上失败；多协议/多 xray 用例因精确端口探针不再匹配而失败；auto-attach 用例因端口仍是 443、链路 landing_port 为随机值之外而失败。

- [ ] **Step 3: 改 `deploy_spec.rs` 与 `jobs/mod.rs`**

删除 `src-tauri/src/servers/deploy_spec.rs` 中整段 `default_port`（约 440–451 行）：

```rust
/// 协议默认端口表(被占时 allocate_port 逐 +1 偏移)。
pub fn default_port(proto: &str) -> u16 {
    match proto {
        "vless" => 443,
        "trojan" => 4443,
        "vmess" => 8443,
        "ss" => 8388,
        "hysteria2" => 2097,
        "tuic" => 2053,
        _ => 0,
    }
}
```

把 `src-tauri/src/jobs/mod.rs` 的注释：

```rust
/// 部署任务请求（jobs 行已由 start_deploy 建立）。protocols 为白名单内协议,
/// 每协议端口由部署流水线按 default_port + 占用偏移自动分配。
```

改为：

```rust
/// 部署任务请求（jobs 行已由 start_deploy 建立）。protocols 为白名单内协议,
/// 每协议端口由部署流水线在主协议端口池（10000–65535，排除分享段）内随机分配。
```

- [ ] **Step 4: 改 `deploy.rs`**

在文件顶部导入区加入：

```rust
use super::port_pool::{
    pool_candidates, random_pool_start, skip_taken, MAIN_PORT_MAX, MAIN_PORT_MIN, MAX_PROBES,
};
```

把 `allocate_port`（约 213–227 行）整段替换为：

```rust
/// 从主协议随机池挑一个空闲端口；`taken` 是本次部署已占用/已分配的端口。
///
/// 随机起点 + 池内回绕，最多探测 [`MAX_PROBES`] 个候选。跳过 `taken` 是必须的：
/// 多协议在一次会话里**先全部分配、再统一写配置**，后分配的协议探测时前一个
/// 还没开始监听，远端 `ss` 看不到它。
async fn allocate_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    protocol: &str,
    taken: &mut HashSet<u16>,
) -> Result<u16, String> {
    let start = random_pool_start(&mut rand::thread_rng());
    for cand in skip_taken(pool_candidates(start, MAX_PROBES), taken) {
        if !probe_port(sess, cancel, protocol, cand).await? {
            taken.insert(cand);
            return Ok(cand);
        }
    }
    Err(format!(
        "端口池 {MAIN_PORT_MIN}–{MAIN_PORT_MAX} 中连续 {MAX_PROBES} 个候选均被占用"
    ))
}
```

把 `run_deploy_with` 里的分配循环（约 1127–1142 行）：

```rust
        // 端口分配：已部署协议复用远端既有端口（不重新探测——防漂移撞防火墙/安全组）；
        // 新协议才探测分配（占用偏移逐 +1）。任一失败即中止（不落配置）。
        let existing =
            read_existing_ports(&mut sess, cancel, need_xray, need_ss, need_singbox).await?;
        let mut ports: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
        for proto in &req.protocols {
            let base = default_port(proto);
            if base == 0 {
                return Err(format!("未知协议: {proto}"));
            }
            let assigned = match existing.ports.get(proto.as_str()) {
                Some(p) => *p,
                None => allocate_port(&mut sess, cancel, proto, base).await?,
            };
            ports.insert(proto.as_str(), assigned);
        }
```

替换为：

```rust
        // 端口分配：已部署协议复用远端既有端口（不重新探测——防漂移撞防火墙/安全组）；
        // 新协议才在随机池里探测分配。任一失败即中止（不落配置）。
        let existing =
            read_existing_ports(&mut sess, cancel, need_xray, need_ss, need_singbox).await?;
        // 初值含远端**所有**协议端口（不只本次请求的）：某个已部署协议本次未勾选、
        // 服务恰好停机时，`ss` 探测看不到它，新协议不能抢它的端口。
        let mut taken: HashSet<u16> = existing.ports.values().copied().collect();
        let mut ports: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
        for proto in &req.protocols {
            if !is_supported_protocol(proto) {
                return Err(format!("未知协议: {proto}"));
            }
            let assigned = match existing.ports.get(proto.as_str()) {
                Some(p) => *p,
                None => allocate_port(&mut sess, cancel, proto, &mut taken).await?,
            };
            ports.insert(proto.as_str(), assigned);
        }
```

- [ ] **Step 5: 运行测试确认 GREEN**

Run: `cd src-tauri; cargo test --features local-tests servers::deploy -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: PASS。若 auto-attach 用例失败，检查提示：第二次部署前必须先用 `script_remote_conf` 把第一次写下的 xray 配置喂给读取命令，否则 `read_existing_ports` 读不到端口会重新随机。

- [ ] **Step 6: 运行 fmt 与 clippy**

Run: `cd src-tauri; cargo fmt --check; cargo clippy --all-targets -- -D warnings`

Expected: 均退出 0。

- [ ] **Step 7: 提交（本地测试改动不入库）**

```bash
git add src-tauri/src/servers/deploy.rs src-tauri/src/servers/deploy_spec.rs src-tauri/src/jobs/mod.rs
git commit -m "feat(deploy): 主协议端口改为随机池分配

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### T3: 分享端口区间纯函数模块

- **Depends on:** 无

**Files:**
- Create: `src-tauri/src/share/ports.rs`
- Modify: `src-tauri/src/share/mod.rs`（在 `pub mod plan;` 之前加 `pub(crate) mod ports;`）

**Interfaces:**
- Consumes: `rand::Rng`
- Produces:
  - `pub(crate) type PortRange = (u16, u16)`
  - `pub(crate) fn random_start_in(range: PortRange, rng: &mut impl Rng) -> u16`
  - `pub(crate) fn range_candidates(range: PortRange, start: u16) -> impl Iterator<Item = u16>`

- **验收:** `cargo test --features local-tests share::ports -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；输出含 3 个 `share::ports::tests::*` 用例。

- **状态:** completed ｜ **证据:** commits `1e08dc5` + `4094cba`；`cargo test ... share::ports` → 3 passed；`cargo fmt --check` exit 0；`cargo clippy --all-targets -- -D warnings` exit 0。审查：spec ✅ / quality Approved（5 条 Minor 已记入 ledger；`#[allow(dead_code)]` 由 T4 移除）。

- [ ] **Step 1: 写失败测试与函数桩**

创建 `src-tauri/src/share/ports.rs`：

```rust
//! 分享端口区间：随机起点与回绕候选。
//!
//! 纯函数；SSH 探测与分配编排在 [`super::install`]。
//! 三个区间互不重叠（订阅 18080–18180 / 公网数据 18200–18499 / 回环 18500–18799），
//! 随机只在各自区间内发生，跨区间隔离由常量保证。

use rand::Rng;

/// 闭区间 `(lo, hi)`。
pub(crate) type PortRange = (u16, u16);

/// 区间内均匀随机起点。
pub(crate) fn random_start_in(range: PortRange, rng: &mut impl Rng) -> u16 {
    let _ = rng;
    range.0 // 桩
}

/// 从 `start` 起、回绕、覆盖整个区间；不重复。
pub(crate) fn range_candidates(range: PortRange, start: u16) -> impl Iterator<Item = u16> {
    let _ = start;
    range.0..=range.0 // 桩：只返回一个端口
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn random_start_stays_in_range() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..1000 {
            let p = random_start_in((18080, 18180), &mut rng);
            assert!((18080..=18180).contains(&p), "{p}");
        }
    }

    #[test]
    fn candidates_cover_range_once_and_wrap() {
        let v: Vec<u16> = range_candidates((18080, 18180), 18178).collect();
        assert_eq!(v.len(), 101);
        assert_eq!(v[0], 18178);
        assert_eq!(v[1], 18179);
        assert_eq!(v[2], 18180);
        assert_eq!(v[3], 18080, "段尾必须回绕到段首");
        let set: std::collections::HashSet<u16> = v.iter().copied().collect();
        assert_eq!(set.len(), 101, "候选不得重复");
    }

    #[test]
    fn start_outside_range_is_clamped_by_offset() {
        let v: Vec<u16> = range_candidates((18200, 18499), 18000).collect();
        assert_eq!(v[0], 18200);
        assert_eq!(v.last().copied(), Some(18499));
    }
}
```

在 `src-tauri/src/share/mod.rs` 的模块列表里加一行（放在 `pub mod plan;` 前）：

```rust
pub(crate) mod ports;
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri; cargo test --features local-tests share::ports -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: FAIL；`candidates_cover_range_once_and_wrap` 因长度只有 1 失败。

- [ ] **Step 3: 实现两个函数**

```rust
/// 区间内均匀随机起点。
pub(crate) fn random_start_in(range: PortRange, rng: &mut impl Rng) -> u16 {
    rng.gen_range(range.0..=range.1)
}

/// 从 `start` 起、回绕、覆盖整个区间；不重复。
pub(crate) fn range_candidates(range: PortRange, start: u16) -> impl Iterator<Item = u16> {
    let (lo, hi) = range;
    let len = u32::from(hi) - u32::from(lo) + 1;
    let offset = u32::from(start.saturating_sub(lo)) % len;
    (0..len).map(move |n| (u32::from(lo) + (offset + n) % len) as u16)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri; cargo test --features local-tests share::ports -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: PASS，3 个用例全绿。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/ports.rs src-tauri/src/share/mod.rs
git commit -m "feat(share): 新增分享端口区间随机模块

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### T4: 分享端口随机分配接入

- **Depends on:** T3

**Files:**
- Modify: `src-tauri/src/share/install.rs`（导入、`SharePortStarts`、`find_free_port`、`allocate_share_ports`）
- Modify（本地测试，不入库）: `tests/rust/share/install_tests.rs`（新增同次去重用例）
- Modify（本地测试，不入库）: `tests/rust/share/real_host_tests.rs`（真实主机断言改区间）

**Interfaces:**
- Consumes: T3 的 `random_start_in`、`range_candidates`、`PortRange`
- Produces:
  - `pub(crate) struct SharePortStarts { pub http: u16, pub pairs: Vec<(u16, u16)> }`
  - `pub(crate) async fn allocate_share_ports_with_starts(sess, cancel, transports, starts: &SharePortStarts) -> Result<AllocatedPorts, String>`
  - `pub async fn allocate_share_ports(sess, cancel, transports) -> Result<AllocatedPorts, String>`（签名不变）

- **验收:** `cargo test --features local-tests share::install -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；新增用例 `allocate_ports_skips_ports_allocated_in_same_call` 绿；既有 `allocate_ports_skips_busy_and_probes_both_families`、`allocate_ports_skips_ports_owned_by_stopped_shares` 仍绿。

- **状态:** completed ｜ **证据:** commit `2756977`；`cargo test ... share::install` → 16 passed（新增去重用例经三轮 RED 验证：去掉任一 insert 都会拿到重复端口；另补整段耗尽用例）；`cargo fmt --check` / clippy exit 0。审查：spec ✅，修复 I1 后复审「All findings addressed」；M1/M3 等 Minor 记入 ledger。

- [ ] **Step 1: 先写失败测试**

在 `tests/rust/share/install_tests.rs` 的导入里加入：

```rust
use crate::share::install::{allocate_share_ports_with_starts, SharePortStarts};
```

在 `allocate_ports_skips_ports_owned_by_stopped_shares` 之后追加：

```rust
/// 同一次调用里多个节点拿到同一起点时，`taken` 必须把已分配的端口排除掉；
/// 否则两个节点会绑同一个公网口，第二个实例启动时 `bind: address already in use`。
#[tokio::test]
async fn allocate_ports_skips_ports_allocated_in_same_call() {
    let dir = TempDir::new().unwrap();
    let _state = crate::share::tests::shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let mut sess = crate::share::tests::connect(&srv).await;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    // 只把预期会被选中的端口压成空闲；未命中回落 exit 0 = 已占用。
    srv.push_script("sport = :18080", 1, "", "");
    srv.push_script("sport = :18201", 1, "", "");
    srv.push_script("sport = :18202", 1, "", "");
    srv.push_script("sport = :18501", 1, "", "");
    srv.push_script("sport = :18502", 1, "", "");

    let starts = SharePortStarts {
        http: 18080,
        pairs: vec![(18201, 18501), (18201, 18501)],
    };
    let out = allocate_share_ports_with_starts(&mut sess, &cancel, &["tcp", "tcp"], &starts)
        .await
        .expect("分配端口");
    assert_eq!(out.http, 18080);
    assert_eq!(
        out.pairs,
        vec![(18201, 18501), (18202, 18502)],
        "第二个节点不得复用第一个节点已分配的端口"
    );
}
```

- [ ] **Step 2: 运行测试确认编译失败（RED）**

Run: `cd src-tauri; cargo test --features local-tests allocate_ports_skips_ports_allocated_in_same_call -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: 编译失败，`allocate_share_ports_with_starts` / `SharePortStarts` 不存在。

- [ ] **Step 3: 改 `install.rs` 的导入与结构**

在导入区加入：

```rust
use super::ports::{random_start_in, range_candidates};
```

在 `AllocatedPorts` 结构体（约 720 行）后加入：

```rust
/// 一次分享分配使用的随机起点（生产由线程 RNG 生成；测试可直接构造）。
#[derive(Debug, Clone)]
pub(crate) struct SharePortStarts {
    pub http: u16,
    /// (public, internal)，与 transports 同序。
    pub pairs: Vec<(u16, u16)>,
}

fn random_share_starts(transports: &[&str]) -> SharePortStarts {
    let mut rng = rand::thread_rng();
    SharePortStarts {
        http: random_start_in((PORT_RANGE_START, PORT_RANGE_END), &mut rng),
        pairs: transports
            .iter()
            .map(|_| {
                (
                    random_start_in((DATA_PORT_START, DATA_PORT_END), &mut rng),
                    random_start_in((INTERNAL_PORT_START, INTERNAL_PORT_END), &mut rng),
                )
            })
            .collect(),
    }
}
```

- [ ] **Step 4: 替换 `find_free_port` 与 `allocate_share_ports`**

把 `find_free_port`（约 726–768 行）整段替换为：

```rust
async fn find_free_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    range: (u16, u16),
    start: u16,
    transport: &str,
    excluded: &HashSet<u16>,
) -> Result<u16, String> {
    for port in range_candidates(range, start) {
        if excluded.contains(&port) {
            continue;
        }
        let mut free = true;
        if transport != "udp" {
            let out = run_cmd(
                sess,
                cancel,
                "探测",
                &tcp_port_probe_cmd(port),
                PROBE_TIMEOUT,
            )
            .await?;
            free &= out.exit_code != 0; // 非 0 = 空闲（与 port_probe_cmd 同方向）
        }
        if transport != "tcp" {
            let out = run_cmd(
                sess,
                cancel,
                "探测",
                &udp_port_probe_cmd(port),
                PROBE_TIMEOUT,
            )
            .await?;
            free &= out.exit_code != 0;
        }
        if free {
            return Ok(port);
        }
    }
    Err(format!("端口段 {}–{} 已耗尽", range.0, range.1))
}
```

把 `allocate_share_ports`（约 827–870 行）整段替换为：

```rust
pub(crate) async fn allocate_share_ports_with_starts(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    transports: &[&str],
    starts: &SharePortStarts,
) -> Result<AllocatedPorts, String> {
    debug_assert_eq!(
        starts.pairs.len(),
        transports.len(),
        "每个 transport 必须有一个随机起点"
    );
    let mut excluded = reserved_share_ports(sess, cancel).await?;
    let http = find_free_port(
        sess,
        cancel,
        (PORT_RANGE_START, PORT_RANGE_END),
        starts.http,
        "tcp",
        &excluded,
    )
    .await?;
    excluded.insert(http);
    let mut pairs = Vec::with_capacity(transports.len());
    for (i, transport) in transports.iter().enumerate() {
        let (public_start, internal_start) = starts.pairs[i];
        let public = find_free_port(
            sess,
            cancel,
            (DATA_PORT_START, DATA_PORT_END),
            public_start,
            transport,
            &excluded,
        )
        .await?;
        excluded.insert(public);
        let internal = find_free_port(
            sess,
            cancel,
            (INTERNAL_PORT_START, INTERNAL_PORT_END),
            internal_start,
            transport,
            &excluded,
        )
        .await?;
        excluded.insert(internal);
        pairs.push((public, internal));
    }
    Ok(AllocatedPorts { http, pairs })
}

/// 订阅端口、公网数据端口、回环内部端口三段各自独立分配；每段从随机起点
/// 开始探测、段尾回绕；已停止分享配置过的端口与本次已分配端口都跳过。
pub async fn allocate_share_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    transports: &[&str],
) -> Result<AllocatedPorts, String> {
    let starts = random_share_starts(transports);
    allocate_share_ports_with_starts(sess, cancel, transports, &starts).await
}
```

- [ ] **Step 5: 更新真实主机测试的精确端口断言**

在 `tests/rust/share/real_host_tests.rs` 约 210 行，把：

```rust
assert_eq!(info.port, 18081, "已停止分享的 18080 仍须保留");
```

改为：

```rust
assert!(
    (18080..=18180).contains(&info.port),
    "订阅口必须落在分享段内: {}",
    info.port
);
assert_ne!(info.port, 18080, "已停止分享的 18080 仍须保留");
```

该测试没有其它端口断言；数据口由 `info.pairs`/manifest 断言，不需要改。

- [ ] **Step 6: 运行测试确认 GREEN**

Run: `cd src-tauri; cargo test --features local-tests share::install -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: PASS。既有分享分配用例仍绿（FakeSshServer 未命中回落 exit 0 = 已占用，只有显式压成空闲的端口会被选中）；新增同次去重用例绿。

- [ ] **Step 7: 运行 fmt 与 clippy**

Run: `cd src-tauri; cargo fmt --check; cargo clippy --all-targets -- -D warnings`

Expected: 均退出 0。

- [ ] **Step 8: 提交（本地测试改动不入库）**

```bash
git add src-tauri/src/share/install.rs
git commit -m "feat(share): 分享端口改为区间内随机起点分配

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### T5: CDN 自动端口随机尝试顺序

- **Depends on:** T3

**Files:**
- Modify: `src-tauri/src/share/cdn.rs`（新增 `auto_candidates` 与内联测试）
- Modify: `src-tauri/src/share/install.rs`（`allocate_cdn_port` 使用轮转候选）

**Interfaces:**
- Consumes: `HTTPS_PORTS`（`cdn.rs` 已有）
- Produces: `pub(crate) fn auto_candidates(start: usize) -> Vec<u16>`

- **验收:** `cargo test --features local-tests auto_candidates -- --skip net::runtime_tests --skip net::runtime_service_tests` 与 `cargo test --features local-tests cdn -- --skip net::runtime_tests --skip net::runtime_service_tests` 通过；`occupied_cdn_port_is_not_overwritten_and_auto_uses_next` 仍绿。

- **状态:** completed ｜ **证据:** 提交 2756977..935fece（`feat(share): CDN 自动端口随机尝试顺序`）；`cargo test --features local-tests cdn` → 6 passed, 1 ignored（含 `auto_candidates_rotate_without_duplicates` 与本地 `auto_cdn_mode_probes_from_varying_start_ports`）；fmt/clippy 通过；review clean

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/share/cdn.rs` 文件末尾加入：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_candidates_rotate_without_duplicates() {
        let v = auto_candidates(4);
        assert_eq!(v.len(), HTTPS_PORTS.len());
        assert_eq!(v[0], HTTPS_PORTS[4]);
        assert_eq!(v[1], HTTPS_PORTS[5]);
        assert_eq!(v[2], HTTPS_PORTS[0], "表尾必须回绕");
        let set: std::collections::HashSet<u16> = v.iter().copied().collect();
        assert_eq!(set.len(), HTTPS_PORTS.len(), "候选不得重复");
        assert!(v.iter().all(|p| HTTPS_PORTS.contains(p)));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri; cargo test --features local-tests auto_candidates -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: 编译失败，`auto_candidates` 不存在。

- [ ] **Step 3: 实现 `auto_candidates`**

在 `src-tauri/src/share/cdn.rs` 的 `HTTPS_PORTS` 常量后加入：

```rust
/// 自动模式的候选顺序：从 `start` 起回绕整个 HTTPS 端口表。
///
/// 端口仍只能落在 Cloudflare 支持的固定集合里，随机只改变尝试顺序——
/// 这里**不能**扩大端口范围，Cloudflare 只代理这几个 HTTPS 端口。
pub(crate) fn auto_candidates(start: usize) -> Vec<u16> {
    let n = HTTPS_PORTS.len();
    (0..n).map(|i| HTTPS_PORTS[(start + i) % n]).collect()
}
```

- [ ] **Step 4: 改 `allocate_cdn_port`**

在 `src-tauri/src/share/install.rs` 的 `allocate_cdn_port`（约 794 行）里，把：

```rust
    let ports = if requested == 0 {
        super::cdn::HTTPS_PORTS.to_vec()
    } else {
        vec![requested]
    };
```

替换为：

```rust
    let ports = if requested == 0 {
        let n = super::cdn::HTTPS_PORTS.len();
        super::cdn::auto_candidates(rand::random::<usize>() % n)
    } else {
        vec![requested]
    };
```

- [ ] **Step 5: 运行测试确认 GREEN**

Run: `cd src-tauri; cargo test --features local-tests auto_candidates -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: PASS。

Run: `cd src-tauri; cargo test --features local-tests cdn -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: PASS；`occupied_cdn_port_is_not_overwritten_and_auto_uses_next` 仍绿（2083 被占、其余候选未命中回落为已占用，最终只有 2053 被选中）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/share/cdn.rs src-tauri/src/share/install.rs
git commit -m "feat(share): CDN 自动端口随机尝试顺序

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### T6: CHANGELOG 与全量验证

- **Depends on:** T1、T2、T3、T4、T5

**Files:**
- Modify: `CHANGELOG.md`（`## [未发布]` 下新增一节）

**Interfaces:**
- Consumes: 前五个 Task 的实现
- Produces: 用户可见的变更说明与最终验证证据

- **验收:** `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、完整本地测试命令全部退出 0；`CHANGELOG.md` 新增节存在；`grep -rn "default_port" src-tauri/src` 无输出。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 写 CHANGELOG**

在 `CHANGELOG.md` 的 `## [未发布]` 之后、第一条 `###` 之前插入：

```markdown
### 部署端口随机化（2026-11-01）

- **新增**：新部署的服务器协议（vless/trojan/vmess/ss/hysteria2/tuic）端口不再固定为
  443/4443/8443/8388/2097/2053，改为在 10000–65535（排除分享端口段 18080–18799）
  内随机起点分配；同一台机器一次部署多个协议时端口互不重复。
- **分享**：订阅口、公网数据口、回环内部口分别在各自端口段内随机起点分配；
  CDN 自动端口仍在 Cloudflare 支持的 6 个端口内，只是随机尝试顺序。
- **兼容**：重部署/编辑分享继续复用远端现有端口，存量节点与已发出的订阅不受影响。
- **边界**：随机端口必须在云服务商安全组放行才能从公网连上；vless 不再默认占用 443
  （REALITY 伪装收益减弱）；中转扩展端口的 DNAT 端口不参与主协议分配的去重。
```

- [ ] **Step 2: 全量检查**

Run: `cd src-tauri; cargo fmt --check`

Expected: 退出 0。

Run: `cd src-tauri; cargo clippy --all-targets -- -D warnings`

Expected: 退出 0。

Run: `cd src-tauri; cargo test --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests`

Expected: 全部通过；记录通过用例总数作为证据。

Run: `cd src-tauri; grep -rn "default_port" src`

Expected: 无输出（退出 1）。

- [ ] **Step 3: 提交**

```bash
git add CHANGELOG.md
git commit -m "docs: 记录部署端口随机化

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

## 自审记录

- **规格覆盖：**
  - 规格 §2 主协议池与分配 → T1 + T2。
  - 规格 §3.1 分享区间随机 → T3 + T4。
  - 规格 §3.2 同次去重 → T1（`skip_taken` 单测）+ T4（新集成用例）。
  - 规格 §3.3 CDN 自动口 → T5。
  - 规格 §4 兼容/不迁移 → T2 Step 1(e) 两次部署用例 + T2 Step 4 `taken` 初值。
  - 规格 §5 错误处理 → T2 `allocate_port` 错误文案 + T4 `find_free_port` 文案。
  - 规格 §6 测试 → T1/T3/T5 内联单测 + T2/T4 本地集成用例 + T6 全量命令。
  - 规格 §7 风险 → T6 CHANGELOG 边界。
- **占位符扫描：** 无 TBD/TODO；每个代码步骤都有完整代码。
- **类型一致性：** `SharePortStarts`、`allocate_share_ports_with_starts`、`pool_candidates`、`skip_taken` 在定义任务与消费任务中使用同一签名。
- **Goal Contract：** 原话、硬约束、编号 AC 齐全；每条 AC 对应到具体 Task 与命令。
- **模块地图：** 层级 / 调用 / 依赖三块分开，节点带中文功能名。
- **Task 卡：** T1–T6 均有稳定 ID、Depends on、验收、状态；依赖无悬空引用。
