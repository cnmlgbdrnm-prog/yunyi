# 分享流量管控与 sing-box 订阅 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. 执行中回写「执行状态」段。

**Goal Contract**

- **Original Goal:** 「现在分享的链接不支持流量管控。不支持sing-box ，现在分享的链接看一下，是什么问题？还有安全管理能不做好？」（后续确认：配额 + 上下行限速 + 到期 + 吊销全套；域名 + 自动 HTTPS；独立实例 + 独立凭据；旧分享不自动迁移；限速在网关内做；分享是快照。）
- **Hard Constraints:**
  - 一条分享只属于一台服务器；只允许该机上 `source=self-built`、`enabled`、非 ssr、非隐私线、非上游出口、未挂中转链路的节点。后端必须重新校验，不能只信前端。
  - 新分享强制 HTTPS：每台服务器 `share_domain` + 全局 `share_acme_email`；未配置或 ACME 失败即拒绝创建，**不退回 HTTP**。
  - 管控口径：配额 = 上下行网关转发字节之和；限速单位 Mbps、按分享聚合、0 = 不限；到期按服务器时钟；超额/到期关闭监听并断开连接；作废后已下载配置立即失效。
  - 限速只在网关进程内做，**不使用 `tc`/qdisc**，不影响主服务与系统其他流量。
  - 分享实例从主服务配置克隆入站，只改端口、监听地址与凭据；入站必须 `127.0.0.1`。分享是快照，主服务后续改动不更新已有分享。
  - 不建本地分享表；服务器是唯一事实源。旧分享不自动迁移，只列举/作废并补防火墙持久化。
  - 端口：订阅 18080-18180；数据公网 18200-18499；回环 18500-18799。分配前 TCP/UDP 都探测，不覆盖。
  - 新 token 32 位小写十六进制（128 bit），内部 id 16 位；旧 16 位 token 仍可读。变更操作只接受 id。
  - 新分享的订阅服务/网关/实例以系统用户 `vpnshare` 运行；共享脚本 root:root 0755；分享目录 0700。
  - 默认值：配额 100 GB（1 GB = 1024³）、上下行各 100 Mbps、有效期 30 天。
  - Python 网关只用标准库；不新增 Rust 依赖；不新增发布二进制。
  - 工作区已有未提交改动（`CHANGELOG.md`、`commands/mod.rs`、`servers/channels.rs`、`ChannelPanel.tsx`、`ServerRow.tsx`、`Servers.tsx`）。**只能按路径/按 hunk 暂存本次任务自己的改动，严禁 `git add -A` / `git add .`。**
  - **`tests/` 整个目录被 `.gitignore` 的 `/tests/` 忽略**（Cargo.toml 也注明测试源码不随仓库上传）。所有提交命令里的 `git add ... tests/...` 都不执行；测试仍按验收命令在本机跑，提交只包含 `src*/docs/CHANGELOG` 等非忽略路径。
  - 注释、错误文案、README/CHANGELOG、提交信息一律中文。
  - 测试门禁：`cargo test --manifest-path src-tauri/Cargo.toml --features local-tests`（带 `--skip net::runtime_tests --skip net::runtime_service_tests`）、`npx tsc --noEmit`、`npm test`、`python -m unittest discover -s tests/python`。
- **Acceptance Criteria:**
  - A1: 新分享同时提供 `https://<域名>:<端口>/sub/<token>`（YAML）与 `/singbox/<token>`（JSON）；两者都能导入 mihomo / sing-box 并实际连通。
  - A2: 跑流量后 `usage.json` 增长；达到配额、到期、手动作废三种情况下，已导入的旧配置立即连不上。
  - A3: 限速设置生效，实测吞吐接近设定值（允许令牌桶/TCP 背压带来的合理偏差）。
  - A4: 关闭桌面应用、重启服务器后分享仍可用；单元与防火墙规则自动恢复。
  - A5: 旧分享仍能列出/作废；其 YAML 链接与行为不变；重启后不再失联。
  - A6: 不满足准入条件的节点在向导中置灰并给出原因，后端创建时同样拒绝。
  - A7: 四道测试门禁全绿；新增/修改测试可在命令输出中指认。
  - A8: 新分享只有 HTTPS；证书由 certbot 签发且续期钩子可执行；没有明文 HTTP 降级路径。
  - A9: 创建任一步失败后，服务器上不残留本次创建的单元/端口/目录。
  - A10: 不新增本地分享表；`list_shares` 不依赖本地状态。

## 模块地图（ASCII）

**层级（包含关系）**

```
SharesView（分享页）
├─ ShareWizard（新建向导）
├─ ShareEditPanel（编辑配额/速率/到期）
└─ ShareRow（单条分享行）
ShareModule（后端分享模块）
├─ ShareModel（类型与状态）
├─ ShareNaming（id/token/路径/URL）
├─ SharePlan（准入、候选、端口计划）
├─ ShareClone（Xray/sing-box/SS 配置克隆）
├─ ShareTls（域名/ACME/证书）
├─ ShareInstall（创建/列举/更新/作废编排）
└─ ShareScripts（服务器端 Python/Shell 脚本）
```

**调用（运行时触发）**

```
SharesView ──▶ list_shares_cmd / create_share_cmd / update_share_cmd / reset_share_usage_cmd / delete_share_cmd
ShareInstall ──▶ SharePlan（准入与端口）──▶ ShareClone（克隆）──▶ write_remote_file / systemctl / curl 验证
ShareGateway（Python）──▶ charge（配额）──▶ block（关闭监听、断开连接）
ShareTls ──▶ certbot ──▶ deploy hook ──▶ 重启 vpnshare-* 服务
```

**依赖（数据 / 契约来源）**

```
ShareModel ⇠── ShareManifest / UsageState / GatewayConfig（服务器文件）
ShareClone ⇠── 主服务 Xray/Sing-box/SS 配置（SSH 读取）
ShareInstall ⇠── domain::Server（share_domain）与 repos::upstreams/privacies
SharesView ⇠── src/api/types.ts（ShareInfo/ShareCandidate/ShareTlsStatus）
```

## 执行状态（进度指针）

- 当前：T12（pending）
- 下一动作：实现 `ShareWizard`/`ShareEditPanel`，接候选与 TLS 状态；补服务器分享域名与设置页 ACME 邮箱
- 阻塞：无
- 已完成：T1（`c820609`）、T2（`234a13c`）、T3（`91ca458`）、T4（`d2d5233`）、T5（`103556d`）、T6（`81a0373`）、T7（`ccd8fb1`）、T8（`dfbf95f`）、T9（`df8c0b3`）、T10（`9697201`）、T11（`1897223`）
- 最后更新：2026-10-20

## 全局前置事实（实现前必读）

1. **`FakeSshServer` 未命中回落 `exit 0` + 空 stdout**（`tests/rust/ssh/tests.rs`）。每个探测命令都必须显式 `push_script`，并断言命令序列；否则用例会假过。
2. **读远端文件用 `script_remote_conf(srv, path, Some/None)`**（`ssh/tests.rs:263`）。手写 `push_script("cat X")` 命中不了 `if [ -e 'X' ]` 的真实命令。
3. **写远端文件**：`write_remote_file(sess, cancel, step, path, content, timeout)`；测试用 `srv.files()` 断言最终内容。
4. **跑命令**：`run_cmd(sess, cancel, step, cmd, timeout)` / `run_cmd_ok(...)`（`servers/deploy.rs`）。
5. **端口探测**：`port_probe_cmd(p)` 退出码 0 = 已占用（方向别写反）。UDP 用
   `ss -uln 'sport = :N' | grep -q '^UNCONN'`，TCP 用 `ss -tln ... | grep -q '^LISTEN'`；
   两者都**不能**用 `grep -q .`——无匹配时 ss 仍打印表头，会把空闲端口误报为占用。
6. **测试状态装配**：照 `tests/rust/share/tests.rs` 的 `shared_state` / `seed_server` / `seed_node` / `connect`；新测试复用它们。
7. **配置生成**：`crate::config::build_yaml_with_privacy(nodes, false, &[], &[], &[])` 与 `crate::config::singbox::build_singbox_with_privacy(nodes, &[])`；分享用空规则/空服务器，避免把 owner 的分流/隐私带进快照。
8. **主配置解析**：Xray 入站形状见 `deploy_spec::xray_config_json_multi`；sing-box 见 `singbox_config_json`；SS 见 `ss_config_json`。
9. **Tauri 命令注册**：`src-tauri/src/main.rs` 的 `invoke_handler` 与 `tests/rust/ipc_tests.rs` 的 `mock_app` 两处都要加新命令。
10. **提交**：每个 Task 末尾只 `git add <本任务列出的文件>`；对工作区已有改动的文件用 `git add -p` 只挑本任务 hunk；提交前 `git diff --cached --stat` 核对。

## 文件结构

**新增（Rust）**

```
src-tauri/src/share/model.rs        // 类型：ShareInfo/ShareStatus/Manifest/Usage/GatewayConfig/Options
src-tauri/src/share/naming.rs       // id/token/路径/单元名/URL
src-tauri/src/share/plan.rs         // 准入、候选、端口计划
src-tauri/src/share/clone.rs        // Xray/sing-box/SS 配置克隆与订阅生成
src-tauri/src/share/tls.rs          // 域名/ACME/证书/续期钩子
src-tauri/src/share/install.rs      // 创建/列举/更新/重置/作废
src-tauri/src/share/scripts.rs      // include_str! 脚本常量
src-tauri/src/share/scripts/share_server.py
src-tauri/src/share/scripts/share_gateway.py
src-tauri/src/share/scripts/share_summary.py
src-tauri/src/share/scripts/vpnshare-firewall.sh
```

**修改（Rust）**

```
src-tauri/src/share/mod.rs          // 保留公开 API，改为 mod + re-export
src-tauri/src/db/migrations.rs      // 迁移 v18
src-tauri/src/db/repos.rs           // servers COLS / insert / update / row_to_server
src-tauri/src/domain.rs             // Server.share_domain / Create/UpdateServerInput
src-tauri/src/servers/mod.rs        // 域名归一化与 create/update
src-tauri/src/commands/ipc.rs       // 新 IPC 命令
src-tauri/src/commands/mod.rs       // update_server 透传 share_domain
src-tauri/src/main.rs               // 注册命令
src-tauri/src/settings.rs           // SHARE_ACME_EMAIL_KEY
```

**前端**

```
src/api/types.ts                    // ShareInfo/ShareStatus/ShareCandidate/ShareTlsStatus；Server.share_domain
src/api/index.ts                    // 新命令封装
src/views/Shares.tsx                // 列表/向导/编辑
src/components/ShareWizard.tsx      // 新建向导
src/components/ShareEditPanel.tsx   // 编辑配额/速率/到期
src/components/ServerFormPanel.tsx  // 分享域名字段
src/views/Servers.tsx               // 表单状态与 API 调用
src/views/Settings.tsx              // ACME 邮箱
```

**测试**

```
tests/rust/share/tests.rs           // 保留并更新旧用例
tests/rust/share/naming_tests.rs
tests/rust/share/plan_tests.rs
tests/rust/share/clone_tests.rs
tests/rust/share/tls_tests.rs
tests/rust/share/install_tests.rs
tests/rust/share/list_tests.rs
tests/rust/share/manage_tests.rs
tests/rust/share/scripts_tests.rs
tests/python/__init__.py
tests/python/test_share_gateway.py
tests/python/test_share_server.py
tests/python/test_share_summary.py
tests/frontend/views/Shares.test.tsx
tests/frontend/components/ShareWizard.test.tsx
tests/frontend/views/Settings.test.tsx
tests/frontend/views/Servers.test.tsx
```

---

### T1: 数据层 — servers.share_domain + 设置键 + 表单

- **Depends on:** 无

**Files:**
- Modify: `src-tauri/src/db/migrations.rs`
- Modify: `src-tauri/src/db/repos.rs:823-961`
- Modify: `src-tauri/src/domain.rs:93-140,216-245`
- Modify: `src-tauri/src/servers/mod.rs:160-300`
- Modify: `src-tauri/src/commands/ipc.rs:244-305`
- Modify: `src-tauri/src/commands/mod.rs:550-610`
- Modify: `src-tauri/src/settings.rs`
- Modify: `src/api/types.ts:21-40,94-120`（Server 类型）
- Modify: `src/api/index.ts:255-283`
- Modify: `src/components/ServerFormPanel.tsx`
- Modify: `src/views/Servers.tsx`
- Test: `tests/rust/db/tests.rs`、`tests/rust/servers/tests.rs`、`tests/frontend/views/Servers.test.tsx`

**Interfaces:**
- Produces: `Server.share_domain: String`；`CreateServerInput.share_domain: String`；`UpdateServerInput.share_domain: Option<String>`；`settings::SHARE_ACME_EMAIL_KEY`。
- Produces: `servers::normalize_share_domain(&str) -> Result<String, String>`。

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests migration_v18` 绿；`cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share_domain` 绿；`npx tsc --noEmit` 绿。

- **状态:** completed ｜ **证据:** commit `c820609`；`cargo test --features local-tests migration_v18` 1 passed；`share_domain` 3 passed；`npx tsc --noEmit` 无输出；`npm test -- Servers` 57 passed；`npm run test:typecheck` 无输出。

- [x] **Step 1: 写迁移失败测试**

`tests/rust/db/tests.rs` 追加：

```rust
#[test]
fn migration_v18_adds_share_domain_defaulting_to_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::Db::open(&dir.path().join("t.db")).unwrap();
    let conn = db.lock();
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(servers)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "share_domain"), "缺 share_domain 列: {cols:?}");
    assert!(crate::db::migrations::latest_version() >= 18);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests migration_v18`
Expected: FAIL，`no such column: share_domain`

- [ ] **Step 3: 加迁移与字段**

`migrations.rs` 的 `MIGRATIONS` 末尾追加：

```sql
-- v18：分享域名。每台服务器一个，解析到该机；新分享用它签证书并生成 https 链接。
-- 空串 = 未配置，新建分享会被拒绝（绝不退回 HTTP）。
ALTER TABLE servers ADD COLUMN share_domain TEXT NOT NULL DEFAULT '';
```

`domain.rs` 的 `Server` 末尾追加：

```rust
    /// 分享域名（解析到该机）；空串 = 未配置。新分享强制 HTTPS。
    pub share_domain: String,
```

`CreateServerInput` 追加 `pub share_domain: String,`；`UpdateServerInput` 追加 `pub share_domain: Option<String>,`。

`repos.rs` 的 `servers` 模块：

```rust
const COLS: &str = "id,name,host,port,user,auth_kind,kind,system,notes,created_at,latency,bandwidth,last_checked_at,last_check_error,host_v6,addr_family,egress_family,share_domain";
```

插入/更新 SQL 追加 `share_domain`：

```rust
"INSERT INTO servers (name,host,host_v6,addr_family,egress_family,share_domain,port,user,auth_kind,auth_ref,kind,system,notes,created_at)
 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"
```

```rust
"UPDATE servers SET name=?2,host=?3,host_v6=?4,addr_family=?5,egress_family=?6,share_domain=?7,port=?8,user=?9,auth_kind=?10,kind=?11,notes=?12 WHERE id=?1"
```

对应 `params!`：

```rust
// insert
params![s.name, s.host, s.host_v6, s.addr_family, s.egress_family, s.share_domain,
        s.port, s.user, s.auth_kind, auth_ref, s.kind, s.system, s.notes, s.created_at]
// update
params![s.id, s.name, s.host, s.host_v6, s.addr_family, s.egress_family, s.share_domain,
        s.port, s.user, s.auth_kind, s.kind, s.notes]
```

`row_to_server` 末尾加 `share_domain: r.get(17)?,`。

- [ ] **Step 4: 域名归一化与 create/update 透传**

`servers/mod.rs`：

```rust
/// 分享域名归一化：只接受裸域名（可空）。空串 = 未配置。
/// 拒绝 scheme、路径、端口、空格与大写；避免把 URL 当域名拼进 certbot 命令。
pub fn normalize_share_domain(raw: &str) -> Result<String, String> {
    let d = raw.trim().to_ascii_lowercase();
    if d.is_empty() {
        return Ok(d);
    }
    if d.len() > 253 || d.contains("://") || d.contains('/') || d.contains(':') || d.contains(' ') {
        return Err("分享域名只能是裸域名，例如 share.example.com".into());
    }
    let ok = d.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    if !ok {
        return Err("分享域名格式不合法".into());
    }
    Ok(d)
}
```

`create_server` 设置 `share_domain: normalize_share_domain(&input.share_domain)?`；`update_server` 用 `match input.share_domain { Some(v) => normalize_share_domain(&v)?, None => existing.share_domain.clone() }`。

`commands/mod.rs` 的 `update_server` 在构造 `Server` 时带上 `share_domain`；`commands/ipc.rs` 的 create/update server 命令增加参数并把 camelCase `shareDomain` 映射进来。

`settings.rs` 加：

```rust
/// 分享域名的 ACME 邮箱（签发证书时使用；空 = 未配置）。
pub const SHARE_ACME_EMAIL_KEY: &str = "share_acme_email";
```

- [ ] **Step 5: 前端类型与表单**

`src/api/types.ts` 的 `Server` 加 `share_domain: string`；`createServer`/`updateServer` 参数加 `shareDomain: string`。

`ServerFormPanel.tsx` 的 `ServerFormState` 加 `shareDomain: string`，`EMPTY_SERVER_FORM` 加 `shareDomain: ''`；在「出站地址族」下方加：

```tsx
<label className="block text-sm">
  分享域名（可选）
  <input
    aria-label="分享域名"
    className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 font-mono text-xs"
    value={f.shareDomain}
    onChange={(e) => onChange({ ...f, shareDomain: e.target.value })}
    placeholder="share.example.com（新分享强制 HTTPS，需解析到本机）"
  />
</label>
```

`Servers.tsx` 的表单初始化与 `createServer`/`updateServer` 调用带上 `shareDomain`。

- [ ] **Step 6: 跑测试与门禁**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share_domain`
Expected: PASS

补齐所有构造点（新增字段会让所有字面量编译失败）：

```bash
rg -n "CreateServerInput \{" src-tauri/src tests/rust
rg -n "Server \{" src-tauri/src tests/rust
rg -n "interface Server|const server: Server|: Server = \{" tests/frontend src
```

每个 `CreateServerInput`/`UpdateServerInput` 加 `share_domain`/`share_domain: None`；每个 `Server` 字面量加 `share_domain: String::new()` 或 `share_domain: ''`。

Run: `npx tsc --noEmit`
Expected: PASS

- [ ] **Step 7: 提交（只暂存本任务路径）**

```bash
git add src-tauri/src/db/migrations.rs src-tauri/src/db/repos.rs src-tauri/src/domain.rs src-tauri/src/servers/mod.rs src-tauri/src/commands/ipc.rs src-tauri/src/commands/mod.rs src-tauri/src/settings.rs src/api/types.ts src/api/index.ts src/components/ServerFormPanel.tsx tests/rust/db/tests.rs tests/rust/servers/tests.rs
git add -p src/views/Servers.tsx
git commit -m "feat(share): 增加服务器分享域名与 ACME 设置"
```

---

### T2: 分享模型 — id/token/路径/URL/状态/清单结构

- **Depends on:** T1

**Files:**
- Create: `src-tauri/src/share/model.rs`
- Create: `src-tauri/src/share/naming.rs`
- Modify: `src-tauri/src/share/mod.rs`
- Modify: `tests/rust/share/mods.rs`
- Create: `tests/rust/share/naming_tests.rs`
- Modify: `tests/rust/share/tests.rs`

**Interfaces:**
- Produces:
  - `naming::{SHARE_ROOT, PORT_RANGE_START, PORT_RANGE_END, DATA_PORT_START, DATA_PORT_END, INTERNAL_PORT_START, INTERNAL_PORT_END}`
  - `naming::{is_valid_id, is_valid_token, new_id, new_token, share_dir, manifest_path, gateway_config_path, usage_path, sub_dir, yaml_path, singbox_path, instance_config_path, instance_unit_name, gateway_unit_name, unit_name, unit_path, tls_dir, share_server_script_path, share_gateway_script_path, firewall_script_path}`
  - `naming::{url_yaml, url_singbox, legacy_url}`
  - `model::{ShareInfo, ShareStatus, ShareList, ShareCandidate, ShareTlsStatus, ShareOptions, ShareManifest, ShareNode, GatewayConfig, GatewayPort, UsageState}`
  - `model::{quota_gb_to_bytes, validate_options}`

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::naming` 绿；`cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::model` 绿。

- **状态:** completed ｜ **证据:** commit `234a13c`；`share::naming` 3 passed；`share::model` 4 passed。

- [x] **Step 1: 写命名测试**

`tests/rust/share/naming_tests.rs`：

```rust
use crate::share::naming::*;

#[test]
fn ids_are_16_hex_and_tokens_are_16_or_32() {
    assert!(is_valid_id("0123456789abcdef"));
    assert!(!is_valid_id("0123456789abcdef0"));
    assert!(!is_valid_id("0123456789ABCDEF"));
    assert!(is_valid_token("0123456789abcdef"));
    assert!(is_valid_token("0123456789abcdef0123456789abcdef"));
    assert!(!is_valid_token("0123456789abcdef0"));
    assert!(!is_valid_token("0123456789abc;rm -rf /"));
    assert_eq!(new_id().len(), 16);
    assert_eq!(new_token().len(), 32);
    assert!(is_valid_token(&new_token()));
}

#[test]
fn paths_are_derived_from_internal_id_and_token() {
    let id = "0123456789abcdef";
    let t = "fedcba9876543210fedcba9876543210";
    assert_eq!(share_dir(id), "/srv/vpnshare/0123456789abcdef");
    assert_eq!(manifest_path(id), "/srv/vpnshare/0123456789abcdef/share.json");
    assert_eq!(yaml_path(id, t), format!("/srv/vpnshare/{id}/sub/{t}"));
    assert_eq!(singbox_path(id, t), format!("/srv/vpnshare/{id}/sub/{t}.json"));
    assert_eq!(unit_name(id), "vpnshare-0123456789abcdef");
    assert_eq!(gateway_unit_name(id), "vpnshare-0123456789abcdef-gateway");
    assert_eq!(instance_unit_name(id, "xray"), "vpnshare-0123456789abcdef-xray");
    assert_eq!(
        unit_path("vpnshare-0123456789abcdef"),
        "/etc/systemd/system/vpnshare-0123456789abcdef.service"
    );
}

#[test]
fn urls_use_https_and_bracket_ipv6() {
    assert_eq!(
        url_yaml("share.example.com", 18080, "abc"),
        "https://share.example.com:18080/sub/abc"
    );
    assert_eq!(
        url_singbox("share.example.com", 18080, "abc"),
        "https://share.example.com:18080/singbox/abc"
    );
    assert_eq!(
        legacy_url("2001:db8::1", 18080, "abc"),
        "http://[2001:db8::1]:18080/sub/abc"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::naming`
Expected: FAIL，`unresolved import crate::share::naming`

- [ ] **Step 3: 实现 naming 与 model**

`naming.rs` 关键实现：

```rust
pub const SHARE_ROOT: &str = "/srv/vpnshare";
pub const UNIT_PREFIX: &str = "vpnshare";
pub const PORT_RANGE_START: u16 = 18080;
pub const PORT_RANGE_END: u16 = 18180;
pub const DATA_PORT_START: u16 = 18200;
pub const DATA_PORT_END: u16 = 18499;
pub const INTERNAL_PORT_START: u16 = 18500;
pub const INTERNAL_PORT_END: u16 = 18799;

pub fn is_valid_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn is_valid_token(s: &str) -> bool {
    (s.len() == 16 || s.len() == 32)
        && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn new_id() -> String { uuid::Uuid::new_v4().simple().to_string()[..16].to_string() }
pub fn new_token() -> String { uuid::Uuid::new_v4().simple().to_string() }

pub fn share_dir(id: &str) -> String { format!("{SHARE_ROOT}/{id}") }
pub fn manifest_path(id: &str) -> String { format!("{SHARE_ROOT}/{id}/share.json") }
pub fn gateway_config_path(id: &str) -> String { format!("{SHARE_ROOT}/{id}/gateway.json") }
pub fn usage_path(id: &str) -> String { format!("{SHARE_ROOT}/{id}/usage.json") }
pub fn sub_dir(id: &str) -> String { format!("{SHARE_ROOT}/{id}/sub") }
pub fn yaml_path(id: &str, token: &str) -> String { format!("{SHARE_ROOT}/{id}/sub/{token}") }
pub fn singbox_path(id: &str, token: &str) -> String { format!("{SHARE_ROOT}/{id}/sub/{token}.json") }
pub fn instance_config_path(id: &str, kind: &str) -> String { format!("{SHARE_ROOT}/{id}/{kind}.json") }
pub fn instance_unit_name(id: &str, kind: &str) -> String { format!("{UNIT_PREFIX}-{id}-{kind}") }
pub fn gateway_unit_name(id: &str) -> String { format!("{UNIT_PREFIX}-{id}-gateway") }
pub fn unit_name(id: &str) -> String { format!("{UNIT_PREFIX}-{id}") }
pub fn unit_path(unit: &str) -> String { format!("/etc/systemd/system/{unit}.service") }
pub fn tls_dir() -> String { format!("{SHARE_ROOT}/tls") }
pub fn share_server_script_path() -> String { format!("{SHARE_ROOT}/share_server.py") }
pub fn share_gateway_script_path() -> String { format!("{SHARE_ROOT}/share_gateway.py") }
pub fn share_summary_script_path() -> String { format!("{SHARE_ROOT}/share_summary.py") }
pub fn firewall_script_path() -> String { format!("{SHARE_ROOT}/vpnshare-firewall.sh") }

pub fn url_yaml(domain: &str, port: u16, token: &str) -> String {
    format!("https://{domain}:{port}/sub/{token}")
}
pub fn url_singbox(domain: &str, port: u16, token: &str) -> String {
    format!("https://{domain}:{port}/singbox/{token}")
}
pub fn legacy_url(host: &str, port: u16, token: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("http://{host}:{port}/sub/{token}")
}
```

`model.rs` 类型与校验：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareInfo {
    pub id: String,
    pub server_id: i64,
    pub server_name: String,
    pub host: String,
    pub token: String,
    pub port: u16,
    pub active: bool,
    pub url: String,
    pub url_singbox: String,
    pub node_names: Vec<String>,
    pub legacy: bool,
    pub status: ShareStatus,
    pub status_detail: Option<String>,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at: Option<String>,
    pub last_access_at: Option<String>,
    pub last_access_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ShareStatus {
    Active,
    BlockedQuota,
    BlockedExpired,
    Degraded,
    Legacy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareList {
    pub shares: Vec<ShareInfo>,
    pub unreachable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareCandidate {
    pub node: Node,
    pub shareable: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareTlsStatus {
    pub domain: String,
    pub ready: bool,
    pub cert_expires_at: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShareOptions {
    pub quota_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_in_days: u32,
}

pub fn quota_gb_to_bytes(gb: u64) -> u64 { gb.saturating_mul(1024 * 1024 * 1024) }

pub fn validate_options(o: &ShareOptions) -> Result<(), String> {
    if o.quota_bytes == 0 { return Err("配额必须大于 0".into()); }
    if o.rate_up_mbps > 100_000 || o.rate_down_mbps > 100_000 {
        return Err("速率上限过大".into());
    }
    if o.expires_in_days == 0 || o.expires_in_days > 3650 {
        return Err("有效期必须在 1-3650 天之间".into());
    }
    Ok(())
}
```

清单/网关/用量结构：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareManifest {
    pub version: u32,
    pub id: String,
    pub token: String,
    pub server_id: i64,
    pub created_at: String,
    pub quota_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at_unix: i64,
    pub http_port: u16,
    pub domain: String,
    pub nodes: Vec<ShareNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareNode {
    pub name: String,
    pub protocol: String,
    pub public_port: u16,
    pub internal_port: u16,
    pub transport: String,
    pub params_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayConfig {
    pub quota_bytes: u64,
    pub rate_up_mbps: u32,
    pub rate_down_mbps: u32,
    pub expires_at_unix: i64,
    pub ports: Vec<GatewayPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayPort {
    pub public: u16,
    pub target: u16,
    pub proto: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageState {
    #[serde(default)] pub used_bytes: u64,
    #[serde(default)] pub blocked: Option<String>,
    #[serde(default)] pub blocked_at: Option<String>,
    #[serde(default)] pub updated_at: String,
}
```

- [ ] **Step 4: 挂载模块与测试**

`share/mod.rs` 顶部加：

```rust
pub mod model;
pub mod naming;
```

**不要**写 `pub use model::*; pub use naming::*;`：`share/mod.rs` 里还留着旧实现的同名项
（`ShareInfo`、`ShareList`、`SHARE_ROOT`、`is_valid_token`、`share_dir`、`unit_name`、
`unit_path`、`share_server_script_path`…），glob 重导出会直接撞名。T2 到 T9 的新模块
（`plan.rs`/`clone.rs`/`tls.rs`/`scripts.rs`/`install.rs`）一律在文件头写：

```rust
use crate::share::model::*;
use crate::share::naming::*;
```

T10 切换 IPC 并删除旧实现后，才在 `share/mod.rs` 根部重导出新类型。

`tests/rust/share/mods.rs` 加 `pub mod naming_tests;`、`pub mod model_tests;`（若新增 model 测试）。

- [ ] **Step 5: 跑测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::naming`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/share/model.rs src-tauri/src/share/naming.rs src-tauri/src/share/mod.rs tests/rust/share/mods.rs tests/rust/share/naming_tests.rs
git commit -m "feat(share): 分享模型、内部 id 与 token 规则"
```

---

### T3: 准入、候选清单与端口计划

- **Depends on:** T2

**Files:**
- Create: `src-tauri/src/share/plan.rs`
- Create: `tests/rust/share/plan_tests.rs`
- Modify: `tests/rust/share/mods.rs`

**Interfaces:**
- Produces: `plan::{Eligibility, check_eligibility, share_candidates, tcp_port_probe_cmd, udp_port_probe_cmd}`
- Consumes: `state.subs.visible_nodes()`, `repos::upstreams::list_by_server`, `repos::privacies::list_all`, `domain::server_owns_addr`, `domain::direct_addr`.

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::plan` 绿。

- **状态:** completed ｜ **证据:** commit `91ca458`；`share::plan` 2 passed（准入拒绝矩阵 + 端口探测命令不匹配表头）。

- [x] **Step 1: 写准入失败测试**

先把 `tests/rust/share/tests.rs` 的 `shared_state` / `seed_server` / `seed_node` / `connect` 改成 `pub(crate)`（兄弟测试模块要复用）。

`tests/rust/share/plan_tests.rs`（复用 `super::tests::{shared_state, seed_server, seed_node}`）：

```rust
use crate::share::plan::check_eligibility;

#[tokio::test]
async fn eligibility_rejects_non_shareable_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let state = super::tests::shared_state(&dir);
    let srv = crate::ssh::tests::FakeSshServer::start().await;
    let server_id = super::tests::seed_server(&state, &srv);
    let server = crate::db::repos::servers::get(&state.db.lock(), server_id).unwrap().unwrap();

    let mut n = super::tests::seed_node(&state, "n1", "节点");
    // seed_node 默认 addr=1.2.3.4，先改成本机地址才属于 seed_server 的 127.0.0.1
    n.addr = "127.0.0.1".into();
    // 正常自建节点应可分享
    assert!(check_eligibility(&n, &server, &Default::default(), &[]).is_ok());

    // 不是自建
    n.source = "import".into();
    assert!(check_eligibility(&n, &server, &Default::default(), &[]).is_err());

    // 不属于该机（direct_addr 既不是 host 也不是 host_v6）
    n.source = "self-built".into();
    n.addr = "9.9.9.9".into();
    assert!(check_eligibility(&n, &server, &Default::default(), &[]).is_err());

    // 未启用 / ssr / 挂中转 / 上游
    n.addr = "127.0.0.1".into();
    n.enabled = false;
    assert!(check_eligibility(&n, &server, &Default::default(), &[]).is_err());
    n.enabled = true;
    n.protocol = "ssr".into();
    assert!(check_eligibility(&n, &server, &Default::default(), &[]).is_err());
    n.protocol = "ss".into();
    n.relay_rule_id = Some(1);
    assert!(check_eligibility(&n, &server, &Default::default(), &[]).is_err());
    n.relay_rule_id = None;
    let mut upstreams = std::collections::HashSet::new();
    upstreams.insert("n1".to_string());
    assert!(check_eligibility(&n, &server, &upstreams, &[]).is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::plan`
Expected: FAIL，`unresolved import crate::share::plan`

- [ ] **Step 3: 实现准入与候选**

`plan.rs`：

```rust
use crate::domain::{direct_addr, server_owns_addr, Node, Privacy, Server};
use crate::state::AppState;
use std::collections::HashSet;
use std::sync::Arc;

pub fn check_eligibility(
    node: &Node,
    server: &Server,
    upstream_ids: &HashSet<String>,
    privacies: &[Privacy],
) -> Result<(), String> {
    if node.source != "self-built" { return Err("不是本应用部署的自建节点".into()); }
    if !server_owns_addr(server, &direct_addr(node)) {
        return Err("节点不属于所选服务器".into());
    }
    if !node.enabled { return Err("节点未启用".into()); }
    if node.protocol == "ssr" { return Err("mihomo 与 sing-box 已移除 SSR 支持".into()); }
    if crate::servers::privacy::effective_privacy(&node.id, privacies).is_some() {
        return Err("隐私线不可分享（需要本机 Tor）".into());
    }
    if upstream_ids.contains(&node.id) { return Err("上游出口节点不可分享".into()); }
    if node.relay_rule_id.is_some() { return Err("挂中转链路的节点不可分享（会绕过中转）".into()); }
    Ok(())
}

pub fn share_candidates(state: &Arc<AppState>, server_id: i64) -> Result<Vec<ShareCandidate>, String> {
    let (server, upstream_ids, privacies) = {
        let conn = state.db.lock();
        let server = crate::db::repos::servers::get(&conn, server_id)
            .map_err(|e| e.to_string())?
            .ok_or("服务器不存在")?;
        let upstream_ids = crate::db::repos::upstreams::list_by_server(&conn, server_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|u| u.node_id)
            .collect::<HashSet<_>>();
        let privacies = crate::db::repos::privacies::list_all(&conn).map_err(|e| e.to_string())?;
        (server, upstream_ids, privacies)
    };
    let nodes = state.subs.visible_nodes().map_err(|e| e.to_string())?;
    Ok(nodes
        .into_iter()
        // 向导只列属于该服务器的节点；别把别的机器/机场节点混进来
        .filter(|node| server_owns_addr(&server, &direct_addr(node)))
        .map(|node| {
            let reason = check_eligibility(&node, &server, &upstream_ids, &privacies).err();
            ShareCandidate { shareable: reason.is_none(), reason, node }
        })
        .collect())
}
```

- [ ] **Step 4: 端口探测命令**

```rust
/// 退出码 0 = 已占用（与 port_probe_cmd 同方向）。
/// **必须只匹配数据行**：无匹配时 ss 仍会打印表头，`grep -q .` 会把空闲端口误报为占用
/// （deploy_spec::port_probe_cmd 已踩过这个坑）。
pub fn tcp_port_probe_cmd(port: u16) -> String {
    format!("ss -tln 'sport = :{port}' | grep -q '^LISTEN'")
}
pub fn udp_port_probe_cmd(port: u16) -> String {
    format!("ss -uln 'sport = :{port}' | grep -q '^UNCONN'")
}
```

- [ ] **Step 5: 跑测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::plan`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/share/plan.rs tests/rust/share/plan_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 节点准入与分享候选清单"
```

---

### T4: 配置克隆 — Xray / sing-box / SS

- **Depends on:** T2、T3

**Files:**
- Create: `src-tauri/src/share/clone.rs`
- Create: `tests/rust/share/clone_tests.rs`
- Modify: `tests/rust/share/mods.rs`

**Interfaces:**
- Produces: `clone::{ShareNodeSpec, ClonedConfigs, clone_configs}`
- Consumes: 主配置字符串（Xray/Sing-box/SS）、`Node` 原参数、`config::build_yaml_with_privacy`、`config::singbox::build_singbox_with_privacy`。

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::clone` 绿。

- **状态:** completed ｜ **证据:** commit `d2d5233`；`share::clone` 3 passed（Xray 流设置/凭据、SS 回环、sing-box 证书路径）。

- [x] **Step 1: 写克隆金样测试**

`tests/rust/share/clone_tests.rs`：

```rust
use crate::servers::deploy_spec::{singbox_config_json, ss_config_json, xray_config_json_multi, EgressFamily, XrayInboundEntry};
use crate::share::clone::{clone_configs, ShareNodeSpec};

fn spec(proto: &str, orig_port: u16, public: u16, internal: u16, transport: &'static str) -> ShareNodeSpec {
    let params = match proto {
        "vless" => serde_json::json!({ "uuid": "old-uuid", "security": "reality", "type": "tcp",
            "flow": "xtls-rprx-vision", "sni": "www.apple.com", "public-key": "pub", "short-id": "abcd1234", "fingerprint": "chrome" }),
        "trojan" => serde_json::json!({ "password": "old-pw", "security": "reality", "type": "tcp",
            "sni": "www.apple.com", "public-key": "pub", "short-id": "abcd1234", "fingerprint": "chrome" }),
        "ss" => serde_json::json!({ "cipher": "2022-blake3-aes-256-gcm", "password": "old-ss" }),
        _ => serde_json::json!({}),
    };
    ShareNodeSpec { name: format!("分享-{proto}"), protocol: proto.into(), original_port: orig_port,
        public_port: public, internal_port: internal, transport, original_params: params,
        new_credential: format!("new-{proto}"), new_password: None }
}

#[test]
fn clone_xray_keeps_stream_but_replaces_port_listen_and_credentials() {
    let main = xray_config_json_multi(&[XrayInboundEntry {
        proto: "vless", port: 443, uuid: "old-uuid".into(), password: String::new(),
        short_id: "abcd1234".into(), private_key: "priv".into(), public_key: "pub".into(),
        reality_dest: "www.apple.com".into(), ws_path: "/ray".into(),
    }], EgressFamily::V4);
    let out = clone_configs(Some(&main), None, None, &[spec("vless", 443, 18200, 18500, "tcp")], "1.2.3.4", "0123456789abcdef", &[]).unwrap();
    let cfg: serde_json::Value = serde_json::from_str(out.xray_config.as_deref().unwrap()).unwrap();
    let ib = &cfg["inbounds"][0];
    assert_eq!(ib["listen"], "127.0.0.1");
    assert_eq!(ib["port"], 18500);
    assert_eq!(ib["settings"]["clients"][0]["id"], "new-vless");
    assert!(ib["settings"]["clients"][0].get("flow").is_some());
    assert_eq!(ib["streamSettings"]["realitySettings"]["privateKey"], "priv");
    assert!(cfg.get("api").is_none());
    assert_eq!(cfg["outbounds"][0]["protocol"], "freedom");
}

#[test]
fn clone_ss_uses_new_password_and_loopback() {
    let main = ss_config_json(8388, "b2xk", EgressFamily::V4);
    let out = clone_configs(None, None, Some(&main), &[spec("ss", 8388, 18210, 18510, "tcp_and_udp")], "1.2.3.4", "0123456789abcdef", &[]).unwrap();
    let cfg: serde_json::Value = serde_json::from_str(out.ss_config.as_deref().unwrap()).unwrap();
    assert_eq!(cfg["servers"][0]["server"], "127.0.0.1");
    assert_eq!(cfg["servers"][0]["server_port"], 18510);
    assert_eq!(cfg["servers"][0]["password"], "new-ss");
    let node = &out.share_nodes[0];
    assert_eq!(node.port, 18210);
    let params: serde_json::Value = serde_json::from_str(&node.params_json).unwrap();
    assert_eq!(params["password"], "new-ss");
}

#[test]
fn clone_singbox_rebinds_and_points_cert_to_share_dir() {
    let main = singbox_config_json(Some((8443, "old-hy2")), None, EgressFamily::V4, Some((1, 12)));
    let out = clone_configs(None, Some(&main), None,
        &[spec("hysteria2", 8443, 18220, 18520, "udp")], "1.2.3.4", "0123456789abcdef", &[]).unwrap();
    let cfg: serde_json::Value = serde_json::from_str(out.singbox_config.as_deref().unwrap()).unwrap();
    let ib = &cfg["inbounds"][0];
    assert_eq!(ib["listen"], "127.0.0.1");
    assert_eq!(ib["listen_port"], 18520);
    assert_eq!(ib["users"][0]["password"], "new-hysteria2");
    assert_eq!(ib["tls"]["certificate_path"], "/srv/vpnshare/0123456789abcdef/singbox.crt");
    assert_eq!(ib["tls"]["key_path"], "/srv/vpnshare/0123456789abcdef/singbox.key");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::clone`
Expected: FAIL，`unresolved import crate::share::clone`

- [ ] **Step 3: 实现克隆**

`ShareNodeSpec` 字段：

```rust
pub struct ShareNodeSpec {
    pub name: String,
    pub protocol: String,
    pub original_port: u16,
    pub public_port: u16,
    pub internal_port: u16,
    pub transport: &'static str, // tcp | udp | tcp_and_udp
    pub original_params: serde_json::Value,
    pub new_credential: String,       // vless/vmess: uuid; trojan/hy2/ss: password; tuic: uuid
    pub new_password: Option<String>, // 仅 tuic 需要
}

pub struct ClonedConfigs {
    pub xray_config: Option<String>,
    pub singbox_config: Option<String>,
    pub ss_config: Option<String>,
    pub share_nodes: Vec<Node>,
    pub yaml: String,
    pub singbox_json: String,
}
```

测试夹具里的 `spec()` 已含 `new_password: None`（tuic 用例传 `Some(...)`）。

`clone.rs` 文件头：

```rust
use crate::domain::{Node, Privacy};
use crate::share::model::*;
use crate::share::naming::share_dir;
```

`clone_configs` 主流程：

```rust
pub fn clone_configs(
    main_xray: Option<&str>,
    main_singbox: Option<&str>,
    main_ss: Option<&str>,
    specs: &[ShareNodeSpec],
    server_host: &str,
    share_id: &str, // sing-box 证书副本落在 /srv/vpnshare/<share_id>/
    privacies: &[Privacy],
) -> Result<ClonedConfigs, String> {
    let xray_specs: Vec<_> = specs.iter().filter(|s| matches!(s.protocol.as_str(), "vless"|"trojan"|"vmess")).collect();
    let sb_specs: Vec<_> = specs.iter().filter(|s| matches!(s.protocol.as_str(), "hysteria2"|"tuic")).collect();
    let ss_specs: Vec<_> = specs.iter().filter(|s| s.protocol == "ss").collect();

    let xray_config = if xray_specs.is_empty() { None } else {
        Some(clone_xray(main_xray.ok_or("缺少 Xray 主配置")?, &xray_specs)?)
    };
    let singbox_config = if sb_specs.is_empty() { None } else {
        Some(clone_singbox(main_singbox.ok_or("缺少 sing-box 主配置")?, &sb_specs, share_id)?)
    };
    let ss_config = if ss_specs.is_empty() { None } else {
        Some(clone_ss(main_ss.ok_or("缺少 Shadowsocks 主配置")?, &ss_specs)?)
    };

    let mut share_nodes = Vec::new();
    for s in specs {
        let mut params = s.original_params.clone();
        match s.protocol.as_str() {
            "vless" | "vmess" => { params["uuid"] = serde_json::Value::String(s.new_credential.clone()); }
            "trojan" | "hysteria2" => { params["password"] = serde_json::Value::String(s.new_credential.clone()); }
            "tuic" => {
                params["uuid"] = serde_json::Value::String(s.new_credential.clone());
                params["password"] = serde_json::Value::String(
                    s.new_password.clone().ok_or("tuic 缺新密码")?,
                );
            }
            "ss" => { params["password"] = serde_json::Value::String(s.new_credential.clone()); }
            _ => return Err(format!("不支持分享的协议: {}", s.protocol)),
        }
        share_nodes.push(Node {
            id: format!("share:{}:{}", s.public_port, s.protocol),
            name: s.name.clone(), source: "self-built".into(), protocol: s.protocol.clone(),
            addr: server_host.into(), port: s.public_port as i64, params_json: params.to_string(),
            group_name: "分享".into(), sub_id: None, relay_rule_id: None, enabled: true,
            latency: None, bandwidth: None, last_checked_at: None, last_check_error: None,
            direct_latency: None, hop_relay_ms: None, hop_landing_ms: None, hop_tunnel_ms: None,
        });
    }
    let (yaml, _) = crate::config::build_yaml_with_privacy(&share_nodes, false, &[], &[], privacies)
        .map_err(|e| e.to_string())?;
    let (singbox_json, _) = crate::config::singbox::build_singbox_with_privacy(&share_nodes, privacies)
        .map_err(|e| e.to_string())?;
    Ok(ClonedConfigs { xray_config, singbox_config, ss_config, share_nodes, yaml, singbox_json })
}
```

`clone_xray`：

```rust
fn clone_xray(main: &str, specs: &[&ShareNodeSpec]) -> Result<String, String> {
    let mut root: serde_json::Value = serde_json::from_str(main).map_err(|e| e.to_string())?;
    let main_inbounds = root.get("inbounds").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut inbounds = Vec::new();
    for s in specs {
        let orig = main_inbounds.iter().find(|ib| {
            ib.get("protocol").and_then(|v| v.as_str()) == Some(s.protocol.as_str())
                && ib.get("port").and_then(|v| v.as_u64()) == Some(s.original_port as u64)
        }).ok_or_else(|| format!("主配置里找不到 {}:{} 入站", s.protocol, s.original_port))?;
        let mut ib = orig.clone();
        ib["listen"] = serde_json::json!("127.0.0.1");
        ib["port"] = serde_json::json!(s.internal_port);
        match s.protocol.as_str() {
            "vless" => {
                let flow = orig.pointer("/settings/clients/0/flow").cloned().unwrap_or(serde_json::json!("xtls-rprx-vision"));
                ib["settings"]["clients"] = serde_json::json!([{ "id": s.new_credential, "flow": flow }]);
            }
            "trojan" => {
                ib["settings"]["clients"] = serde_json::json!([{ "password": s.new_credential }]);
            }
            _ => {
                let alter = orig.pointer("/settings/clients/0/alterId").cloned().unwrap_or(serde_json::json!(0));
                ib["settings"]["clients"] = serde_json::json!([{ "id": s.new_credential, "alterId": alter }]);
            }
        }
        inbounds.push(ib);
    }
    let mut out = serde_json::json!({ "inbounds": inbounds });
    if let Some(v) = root.get("outbounds") { out["outbounds"] = v.clone(); }
    if let Some(v) = root.get("routing") { out["routing"] = v.clone(); }
    Ok(out.to_string())
}
```

`clone_singbox`：

```rust
fn clone_singbox(
    main: &str,
    specs: &[&ShareNodeSpec],
    share_id: &str,
) -> Result<String, String> {
    let mut root: serde_json::Value = serde_json::from_str(main).map_err(|e| e.to_string())?;
    let main_inbounds = root
        .get("inbounds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut inbounds = Vec::new();
    for s in specs {
        let orig = main_inbounds
            .iter()
            .find(|ib| {
                ib.get("type").and_then(|v| v.as_str()) == Some(s.protocol.as_str())
                    && ib.get("listen_port").and_then(|v| v.as_u64())
                        == Some(s.original_port as u64)
            })
            .ok_or_else(|| {
                format!("主配置里找不到 {}:{} 入站", s.protocol, s.original_port)
            })?;
        let mut ib = orig.clone();
        ib["listen"] = serde_json::json!("127.0.0.1");
        ib["listen_port"] = serde_json::json!(s.internal_port);
        let tls = ib
            .get_mut("tls")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| format!("sing-box {} 入站缺 tls 块", s.protocol))?;
        tls.insert(
            "certificate_path".into(),
            serde_json::json!(format!("{}/singbox.crt", share_dir(share_id))),
        );
        tls.insert(
            "key_path".into(),
            serde_json::json!(format!("{}/singbox.key", share_dir(share_id))),
        );
        match s.protocol.as_str() {
            "hysteria2" => {
                ib["users"] = serde_json::json!([{ "password": s.new_credential }]);
            }
            "tuic" => {
                ib["users"] = serde_json::json!([{
                    "uuid": s.new_credential,
                    "password": s.new_password.clone().ok_or("tuic 缺新密码")?,
                }]);
            }
            other => return Err(format!("sing-box 不支持分享的协议: {other}")),
        }
        inbounds.push(ib);
    }
    let mut out = serde_json::json!({ "inbounds": inbounds });
    for key in ["outbounds", "route", "dns"] {
        if let Some(v) = root.get(key) {
            out[key] = v.clone();
        }
    }
    Ok(out.to_string())
}
```

`clone_ss`（shadowsocks-rust 主配置本身就是单 `servers[0]`；分享只允许一个 ss 节点）：

```rust
fn clone_ss(main: &str, specs: &[&ShareNodeSpec]) -> Result<String, String> {
    let s = specs.first().ok_or("SS 分享缺节点")?;
    if specs.len() != 1 {
        return Err("SS 分享一次只允许一个节点".into());
    }
    let root: serde_json::Value = serde_json::from_str(main).map_err(|e| e.to_string())?;
    let ipv6_first = root
        .get("ipv6_first")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(serde_json::json!({
        "servers": [{
            "server": "127.0.0.1",
            "server_port": s.internal_port,
            "method": "2022-blake3-aes-256-gcm",
            "password": s.new_credential,
            "mode": "tcp_and_udp"
        }],
        "locals": [],
        "ipv6_first": ipv6_first
    })
    .to_string())
}
```

- [ ] **Step 4: 跑测试与全量分享测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::clone`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/share/clone.rs tests/rust/share/clone_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 克隆主服务入站并生成分享专属配置"
```

---

### T5: 服务器运行时脚本 — 订阅 TLS 服务 / 网关 / 防火墙

- **Depends on:** T2

**Files:**
- Create: `src-tauri/src/share/scripts.rs`
- Create: `src-tauri/src/share/scripts/share_server.py`
- Create: `src-tauri/src/share/scripts/share_gateway.py`
- Create: `src-tauri/src/share/scripts/share_summary.py`
- Create: `src-tauri/src/share/scripts/vpnshare-firewall.sh`
- Create: `tests/python/__init__.py`
- Create: `tests/python/test_share_server.py`
- Create: `tests/python/test_share_gateway.py`
- Create: `tests/python/test_share_summary.py`
- Create: `tests/rust/share/scripts_tests.rs`
- Modify: `tests/rust/share/mods.rs`

**Interfaces:**
- Produces: `scripts::{SHARE_SERVER_PY, SHARE_GATEWAY_PY, SHARE_SUMMARY_PY, FIREWALL_SH}`，以及 `share_server_script()/share_gateway_script()/share_summary_script()/firewall_script()`。
- Produces: 订阅服务路由 `/sub/<token>`、`/singbox/<token>`；网关 `--config --usage --bind`；摘要脚本输出 JSON 数组。

- **验收:** `python -m unittest discover -s tests/python -v` 全绿；`cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::scripts` 绿。

- **状态:** completed ｜ **证据:** commit `103556d`；`python -m unittest discover -s tests/python -v` 4 passed（订阅双路由/无目录列表、网关配额拦截、摘要新旧格式）；`cargo test --features local-tests share::scripts` 4 passed。

- [ ] **Step 1: 写订阅服务与网关测试**

`tests/python/test_share_server.py`：

```python
import json, os, socket, subprocess, sys, tempfile, time, unittest, urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(ROOT, "src-tauri", "src", "share", "scripts", "share_server.py")

def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

class ServerTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        os.makedirs(os.path.join(self.dir, "sub"))
        self.token = "a" * 32
        with open(os.path.join(self.dir, "share.json"), "w") as f:
            json.dump({"token": self.token}, f)
        with open(os.path.join(self.dir, "sub", self.token), "w") as f:
            f.write("proxies: []\n")
        with open(os.path.join(self.dir, "sub", self.token + ".json"), "w") as f:
            f.write('{"outbounds": []}\n')
        self.port = free_port()
        self.proc = subprocess.Popen([sys.executable, SCRIPT, str(self.port), self.dir, "--bind", "127.0.0.1"])
        self._wait()

    def _wait(self):
        for _ in range(50):
            try:
                socket.create_connection(("127.0.0.1", self.port), 0.1).close(); return
            except OSError:
                time.sleep(0.05)
        self.fail("share_server 未启动")

    def tearDown(self):
        self.proc.terminate(); self.proc.wait(5)

    def test_yaml_and_singbox_routes(self):
        base = f"http://127.0.0.1:{self.port}"
        y = urllib.request.urlopen(base + "/sub/" + self.token).read().decode()
        j = urllib.request.urlopen(base + "/singbox/" + self.token).read().decode()
        self.assertIn("proxies", y)
        self.assertIn("outbounds", j)
        with self.assertRaises(urllib.error.HTTPError):
            urllib.request.urlopen(base + "/sub/" + "b" * 32)

    def test_no_directory_listing(self):
        with self.assertRaises(urllib.error.HTTPError):
            urllib.request.urlopen(f"http://127.0.0.1:{self.port}/")
```

`tests/python/test_share_gateway.py`：

```python
import json, os, socket, subprocess, sys, tempfile, threading, time, unittest

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(ROOT, "src-tauri", "src", "share", "scripts", "share_gateway.py")

def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

def echo_server(port):
    srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port)); srv.listen(5)
    def loop():
        while True:
            try: c, _ = srv.accept()
            except OSError: return
            threading.Thread(target=lambda: c.sendall(c.recv(65536)) or c.close(), daemon=True).start()
    threading.Thread(target=loop, daemon=True).start()
    return srv

def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            break
        buf += chunk
    return buf

class GatewayTest(unittest.TestCase):
    def test_forward_and_quota_block(self):
        target = free_port(); public = free_port()
        echo_server(target)
        d = tempfile.mkdtemp()
        cfg = os.path.join(d, "gateway.json"); usage = os.path.join(d, "usage.json")
        # 配额 4096：第一轮 1024 up + 1024 down = 2048，应正常回显；
        # 第二轮 4096 up 会越过配额，网关在转发前停用并写 blocked。
        with open(cfg, "w") as f:
            json.dump({"quota_bytes": 4096, "rate_up_mbps": 0, "rate_down_mbps": 0,
                       "expires_at_unix": 0,
                       "ports": [{"public": public, "target": target, "proto": "tcp"}]}, f)
        p = subprocess.Popen([sys.executable, SCRIPT, "--config", cfg, "--usage", usage, "--bind", "127.0.0.1"])
        try:
            for _ in range(50):
                try: socket.create_connection(("127.0.0.1", public), 0.1).close(); break
                except OSError: time.sleep(0.05)
            s = socket.create_connection(("127.0.0.1", public), 2)
            s.sendall(b"x" * 1024)
            s.settimeout(2)
            self.assertEqual(recv_exact(s, 1024), b"x" * 1024)
            s.close()
            s2 = socket.create_connection(("127.0.0.1", public), 2)
            s2.sendall(b"y" * 4096)
            s2.settimeout(2)
            with self.assertRaises((ConnectionError, socket.timeout, OSError)):
                recv_exact(s2, 1)
            s2.close()
            time.sleep(0.3)
            with open(usage) as f:
                u = json.load(f)
            self.assertEqual(u["blocked"], "quota")
            self.assertGreaterEqual(u["used_bytes"], 4096)
        finally:
            p.terminate(); p.wait(5)
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python -m unittest discover -s tests/python -v`
Expected: FAIL，脚本不存在

- [ ] **Step 3: 写脚本**

`share_server.py`（完整）：

```python
#!/usr/bin/env python3
"""vpn-ctl 分享订阅服务：TLS 静态服务，只回两条路由，禁目录列举。"""
import argparse, http.server, json, os, socket, socketserver, ssl, threading, time

def load_token(dirpath):
    p = os.path.join(dirpath, "share.json")
    if os.path.isfile(p):
        with open(p, "r", encoding="utf-8") as f:
            return json.load(f)["token"]
    return os.path.basename(os.path.normpath(dirpath))

class Handler(http.server.SimpleHTTPRequestHandler):
    token = ""
    dirpath = ""
    failures = {}
    lock = threading.Lock()

    def log_message(self, *a):
        pass

    def list_directory(self, path):
        self.send_error(404)
        return None

    def _rate_ok(self):
        ip = self.client_address[0]
        now = time.time()
        with self.lock:
            count, started = self.failures.get(ip, (0, now))
            if now - started > 60:
                count, started = 0, now
            if count >= 30:
                return False
            self.failures[ip] = (count + 1, started)
            return True

    def _record(self, fmt):
        try:
            now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            tmp = os.path.join(self.dirpath, "last_access.json.tmp")
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump({"at": now, "ip": self.client_address[0], "format": fmt}, f, ensure_ascii=False)
            os.replace(tmp, os.path.join(self.dirpath, "last_access.json"))
            log = os.path.join(self.dirpath, "access.log")
            with open(log, "a", encoding="utf-8") as f:
                f.write(f"{now} {self.client_address[0]} {fmt} 200\n")
            if os.path.getsize(log) > 1024 * 1024:
                os.truncate(log, 0)
        except OSError:
            pass

    def _serve(self, path, content_type, fmt, head=False):
        if not os.path.isfile(path):
            self.send_error(404)
            return
        data = open(path, "rb").read()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.end_headers()
        if not head:
            self.wfile.write(data)
            self._record(fmt)

    def do_GET(self):
        self._route(False)

    def do_HEAD(self):
        self._route(True)

    def _route(self, head):
        path, _, query = self.path.partition("?")
        yaml = f"/sub/{self.token}"
        singbox = f"/singbox/{self.token}"
        if path == yaml:
            if "format=singbox" in query:
                self._serve(os.path.join(self.dirpath, "sub", self.token + ".json"),
                            "application/json; charset=utf-8", "singbox", head)
            else:
                self._serve(os.path.join(self.dirpath, "sub", self.token),
                            "text/yaml; charset=utf-8", "yaml", head)
        elif path == singbox:
            self._serve(os.path.join(self.dirpath, "sub", self.token + ".json"),
                        "application/json; charset=utf-8", "singbox", head)
        else:
            if not self._rate_ok():
                self.send_error(429)
            else:
                self.send_error(404)

class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

    def __init__(self, addr, handler, dirpath):
        self.dirpath = dirpath
        self._sem = threading.BoundedSemaphore(64)
        super().__init__(addr, handler)

    def process_request(self, request, client_address):
        self._sem.acquire()
        try:
            super().process_request(request, client_address)
        except Exception:
            self._sem.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._sem.release()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("dir")
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--cert")
    ap.add_argument("--key")
    args = ap.parse_args()
    Handler.token = load_token(args.dir)
    Handler.dirpath = os.path.abspath(args.dir)
    if ":" in args.bind:
        Server.address_family = socket.AF_INET6
    srv = Server((args.bind, args.port), Handler, Handler.dirpath)
    if args.cert and args.key:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(args.cert, args.key)
        srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    srv.serve_forever()

if __name__ == "__main__":
    main()
```

`share_gateway.py`（完整）：

```python
#!/usr/bin/env python3
"""vpn-ctl 分享入口网关：TCP/UDP 转发、字节计量、令牌桶限速、配额与到期。"""
import argparse, asyncio, json, os, signal, time

class Bucket:
    def __init__(self, rate_bps):
        self.rate = max(0, int(rate_bps))
        # 桶容量至少 64KB：TCP 单次 read 就是 64KB，容量小于它会让 take 永远凑不齐。
        self.cap = float(max(self.rate, 65536))
        self.tokens = self.cap
        self.ts = time.monotonic()

    def _refill(self):
        now = time.monotonic()
        if self.rate:
            self.tokens = min(self.cap, self.tokens + (now - self.ts) * self.rate)
        self.ts = now

    async def take(self, n):
        if self.rate <= 0 or n <= 0:
            return
        while True:
            self._refill()
            if self.tokens >= n:
                self.tokens -= n
                return
            await asyncio.sleep((n - self.tokens) / self.rate)

    def try_take(self, n):
        if self.rate <= 0:
            return True
        self._refill()
        if self.tokens >= n:
            self.tokens -= n
            return True
        return False

class Gateway:
    def __init__(self, cfg, usage_path):
        self.cfg = cfg
        self.usage_path = usage_path
        self.used = 0
        self.blocked = None
        self.blocked_at = None
        self.servers = []
        self.transports = []
        self.conns = set()  # 已建立的 TCP 客户端连接；停用时主动断开
        self.up = Bucket(cfg.get("rate_up_mbps", 0) * 125000)
        self.down = Bucket(cfg.get("rate_down_mbps", 0) * 125000)
        self.load()

    def load(self):
        try:
            with open(self.usage_path, "r", encoding="utf-8") as f:
                u = json.load(f)
            self.used = int(u.get("used_bytes", 0))
            self.blocked = u.get("blocked")
            self.blocked_at = u.get("blocked_at")
        except (OSError, ValueError, TypeError):
            self.used = 0

    def flush(self):
        data = {"used_bytes": self.used, "blocked": self.blocked,
                "blocked_at": self.blocked_at,
                "updated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        tmp = self.usage_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False)
        os.replace(tmp, self.usage_path)

    def charge(self, n):
        if self.blocked:
            return False
        quota = int(self.cfg.get("quota_bytes", 0))
        if quota > 0 and self.used + n >= quota:
            self.used = quota
            self.block("quota")
            return False
        self.used += n
        return True

    def block(self, reason):
        if self.blocked:
            return
        self.blocked = reason
        self.blocked_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        self.flush()
        for s in self.servers:
            s.close()
        self.servers.clear()
        for t in self.transports:
            try: t.close()
            except Exception: pass
        self.transports.clear()
        for w in list(self.conns):
            try: w.close()
            except Exception: pass
        self.conns.clear()

    def expired(self):
        exp = int(self.cfg.get("expires_at_unix", 0))
        return exp > 0 and time.time() >= exp

    async def pipe(self, reader, writer, bucket):
        try:
            while not self.blocked:
                # 1s 超时只为定期检查 blocked；超时不会丢已缓冲数据。
                try:
                    data = await asyncio.wait_for(reader.read(65536), timeout=1.0)
                except asyncio.TimeoutError:
                    continue
                if not data:
                    break
                if not self.charge(len(data)):
                    break
                await bucket.take(len(data))
                if self.blocked:
                    break
                writer.write(data)
                await writer.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            try: writer.close()
            except Exception: pass

    async def handle_tcp(self, reader, writer, target):
        if self.blocked:
            writer.close()
            return
        self.conns.add(writer)
        try:
            ur, uw = await asyncio.open_connection("127.0.0.1", target)
        except OSError:
            self.conns.discard(writer)
            writer.close()
            return
        try:
            await asyncio.gather(self.pipe(reader, uw, self.up),
                                 self.pipe(ur, writer, self.down),
                                 return_exceptions=True)
        finally:
            self.conns.discard(writer)
            try: writer.close()
            except Exception: pass

class UdpUpstream(asyncio.DatagramProtocol):
    def __init__(self, public, client, gateway):
        self.public = public
        self.client = client
        self.gateway = gateway

    def datagram_received(self, data, addr):
        if not self.gateway.charge(len(data)):
            return
        if not self.gateway.down.try_take(len(data)):
            return
        self.public.sendto(data, self.client)

class UdpPublic(asyncio.DatagramProtocol):
    def __init__(self, target, gateway):
        self.target = target
        self.gateway = gateway
        self.peers = {}
        self.lock = asyncio.Lock()

    def connection_made(self, transport):
        self.transport = transport

    async def _upstream(self, addr):
        async with self.lock:
            entry = self.peers.get(addr)
            if entry:
                entry[1] = time.monotonic()
                return entry[0]
            loop = asyncio.get_running_loop()
            tr, _ = await loop.create_datagram_endpoint(
                lambda: UdpUpstream(self.transport, addr, self.gateway),
                remote_addr=("127.0.0.1", self.target),
            )
            self.peers[addr] = [tr, time.monotonic()]
            return tr

    def datagram_received(self, data, addr):
        if self.gateway.blocked or not self.gateway.charge(len(data)):
            return
        if not self.gateway.up.try_take(len(data)):
            return
        asyncio.create_task(self._send(data, addr))

    async def _send(self, data, addr):
        tr = await self._upstream(addr)
        tr.sendto(data)

    async def sweep(self):
        while True:
            await asyncio.sleep(30)
            now = time.monotonic()
            for addr, entry in list(self.peers.items()):
                if now - entry[1] > 60:
                    entry[0].close()
                    del self.peers[addr]

async def flusher(gw):
    while True:
        await asyncio.sleep(5)
        gw.flush()

async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--usage", required=True)
    ap.add_argument("--bind", default="0.0.0.0")
    args = ap.parse_args()
    with open(args.config, "r", encoding="utf-8") as f:
        cfg = json.load(f)
    gw = Gateway(cfg, args.usage)
    gw.flush()
    if gw.expired():
        gw.block("expired")
    if gw.blocked:
        while True:
            await asyncio.sleep(3600)
    loop = asyncio.get_running_loop()
    for entry in cfg.get("ports", []):
        public = int(entry["public"])
        target = int(entry["target"])
        proto = entry.get("proto", "tcp")
        if "tcp" in proto:
            srv = await asyncio.start_server(
                lambda r, w, t=target: gw.handle_tcp(r, w, t), args.bind, public)
            gw.servers.append(srv)
        if "udp" in proto:
            proto_obj = UdpPublic(target, gw)
            tr, _ = await loop.create_datagram_endpoint(
                lambda p=proto_obj: p, local_addr=(args.bind, public))
            gw.transports.append(tr)
            asyncio.create_task(proto_obj.sweep())

    async def stop():
        gw.flush()
        for s in gw.servers:
            s.close()
        for t in gw.transports:
            t.close()
        loop.stop()

    try:
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, lambda: asyncio.create_task(stop()))
    except NotImplementedError:
        pass  # Windows 本地测试；生产在 Linux 上走 add_signal_handler

    asyncio.create_task(flusher(gw))
    while True:
        if gw.expired():
            gw.block("expired")
        await asyncio.sleep(1)

if __name__ == "__main__":
    asyncio.run(main())
```

`vpnshare-firewall.sh`（完整）：

```sh
#!/bin/sh
# 用法: vpnshare-firewall.sh open|close <comment-prefix> <port:proto>...
set -u
action=$1; prefix=$2; shift 2
for spec in "$@"; do
  port=${spec%%:*}
  proto=${spec##*:}
  [ "$proto" = "$spec" ] && proto=tcp
  comment="$prefix-$port-$proto"
  if [ "$action" = open ]; then
    iptables -C INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT 2>/dev/null || \
      iptables -I INPUT 1 -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT
  else
    while iptables -C INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT 2>/dev/null; do
      iptables -D INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT
    done
  fi
done
```

`share_summary.py`（完整）：

```python
#!/usr/bin/env python3
"""一次性输出服务器上所有分享的摘要；只读，不改任何文件。"""
import json, os, subprocess, sys

def read(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    except OSError:
        return None

def unit_state(name):
    try:
        r = subprocess.run(["systemctl", "is-active", name], capture_output=True,
                           text=True, timeout=5)
        return r.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None

def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "/srv/vpnshare"
    try:
        names = sorted(os.listdir(root))
    except OSError:
        names = []
    out = []
    for name in names:
        d = os.path.join(root, name)
        if not os.path.isdir(d):
            continue
        manifest = read(os.path.join(d, "share.json"))
        if manifest is not None:
            try:
                m = json.loads(manifest)
                mid = m["id"]
            except (ValueError, KeyError):
                continue
            usage = read(os.path.join(d, "usage.json"))
            last = read(os.path.join(d, "last_access.json"))
            units = {}
            for suffix in ("", "-gateway", "-xray", "-singbox", "-ss"):
                units[f"vpnshare-{mid}{suffix}"] = unit_state(f"vpnshare-{mid}{suffix}")
            out.append({
                "kind": "new", "id": mid, "manifest": m,
                "usage": json.loads(usage) if usage else None,
                "last_access": json.loads(last) if last else None,
                "units": units,
            })
        else:
            out.append({
                "kind": "legacy", "id": name,
                "unit": read(f"/etc/systemd/system/vpnshare-{name}.service"),
                "snapshot": read(os.path.join(d, "sub", name)),
                "active": unit_state(f"vpnshare-{name}") == "active",
            })
    print(json.dumps(out, ensure_ascii=False))

if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Rust 脚本常量与内容测试**

`scripts.rs`：

```rust
pub const SHARE_SERVER_PY: &str = include_str!("scripts/share_server.py");
pub const SHARE_GATEWAY_PY: &str = include_str!("scripts/share_gateway.py");
pub const SHARE_SUMMARY_PY: &str = include_str!("scripts/share_summary.py");
pub const FIREWALL_SH: &str = include_str!("scripts/vpnshare-firewall.sh");

pub fn share_server_script() -> String { SHARE_SERVER_PY.to_string() }
pub fn share_gateway_script() -> String { SHARE_GATEWAY_PY.to_string() }
pub fn share_summary_script() -> String { SHARE_SUMMARY_PY.to_string() }
pub fn firewall_script() -> String { FIREWALL_SH.to_string() }
```

`scripts_tests.rs` 断言：订阅脚本含 `no-store` 与两条路由；网关含 `quota_bytes`、`rate_up_mbps`、`expires_at_unix`；防火墙脚本含 `-C`/`-I`/`-D` 与 `comment`。

- [ ] **Step 5: 跑测试**

Run: `python -m unittest discover -s tests/python -v`
Expected: PASS
Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::scripts`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/share/scripts.rs src-tauri/src/share/scripts tests/python tests/rust/share/scripts_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 订阅 TLS 服务、入口网关与防火墙脚本"
```

---

### T6: TLS/ACME 编排

- **Depends on:** T1、T2、T5

**Files:**
- Create: `src-tauri/src/share/tls.rs`
- Create: `tests/rust/share/tls_tests.rs`
- Modify: `tests/rust/share/mods.rs`

**Interfaces:**
- Produces: `tls::{DEPLOY_HOOK_PATH, tls_status, prepare_tls, probe_python3, validate_email, dns_check_cmd, certbot_install_script, certbot_issue_cmd, renewal_hook_content}`
- Consumes: `servers::load_auth`, `SshSession::connect`, `read_remote_file`, `write_remote_file`, `run_cmd`, `run_cmd_ok`, `scripts::firewall_script`.

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::tls` 绿。

- **状态:** completed ｜ **证据:** commit `81a0373`；`cargo test --features local-tests share::tls` 16 passed（命令/钩子/邮箱纯函数；未配域名/邮箱/非法邮箱不连服务器；DNS 不匹配与 80 占用不发 certbot；签发失败/无包管理器撤回 80 放行；成功写钩子并返回到期时间；状态查询含缺证书与就绪路径）。

- [ ] **Step 1: 写 TLS 失败测试**

```rust
use crate::share::tls::{certbot_issue_cmd, dns_check_cmd, renewal_hook_content, validate_email};

#[test]
fn dns_check_command_prints_both_families() {
    let cmd = dns_check_cmd("share.example.com");
    assert!(cmd.contains("getaddrinfo"));
    assert!(cmd.contains("share.example.com"));
}

#[test]
fn renewal_hook_copies_cert_and_restarts_share_units() {
    let h = renewal_hook_content("share.example.com");
    assert!(h.contains("/etc/letsencrypt/live/share.example.com/fullchain.pem"));
    assert!(h.contains("/srv/vpnshare/tls/fullchain.pem"));
    assert!(h.contains("systemctl restart"));
    assert!(h.contains("vpnshare-"));
}

#[test]
fn certbot_issue_cmd_uses_standalone_http01() {
    let c = certbot_issue_cmd("share.example.com", "me@example.com");
    assert!(c.contains("certonly --standalone"));
    assert!(c.contains("-d 'share.example.com'"));
    assert!(c.contains("-m 'me@example.com'"));
    assert!(c.contains("--keep-until-expiring"));
}

#[test]
fn email_validation_rejects_shell_metacharacters() {
    assert!(validate_email("me@example.com").is_ok());
    assert!(validate_email("a@b.com; rm -rf /").is_err());
    assert!(validate_email("a'b@example.com").is_err());
    assert!(validate_email("no-at-sign").is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::tls`
Expected: FAIL，`unresolved import crate::share::tls`

- [ ] **Step 3: 实现纯函数**

`tls.rs` 文件头：

```rust
use crate::domain::effective_host;
use crate::servers::deploy::{run_cmd, run_cmd_ok, write_remote_file, CONFIG_TIMEOUT, INSTALL_TIMEOUT};
use crate::servers::remote_file::{read_remote_file, RemoteFile};
use crate::servers::{CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::share::model::ShareTlsStatus;
use crate::share::naming::*;
use crate::share::scripts;
use crate::settings::SHARE_ACME_EMAIL_KEY;
use crate::ssh::SshSession;
use crate::state::AppState;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
```

```rust
pub const CERT_ROOT: &str = "/etc/letsencrypt/live";
pub const TLS_DIR: &str = "/srv/vpnshare/tls";
pub const DEPLOY_HOOK_PATH: &str = "/etc/letsencrypt/renewal-hooks/deploy/vpnshare.sh";

/// 单引号安全包装：shell 里唯一不能出现在单引号串中的字符就是 `'`。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn dns_check_cmd(domain: &str) -> String {
    format!(
        "python3 -c 'import socket,sys;print(\"\\n\".join(sorted({{a[4][0] for a in socket.getaddrinfo(sys.argv[1],None)}})))' {}",
        sh_quote(domain)
    )
}

/// ACME 邮箱会拼进 root shell 命令，必须先拒绝空白/引号/元字符。
pub fn validate_email(email: &str) -> Result<(), String> {
    if email.len() > 254
        || email.contains(|c: char| {
            c.is_whitespace() || "'\"`$;|&<>(){}[]\\".contains(c)
        })
    {
        return Err("ACME 邮箱格式不合法".into());
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("ACME 邮箱格式不合法".into());
    };
    if local.is_empty() || !domain.contains('.') {
        return Err("ACME 邮箱格式不合法".into());
    }
    Ok(())
}

pub fn certbot_install_script() -> String {
    concat!(
        "if command -v certbot >/dev/null 2>&1; then exit 0; fi\n",
        "if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && apt-get install -y certbot;\n",
        "elif command -v dnf >/dev/null 2>&1; then dnf install -y certbot;\n",
        "elif command -v yum >/dev/null 2>&1; then yum install -y certbot;\n",
        "elif command -v pacman >/dev/null 2>&1; then pacman -Sy --noconfirm certbot;\n",
        "elif command -v zypper >/dev/null 2>&1; then zypper --non-interactive install certbot;\n",
        "elif command -v apk >/dev/null 2>&1; then apk add --no-cache certbot;\n",
        "else echo NO_CERTBOT_PKG_MANAGER; exit 1; fi\n"
    ).to_string()
}

pub fn certbot_issue_cmd(domain: &str, email: &str) -> String {
    format!(
        "certbot certonly --standalone -d {} --non-interactive --agree-tos -m {} --keep-until-expiring",
        sh_quote(domain),
        sh_quote(email)
    )
}

pub fn renewal_hook_content(domain: &str) -> String {
    format!(
        "#!/bin/sh\nset -e\ncp {CERT_ROOT}/{domain}/fullchain.pem {TLS_DIR}/fullchain.pem\n\
         cp {CERT_ROOT}/{domain}/privkey.pem {TLS_DIR}/privkey.pem\n\
         chown vpnshare:vpnshare {TLS_DIR}/fullchain.pem {TLS_DIR}/privkey.pem\n\
         chmod 600 {TLS_DIR}/fullchain.pem {TLS_DIR}/privkey.pem\n\
         for u in $(systemctl list-units --type=service --all --no-legend 'vpnshare-*.service' | awk '{{print $1}}'); do systemctl restart \"$u\" || true; done\n"
    )
}

/// 订阅/实例都依赖 python3；T6 与 T7 共用，避免各写一份判断。
pub(crate) async fn probe_python3(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let out = run_cmd(sess, cancel, "检查", "command -v python3", PROBE_TIMEOUT).await?;
    if out.exit_code != 0 {
        return Err("服务器缺少 python3（订阅服务与网关依赖它）".into());
    }
    Ok(())
}
```

- [ ] **Step 4: 实现 `prepare_tls` 与 `tls_status`**

`prepare_tls(state, server_id)`：
1. 读服务器与全局 `share_acme_email`；空域名/邮箱报错。
2. `load_auth` → `SshSession::connect`（`CONNECT_TIMEOUT`）。
3. `probe_python3`（复用 `share` 模块的实现或新写）。
4. DNS：分别跑 `dns_check_cmd(domain)` 与 `dns_check_cmd(host)`/`host_v6`，取输出集合求交集；空交集报错并带上两边输出。
5. 探测 80：`run_cmd(... tcp_port_probe_cmd(80))`；exit 0 = 占用 → 报错。
6. 确保用户/目录：`id -u vpnshare >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin vpnshare 2>/dev/null || adduser -S -D -H -s /sbin/nologin vpnshare`（最后一段给 Alpine）；`mkdir -p /srv/vpnshare/tls && chown vpnshare:vpnshare /srv/vpnshare/tls && chmod 700 /srv/vpnshare/tls`。
7. 写防火墙脚本并 `chmod 755`；临时放行 `80:tcp`。
8. 安装 certbot（`run_cmd` + 识别 `NO_CERTBOT_PKG_MANAGER`），超时 `INSTALL_TIMEOUT`。
9. 执行 `certbot_issue_cmd`（超时 `INSTALL_TIMEOUT`）；失败把 stdout/stderr 带进错误。
10. 复制证书：`install -m 600 .../fullchain.pem /srv/vpnshare/tls/fullchain.pem`、`privkey.pem`；`chown`。
11. 写续期钩子到 `DEPLOY_HOOK_PATH`，`chmod 755`。
12. 用 `openssl x509 -enddate -noout -in ...` 解析 `notAfter`；返回 `ShareTlsStatus`。
13. `finally` 撤销 80 临时规则。

前置读取（放在函数开头，未配置域名/邮箱时不连服务器）：

```rust
let (auth, server) = crate::servers::load_auth(state, server_id)
    .map_err(|e| e.to_string())?;
let domain = crate::servers::normalize_share_domain(&server.share_domain)?;
let email = state
    .settings
    .get(SHARE_ACME_EMAIL_KEY)
    .map_err(|e| e.to_string())?
    .unwrap_or_default()
    .trim()
    .to_string();
if domain.is_empty() {
    return Err("该服务器未配置分享域名".into());
}
if email.is_empty() {
    return Err("未配置 ACME 邮箱（设置 → 分享）".into());
}
validate_email(&email)?;
```

关键命令（照抄进 `prepare_tls`）：

```rust
// DNS：域名解析结果必须与本机登记地址有交集，否则 certbot 必然失败。
let mut server_ips = std::collections::HashSet::new();
for h in [&server.host, &server.host_v6] {
    if h.is_empty() { continue; }
    let out = run_cmd_ok(&mut sess, &cancel, "DNS", &dns_check_cmd(h), PROBE_TIMEOUT).await?;
    server_ips.extend(out.stdout.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
}
let out = run_cmd_ok(&mut sess, &cancel, "DNS", &dns_check_cmd(&domain), PROBE_TIMEOUT).await?;
let domain_ips: std::collections::HashSet<String> = out
    .stdout
    .lines()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect();
if domain_ips.is_empty() || server_ips.is_empty() || domain_ips.is_disjoint(&server_ips) {
    return Err(format!(
        "域名 {domain} 未解析到本机（域名: {domain_ips:?}，本机: {server_ips:?}）"
    ));
}
// HTTP-01 需要 80 空闲。
let out = run_cmd(&mut sess, &cancel, "探测", &crate::share::plan::tcp_port_probe_cmd(80), PROBE_TIMEOUT).await?;
if out.exit_code == 0 {
    return Err("80 端口已被占用，certbot --standalone 无法使用".into());
}

write_remote_file(&mut sess, &cancel, "写入", &firewall_script_path(), &scripts::firewall_script(), CONFIG_TIMEOUT).await?;
run_cmd_ok(&mut sess, &cancel, "准备", &format!("chmod 755 {}", firewall_script_path()), CONFIG_TIMEOUT).await?;
run_cmd_ok(&mut sess, &cancel, "准备", &format!("{} open vpnshare-tls 80:tcp", firewall_script_path()), CONFIG_TIMEOUT).await?;
let out = run_cmd_ok(&mut sess, &cancel, "申请证书", &certbot_issue_cmd(&domain, &email), INSTALL_TIMEOUT).await?;
let _ = out;
run_cmd_ok(&mut sess, &cancel, "部署证书", &format!(
    "install -m 600 /etc/letsencrypt/live/{domain}/fullchain.pem /srv/vpnshare/tls/fullchain.pem && \
     install -m 600 /etc/letsencrypt/live/{domain}/privkey.pem /srv/vpnshare/tls/privkey.pem && \
     chown vpnshare:vpnshare /srv/vpnshare/tls/fullchain.pem /srv/vpnshare/tls/privkey.pem"
), CONFIG_TIMEOUT).await?;
run_cmd_ok(&mut sess, &cancel, "部署证书", "mkdir -p /etc/letsencrypt/renewal-hooks/deploy", CONFIG_TIMEOUT).await?;
write_remote_file(&mut sess, &cancel, "部署证书", DEPLOY_HOOK_PATH, &renewal_hook_content(&domain), CONFIG_TIMEOUT).await?;
run_cmd_ok(&mut sess, &cancel, "部署证书", &format!("chmod 755 {DEPLOY_HOOK_PATH}"), CONFIG_TIMEOUT).await?;
run_cmd_ok(&mut sess, &cancel, "证书校验", "openssl x509 -checkend 0 -noout -in /srv/vpnshare/tls/fullchain.pem", PROBE_TIMEOUT).await?;
let _ = run_cmd_ok(&mut sess, &cancel, "清理", &format!("{} close vpnshare-tls 80:tcp", firewall_script_path()), CONFIG_TIMEOUT).await;

let out = run_cmd(&mut sess, &cancel, "证书校验", "openssl x509 -enddate -noout -in /srv/vpnshare/tls/fullchain.pem", PROBE_TIMEOUT).await?;
let cert_expires_at = out
    .stdout
    .trim()
    .strip_prefix("notAfter=")
    .map(|s| s.trim().to_string());
Ok(ShareTlsStatus {
    domain,
    ready: true,
    cert_expires_at,
    detail: "证书已就绪".into(),
})
```

`tls_status`：读 `share_domain` 与邮箱；SSH 读 `openssl x509 -enddate -noout -in <cert>`；空/过期 → `ready=false`。

- [ ] **Step 5: 测试命令序列**

用例至少覆盖：未配域名 → 不连服务器；DNS 不匹配 → 报错且不发 certbot；80 占用 → 不发 certbot；certbot 失败 → 错误带输出；成功 → 写钩子、复制证书、返回 ready。

- [ ] **Step 6: 跑测试与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::tls`
Expected: PASS

```bash
git add src-tauri/src/share/tls.rs tests/rust/share/tls_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 域名校验与 certbot 自动 HTTPS"
```

---

### T7: 创建分享编排（端口、克隆、单元、验证、回滚）

- **Depends on:** T2-T6

**Files:**
- Create: `src-tauri/src/share/install.rs`
- Create: `tests/rust/share/install_tests.rs`
- Modify: `tests/rust/share/mods.rs`
- Modify: `src-tauri/src/share/mod.rs`

**Interfaces:**
- Produces: `install::{CreateShareInput, create_share, create_share_inner, allocate_share_ports, build_specs, read_main, unit_contents, rollback_created}`
- Produces: `install::create_share(state, input)` 供 T10 的 IPC 调用（T10 再决定是否在 `share` 根重导出）。

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install::create` 绿；失败用例断言回滚命令。

- **状态:** completed ｜ **证据:** commit `ccd8fb1`；`cargo test --features local-tests share::install` 9 passed（准入先于 SSH、TLS 未就绪拒绝、缺主配置不写文件、端口顺延、凭据轮换、成功落盘 manifest/gateway/YAML/JSON/单元、写失败与启动失败均回滚并撤端口）。

- [ ] **Step 1: 写创建失败测试**

```rust
#[tokio::test]
async fn create_share_rejects_legacy_or_unshareable_nodes_before_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let state = super::tests::shared_state(&dir);
    let srv = crate::ssh::tests::FakeSshServer::start().await;
    let server_id = super::tests::seed_server(&state, &srv);
    let mut n = super::tests::seed_node(&state, "n1", "节点");
    n.relay_rule_id = Some(1);
    crate::db::repos::nodes::upsert(&state.db.lock(), &n, &state.creds).unwrap();
    let e = create_share_inner(&state, CreateShareInput {
        server_id, node_ids: vec!["n1".into()],
        options: ShareOptions { quota_bytes: 100, rate_up_mbps: 1, rate_down_mbps: 1, expires_in_days: 1 },
    }).await.unwrap_err();
    assert!(e.contains("中转"));
    assert!(srv.recorded().is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install::create`
Expected: FAIL，`unresolved import crate::share::install`

- [ ] **Step 3: 实现创建流程**

`CreateShareInput { server_id: i64, node_ids: Vec<String>, options: ShareOptions }`。

`install.rs` 文件头：

```rust
use crate::domain::{effective_host, Server};
use crate::servers::deploy::{
    run_cmd, run_cmd_ok, write_remote_file, CONFIG_TIMEOUT,
    SINGBOX_CONFIG_PATH, SS_CONFIG_PATH, XRAY_CONFIG_PATH,
};
use crate::servers::remote_file::{read_remote_file, RemoteFile};
use crate::servers::{CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::share::model::*;
use crate::share::naming::*;
use crate::share::plan;
use crate::share::plan::{tcp_port_probe_cmd, udp_port_probe_cmd};
use crate::share::{clone, scripts, tls};
use crate::ssh::SshSession;
use crate::state::AppState;
use serde::Deserialize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
```

`create_share` 实现（按此顺序）：

```rust
pub async fn create_share(state: &Arc<AppState>, input: CreateShareInput) -> Result<ShareInfo, String> {
    // 参数错误必须在连服务器之前返回（IPC 测试断言「空 nodeIds 不产生 SSH」）。
    validate_options(&input.options)?;
    if input.node_ids.is_empty() {
        return Err("至少选择一个节点".into());
    }
    let tls = tls::tls_status(state, input.server_id).await?;
    if !tls.ready {
        return Err(format!("分享 HTTPS 未就绪: {}", tls.detail));
    }
    create_share_inner(state, input).await
}

/// 测试直接调用它，绕过 TLS 的外部依赖；生产路径必须经 `create_share`。
pub(crate) async fn create_share_inner(state: &Arc<AppState>, input: CreateShareInput) -> Result<ShareInfo, String> {
    validate_options(&input.options)?;
    if input.node_ids.is_empty() {
        return Err("至少选择一个节点".into());
    }
    // 1. 候选校验（在连服务器之前）
    let candidates = plan::share_candidates(state, input.server_id)?;
    let chosen: Vec<Node> = input.node_ids.iter()
        .map(|id| candidates.iter().find(|c| &c.node.id == id)
            .ok_or_else(|| format!("节点不存在: {id}")))
        .collect::<Result<_,_>>()?
        .into_iter()
        .map(|c| {
            if let Some(r) = &c.reason { Err(format!("节点 {} 不可分享: {r}", c.node.name)) }
            else { Ok(c.node.clone()) }
        })
        .collect::<Result<_,_>>()?;
    // 2. 读服务器/凭据/建立 SSH
    let (auth, server) = crate::servers::load_auth(state, input.server_id)
        .map_err(|e| e.to_string())?;
    let domain = crate::servers::normalize_share_domain(&server.share_domain)?;
    let mut sess = SshSession::connect(effective_host(&server), server.port as u16, &server.user, &auth, CONNECT_TIMEOUT).await?;
    let cancel = AtomicBool::new(false);
    tls::probe_python3(&mut sess, &cancel).await?;
    // 3. 分配端口，再生成克隆规格（规格里带一次性凭据）
    let transports: Vec<&str> = chosen.iter().map(|n| match n.protocol.as_str() {
        "hysteria2" | "tuic" => "udp",
        "ss" => "tcp_and_udp",
        _ => "tcp",
    }).collect();
    let allocated = allocate_share_ports(&mut sess, &cancel, &transports).await?;
    let specs = build_specs(&chosen, &allocated.pairs)?;
    // 4. 读主配置（缺哪个协议就报哪个）；id/token 先定，sing-box 证书副本路径要用
    let id = new_id();
    let token = new_token();
    let main_xray = read_main(&mut sess, &cancel, &specs, "vless", XRAY_CONFIG_PATH).await?;
    let main_sb = read_main(&mut sess, &cancel, &specs, "hysteria2", SINGBOX_CONFIG_PATH).await?;
    let main_ss = read_main(&mut sess, &cancel, &specs, "ss", SS_CONFIG_PATH).await?;
    // 5. 克隆 + 生成订阅
    let cloned = clone_configs(
        main_xray.as_deref(), main_sb.as_deref(), main_ss.as_deref(),
        &specs, &effective_host(&server), &id, &[],
    )?;
    // 6. 生成清单与网关配置（服务器时间算到期）
    let dir = share_dir(&id);
    let bind = if server.addr_family == "v6" { "::" } else { "0.0.0.0" };
    let expires_at_unix = server_now_unix(&mut sess, &cancel).await?
        + input.options.expires_in_days as i64 * 86400;
    let share_nodes: Vec<ShareNode> = specs.iter().zip(cloned.share_nodes.iter())
        .map(|(s, n)| ShareNode {
            name: s.name.clone(), protocol: s.protocol.clone(),
            public_port: s.public_port, internal_port: s.internal_port,
            transport: s.transport.into(), params_json: n.params_json.clone(),
        }).collect();
    let manifest = ShareManifest {
        version: 1, id: id.clone(), token: token.clone(), server_id: server.id,
        created_at: chrono::Utc::now().to_rfc3339(),
        quota_bytes: input.options.quota_bytes,
        rate_up_mbps: input.options.rate_up_mbps,
        rate_down_mbps: input.options.rate_down_mbps,
        expires_at_unix, http_port: allocated.http,
        domain: domain.clone(), nodes: share_nodes,
    };
    let gateway = GatewayConfig {
        quota_bytes: manifest.quota_bytes, rate_up_mbps: manifest.rate_up_mbps,
        rate_down_mbps: manifest.rate_down_mbps, expires_at_unix,
        ports: specs.iter().map(|s| GatewayPort {
            public: s.public_port, target: s.internal_port, proto: s.transport.into(),
        }).collect(),
    };
    // tcp_and_udp 必须拆成两条防火墙规则：script 里的 -p 只认 tcp/udp 单值。
    let public_pairs: Vec<(u16, &str)> = specs.iter().flat_map(|s| match s.transport {
        "tcp_and_udp" => vec![(s.public_port, "tcp"), (s.public_port, "udp")],
        other => vec![(s.public_port, other)],
    }).collect();

    // 7. 写脚本、用户/目录、清单、订阅、实例配置、单元
    let mut units = vec![unit_name(&id)];
    write_remote_file(&mut sess, &cancel, "写入", &share_server_script_path(), &scripts::share_server_script(), CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &share_gateway_script_path(), &scripts::share_gateway_script(), CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &firewall_script_path(), &scripts::firewall_script(), CONFIG_TIMEOUT).await?;
    run_cmd_ok(&mut sess, &cancel, "准备", "chmod 755 /srv/vpnshare/share_server.py /srv/vpnshare/share_gateway.py /srv/vpnshare/vpnshare-firewall.sh", CONFIG_TIMEOUT).await?;
    run_cmd_ok(&mut sess, &cancel, "准备", "id -u vpnshare >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin vpnshare 2>/dev/null || adduser -S -D -H -s /sbin/nologin vpnshare", CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &manifest_path(&id), &serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &gateway_config_path(&id), &serde_json::to_string_pretty(&gateway).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &usage_path(&id), &serde_json::to_string(&UsageState::default()).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &yaml_path(&id, &token), &cloned.yaml, CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &singbox_path(&id, &token), &cloned.singbox_json, CONFIG_TIMEOUT).await?;

    if let Some(cfg) = &cloned.xray_config {
        let unit = instance_unit_name(&id, "xray");
        let path = instance_config_path(&id, "xray");
        write_remote_file(&mut sess, &cancel, "写入", &path, cfg, CONFIG_TIMEOUT).await?;
        write_remote_file(&mut sess, &cancel, "写入", &unit_path(&unit), &instance_unit(&id, "xray", &dir, &path), CONFIG_TIMEOUT).await?;
        units.push(unit);
    }
    if let Some(cfg) = &cloned.singbox_config {
        run_cmd_ok(&mut sess, &cancel, "准备", &format!(
            "install -m 600 /etc/sing-box/self.crt {dir}/singbox.crt && install -m 600 /etc/sing-box/self.key {dir}/singbox.key"
        ), CONFIG_TIMEOUT).await?;
        let unit = instance_unit_name(&id, "singbox");
        let path = instance_config_path(&id, "singbox");
        write_remote_file(&mut sess, &cancel, "写入", &path, cfg, CONFIG_TIMEOUT).await?;
        write_remote_file(&mut sess, &cancel, "写入", &unit_path(&unit), &instance_unit(&id, "singbox", &dir, &path), CONFIG_TIMEOUT).await?;
        units.push(unit);
    }
    if let Some(cfg) = &cloned.ss_config {
        let unit = instance_unit_name(&id, "ss");
        let path = instance_config_path(&id, "ss");
        write_remote_file(&mut sess, &cancel, "写入", &path, cfg, CONFIG_TIMEOUT).await?;
        write_remote_file(&mut sess, &cancel, "写入", &unit_path(&unit), &instance_unit(&id, "ss", &dir, &path), CONFIG_TIMEOUT).await?;
        units.push(unit);
    }
    write_remote_file(&mut sess, &cancel, "写入", &unit_path(&unit_name(&id)), &http_unit(&id, allocated.http, bind, &dir), CONFIG_TIMEOUT).await?;
    let gw_unit = gateway_unit_name(&id);
    write_remote_file(&mut sess, &cancel, "写入", &unit_path(&gw_unit), &gateway_unit(&id, &dir, bind, &public_pairs), CONFIG_TIMEOUT).await?;
    units.push(gw_unit);
    run_cmd_ok(&mut sess, &cancel, "准备", &format!(
        "chown -R vpnshare:vpnshare {dir} && chmod 700 {dir} {dir}/sub && chmod 600 {dir}/*.json {dir}/sub/*"
    ), CONFIG_TIMEOUT).await?;

    // 8. 启动与验证；任一步失败即回滚
    run_cmd_ok(&mut sess, &cancel, "启动", "systemctl daemon-reload", CONFIG_TIMEOUT).await?;
    for unit in &units {
        if let Err(e) = run_cmd_ok(&mut sess, &cancel, "启动", &format!("systemctl enable --now {unit}"), CONFIG_TIMEOUT).await {
            let left = rollback_created(&mut sess, &cancel, &id, &units, &public_pairs).await;
            return Err(format!("启动 {unit} 失败：{e}；残留：{left:?}"));
        }
    }
    for unit in &units {
        let out = run_cmd(&mut sess, &cancel, "确认", &format!("systemctl is-active {unit}"), PROBE_TIMEOUT).await?;
        if out.stdout.trim() != "active" {
            let left = rollback_created(&mut sess, &cancel, &id, &units, &public_pairs).await;
            return Err(format!("单元 {unit} 未 active；残留：{left:?}"));
        }
    }
    let code = run_cmd(&mut sess, &cancel, "自测", &format!(
        "curl -s -o /dev/null -w '%{{http_code}}' --resolve {domain}:{http_port}:127.0.0.1 https://{domain}:{http_port}/sub/{token} --max-time 10",
        domain = domain, http_port = allocated.http, token = token,
    ), PROBE_TIMEOUT).await?.stdout;
    if code.trim() != "200" {
        let left = rollback_created(&mut sess, &cancel, &id, &units, &public_pairs).await;
        return Err(format!("分享链接自测返回 {}（期望 200）；残留：{left:?}", code.trim()));
    }

    Ok(info_from_manifest(&manifest, &UsageState::default(), &server, true))
}

/// 展示用状态：配额按 `used >= quota` 判定（不依赖时钟）；到期用本机时钟近似，
/// 真正的断开由网关按服务器时钟执行。
pub fn status_from(m: &ShareManifest, u: &UsageState, active: bool) -> ShareStatus {
    let quota_hit = m.quota_bytes > 0 && u.used_bytes >= m.quota_bytes;
    let expired = m.expires_at_unix > 0
        && chrono::Utc::now().timestamp() >= m.expires_at_unix;
    if u.blocked.as_deref() == Some("quota") || quota_hit {
        ShareStatus::BlockedQuota
    } else if u.blocked.as_deref() == Some("expired") || expired {
        ShareStatus::BlockedExpired
    } else if !active {
        ShareStatus::Degraded
    } else {
        ShareStatus::Active
    }
}

fn info_from_manifest(
    m: &ShareManifest,
    u: &UsageState,
    server: &Server,
    active: bool,
) -> ShareInfo {
    let status = status_from(m, u, active);
    ShareInfo {
        id: m.id.clone(), server_id: m.server_id, server_name: server.name.clone(),
        host: crate::domain::effective_host(server).to_string(), token: m.token.clone(),
        port: m.http_port, active,
        url: url_yaml(&m.domain, m.http_port, &m.token),
        url_singbox: url_singbox(&m.domain, m.http_port, &m.token),
        node_names: m.nodes.iter().map(|n| n.name.clone()).collect(),
        legacy: false, status, status_detail: u.blocked.clone(),
        quota_bytes: m.quota_bytes, used_bytes: u.used_bytes,
        rate_up_mbps: m.rate_up_mbps, rate_down_mbps: m.rate_down_mbps,
        expires_at: chrono::DateTime::from_timestamp(m.expires_at_unix, 0)
            .map(|t| t.to_rfc3339()),
        last_access_at: None, last_access_ip: None,
    }
}

/// 只有当选中节点里出现该协议族时才读主配置；文件不存在即报错，不静默跳过。
async fn read_main(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    specs: &[ShareNodeSpec],
    any_proto: &str,
    path: &str,
) -> Result<Option<String>, String> {
    let needed = specs.iter().any(|s| {
        (any_proto == "vless" && matches!(s.protocol.as_str(), "vless" | "trojan" | "vmess"))
            || (any_proto == "hysteria2" && matches!(s.protocol.as_str(), "hysteria2" | "tuic"))
            || (any_proto == "ss" && s.protocol == "ss")
    });
    if !needed {
        return Ok(None);
    }
    match crate::servers::remote_file::read_remote_file(sess, cancel, "读取", path, PROBE_TIMEOUT).await? {
        crate::servers::remote_file::RemoteFile::Absent => {
            Err(format!("主服务配置不存在：{path}（请先部署对应协议）"))
        }
        crate::servers::remote_file::RemoteFile::Content(c) => Ok(Some(c)),
    }
}

async fn server_now_unix(sess: &mut SshSession, cancel: &AtomicBool) -> Result<i64, String> {
    let out = run_cmd(sess, cancel, "授时", "date +%s", PROBE_TIMEOUT).await?;
    out.stdout.trim().parse().map_err(|_| "服务器时间解析失败".to_string())
}
```

**回滚必须覆盖写失败（A9）：** 上面步骤 7 的每个 `?` 都可能把半成品留在服务器上。
实现时把步骤 7-8 整体包进 `let setup: Result<(), String> = async { ... }.await;`，
`setup.is_err()` 时统一调用 `rollback_created`。启动/验证分支里已有的回滚可以保留
（重复回滚幂等），但**写文件失败必须由外层兜住**——否则 A9 不成立。

端口分配：

```rust
pub struct AllocatedPorts {
    pub http: u16,
    pub pairs: Vec<(u16, u16)>, // (public, internal)，与传入 transports 同序
}

async fn find_free_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    cursor: &mut u16,
    end: u16,
    transport: &str,
) -> Result<u16, String> {
    while *cursor <= end {
        let p = *cursor;
        *cursor += 1;
        let mut free = true;
        if transport != "udp" {
            let out = run_cmd(sess, cancel, "探测", &tcp_port_probe_cmd(p), PROBE_TIMEOUT).await?;
            free &= out.exit_code != 0; // 非 0 = 空闲（与 port_probe_cmd 同方向）
        }
        if transport != "tcp" {
            let out = run_cmd(sess, cancel, "探测", &udp_port_probe_cmd(p), PROBE_TIMEOUT).await?;
            free &= out.exit_code != 0;
        }
        if free {
            return Ok(p);
        }
    }
    Err(format!("端口段到 {end} 已耗尽"))
}

pub async fn allocate_share_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    transports: &[&str],
) -> Result<AllocatedPorts, String> {
    let mut http_cursor = PORT_RANGE_START;
    let http = find_free_port(sess, cancel, &mut http_cursor, PORT_RANGE_END, "tcp").await?;
    let mut public_cursor = DATA_PORT_START;
    let mut internal_cursor = INTERNAL_PORT_START;
    let mut pairs = Vec::new();
    for transport in transports {
        let public = find_free_port(sess, cancel, &mut public_cursor, DATA_PORT_END, transport).await?;
        let internal = find_free_port(sess, cancel, &mut internal_cursor, INTERNAL_PORT_END, transport).await?;
        pairs.push((public, internal));
    }
    Ok(AllocatedPorts { http, pairs })
}

/// 由节点与端口对生成克隆规格；凭据在这里一次性生成。
pub fn build_specs(nodes: &[Node], pairs: &[(u16, u16)]) -> Result<Vec<ShareNodeSpec>, String> {
    nodes.iter().zip(pairs).map(|(n, (public, internal))| {
        let transport = match n.protocol.as_str() {
            "hysteria2" | "tuic" => "udp",
            "ss" => "tcp_and_udp",
            "vless" | "trojan" | "vmess" => "tcp",
            other => return Err(format!("不支持分享的协议: {other}")),
        };
        let original_params = serde_json::from_str(&n.params_json)
            .map_err(|e| format!("节点 {} 参数解析失败: {e}", n.name))?;
        let (new_credential, new_password) = if n.protocol == "tuic" {
            (uuid::Uuid::new_v4().to_string(), Some(uuid::Uuid::new_v4().to_string()))
        } else if n.protocol == "ss" {
            let key: [u8; 32] = rand::random();
            (base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key), None)
        } else {
            (uuid::Uuid::new_v4().to_string(), None)
        };
        Ok(ShareNodeSpec {
            name: n.name.clone(), protocol: n.protocol.clone(),
            original_port: n.port as u16, public_port: *public, internal_port: *internal,
            transport, original_params, new_credential, new_password,
        })
    }).collect()
}
```

单元内容（`unit_contents`）：

```rust
fn firewall_specs(pairs: &[(u16, &str)]) -> String {
    pairs.iter().map(|(p, proto)| format!("{p}:{proto}")).collect::<Vec<_>>().join(" ")
}

fn hardening(dir: &str) -> String {
    format!(
        "User=vpnshare\nGroup=vpnshare\nNoNewPrivileges=yes\nProtectSystem=strict\n\
         ProtectHome=yes\nPrivateTmp=yes\nReadWritePaths={dir}\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK\n"
    )
}

fn http_unit(id: &str, port: u16, bind: &str, dir: &str) -> String {
    format!(
        "[Unit]\nDescription=vpn-ctl share {id} subscription\nAfter=network.target\n\n[Service]\n{hardening}\
         ExecStart=/usr/bin/env python3 /srv/vpnshare/share_server.py {port} {dir} --bind {bind} \
--cert /srv/vpnshare/tls/fullchain.pem --key /srv/vpnshare/tls/privkey.pem\n\
         ExecStartPost=+/srv/vpnshare/vpnshare-firewall.sh open vpnshare-{id} {port}:tcp\n\
         ExecStopPost=+/srv/vpnshare/vpnshare-firewall.sh close vpnshare-{id} {port}:tcp\n\
         Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n",
        hardening = hardening(dir),
    )
}

fn gateway_unit(id: &str, dir: &str, bind: &str, pairs: &[(u16, &str)]) -> String {
    let specs = firewall_specs(pairs);
    format!(
        "[Unit]\nDescription=vpn-ctl share {id} gateway\nAfter=network.target\n\n[Service]\n{hardening}\
         ExecStart=/usr/bin/env python3 /srv/vpnshare/share_gateway.py --config {dir}/gateway.json \
--usage {dir}/usage.json --bind {bind}\n\
         ExecStartPost=+/srv/vpnshare/vpnshare-firewall.sh open vpnshare-{id} {specs}\n\
         ExecStopPost=+/srv/vpnshare/vpnshare-firewall.sh close vpnshare-{id} {specs}\n\
         Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n",
        hardening = hardening(dir),
    )
}

fn instance_unit(id: &str, kind: &str, dir: &str, config: &str) -> String {
    let exec = match kind {
        "xray" => format!("/usr/local/bin/xray run -config {config}"),
        "singbox" => format!("/usr/local/bin/sing-box run -c {config}"),
        _ => format!("/usr/local/bin/ssserver -c {config}"),
    };
    format!(
        "[Unit]\nDescription=vpn-ctl share {id} {kind}\nAfter=network.target\n\n[Service]\n{hardening}\
         ExecStart={exec}\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n",
        hardening = hardening(dir),
    )
}
```

验证命令：

```rust
// 单元 active
run_cmd(sess, cancel, "确认", &format!("systemctl is-active {}", unit), PROBE_TIMEOUT)
// 公网监听
run_cmd(sess, cancel, "确认", &format!("ss -H -tln 'sport = :{port}' | grep -q ."), PROBE_TIMEOUT) // exit 0 = 在听
// 回环只绑 127.0.0.1：ss 输出不得含 0.0.0.0:<internal>
// HTTPS
run_cmd(sess, cancel, "自测", &format!(
  "curl -s -o /dev/null -w '%{{http_code}}' --resolve {domain}:{http_port}:127.0.0.1 https://{domain}:{http_port}/sub/{token} --max-time 10"), PROBE_TIMEOUT)
```

回滚：

```rust
pub async fn rollback_created(sess: &mut SshSession, cancel: &AtomicBool, id: &str, units: &[String], ports: &[(u16, &str)]) -> Vec<String> {
    let mut leftovers = Vec::new();
    for unit in units {
        if run_cmd_ok(sess, cancel, "回滚", &format!("systemctl disable --now {unit} 2>/dev/null || true"), CONFIG_TIMEOUT).await.is_err() {
            leftovers.push(format!("unit {unit}"));
        }
        let _ = run_cmd_ok(sess, cancel, "回滚", &format!("rm -f {}", unit_path(unit)), CONFIG_TIMEOUT).await;
    }
    let _ = run_cmd_ok(sess, cancel, "回滚", &format!("rm -rf {}", share_dir(id)), CONFIG_TIMEOUT).await;
    for (port, proto) in ports {
        let _ = run_cmd_ok(sess, cancel, "回滚", &format!("iptables -D INPUT -p {proto} --dport {port} -m comment --comment vpnshare-{id}-{port}-{proto} -j ACCEPT 2>/dev/null || true"), CONFIG_TIMEOUT).await;
    }
    leftovers
}
```

- [ ] **Step 4: 成功路径测试**

用 FakeSshServer 压桩：`command -v python3`、端口探测（全部 exit 1）、`read_remote_file` 主配置（用 `script_remote_conf`）、`systemctl is-active`、`ss`、`curl` 200。断言写入的文件包含 manifest/gateway/YAML/JSON/单元；断言命令序列含 `daemon-reload`、`enable --now`、防火墙 open。

- [ ] **Step 5: 跑测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/share/install.rs src-tauri/src/share/mod.rs tests/rust/share/install_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 创建分享的完整编排与回滚"
```

---

### T8: 列举分享（新格式 + 旧格式）

- **Depends on:** T7

**Files:**
- Modify: `src-tauri/src/share/install.rs`
- Modify: `src-tauri/src/share/mod.rs`（`proxy_names`、`unit_needs_upgrade`、`upgrade_share_unit`、`unit_content` 改 `pub(crate)`，供旧格式列表/升级复用）
- Create: `tests/rust/share/list_tests.rs`
- Modify: `tests/rust/share/mods.rs`

**Interfaces:**
- Produces: `install::{list_shares, parse_summary, legacy_info}`；状态统一复用 T7 的 `install::status_from`。
- Consumes: `share_summary.py` 输出。

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install::list` 绿。

- **状态:** completed ｜ **证据:** commit `dfbf95f`；`cargo test --features local-tests share::` 71 passed，其中 `list_tests` 4 passed（摘要解析、新分享状态/用量/last_access、过滤 `tls` 非分享目录、旧分享只读映射、旧单元补防火墙持久化并验链接）；旧升级用例同步改为断言删旧规则与 `ExecStartPost/ExecStopPost`。

- [ ] **Step 1: 写摘要解析测试**

```rust
#[test]
fn parse_summary_maps_new_and_legacy_entries() {
    let json = r#"[
      {"kind":"new","id":"0123456789abcdef","manifest":{"version":1,"id":"0123456789abcdef","token":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","server_id":1,"created_at":"2026-10-20T00:00:00Z","quota_bytes":100,"rate_up_mbps":10,"rate_down_mbps":20,"expires_at_unix":1,"http_port":18080,"domain":"share.example.com","nodes":[]},"usage":{"used_bytes":50,"blocked":null,"blocked_at":null,"updated_at":"2026-10-20T00:00:00Z"},"units":{"vpnshare-0123456789abcdef":"active"}},
      {"kind":"legacy","id":"fedcba9876543210","unit":"[Service]\nExecStart=/usr/bin/env python3 /srv/vpnshare/share_server.py 18081 /srv/vpnshare/fedcba9876543210\n","snapshot":"proxies:\n- name: 旧节点\n","active":true}
    ]"#;
    let parsed = parse_summary(json).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].kind, "new");
    assert_eq!(parsed[1].kind, "legacy");
}
```

- [ ] **Step 2: 实现 `list_shares`**

摘要类型与解析：

```rust
#[derive(Debug, Deserialize)]
pub struct SummaryEntry {
    pub kind: String,
    pub id: String,
    #[serde(default)] pub manifest: Option<ShareManifest>,
    #[serde(default)] pub usage: Option<UsageState>,
    #[serde(default)] pub last_access: Option<LastAccess>,
    #[serde(default)] pub units: Option<std::collections::HashMap<String, Option<String>>>,
    #[serde(default)] pub unit: Option<String>,
    #[serde(default)] pub snapshot: Option<String>,
    #[serde(default)] pub active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LastAccess {
    pub at: String,
    pub ip: String,
    #[serde(default)] pub format: Option<String>,
}

pub fn parse_summary(json: &str) -> Result<Vec<SummaryEntry>, String> {
    serde_json::from_str(json).map_err(|e| format!("分享摘要解析失败: {e}"))
}
```

- 逐服务器并发（沿用 `JoinSet`）。每台先幂等写入 `/srv/vpnshare/share_summary.py`（`write_remote_file` + `chmod 755`），再跑 `python3 /srv/vpnshare/share_summary.py /srv/vpnshare`，解析 JSON。旧服务器没有该脚本也能列出；写入失败按「读不到」处理并带原因。
- 新分享：`ShareManifest` + `UsageState` → `ShareInfo`；若 `last_access` 存在，填入 `last_access_at`/`last_access_ip`。状态复用 T7 的 `status_from`；`active` = HTTP、网关与所有实例单元均 `active`。
- 旧分享：从 `unit` 解析端口（沿用 `parse_port_from_unit`）、从 `snapshot` 解析节点名（`proxy_names`）、`active` → `ShareStatus::Legacy`。
- 读不到的服务器进 `unreachable`，带原因。
- 旧分享升级：若单元缺 `share_server.py`，调用现有升级逻辑；`unit_content` 同时加
  `ExecStartPost=+... firewall open vpnshare-<token> <port>:tcp` 与对应的 `ExecStopPost`。
  `upgrade_share_unit` 在 restart 前先删旧注释规则：
  `iptables -D INPUT -p tcp --dport <port> -m comment --comment "vpnshare-<token>" -j ACCEPT 2>/dev/null || true`，
  再由 `ExecStartPost` 用新注释重开。

旧格式 → `ShareInfo`：

```rust
fn legacy_info(
    server: &Server,
    id: &str,
    unit: &str,
    snapshot: Option<&str>,
    active: bool,
) -> ShareInfo {
    let port = crate::share::parse_port_from_unit(unit).unwrap_or(0);
    let host = crate::domain::effective_host(server).to_string();
    ShareInfo {
        id: id.to_string(),
        server_id: server.id,
        server_name: server.name.clone(),
        host: host.clone(),
        token: id.to_string(),
        port,
        active,
        url: legacy_url(&host, port, id),
        url_singbox: String::new(), // 旧分享没有 sing-box 与 HTTPS
        node_names: snapshot.map(crate::share::proxy_names).unwrap_or_default(),
        legacy: true,
        status: ShareStatus::Legacy,
        status_detail: Some("旧版·无管控".into()),
        quota_bytes: 0,
        used_bytes: 0,
        rate_up_mbps: 0,
        rate_down_mbps: 0,
        expires_at: None,
        last_access_at: None,
        last_access_ip: None,
    }
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install::list`
Expected: PASS

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/share/install.rs tests/rust/share/list_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 批量列举新旧分享与状态推导"
```

---

### T9: 编辑、重置用量与作废

- **Depends on:** T8

**Files:**
- Modify: `src-tauri/src/share/install.rs`
- Create: `tests/rust/share/manage_tests.rs`
- Modify: `tests/rust/share/mods.rs`

**Interfaces:**
- Produces: `install::{update_share, reset_usage, delete_share}`。

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install::manage` 绿。

- **状态:** completed ｜ **证据:** commit `df8c0b3`；`cargo test --features local-tests share::manage_tests` 4 passed（编辑停-写-起且保留已用字节、重置归零并恢复 active、作废清理新旧防火墙注释且旧分享二次调用幂等、非法 id 不连服务器）；作废时逐个核验单元确实停止后才删文件。

- [ ] **Step 1: 写更新/重置/作废测试**

```rust
use crate::share::install::{delete_share, reset_usage, update_share};
use crate::share::model::{ShareOptions, ShareStatus};
use crate::ssh::tests::{script_remote_conf, FakeSshServer};

const ID: &str = "0123456789abcdef";

fn manifest_json(server_id: i64) -> String {
    serde_json::json!({
        "version": 1, "id": ID, "token": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "server_id": server_id, "created_at": "2026-10-20T00:00:00Z",
        "quota_bytes": 1024, "rate_up_mbps": 1, "rate_down_mbps": 2,
        "expires_at_unix": 1800000000, "http_port": 18080,
        "domain": "share.example.com",
        "nodes": [{
            "name": "分享-ss", "protocol": "ss", "public_port": 18200,
            "internal_port": 18500, "transport": "tcp_and_udp",
            "params_json": "{\"password\":\"x\"}"
        }]
    })
    .to_string()
}

fn usage_json(used: u64, blocked: &str) -> String {
    serde_json::json!({
        "used_bytes": used, "blocked": blocked,
        "blocked_at": "2026-10-20T00:00:00Z",
        "updated_at": "2026-10-20T00:00:00Z"
    })
    .to_string()
}

#[tokio::test]
async fn update_share_rewrites_manifest_and_gateway_then_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let state = super::tests::shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let server_id = super::tests::seed_server(&state, &srv);
    script_remote_conf(&srv, &manifest_path(ID), Some(&manifest_json(server_id)));
    script_remote_conf(&srv, &usage_path(ID), Some(&usage_json(500, "quota")));
    srv.push_script("date +%s", 0, "1700000000\n", "");
    srv.push_script(
        "systemctl is-active vpnshare-0123456789abcdef-gateway",
        0,
        "active\n",
        "",
    );

    let info = update_share(
        &state,
        server_id,
        ID,
        ShareOptions { quota_bytes: 8192, rate_up_mbps: 3, rate_down_mbps: 4, expires_in_days: 10 },
    )
    .await
    .unwrap();

    assert_eq!(info.quota_bytes, 8192);
    assert_eq!(info.rate_up_mbps, 3);
    assert_eq!(info.expires_at.as_deref(), Some("2023-11-24T22:13:20+00:00"));
    assert!(srv.saw_command("systemctl stop vpnshare-0123456789abcdef-gateway"));
    assert!(srv.saw_command("systemctl start vpnshare-0123456789abcdef-gateway"));

    let files = srv.files.lock().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&files[&manifest_path(ID)]).unwrap();
    assert_eq!(manifest["quota_bytes"], 8192);
    let gateway: serde_json::Value =
        serde_json::from_slice(&files[&gateway_config_path(ID)]).unwrap();
    assert_eq!(gateway["quota_bytes"], 8192);
    assert_eq!(gateway["expires_at_unix"], 1700000000 + 10 * 86400);
    assert_eq!(gateway["ports"][0]["public"], 18200);
    assert_eq!(gateway["ports"][0]["target"], 18500);

    // 编辑配额解除停用，但保留已用字节。
    let usage: serde_json::Value = serde_json::from_slice(&files[&usage_path(ID)]).unwrap();
    assert_eq!(usage["used_bytes"], 500);
    assert!(usage["blocked"].is_null());
}

#[tokio::test]
async fn reset_usage_clears_counter_and_unblocks() {
    let dir = tempfile::tempdir().unwrap();
    let state = super::tests::shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let server_id = super::tests::seed_server(&state, &srv);
    script_remote_conf(&srv, &manifest_path(ID), Some(&manifest_json(server_id)));
    script_remote_conf(&srv, &usage_path(ID), Some(&usage_json(999, "quota")));
    srv.push_script(
        "systemctl is-active vpnshare-0123456789abcdef-gateway",
        0,
        "active\n",
        "",
    );

    let info = reset_usage(&state, server_id, ID).await.unwrap();
    assert_eq!(info.used_bytes, 0);
    assert_eq!(info.status, ShareStatus::Active);

    let files = srv.files.lock().unwrap();
    let usage: serde_json::Value = serde_json::from_slice(&files[&usage_path(ID)]).unwrap();
    assert_eq!(usage["used_bytes"], 0);
    assert!(usage["blocked"].is_null());
}

#[tokio::test]
async fn delete_share_stops_units_and_is_idempotent_for_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let state = super::tests::shared_state(&dir);
    let srv = FakeSshServer::start().await;
    let server_id = super::tests::seed_server(&state, &srv);
    script_remote_conf(&srv, &manifest_path(ID), Some(&manifest_json(server_id)));

    delete_share(&state, server_id, ID).await.unwrap();
    for unit in [
        "vpnshare-0123456789abcdef",
        "vpnshare-0123456789abcdef-gateway",
        "vpnshare-0123456789abcdef-xray",
        "vpnshare-0123456789abcdef-singbox",
        "vpnshare-0123456789abcdef-ss",
    ] {
        assert!(srv.saw_command(&format!("systemctl disable --now {unit}")));
    }
    assert!(srv.saw_command("rm -rf /srv/vpnshare/0123456789abcdef"));
    assert!(srv.saw_command("--dport 18200"));
    assert!(srv.saw_command("--dport 18200 -m comment --comment"));
    assert!(srv.saw_command("-p udp --dport 18200"));

    // 第二次按旧分享路径：没有 share.json，从单元文件取旧端口；仍不报错。
    script_remote_conf(&srv, &manifest_path(ID), None);
    script_remote_conf(
        &srv,
        &unit_path("vpnshare-0123456789abcdef"),
        Some("[Service]\nExecStart=/usr/bin/env python3 /srv/vpnshare/share_server.py 18080 /srv/vpnshare/0123456789abcdef\n"),
    );
    delete_share(&state, server_id, ID).await.unwrap();
    assert!(srv.saw_command("--comment \"vpnshare-0123456789abcdef\""));
    assert_eq!(
        srv.recorded()
            .iter()
            .filter(|c| c.contains("rm -rf /srv/vpnshare/0123456789abcdef"))
            .count(),
        2
    );
}
```

- [ ] **Step 2: 实现**

```rust
pub async fn update_share(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
    options: ShareOptions,
) -> Result<ShareInfo, String> {
    if !is_valid_id(id) {
        return Err("分享 id 非法".into());
    }
    validate_options(&options)?;
    let (auth, server) = crate::servers::load_auth(state, server_id)
        .map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await?;
    let cancel = AtomicBool::new(false);

    let mut manifest: ShareManifest =
        match read_remote_file(&mut sess, &cancel, "读取", &manifest_path(id), PROBE_TIMEOUT).await? {
            RemoteFile::Absent => return Err("分享不存在或不是新版分享".into()),
            RemoteFile::Content(c) => {
                serde_json::from_str(&c).map_err(|e| format!("分享清单损坏: {e}"))?
            }
        };
    if manifest.server_id != server_id {
        return Err("分享不属于该服务器".into());
    }
    let mut usage: UsageState =
        match read_remote_file(&mut sess, &cancel, "读取", &usage_path(id), PROBE_TIMEOUT).await? {
            RemoteFile::Content(c) => serde_json::from_str(&c).unwrap_or_default(),
            RemoteFile::Absent => UsageState::default(),
        };

    let expires_at_unix =
        server_now_unix(&mut sess, &cancel).await? + options.expires_in_days as i64 * 86400;
    manifest.quota_bytes = options.quota_bytes;
    manifest.rate_up_mbps = options.rate_up_mbps;
    manifest.rate_down_mbps = options.rate_down_mbps;
    manifest.expires_at_unix = expires_at_unix;
    let gateway = GatewayConfig {
        quota_bytes: manifest.quota_bytes,
        rate_up_mbps: manifest.rate_up_mbps,
        rate_down_mbps: manifest.rate_down_mbps,
        expires_at_unix,
        ports: manifest
            .nodes
            .iter()
            .map(|n| GatewayPort {
                public: n.public_port,
                target: n.internal_port,
                proto: n.transport.clone(),
            })
            .collect(),
    };

    // 编辑解除停用，但保留已用字节；新配额仍不足时 status_from 会继续显示超额。
    usage.blocked = None;
    usage.blocked_at = None;
    usage.updated_at = chrono::Utc::now().to_rfc3339();

    let gw_unit = gateway_unit_name(id);
    run_cmd_ok(&mut sess, &cancel, "停止网关", &format!("systemctl stop {gw_unit}"), CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &manifest_path(id), &serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &gateway_config_path(id), &serde_json::to_string_pretty(&gateway).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "写入", &usage_path(id), &serde_json::to_string(&usage).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    run_cmd_ok(&mut sess, &cancel, "启动网关", &format!("systemctl start {gw_unit}"), CONFIG_TIMEOUT).await?;
    let out = run_cmd(&mut sess, &cancel, "确认", &format!("systemctl is-active {gw_unit}"), PROBE_TIMEOUT).await?;
    if out.stdout.trim() != "active" {
        return Err(format!("网关 {gw_unit} 未 active"));
    }
    Ok(info_from_manifest(&manifest, &usage, &server, true))
}

pub async fn reset_usage(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
) -> Result<ShareInfo, String> {
    if !is_valid_id(id) {
        return Err("分享 id 非法".into());
    }
    let (auth, server) = crate::servers::load_auth(state, server_id)
        .map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await?;
    let cancel = AtomicBool::new(false);
    let manifest: ShareManifest =
        match read_remote_file(&mut sess, &cancel, "读取", &manifest_path(id), PROBE_TIMEOUT).await? {
            RemoteFile::Absent => return Err("分享不存在或不是新版分享".into()),
            RemoteFile::Content(c) => {
                serde_json::from_str(&c).map_err(|e| format!("分享清单损坏: {e}"))?
            }
        };
    let usage = UsageState::default();
    let gw_unit = gateway_unit_name(id);
    run_cmd_ok(&mut sess, &cancel, "停止网关", &format!("systemctl stop {gw_unit}"), CONFIG_TIMEOUT).await?;
    write_remote_file(&mut sess, &cancel, "重置用量", &usage_path(id), &serde_json::to_string(&usage).map_err(|e| e.to_string())?, CONFIG_TIMEOUT).await?;
    run_cmd_ok(&mut sess, &cancel, "启动网关", &format!("systemctl start {gw_unit}"), CONFIG_TIMEOUT).await?;
    let out = run_cmd(&mut sess, &cancel, "确认", &format!("systemctl is-active {gw_unit}"), PROBE_TIMEOUT).await?;
    if out.stdout.trim() != "active" {
        return Err(format!("网关 {gw_unit} 未 active"));
    }
    Ok(info_from_manifest(&manifest, &usage, &server, true))
}

pub async fn delete_share(
    state: &Arc<AppState>,
    server_id: i64,
    id: &str,
) -> Result<(), String> {
    if !is_valid_id(id) {
        return Err("分享 id 非法".into());
    }
    let (auth, server) = crate::servers::load_auth(state, server_id)
        .map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await?;
    let cancel = AtomicBool::new(false);

    let manifest: Option<ShareManifest> =
        match read_remote_file(&mut sess, &cancel, "读取", &manifest_path(id), PROBE_TIMEOUT).await? {
            RemoteFile::Absent => None,
            RemoteFile::Content(c) => serde_json::from_str(&c).ok(),
        };
    let mut ports: Vec<(u16, &'static str)> = Vec::new();
    match &manifest {
        Some(m) => {
            for n in &m.nodes {
                match n.transport.as_str() {
                    "tcp_and_udp" => {
                        ports.push((n.public_port, "tcp"));
                        ports.push((n.public_port, "udp"));
                    }
                    "tcp" => ports.push((n.public_port, "tcp")),
                    "udp" => ports.push((n.public_port, "udp")),
                    _ => {}
                }
            }
        }
        None => {
            // 旧分享：端口的事实源是单元文件。
            if let RemoteFile::Content(unit) =
                read_remote_file(&mut sess, &cancel, "读取", &unit_path(&unit_name(id)), PROBE_TIMEOUT).await?
            {
                if let Some(p) = crate::share::parse_port_from_unit(&unit) {
                    ports.push((p, "tcp"));
                }
            }
        }
    }

    for unit in [
        unit_name(id),
        gateway_unit_name(id),
        instance_unit_name(id, "xray"),
        instance_unit_name(id, "singbox"),
        instance_unit_name(id, "ss"),
    ] {
        let _ = run_cmd_ok(&mut sess, &cancel, "作废", &format!("systemctl disable --now {unit} 2>/dev/null || true"), CONFIG_TIMEOUT).await;
        let _ = run_cmd(&mut sess, &cancel, "作废", &format!("rm -f {}", unit_path(&unit)), CONFIG_TIMEOUT).await;
    }
    run_cmd_ok(&mut sess, &cancel, "作废", &format!("rm -rf {}", share_dir(id)), CONFIG_TIMEOUT).await?;

    for (port, proto) in ports {
        // 新格式注释带端口/proto；旧格式注释只有 vpnshare-<id>。两种都清。
        for comment in [
            format!("vpnshare-{id}-{port}-{proto}"),
            format!("vpnshare-{id}"),
        ] {
            let _ = run_cmd(
                &mut sess,
                &cancel,
                "撤防火墙",
                &format!(
                    "while iptables -C INPUT -p {proto} --dport {port} -m comment --comment \"{comment}\" -j ACCEPT 2>/dev/null; do \
                     iptables -D INPUT -p {proto} --dport {port} -m comment --comment \"{comment}\" -j ACCEPT; done"
                ),
                CONFIG_TIMEOUT,
            )
            .await;
        }
    }
    run_cmd_ok(&mut sess, &cancel, "作废", "systemctl daemon-reload", CONFIG_TIMEOUT).await?;
    Ok(())
}
```

- 旧分享：`id == token`（16 位），单元名 `vpnshare-<id>`，端口从单元解析；不删除证书。
- 新分享：`share.json` 里的 `nodes[].public_port/transport` 是防火墙端口的权威来源；单元先 `disable --now` 再删文件，重复调用不报错。

- [ ] **Step 3: 跑测试与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share::install::manage`
Expected: PASS

```bash
git add src-tauri/src/share/install.rs tests/rust/share/manage_tests.rs tests/rust/share/mods.rs
git commit -m "feat(share): 编辑配额、重置用量与作废"
```

---

### T10: IPC 命令与注册

- **Depends on:** T7-T9

**Files:**
- Modify: `src-tauri/src/share/mod.rs`
- Modify: `src-tauri/src/commands/ipc.rs`
- Modify: `src-tauri/src/main.rs:287-330`
- Modify: `tests/rust/ipc_tests.rs`
- Modify: `tests/rust/share/tests.rs`
- Modify: `docs/design/ipc-interface-reference.md`

**Interfaces:**
- Produces IPC：`list_share_candidates_cmd`、`create_share_cmd(server_id,node_ids,quota_gb,rate_up_mbps,rate_down_mbps,expires_in_days)`、`update_share_cmd(...)`、`reset_share_usage_cmd`、`delete_share_cmd(server_id,id)`、`prepare_share_tls_cmd`、`share_tls_status_cmd`。

- **验收:** `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share_cmd` 绿；命令注册无遗漏。

- **状态:** completed ｜ **证据:** commit `9697201`；旧 `share/mod.rs` 上删除 738 行后收敛为模块声明 + 根导出 + 旧分享辅助；`cargo test --features local-tests share_cmd` 2 passed；新增 `list_share_candidates_cmd`/`prepare_share_tls_cmd` IPC 用例各 1 passed；`cargo test --features local-tests share::` 55 passed；`main.rs` 与 `ipc_tests` 的 invoke_handler 同步注册 8 条分享命令；`docs/design/ipc-interface-reference.md` 已更新。

- [ ] **Step 1: 删除旧实现，建立根导出**

`share/mod.rs` 收敛为模块声明 + 根导出 + 旧格式要用的辅助函数：

```rust
pub mod clone;
pub mod install;
pub mod model;
pub mod naming;
pub mod plan;
pub mod scripts;
pub mod tls;

pub use install::{create_share, delete_share, list_shares, reset_usage, update_share, CreateShareInput};
pub use model::*;
pub use naming::*;
pub use plan::share_candidates;
pub use tls::{prepare_tls, tls_status};

pub fn parse_port_from_unit(unit: &str) -> Option<u16> { /* 原样保留 */ }
pub(crate) fn proxy_names(yaml: &str) -> Vec<String> { /* 原样保留 */ }
pub fn unit_needs_upgrade(unit: &str) -> bool { /* 原样保留 */ }
pub(crate) fn unit_content(token: &str, port: u16) -> String { /* 改调 naming::share_dir / naming::share_server_script_path，并加防火墙 ExecStartPost/ExecStopPost */ }
pub(crate) async fn upgrade_share_unit(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    token: &str,
    port: u16,
) -> Result<(), String> { /* 保留原实现，改调 scripts::share_server_script() 与 naming::* */ }
```

删除旧文件中已由新模块接管的全部内容：`create_share`、`list_shares`、`read_server_shares`、
`delete_share`、`last_known_port`、`probe_share_link`、`probe_service_active`、`allocate_port`、
`rule_body`/`accept_rule`/`check_rule`/`drop_rule`、`url_for`、`config_path`、`new_token`、
`is_valid_token`、`share_dir`、`unit_name`、`unit_path`、`share_server_script`、
`share_server_script_path`、`SHARE_SERVER_SCRIPT_NAME`、旧常量 `SHARE_ROOT`/`PORT_RANGE_*`、
旧 `ShareInfo`/`ShareList`。保留 `parse_port_from_unit`、`proxy_names`、`unit_needs_upgrade`、
`unit_content`、`upgrade_share_unit`（T8 的旧分享升级仍要用）。

`tests/rust/share/tests.rs` 同步删掉针对旧 API 的用例，保留 `shared_state`/`seed_server`/
`seed_node`/`connect` 夹具（T7-T9 的测试复用）与上述辅助函数用例。

Run: `rg -n "share::(create_share|list_shares|delete_share|ShareInfo|ShareList|url_for|accept_rule|drop_rule|check_rule)" src-tauri/src tests/rust`
Expected: 无输出（IPC 尚未改，下一步改完后应仍为空）。

- [ ] **Step 2: 写 IPC 测试**

- `create_share_cmd` 空 nodeIds → 报错且不连服务器（沿用现有测试，参数加 `quotaGb/rateUpMbps/rateDownMbps/expiresInDays`）。
- `delete_share_cmd` 非法 id → 报错（字母大写/注入串）。
- `list_share_candidates_cmd` 返回候选与 `shareable`。
- `prepare_share_tls_cmd` 未配域名 → 可读错误。

- [ ] **Step 3: 实现命令并注册**

`ipc.rs`：

```rust
#[tauri::command]
pub async fn list_share_candidates_cmd(state: tauri::State<'_, Arc<AppState>>, server_id: i64) -> R<Vec<crate::share::ShareCandidate>> {
    crate::share::share_candidates(state.inner(), server_id).map_err(err)
}

#[tauri::command]
pub async fn create_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    node_ids: Vec<String>,
    quota_gb: u64,
    rate_up_mbps: u32,
    rate_down_mbps: u32,
    expires_in_days: u32,
) -> R<crate::share::ShareInfo> {
    crate::share::create_share(state.inner(), crate::share::CreateShareInput {
        server_id, node_ids,
        options: crate::share::ShareOptions {
            quota_bytes: crate::share::quota_gb_to_bytes(quota_gb),
            rate_up_mbps, rate_down_mbps, expires_in_days,
        },
    }).await.map_err(err)
}

#[tauri::command]
pub async fn list_shares_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<crate::share::ShareList> {
    crate::share::list_shares(state.inner()).await.map_err(err)
}

#[tauri::command]
pub async fn update_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    id: String,
    quota_gb: u64,
    rate_up_mbps: u32,
    rate_down_mbps: u32,
    expires_in_days: u32,
) -> R<crate::share::ShareInfo> {
    crate::share::update_share(
        state.inner(),
        server_id,
        &id,
        crate::share::ShareOptions {
            quota_bytes: crate::share::quota_gb_to_bytes(quota_gb),
            rate_up_mbps,
            rate_down_mbps,
            expires_in_days,
        },
    )
    .await
    .map_err(err)
}

#[tauri::command]
pub async fn reset_share_usage_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    id: String,
) -> R<crate::share::ShareInfo> {
    crate::share::reset_usage(state.inner(), server_id, &id).await.map_err(err)
}

#[tauri::command]
pub async fn delete_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    id: String,
) -> R<()> {
    crate::share::delete_share(state.inner(), server_id, &id).await.map_err(err)
}

#[tauri::command]
pub async fn prepare_share_tls_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<crate::share::ShareTlsStatus> {
    crate::share::prepare_tls(state.inner(), server_id).await.map_err(err)
}

#[tauri::command]
pub async fn share_tls_status_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<crate::share::ShareTlsStatus> {
    crate::share::tls_status(state.inner(), server_id).await.map_err(err)
}
```

`main.rs` 与 `tests/rust/ipc_tests.rs` 的 `invoke_handler` 都加新命令。

- [ ] **Step 4: 跑测试与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --features local-tests share_cmd`
Expected: PASS

```bash
git add src-tauri/src/commands/ipc.rs src-tauri/src/main.rs tests/rust/ipc_tests.rs docs/design/ipc-interface-reference.md
git commit -m "feat(share): 分享管控 IPC 命令"
```

---

### T11: 前端 API 与分享页列表

- **Depends on:** T10

**Files:**
- Modify: `src/api/types.ts`
- Modify: `src/api/index.ts`
- Modify: `src/views/Shares.tsx`
- Modify: `tests/frontend/views/Shares.test.tsx`

**Interfaces:**
- Produces: `ShareInfo/ShareStatus/ShareCandidate/ShareTlsStatus` TS 类型；`listShareCandidates/createShare/updateShare/resetShareUsage/deleteShare/prepareShareTls/shareTlsStatus`。

- **验收:** `npx tsc --noEmit` 绿；`npm test -- Shares` 绿。

- **状态:** completed ｜ **证据:** commit `1897223`；`npx tsc --noEmit` 无输出；`npm test -- Shares` 15 passed（双格式复制、用量进度条、超额文案、旧版行只读/无 sing-box、作废与失联提示）；向导暂以默认 100GB/100Mbps/30 天调用新 API，T12 接入用户输入。

- [ ] **Step 1: 更新 TS 类型与 API**

`types.ts`：

```ts
export type ShareStatus = 'active' | 'blocked_quota' | 'blocked_expired' | 'degraded' | 'legacy'

export interface ShareInfo {
  id: string
  server_id: number
  server_name: string
  host: string
  token: string
  port: number
  active: boolean
  url: string
  url_singbox: string
  node_names: string[]
  legacy: boolean
  status: ShareStatus
  status_detail: string | null
  quota_bytes: number
  used_bytes: number
  rate_up_mbps: number
  rate_down_mbps: number
  expires_at: string | null
  last_access_at: string | null
  last_access_ip: string | null
}

export interface ShareList {
  shares: ShareInfo[]
  unreachable: string[]
}

export interface ShareCandidate {
  node: Node
  shareable: boolean
  reason: string | null
}

export interface ShareTlsStatus {
  domain: string
  ready: boolean
  cert_expires_at: string | null
  detail: string
}
```

`api/index.ts`：

```ts
export function listShares(): Promise<ShareList> {
  return invoke('list_shares_cmd')
}
export function listShareCandidates(serverId: number): Promise<ShareCandidate[]> {
  return invoke('list_share_candidates_cmd', { serverId })
}
export function createShare(serverId: number, nodeIds: string[], quotaGb: number, rateUpMbps: number, rateDownMbps: number, expiresInDays: number): Promise<ShareInfo> {
  return invoke('create_share_cmd', { serverId, nodeIds, quotaGb, rateUpMbps, rateDownMbps, expiresInDays })
}
export function updateShare(serverId: number, id: string, quotaGb: number, rateUpMbps: number, rateDownMbps: number, expiresInDays: number): Promise<ShareInfo> {
  return invoke('update_share_cmd', { serverId, id, quotaGb, rateUpMbps, rateDownMbps, expiresInDays })
}
export function resetShareUsage(serverId: number, id: string): Promise<ShareInfo> {
  return invoke('reset_share_usage_cmd', { serverId, id })
}
export function deleteShare(serverId: number, id: string): Promise<void> {
  return invoke('delete_share_cmd', { serverId, id })
}
export function prepareShareTls(serverId: number): Promise<ShareTlsStatus> {
  return invoke('prepare_share_tls_cmd', { serverId })
}
export function shareTlsStatus(serverId: number): Promise<ShareTlsStatus> {
  return invoke('share_tls_status_cmd', { serverId })
}
```

- [ ] **Step 2: 分享列表渲染**

`Shares.tsx` 列：链接（两个复制按钮）、节点、服务器、已用/配额进度条、速率、到期、状态、操作。状态文案映射；旧版行只保留 YAML 复制与作废。保留「读取中 / 还没有分享」逻辑。

- [ ] **Step 3: 更新测试**

`tests/frontend/views/Shares.test.tsx` 的 `share` 夹具补新字段；新增用例：配额进度条、`blocked_quota` 文案、旧版行禁用编辑、双格式复制。

- [ ] **Step 4: 跑测试与提交**

Run: `npx tsc --noEmit`；`npm test -- Shares`
Expected: PASS

```bash
git add src/api/types.ts src/api/index.ts src/views/Shares.tsx tests/frontend/views/Shares.test.tsx
git commit -m "feat(share): 前端展示配额、状态与双格式链接"
```

---

### T12: 新建向导、编辑面板、服务器域名与 ACME 邮箱

- **Depends on:** T11

**Files:**
- Create: `src/components/ShareWizard.tsx`
- Create: `src/components/ShareEditPanel.tsx`
- Modify: `src/views/Shares.tsx`
- Modify: `src/views/Servers.tsx`
- Modify: `src/views/Settings.tsx`
- Modify: `tests/frontend/views/Shares.test.tsx`
- Create: `tests/frontend/components/ShareWizard.test.tsx`
- Modify: `tests/frontend/views/Settings.test.tsx`

**Interfaces:**
- Consumes: `listShareCandidates`、`prepareShareTls`、`shareTlsStatus`、`createShare`、`updateShare`、`resetShareUsage`。

- **验收:** `npm test -- ShareWizard Shares Settings` 绿。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: 向导**

- 打开时 `listShareCandidates(serverId)`；不可分享节点置灰并显示 `reason`。
- TLS 状态条：`share_tls_status_cmd`；未 ready 时显示「配置 HTTPS」按钮（调用 `prepare_share_tls_cmd`，显示进度/错误），并禁用生成。
- 配额/速率/到期输入（默认 100 GB / 100 / 100 Mbps / 30 天），前端做同样的边界校验。
- 生成按钮调用 `createShare`；成功后显示两个链接。

- [ ] **Step 2: 编辑面板**

- 配额/速率/到期输入；保存调用 `updateShare`；「重置用量」二次确认后调用 `resetShareUsage`；到期停用需同时延长到期（提示）。

- [ ] **Step 3: 服务器域名字段与 ACME 邮箱**

- `ServerFormPanel.tsx` 的 `shareDomain` 已在 T1 加；确认 `Servers.tsx` 的 `createServer/updateServer` 透传。
- `Settings.tsx` 加「ACME 邮箱」输入，读写 `share_acme_email`，保存失败回滚。

- [ ] **Step 4: 测试与提交**

Run: `npm test -- ShareWizard Shares Settings`
Expected: PASS

```bash
git add src/components/ShareWizard.tsx src/components/ShareEditPanel.tsx src/views/Shares.tsx src/views/Servers.tsx src/views/Settings.tsx tests/frontend/components/ShareWizard.test.tsx tests/frontend/views/Shares.test.tsx tests/frontend/views/Settings.test.tsx
git add -p src/views/Servers.tsx
git commit -m "feat(share): 新建向导、编辑面板与 HTTPS 配置入口"
```

---

### T13: 文档、CHANGELOG 与全量门禁

- **Depends on:** T1-T12

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `docs/design/ipc-interface-reference.md`
- Modify: `README.md`（如测试/端口说明有变化）

- **验收:** 四道门禁全绿；CHANGELOG 记录新能力与已知限制；真机验证清单逐项有结论。

- **状态:** pending ｜ **证据:**（完成时填）

- [ ] **Step 1: CHANGELOG**

新增版本节（不覆盖现有未提交内容）：

```markdown
### 分享流量管控与 sing-box 订阅

- **新增**：每条分享独立代理实例、专属端口与凭据；订阅同时提供 Clash YAML 与 sing-box JSON。
- **新增**：配额（上下行之和）、上下行限速、到期与作废；超额/到期/作废后已导入配置立即失效。
- **新分享强制 HTTPS**：每台服务器配置分享域名，certbot 自动签发与续期；未配置不降级。
- **旧分享**：不自动迁移，标记「旧版·无管控」；补防火墙持久化，重启后不再失联。
- **限制**：分享只支持选定服务器上的自建节点；不支持跨服务器、隐私线、上游出口与挂中转节点；一条链接的配额由所有使用者共享；Python 网关吞吐有上限；HTTP-01 要求 80 端口可用。
```

- [ ] **Step 2: 全量门禁**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests
npx tsc --noEmit
npm test
python -m unittest discover -s tests/python -v
```

Expected: 全绿。若 `net::runtime_tests` 因用户开着网络自跳过（见既有约定），先用「注册表 + 端口 + 经代理 curl」三项核实，再记录实际结果。

- [ ] **Step 3: 真机验证清单**

在真实服务器逐项执行并记录：

1. `prepare_share_tls_cmd` 签发成功，证书到期时间可读。
2. `create_share_cmd` 返回 HTTPS YAML/sing-box 链接；`curl` 证书校验通过。
3. mihomo 与 sing-box 各导入一次并连通。
4. 跑流量后 `usage.json` 增长；配额耗尽后连接失败；调高配额并重置后恢复。
5. 到期后连接失败；延长到期后恢复。
6. 作废后旧配置立即失败。
7. 关闭应用、重启服务器后分享仍可用；防火墙规则恢复。
8. 旧分享仍可列出/作废，YAML 链接不变，重启后不再失联。

- [ ] **Step 4: 提交**

```bash
git add -p CHANGELOG.md
git add docs/design/ipc-interface-reference.md README.md
git commit -m "docs(share): 记录流量管控、sing-box 与 HTTPS 的行为与限制"
```

---

## 计划自审记录

- **Spec 覆盖：** 现状根因（T1-T13 的实现背景）、独立实例（T4/T7）、配额/限速/到期/吊销（T5/T7/T9）、HTTPS（T6）、安全（T1/T5/T7）、接口/UI（T10-T12）、兼容（T8/T9/T13）均有任务对应。
- **占位符：** 无 TBD/TODO；代码步骤均给出实现或明确命令。
- **类型一致：** `ShareInfo`/`ShareStatus`/`ShareOptions`/`ShareManifest`/`GatewayConfig`/`UsageState` 在 T2 定义，后续任务只引用这些名字；`create_share` 的输入在 T7/T10 一致。
- **Goal Contract：** 三要素齐；A1-A10 均可用命令或真机步骤验证。
- **模块地图：** 层级/调用/依赖三块分开，节点带中文功能名。
- **Task 卡：** ID 稳定、Depends on 闭合、每卡有验收与状态。
