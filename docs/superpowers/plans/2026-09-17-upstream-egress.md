# 上游出口（静态住宅 IP 代理）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让「客户端 → 中转机 → 买来的静态住宅 IP 代理 → 目标」这条链路能被一键建立与收回，客户端拿到的仍是一条普通 VLESS+REALITY 节点。

**Architecture:** 在中转机上跑一个**专用的第二个 Xray 实例**（独立配置文件 + 独立 systemd unit），一条「上游出口」= 该实例里的一个 VLESS+REALITY inbound + 一个 socks5/http outbound + 一条 routing 配对，多条出口靠 `inboundTag → outboundTag` 共存。客户端侧协议不变，socks5/http 永不出现在节点导出面。

**Tech Stack:** Rust（Tauri 2 命令层 + rusqlite + russh）、React 19 + TypeScript + Tailwind、vitest、Xray-core。

**规格：** [`docs/superpowers/specs/2026-09-17-upstream-egress-design.md`](../specs/2026-09-17-upstream-egress-design.md)

## Global Constraints

- **不做客户端直连形态**：socks5/http 不得进入 `config/mod.rs`、`config/singbox.rs`、订阅解析 — 这三个文件本计划一行都不改。
- **不碰 WireGuard 与 relay_rules**：中转链路（`relay/`）本计划一行都不改。
- **专用实例路径固定**：配置 `/usr/local/etc/xray-upstream/config.json`，unit `/etc/systemd/system/xray-upstream.service`，服务名 `xray-upstream`。**不得**写 `/usr/local/etc/xray/config.json`（那是 deploy 的地盘）。
- **outbound 的 `settings` 是扁平结构**（`address`/`port`/`user`/`pass`），**不是**入站那种 `servers[]` 数组。
- **密码三级隔离**：入库走 `crypto::seal_param`；**不得**出现在 `Upstream` 结构体里（该结构体经 IPC 序列化给前端）；**不得**出现在任何远端命令行中。
- **节点身份按 `upstreams.node_id` 定位**：**禁止**用 `repos::nodes::delete_self_built(protocol, addr)` — 它按协议+地址删，会误删同一中转机上兄弟出口的节点（它们的 protocol 都是 `vless`、addr 都是中转机 host）。
- **节点删除发生在命令层，不在 job 层**：`remove_upstream` 先从行上取 `node_id` 删节点、再删行，因为 job 是从 DB 现读出口集合的，行删掉之后 job 再也拿不到 `node_id`，节点会变成永远清不掉的孤儿。（实现时修正：原计划把删节点放在 job 的 StopAndClean 分支，那条路径实际可达时已经拿不到 node_id。）
- 端口范围 1-65535；`listen_port` 在同一中转机内唯一。
- Rust 门禁：`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test` 全绿。
- 前端门禁：`npm test` + `npx tsc --noEmit -p tsconfig.json` 全绿。
- 注释与用户可见文案用中文，与既有代码一致。
- **跑任何 cargo 命令前先 `npm run fetch:sidecar`**（tauri-build 校验 bundle.resources 存在性，缺失则 cargo 命令直接失败）。

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `src-tauri/src/db/migrations.rs` | 追加 v11 迁移（`upstreams` 表） | 改 |
| `src-tauri/src/domain.rs` | `Upstream` 序列化类型（**无密码字段**） | 改 |
| `src-tauri/src/db/repos.rs` | `pub mod upstreams` 仓储 + `has_dependents` 覆盖新表 | 改 |
| `src-tauri/src/db/tests.rs` | 迁移与仓储单测 | 改 |
| `src-tauri/src/servers/upstream.rs` | **新模块**：校验、Xray 配置渲染（纯函数）、部署/拆除执行器（SSH） | 建 |
| `src-tauri/src/servers/upstream_tests.rs` | 上述模块的单测 | 建 |
| `src-tauri/src/servers/mod.rs` | 挂 `pub mod upstream;` + 更新 `HasDependents` 文案 | 改 |
| `src-tauri/src/jobs/mod.rs` | `JobRequest::Upstream` 变体 + `job_id()`/`kind()` | 改 |
| `src-tauri/src/servers/deploy.rs` | `ProdRunner` 的 dispatch 加一个 match 臂 | 改 |
| `src-tauri/src/commands.rs` | 4 条命令（内层函数 + `_cmd` 包装） | 改 |
| `src-tauri/src/main.rs` | `generate_handler!` 注册 | 改 |
| `src-tauri/src/ipc_tests.rs` | 命令层测试 | 改 |
| `src/api/types.ts` / `src/api/index.ts` | 前端类型与调用 | 改 |
| `src/components/UpstreamPanel.tsx` | **新组件**：出口列表 + 添加/删除/重建 | 建 |
| `src/components/UpstreamPanel.test.tsx` | 组件测试 | 建 |
| `src/views/Servers.tsx` | 挂载面板 | 改 |
| `src-tauri/src/speed/engine.rs` | `SpeedEngine` 加 `fetch_text`（出口 IP 探测用） | 改 |
| `src-tauri/src/speed/mod.rs` | `probe_egress_ip` 执行体 | 改 |

---

### Task 1: 数据层（迁移 + 类型 + 仓储）

**Files:**
- Modify: `src-tauri/src/db/migrations.rs`（`MIGRATIONS` 数组末尾追加一条）
- Modify: `src-tauri/src/domain.rs`（文件末尾追加 `Upstream`）
- Modify: `src-tauri/src/db/repos.rs`（`pub mod upstreams` 追加在文件末尾；`servers::has_dependents` 加一处 COUNT）
- Modify: `src-tauri/src/db/tests.rs`
- Modify: `src-tauri/src/servers/mod.rs`（`HasDependents` 的错误文案）

**Interfaces:**
- Consumes: 无（本计划第一个任务）
- Produces:
  - `domain::Upstream { id: i64, server_id: i64, name: String, proto: String, host: String, port: i64, username: String, listen_port: i64, reality_dest: String, node_id: Option<String>, egress_ip: Option<String>, last_check_error: Option<String>, created_at: String }`（`Serialize`/`Deserialize`/`PartialEq`/`Clone`/`Debug`；**无 password 字段**）
  - `repos::upstreams::list_by_server(&Connection, i64) -> rusqlite::Result<Vec<Upstream>>`
  - `repos::upstreams::get(&Connection, i64) -> rusqlite::Result<Option<Upstream>>`
  - `repos::upstreams::insert(&Connection, &Upstream, password_sealed: &str, material_json: &str) -> rusqlite::Result<i64>`
  - `repos::upstreams::update(&Connection, &Upstream, password_sealed: Option<&str>) -> rusqlite::Result<()>`
  - `repos::upstreams::secrets(&Connection, i64) -> rusqlite::Result<Option<(String, String)>>`（返回 `(seal 密文, material_json)`）
  - `repos::upstreams::set_node_id(&Connection, i64, &str) -> rusqlite::Result<()>`
  - `repos::upstreams::set_material(&Connection, i64, &str) -> rusqlite::Result<()>`
  - `repos::upstreams::set_probe(&Connection, i64, Option<&str>, Option<&str>) -> rusqlite::Result<()>`（`egress_ip` / `last_check_error`）
  - `repos::upstreams::delete(&Connection, i64) -> rusqlite::Result<()>`

- [ ] **Step 1: 写失败的迁移测试**

在 `src-tauri/src/db/tests.rs` 末尾追加：

```rust
#[test]
fn v11_creates_upstreams_table_and_bumps_user_version() {
    let mut conn = crate::db::open_in_memory().expect("open");
    crate::db::migrations::run(&mut conn).expect("migrate");
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("version");
    assert_eq!(version, crate::db::migrations::MIGRATIONS.len() as i64);
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM upstreams", [], |r| r.get(0))
        .expect("upstreams 表应存在");
    assert_eq!(n, 0);
}

#[test]
fn v11_migration_preserves_existing_servers() {
    let mut conn = crate::db::open_in_memory().expect("open");
    // 先迁到 v10：MIGRATIONS 逐条应用，这里直接全量迁完再插入即为最简形态
    crate::db::migrations::run(&mut conn).expect("migrate");
    let s = crate::domain::Server {
        id: 0,
        name: "中转机-01".into(),
        host: "1.2.3.4".into(),
        port: 22,
        user: "root".into(),
        auth_kind: "password".into(),
        kind: "relay".into(),
        system: String::new(),
        notes: String::new(),
        created_at: "t1".into(),
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
    };
    let id = crate::db::repos::servers::insert(&conn, &s, "ref-1").expect("insert");
    assert!(crate::db::repos::servers::get(&conn, id).expect("get").is_some());
}
```

> 注：若 `db::open_in_memory` 不存在，用 `db/tests.rs` 中既有用例的同款开库辅助（读该文件顶部的既有用例照抄）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test v11_`
Expected: FAIL — `no such table: upstreams`

- [ ] **Step 3: 加迁移**

`src-tauri/src/db/migrations.rs` 的 `MIGRATIONS` 数组**末尾**（`-- v10：分跳延迟` 那条之后）追加：

```rust
    r#"
-- v11：上游出口（静态住宅 IP 代理作为中转机的出站）。一条记录 = 中转机上专用 Xray
-- 实例里的一个 inbound + 一个 outbound + 一条 routing 配对；多出口靠 inboundTag 配对共存。
-- password 为 seal 密文；material_json 首建时生成后固定（重建复用，否则客户端节点失效）。
CREATE TABLE upstreams (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id),
  name TEXT NOT NULL,
  proto TEXT NOT NULL CHECK (proto IN ('socks5','http')),
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  username TEXT NOT NULL DEFAULT '',
  password TEXT NOT NULL,
  listen_port INTEGER NOT NULL,
  reality_dest TEXT NOT NULL,
  material_json TEXT NOT NULL,
  node_id TEXT,
  egress_ip TEXT,
  last_check_error TEXT,
  created_at TEXT NOT NULL
);
"#,
```

- [ ] **Step 4: 加 domain 类型**

`src-tauri/src/domain.rs` 末尾追加：

```rust
/// 上游出口：中转机上专用 Xray 实例的一个“入站 + 出站”配对，出站指向买来的
/// 静态住宅 IP 代理。**刻意不含 password 字段**——该结构体经 IPC 序列化给前端，
/// 凭据只能经 repos::upstreams::secrets 在服务端取出即用即弃。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Upstream {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    /// "socks5" | "http"
    pub proto: String,
    pub host: String,
    pub port: i64,
    pub username: String,
    /// 中转机上本实例的入站端口（客户端节点连的就是它）。
    pub listen_port: i64,
    /// REALITY 伪装域名。
    pub reality_dest: String,
    /// 写回的节点 id（拆除时按它删，见 Global Constraints 的同名条目）。
    pub node_id: Option<String>,
    /// 最近一次探测到的出口 IP。
    pub egress_ip: Option<String>,
    /// 最近一次失败原因（脱敏）。
    pub last_check_error: Option<String>,
    pub created_at: String,
}
```

- [ ] **Step 5: 加仓储模块**

`src-tauri/src/db/repos.rs` 末尾追加（照 `pub mod channels` 的写法）：

```rust
pub mod upstreams {
    use super::*;

    const COLS: &str = "id,server_id,name,proto,host,port,username,listen_port,\
                        reality_dest,node_id,egress_ip,last_check_error,created_at";

    fn row_to_upstream(r: &rusqlite::Row<'_>) -> rusqlite::Result<Upstream> {
        Ok(Upstream {
            id: r.get(0)?,
            server_id: r.get(1)?,
            name: r.get(2)?,
            proto: r.get(3)?,
            host: r.get(4)?,
            port: r.get(5)?,
            username: r.get(6)?,
            listen_port: r.get(7)?,
            reality_dest: r.get(8)?,
            node_id: r.get(9)?,
            egress_ip: r.get(10)?,
            last_check_error: r.get(11)?,
            created_at: r.get(12)?,
        })
    }

    /// 单台中转机的出口列表（按 id 排序，输出确定性）。
    pub fn list_by_server(conn: &Connection, server_id: i64) -> rusqlite::Result<Vec<Upstream>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM upstreams WHERE server_id=?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![server_id], row_to_upstream)?;
        rows.collect()
    }

    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Upstream>> {
        conn.query_row(
            &format!("SELECT {COLS} FROM upstreams WHERE id=?1"),
            params![id],
            row_to_upstream,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    pub fn insert(
        conn: &Connection,
        u: &Upstream,
        password_sealed: &str,
        material_json: &str,
    ) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO upstreams (server_id,name,proto,host,port,username,password,\
             listen_port,reality_dest,material_json,node_id,egress_ip,last_check_error,created_at)\
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                u.server_id, u.name, u.proto, u.host, u.port, u.username, password_sealed,
                u.listen_port, u.reality_dest, material_json, u.node_id, u.egress_ip,
                u.last_check_error, u.created_at
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 更新可编辑字段；`password_sealed` 为 None 表示保留原密码。
    pub fn update(
        conn: &Connection,
        u: &Upstream,
        password_sealed: Option<&str>,
    ) -> rusqlite::Result<()> {
        match password_sealed {
            Some(pw) => conn.execute(
                "UPDATE upstreams SET name=?2,proto=?3,host=?4,port=?5,username=?6,password=?7,\
                 listen_port=?8,reality_dest=?9 WHERE id=?1",
                params![
                    u.id, u.name, u.proto, u.host, u.port, u.username, pw, u.listen_port,
                    u.reality_dest
                ],
            ),
            None => conn.execute(
                "UPDATE upstreams SET name=?2,proto=?3,host=?4,port=?5,username=?6,\
                 listen_port=?7,reality_dest=?8 WHERE id=?1",
                params![
                    u.id, u.name, u.proto, u.host, u.port, u.username, u.listen_port,
                    u.reality_dest
                ],
            ),
        }?;
        Ok(())
    }

    /// 取凭据材料：`(seal 密文, material_json)`。调用方解密即用即弃，不得外传。
    pub fn secrets(conn: &Connection, id: i64) -> rusqlite::Result<Option<(String, String)>> {
        conn.query_row(
            "SELECT password,material_json FROM upstreams WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    pub fn set_node_id(conn: &Connection, id: i64, node_id: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE upstreams SET node_id=?2 WHERE id=?1",
            params![id, node_id],
        )?;
        Ok(())
    }

    /// 写回 REALITY 身份材料（首建时由部署 job 生成后调用一次，此后固定不变）。
    pub fn set_material(conn: &Connection, id: i64, material_json: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE upstreams SET material_json=?2 WHERE id=?1",
            params![id, material_json],
        )?;
        Ok(())
    }

    /// 探测结果落库：成功时 error 置 NULL（照 servers::update_speed 的同款语义）。
    pub fn set_probe(
        conn: &Connection,
        id: i64,
        egress_ip: Option<&str>,
        last_check_error: Option<&str>,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE upstreams SET egress_ip=?2,last_check_error=?3 WHERE id=?1",
            params![id, egress_ip, last_check_error],
        )?;
        Ok(())
    }

    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM upstreams WHERE id=?1", params![id])?;
        Ok(())
    }
}
```

若 `db/repos.rs` 顶部没有 `use crate::domain::...`，按现有 `pub mod channels` 的可见性写法（`use super::*;` + `Channel` 已在作用域）确认 `Upstream` 同样可用；不可用则在该 mod 内加 `use crate::domain::Upstream;`。

- [ ] **Step 6: 把新表纳入删除服务器前的关联检查**

`src-tauri/src/db/repos.rs` 的 `servers::has_dependents`（约 747 行）在现有三处 COUNT 之后追加：

```rust
        let u: i64 = conn.query_row(
            "SELECT COUNT(*) FROM upstreams WHERE server_id=?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(d + r + c + u > 0)
```

同时把 `src-tauri/src/servers/mod.rs:38` 的文案改为：

```rust
    #[error("存在关联记录，不能删除（有部署、中转规则、通道或上游出口引用该服务器）")]
    HasDependents,
```

- [ ] **Step 7: 写仓储测试**

`src-tauri/src/db/tests.rs` 末尾追加：

```rust
fn sample_upstream(server_id: i64, name: &str, listen_port: i64) -> crate::domain::Upstream {
    crate::domain::Upstream {
        id: 0,
        server_id,
        name: name.into(),
        proto: "socks5".into(),
        host: "9.9.9.9".into(),
        port: 1080,
        username: "u".into(),
        listen_port,
        reality_dest: "www.microsoft.com".into(),
        node_id: None,
        egress_ip: None,
        last_check_error: None,
        created_at: "t1".into(),
    }
}

#[test]
fn upstreams_crud_roundtrip_keeps_password_out_of_struct() {
    let mut conn = crate::db::open_in_memory().expect("open");
    crate::db::migrations::run(&mut conn).expect("migrate");
    let sid = crate::db::repos::servers::insert(
        &conn,
        &crate::domain::Server {
            id: 0,
            name: "中转机-01".into(),
            host: "1.2.3.4".into(),
            port: 22,
            user: "root".into(),
            auth_kind: "password".into(),
            kind: "relay".into(),
            system: String::new(),
            notes: String::new(),
            created_at: "t1".into(),
            latency: None,
            bandwidth: None,
            last_checked_at: None,
            last_check_error: None,
        },
        "ref-1",
    )
    .expect("insert server");

    let u = sample_upstream(sid, "住宅01", 443);
    let id = crate::db::repos::upstreams::insert(&conn, &u, "SEALED-PW", "{\"uuid\":\"x\"}")
        .expect("insert upstream");

    let got = crate::db::repos::upstreams::get(&conn, id)
        .expect("get")
        .expect("some");
    assert_eq!(got.name, "住宅01");
    assert_eq!(got.listen_port, 443);
    assert_eq!(got.node_id, None);

    let (pw, material) = crate::db::repos::upstreams::secrets(&conn, id)
        .expect("secrets")
        .expect("some");
    assert_eq!(pw, "SEALED-PW");
    assert_eq!(material, "{\"uuid\":\"x\"}");

    crate::db::repos::upstreams::set_node_id(&conn, id, "node-1").expect("set node");
    crate::db::repos::upstreams::set_probe(&conn, id, Some("5.5.5.5"), Some("旧错误"))
        .expect("set probe");
    let got = crate::db::repos::upstreams::get(&conn, id)
        .expect("get")
        .expect("some");
    assert_eq!(got.node_id.as_deref(), Some("node-1"));
    assert_eq!(got.egress_ip.as_deref(), Some("5.5.5.5"));
    assert_eq!(got.last_check_error.as_deref(), Some("旧错误"));

    crate::db::repos::upstreams::set_probe(&conn, id, Some("5.5.5.5"), None).expect("clear err");
    let got = crate::db::repos::upstreams::get(&conn, id)
        .expect("get")
        .expect("some");
    assert_eq!(got.last_check_error, None);

    crate::db::repos::upstreams::delete(&conn, id).expect("delete");
    assert!(crate::db::repos::upstreams::get(&conn, id)
        .expect("get")
        .is_none());
}

#[test]
fn has_dependents_counts_upstreams() {
    let mut conn = crate::db::open_in_memory().expect("open");
    crate::db::migrations::run(&mut conn).expect("migrate");
    let sid = crate::db::repos::servers::insert(
        &conn,
        &crate::domain::Server {
            id: 0,
            name: "中转机-01".into(),
            host: "1.2.3.4".into(),
            port: 22,
            user: "root".into(),
            auth_kind: "password".into(),
            kind: "relay".into(),
            system: String::new(),
            notes: String::new(),
            created_at: "t1".into(),
            latency: None,
            bandwidth: None,
            last_checked_at: None,
            last_check_error: None,
        },
        "ref-1",
    )
    .expect("insert server");
    assert!(!crate::db::repos::servers::has_dependents(&conn, sid).expect("check"));

    let u = sample_upstream(sid, "住宅01", 443);
    crate::db::repos::upstreams::insert(&conn, &u, "S", "{}").expect("insert upstream");
    assert!(crate::db::repos::servers::has_dependents(&conn, sid).expect("check"));
}
```

- [ ] **Step 8: 运行测试确认通过**

Run: `cd src-tauri && cargo test`（先 `npm run fetch:sidecar`）
Expected: PASS，含新增 3 个用例

- [ ] **Step 9: 门禁 + 提交**

```bash
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings
cd .. && git add src-tauri/src/db/migrations.rs src-tauri/src/domain.rs src-tauri/src/db/repos.rs src-tauri/src/db/tests.rs src-tauri/src/servers/mod.rs
git commit -m "feat(upstream): upstreams 表与仓储（v11 迁移 + 删除检查覆盖）"
```

---

### Task 2: 校验与 Xray 配置渲染（纯函数）

**Files:**
- Create: `src-tauri/src/servers/upstream.rs`
- Create: `src-tauri/src/servers/upstream_tests.rs`
- Modify: `src-tauri/src/servers/mod.rs`（挂模块）

**Interfaces:**
- Consumes: `domain::Upstream`（Task 1）
- Produces:
  - `upstream::UPSTREAM_CONFIG_PATH: &str`、`UPSTREAM_UNIT_PATH: &str`、`UPSTREAM_SERVICE: &str`
  - `upstream::PROTOS: &[&str]`、`upstream::DEFAULT_REALITY_DEST: &str`、`upstream::DEFAULT_LISTEN_PORT: u16`
  - `upstream::Material { uuid: String, private_key: String, public_key: String, short_id: String }`（`Serialize`/`Deserialize`；**公钥一并存**，重建时无需再从私钥推导，省掉对 `xray x25519 -i` 这一版本相关参数的依赖）
  - `upstream::UpstreamInput { server_id: i64, name: String, proto: String, host: String, port: i64, username: String, password: String, listen_port: i64, reality_dest: String }`
  - `upstream::validate(input: &UpstreamInput, existing: &[Upstream]) -> Result<(), ServersError>`
  - `upstream::RenderEntry { tag: String, listen_port: u16, reality_dest: String, material: Material, proto: String, host: String, port: u16, username: String, password: String }`
  - `upstream::render_config(entries: &[RenderEntry]) -> String`
  - `upstream::render_unit() -> String`
  - `upstream::node_params(material: &Material, reality_dest: &str) -> String`
  - `upstream::tag_of(id: i64) -> String`（`format!("up-{id}")`）

- [ ] **Step 1: 写失败的渲染金样测试**

创建 `src-tauri/src/servers/upstream_tests.rs`：

```rust
//! 上游出口的纯函数单测：校验与专用实例配置渲染金样。
//! 渲染是整份配置的唯一来源（多出口共存靠它读全表），故金样必须覆盖多出口。
use super::upstream::*;
use crate::domain::Upstream;

fn entry(id: i64, proto: &str, host: &str, port: u16, listen: u16) -> RenderEntry {
    RenderEntry {
        tag: tag_of(id),
        listen_port: listen,
        reality_dest: "www.microsoft.com".into(),
        material: Material {
            uuid: format!("uuid-{id}"),
            private_key: format!("priv-{id}"),
            short_id: format!("sid{id}"),
        },
        proto: proto.into(),
        host: host.into(),
        port,
        username: "user1".into(),
        password: "pass1".into(),
    }
}

#[test]
fn render_config_single_outlet_has_flat_outbound_settings() {
    let cfg = render_config(&[entry(1, "socks5", "1.2.3.4", 1080, 443)]);
    let v: serde_json::Value = serde_json::from_str(&cfg).expect("合法 JSON");

    let inb = &v["inbounds"][0];
    assert_eq!(inb["tag"], "up-1");
    assert_eq!(inb["port"], 443);
    assert_eq!(inb["protocol"], "vless");
    assert_eq!(inb["settings"]["clients"][0]["flow"], "xtls-rprx-vision");
    assert_eq!(inb["streamSettings"]["security"], "reality");
    assert_eq!(
        inb["streamSettings"]["realitySettings"]["dest"],
        "www.microsoft.com:443"
    );
    assert_eq!(inb["sniffing"]["enabled"], true);

    // outbound 的 settings 是扁平结构——不是入站那种 servers[] 数组。写错会让
    // Xray 报配置错误且难以定位，故单独立断言。
    let out = &v["outbounds"][0];
    assert_eq!(out["protocol"], "socks");
    assert_eq!(out["tag"], "out-up-1");
    assert_eq!(out["settings"]["address"], "1.2.3.4");
    assert_eq!(out["settings"]["port"], 1080);
    assert_eq!(out["settings"]["user"], "user1");
    assert_eq!(out["settings"]["pass"], "pass1");
    assert!(out["settings"].get("servers").is_none());

    assert_eq!(v["routing"]["domainStrategy"], "AsIs");
    assert_eq!(v["routing"]["rules"][0]["inboundTag"][0], "up-1");
    assert_eq!(v["routing"]["rules"][0]["outboundTag"], "out-up-1");
}

#[test]
fn render_config_http_proto_uses_http_outbound() {
    let cfg = render_config(&[entry(1, "http", "5.6.7.8", 3128, 8443)]);
    let v: serde_json::Value = serde_json::from_str(&cfg).expect("合法 JSON");
    assert_eq!(v["outbounds"][0]["protocol"], "http");
    assert_eq!(v["outbounds"][0]["settings"]["address"], "5.6.7.8");
}

#[test]
fn render_config_two_outlets_are_paired_by_tag() {
    let cfg = render_config(&[
        entry(1, "socks5", "1.1.1.1", 1080, 443),
        entry(2, "http", "2.2.2.2", 3128, 8443),
    ]);
    let v: serde_json::Value = serde_json::from_str(&cfg).expect("合法 JSON");

    assert_eq!(v["inbounds"].as_array().unwrap().len(), 2);
    assert_eq!(v["outbounds"].as_array().unwrap().len(), 2);
    assert_eq!(v["routing"]["rules"].as_array().unwrap().len(), 2);

    // 两条规则各自配对，不得串号
    assert_eq!(v["routing"]["rules"][0]["inboundTag"][0], "up-1");
    assert_eq!(v["routing"]["rules"][0]["outboundTag"], "out-up-1");
    assert_eq!(v["routing"]["rules"][1]["inboundTag"][0], "up-2");
    assert_eq!(v["routing"]["rules"][1]["outboundTag"], "out-up-2");

    // 监听端口各自独立
    let ports: Vec<u16> = v["inbounds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["port"].as_u64().unwrap() as u16)
        .collect();
    assert_eq!(ports, vec![443, 8443]);
}

#[test]
fn validate_rejects_bad_input() {
    let existing = vec![Upstream {
        id: 1,
        server_id: 7,
        name: "住宅01".into(),
        proto: "socks5".into(),
        host: "1.1.1.1".into(),
        port: 1080,
        username: String::new(),
        listen_port: 443,
        reality_dest: DEFAULT_REALITY_DEST.into(),
        node_id: None,
        egress_ip: None,
        last_check_error: None,
        created_at: "t".into(),
    }];
    let mk = |name: &str, proto: &str, host: &str, port: i64, listen: i64| UpstreamInput {
        server_id: 7,
        name: name.into(),
        proto: proto.into(),
        host: host.into(),
        port,
        username: String::new(),
        password: "p".into(),
        listen_port: listen,
        reality_dest: DEFAULT_REALITY_DEST.into(),
    };

    assert!(validate(&mk("", "socks5", "1.1.1.1", 1080, 8443), &existing).is_err());
    assert!(validate(&mk("住宅02", "", "1.1.1.1", 1080, 8443), &existing).is_err());
    assert!(validate(&mk("住宅02", "quic", "1.1.1.1", 1080, 8443), &existing).is_err());
    assert!(validate(&mk("住宅02", "socks5", "", 1080, 8443), &existing).is_err());
    assert!(validate(&mk("住宅02", "socks5", "1.1.1.1", 0, 8443), &existing).is_err());
    assert!(validate(&mk("住宅02", "socks5", "1.1.1.1", 1080, 70000), &existing).is_err());
    // 同一中转机上 listen_port 重复
    assert!(validate(&mk("住宅02", "socks5", "1.1.1.1", 1080, 443), &existing).is_err());
    // 正常
    assert!(validate(&mk("住宅02", "http", "1.1.1.1", 3128, 8443), &existing).is_ok());
}

#[test]
fn node_params_matches_deploy_reality_shape() {
    let m = Material {
        uuid: "u-1".into(),
        private_key: "priv".into(),
        public_key: "pub-1".into(),
        short_id: "sid".into(),
    };
    let v: serde_json::Value =
        serde_json::from_str(&node_params(&m, "www.microsoft.com")).expect("JSON");
    assert_eq!(v["uuid"], "u-1");
    assert_eq!(v["security"], "reality");
    assert_eq!(v["type"], "tcp");
    assert_eq!(v["flow"], "xtls-rprx-vision");
    assert_eq!(v["sni"], "www.microsoft.com");
    assert_eq!(v["public-key"], "pub-1");
    assert_eq!(v["short-id"], "sid");
    assert_eq!(v["fingerprint"], "chrome");
    // 私钥绝不出现在下发客户端的参数里
    assert!(v.get("private-key").is_none());
}

#[test]
fn render_unit_runs_own_config_path() {
    let unit = render_unit();
    assert!(unit.contains("ExecStart=/usr/local/bin/xray run -config /usr/local/etc/xray-upstream/config.json"));
    assert!(unit.contains("Restart=on-failure"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test upstream`
Expected: FAIL — 模块不存在（先在 `servers/mod.rs` 加 `pub mod upstream;` 与 `#[cfg(test)] #[path = "upstream_tests.rs"] mod upstream_tests;` 才会编译到，第一次跑会是 `unresolved import`）

- [ ] **Step 3: 挂模块**

`src-tauri/src/servers/mod.rs` 的 `pub mod channels;` 之后加：

```rust
pub mod upstream;
#[cfg(test)]
pub mod upstream_tests;
```

> 注：现有测试模块用 `#[path = "..."]` 的写法（见 `servers/channels.rs` 末尾的 `mod channels_tests;`）。本任务把 `upstream_tests.rs` 作为 `servers` 下的公开模块挂载，与 `deploy_tests` 的挂法一致。

- [ ] **Step 4: 实现纯函数**

创建 `src-tauri/src/servers/upstream.rs`：

```rust
//! 上游出口：中转机上**专用的第二个 Xray 实例**，把买来的静态住宅 IP 代理
//! （SOCKS5/HTTP，无 SSH）接成落地。一条出口 = 一个 VLESS+REALITY inbound
//! + 一个 socks5/http outbound + 一条 routing 配对，多出口靠 inboundTag 共存。
//!
//! 为什么是独立实例而不是塞进 deploy 管的配置：`deploy` 是整文件重渲染
//! （merge_deploy_protocols + read_existing_ports），混进去后任何一次「重新部署
//! 协议」都可能把用户的住宅线路静默搞断。独立实例 = 拆除干净、零耦合。
//!
//! 客户端侧协议不变（VLESS+REALITY）——socks5/http 永不出现在节点导出面。
//! 密码只在服务端解密即用即弃，不进 IPC、不进远端命令行。

use crate::domain::Upstream;
use crate::servers::{clean_text, ServersError};

/// 专用实例的远端路径（白名单常量）。
pub const UPSTREAM_CONFIG_PATH: &str = "/usr/local/etc/xray-upstream/config.json";
pub const UPSTREAM_UNIT_PATH: &str = "/etc/systemd/system/xray-upstream.service";
/// systemd 服务名（与 unit 文件名一致）。
pub const UPSTREAM_SERVICE: &str = "xray-upstream";
/// 上游协议白名单（与前端选项同步；扩展时改这里与 src/api/types.ts 的注释）。
pub const PROTOS: &[&str] = &["socks5", "http"];
/// 默认 REALITY 伪装域名（与 DeployWizard.tsx 和 deploy.rs 的兜底一致）。
pub const DEFAULT_REALITY_DEST: &str = "www.microsoft.com";
/// 默认入站端口。
pub const DEFAULT_LISTEN_PORT: u16 = 443;

/// REALITY 身份材料：首建生成后固定，重建复用——否则客户端节点每次都要重新导入。
/// 公钥一并存下来（而非每次从私钥推导），重建时就无需依赖 `xray x25519 -i` 这个
/// 随版本变化的参数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Material {
    pub uuid: String,
    pub private_key: String,
    pub public_key: String,
    pub short_id: String,
}

/// 添加/编辑输入（IPC 扁平参数形态）。
#[derive(Debug, Clone)]
pub struct UpstreamInput {
    pub server_id: i64,
    pub name: String,
    pub proto: String,
    pub host: String,
    pub port: i64,
    pub username: String,
    pub password: String,
    pub listen_port: i64,
    pub reality_dest: String,
}

/// 出口在专用实例内的 tag：入站 `up-{id}`，出站 `out-up-{id}`。
pub fn tag_of(id: i64) -> String {
    format!("up-{id}")
}

/// 校验输入；`existing` 为同一中转机上已有的出口（用于监听端口唯一性）。
/// 编辑场景由调用方把被编辑行从 `existing` 中排除。
pub fn validate(input: &UpstreamInput, existing: &[Upstream]) -> Result<(), ServersError> {
    let name = clean_text(&input.name);
    if name.is_empty() {
        return Err(ServersError::Validation("出口名称不能为空".into()));
    }
    if !PROTOS.contains(&input.proto.as_str()) {
        return Err(ServersError::Validation(format!(
            "上游协议只支持 {}",
            PROTOS.join("/")
        )));
    }
    if clean_text(&input.host).is_empty() {
        return Err(ServersError::Validation("上游地址不能为空".into()));
    }
    if !(1..=65535).contains(&input.port) {
        return Err(ServersError::Validation("上游端口必须在 1-65535 之间".into()));
    }
    if !(1..=65535).contains(&input.listen_port) {
        return Err(ServersError::Validation(
            "监听端口必须在 1-65535 之间".into(),
        ));
    }
    if existing.iter().any(|u| u.listen_port == input.listen_port) {
        return Err(ServersError::Validation(format!(
            "监听端口 {} 已被这台中转机上的其他出口占用",
            input.listen_port
        )));
    }
    if clean_text(&input.reality_dest).is_empty() {
        return Err(ServersError::Validation("伪装域名不能为空".into()));
    }
    Ok(())
}

/// 一条出口渲染所需的全量信息（密码为**明文**，仅供渲染，不得落库或外传）。
pub struct RenderEntry {
    pub tag: String,
    pub listen_port: u16,
    pub reality_dest: String,
    pub material: Material,
    pub proto: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// 渲染专用实例的**整份**配置：每个出口一个入站 + 一个出站，routing 按 tag 配对。
/// 多出口共存靠这里读全表——整文件重渲染是安全的，因为整个实例由本工具独占。
pub fn render_config(entries: &[RenderEntry]) -> String {
    let inbounds: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "listen": "0.0.0.0",
                "port": e.listen_port,
                "protocol": "vless",
                "tag": e.tag,
                "settings": {
                    "clients": [{ "id": e.material.uuid, "flow": "xtls-rprx-vision" }],
                    "decryption": "none"
                },
                "streamSettings": {
                    "network": "tcp",
                    "security": "reality",
                    "realitySettings": {
                        "dest": format!("{}:443", e.reality_dest),
                        "serverNames": [e.reality_dest],
                        "privateKey": e.material.private_key,
                        "shortIds": [e.material.short_id]
                    }
                },
                "sniffing": { "enabled": true, "destOverride": ["http", "tls", "quic"] }
            })
        })
        .collect();

    let outbounds: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            // http 的 outbound 是 "http"，socks5 在 Xray 里叫 "socks"。
            // settings 为**扁平结构**（address/port/user/pass），不是入站的 servers[] 数组。
            serde_json::json!({
                "protocol": if e.proto == "http" { "http" } else { "socks" },
                "tag": outbound_tag(&e.tag),
                "settings": {
                    "address": e.host,
                    "port": e.port,
                    "user": e.username,
                    "pass": e.password
                }
            })
        })
        .collect();

    let rules: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "inboundTag": [e.tag],
                "outboundTag": outbound_tag(&e.tag)
            })
        })
        .collect();

    serde_json::json!({
        "inbounds": inbounds,
        "outbounds": outbounds,
        // AsIs：域名保持未解析交给上游代理远端解析——既避开 GFW 的 DNS 污染，
        // 也避免 DNS 泄漏到本机。
        "routing": { "domainStrategy": "AsIs", "rules": rules }
    })
    .to_string()
}

/// 由入站 tag 推出去站 tag（`up-1` → `out-up-1`）。
fn outbound_tag(inbound_tag: &str) -> String {
    format!("out-{inbound_tag}")
}

/// 专用实例的 systemd unit（工具自写，不依赖发布包内容；照 ss_systemd_unit 的写法）。
pub fn render_unit() -> String {
    format!(
        "[Unit]\nDescription=Xray upstream egress (managed by vpn-ctl)\nAfter=network.target\n\n\
         [Service]\nExecStart=/usr/local/bin/xray run -config {UPSTREAM_CONFIG_PATH}\n\
         Restart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

/// 下发客户端的节点参数：与 deploy 的 REALITY 节点同构（deploy.rs 的 vless 分支）。
/// **不含私钥**——私钥只在服务端。
pub fn node_params(material: &Material, reality_dest: &str) -> String {
    serde_json::json!({
        "uuid": material.uuid,
        "security": "reality",
        "type": "tcp",
        "flow": "xtls-rprx-vision",
        "sni": reality_dest,
        "public-key": material.public_key,
        "short-id": material.short_id,
        "fingerprint": "chrome"
    })
    .to_string()
}

```

> `outbound_tag` 接收的是**入站 tag 字符串**（`"up-1"` → `"out-up-1"`），不是 id —— 渲染时手里只有 tag。测试只断言 `tag_of(id)`。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd src-tauri && cargo test upstream`
Expected: PASS（6 个用例）

- [ ] **Step 6: 门禁 + 提交**

```bash
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings
cd .. && git add src-tauri/src/servers/upstream.rs src-tauri/src/servers/upstream_tests.rs src-tauri/src/servers/mod.rs
git commit -m "feat(upstream): 校验与专用 Xray 实例配置渲染（纯函数）"
```

---

### Task 3: 部署执行器与 Job 接线

**Files:**
- Modify: `src-tauri/src/servers/upstream.rs`（追加执行器）
- Modify: `src-tauri/src/servers/upstream_tests.rs`
- Modify: `src-tauri/src/jobs/mod.rs`
- Modify: `src-tauri/src/servers/deploy.rs`（`ProdRunner` 的 dispatch 加一个臂）

**Interfaces:**
- Consumes: Task 1 的 repos、Task 2 的渲染函数、`deploy.rs` 的 `run_cmd`/`run_cmd_ok`/`write_remote_file`/`CAPABILITY_PROBE_SCRIPT`/`parse_capability`/`parse_x25519_keys`/`INSTALL_TIMEOUT`/`CONFIG_TIMEOUT`/`XRAY_INSTALL_URL`（最后一个是私有常量，见 Step 4 的处理）
- Produces:
  - `jobs::JobRequest::Upstream(jobs::UpstreamReq)`，`kind() == "upstream"`
  - `jobs::UpstreamReq { pub job_id: i64, pub server_id: i64 }`
  - `upstream::run_upstream_job(state: &AppState, req: &UpstreamReq, cancel: &AtomicBool, emit_raw: &EmitFn) -> Result<(), String>` — 重渲染该中转机**全部**出口并写回各自节点；无出口时停服务并清文件
  - `upstream::ensure_material(sess: &mut SshSession, cancel: &AtomicBool, state: &AppState, id: i64) -> Result<Material, String>` — 取该出口的身份材料；`material_json` 仍是占位（首建）则用远端 `xray x25519` 生成一套并落库，此后一律复用

- [ ] **Step 1: 写失败的测试**

在 `src-tauri/src/servers/upstream_tests.rs` 末尾追加：

```rust
#[test]
fn desired_state_empty_means_stop_and_clean() {
    // 无出口 → 停服务、删配置与 unit；有出口 → 重启
    assert_eq!(service_action(0), ServiceAction::StopAndClean);
    assert_eq!(service_action(1), ServiceAction::Restart);
    assert_eq!(service_action(3), ServiceAction::Restart);
}

#[test]
fn xray_install_cmd_is_idempotent_form() {
    // 已装则不重装：命令由执行器按 command -v xray 的探测结果决定
    assert!(install_xray_cmd().contains("Xray-install"));
    assert!(install_xray_cmd().contains("@ install"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test upstream`
Expected: FAIL — `cannot find function service_action` / `install_xray_cmd`

- [ ] **Step 3: 加 JobRequest 变体**

`src-tauri/src/jobs/mod.rs`：

```rust
/// 上游出口部署/拆除：重渲染该中转机专用实例的整份配置并写回节点。
#[derive(Debug, Clone)]
pub struct UpstreamReq {
    pub job_id: i64,
    pub server_id: i64,
}
```

在 `pub enum JobRequest` 加变体 `Upstream(UpstreamReq),`；`job_id()` 加 `JobRequest::Upstream(r) => r.job_id,`；`kind()` 加 `JobRequest::Upstream(_) => "upstream",`。

- [ ] **Step 4: 实现执行器**

在 `src-tauri/src/servers/upstream.rs` 追加（文件头补 `use`）：

```rust
use crate::jobs::{EmitFn, JobProgress, UpstreamReq};
use crate::servers::deploy::{
    parse_capability, parse_x25519_keys, run_cmd, run_cmd_ok, write_remote_file,
    CAPABILITY_PROBE_SCRIPT, CONFIG_TIMEOUT, INSTALL_TIMEOUT,
};
use crate::servers::{load_auth, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use crate::{db::repos, domain::Node};
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

/// Xray 官方安装脚本（与 deploy.rs 同址；deploy 里是私有常量，此处复制一份并注明来源）。
const XRAY_INSTALL_URL: &str = "https://github.com/XTLS/Xray-install/raw/main/install-release.sh";

/// 出口齐备度 → 服务动作。
#[derive(Debug, PartialEq)]
pub enum ServiceAction {
    Restart,
    StopAndClean,
}

pub fn service_action(n: usize) -> ServiceAction {
    if n == 0 {
        ServiceAction::StopAndClean
    } else {
        ServiceAction::Restart
    }
}

pub fn install_xray_cmd() -> String {
    format!("bash -c \"$(curl -L {XRAY_INSTALL_URL})\" @ install")
}

/// 部署/拆除执行体：该中转机的出口集合是唯一事实源，整份重渲染 + 服务动作 + 节点写回。
/// 一条出口都没有时停服务并清文件（不残留监听端口）。
pub async fn run_upstream_job(
    state: &AppState,
    req: &UpstreamReq,
    cancel: &AtomicBool,
    emit_raw: &EmitFn,
) -> Result<(), String> {
    // 事件发射：EmitFn 吃的是 JobProgress（jobs/mod.rs:24），这里包一层三元组签名，
    // 与 deploy 的 emit_progress 同款。闭包直接叫 emit，函数体里所有调用无需改动。
    let emit = |step: &str, message: &str, progress: i64| {
        emit_raw(JobProgress {
            job_id: req.job_id,
            kind: "upstream".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    let check_cancel = || -> Result<(), String> {
        if cancel.load(Ordering::SeqCst) {
            Err("已取消".to_string())
        } else {
            Ok(())
        }
    };

    // 1 装载：服务器 + 凭据 + 该机全部出口
    emit("探测", "读取中转机与出口配置", 5);
    let (auth, server, rows) = {
        let (auth, server) = load_auth(state, req.server_id)?;
        let conn = state.db.lock();
        let rows = repos::upstreams::list_by_server(&conn, req.server_id)
            .map_err(|e| format!("读取出口失败: {e}"))?;
        (auth, server, rows)
    };

    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 2 探测：OS / systemd
    let caps = run_cmd(&mut sess, cancel, "探测", CAPABILITY_PROBE_SCRIPT, PROBE_TIMEOUT)
        .await?
        .stdout;
    let (os, has_systemd) = parse_capability(&caps);
    if !has_systemd {
        return Err(format!("该系统未运行 systemd（{os}），暂不支持"));
    }
    check_cancel()?;

    // 3 安装（缺才装）
    let xray_installed = run_cmd(&mut sess, cancel, "探测", "command -v xray", PROBE_TIMEOUT)
        .await?
        .exit_code
        == 0;
    if !xray_installed {
        emit("安装", "安装 Xray（可能耗时数分钟）", 30);
        run_cmd_ok(&mut sess, cancel, "安装", &install_xray_cmd(), INSTALL_TIMEOUT).await?;
    }
    check_cancel()?;

    // 4 渲染 + 下发（无出口则清场）
    emit("配置", "渲染专用实例配置", 55);
    // 身份材料：首建生成、此后一律复用（否则客户端节点每次重建都要重新导入）
    let material_of = {
        let mut m = std::collections::HashMap::new();
        for u in &rows {
            m.insert(u.id, ensure_material(&mut sess, cancel, state, u.id).await?);
        }
        m
    };

    match service_action(rows.len()) {
        ServiceAction::Restart => {
            // 逐条解密密码并组装渲染条目（明文只在本次调用内存中存活）
            let mut entries = Vec::with_capacity(rows.len());
            for u in &rows {
                let (sealed_pw, _) = {
                    let conn = state.db.lock();
                    repos::upstreams::secrets(&conn, u.id)
                        .map_err(|e| format!("读取凭据失败: {e}"))?
                        .ok_or_else(|| "出口不存在".to_string())?
                };
                let password = crate::crypto::open_param(&state.creds, &sealed_pw);
                let material = material_of
                    .get(&u.id)
                    .cloned()
                    .ok_or_else(|| "身份材料缺失".to_string())?;
                entries.push(RenderEntry {
                    tag: tag_of(u.id),
                    listen_port: u.listen_port as u16,
                    reality_dest: u.reality_dest.clone(),
                    material,
                    proto: u.proto.clone(),
                    host: u.host.clone(),
                    port: u.port as u16,
                    username: u.username.clone(),
                    password,
                });
            }
            let cfg = render_config(&entries);
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                UPSTREAM_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                UPSTREAM_UNIT_PATH,
                &render_unit(),
                CONFIG_TIMEOUT,
            )
            .await?;
            check_cancel()?;
            emit("启动", "启动专用实例", 80);
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                &format!(
                    "systemctl daemon-reload && systemctl enable --now {UPSTREAM_SERVICE} \
                     && systemctl restart {UPSTREAM_SERVICE}"
                ),
                CONFIG_TIMEOUT,
            )
            .await?;
        }
        ServiceAction::StopAndClean => {
            emit("拆除", "停用并清理专用实例", 80);
            let _ = run_cmd(
                &mut sess,
                cancel,
                "拆除",
                &format!("systemctl disable --now {UPSTREAM_SERVICE}"),
                CONFIG_TIMEOUT,
            )
            .await;
            let _ = run_cmd(
                &mut sess,
                cancel,
                "拆除",
                &format!(
                    "rm -f {UPSTREAM_CONFIG_PATH} {UPSTREAM_UNIT_PATH} && systemctl daemon-reload"
                ),
                CONFIG_TIMEOUT,
            )
            .await;
        }
    }
    check_cancel()?;

    // 5 写回节点：按 upstreams.node_id 定位（**不得**用 delete_self_built——
    // 它按 protocol+addr 删，会误删同一中转机上兄弟出口的节点）
    if service_action(rows.len()) == ServiceAction::Restart {
        emit("写回", "写入节点", 95);
        let mut conn = state.db.lock();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for u in &rows {
            let material = material_of
                .get(&u.id)
                .cloned()
                .ok_or_else(|| "身份材料缺失".to_string())?;
            let node = Node {
                id: u.node_id.clone().unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
                name: format!("{}-{}", server.name, u.name),
                source: "self-built".into(),
                protocol: "vless".into(),
                addr: server.host.clone(),
                port: u.listen_port,
                params_json: node_params(&material, &u.reality_dest),
                group_name: "自建".into(),
                sub_id: None,
                relay_rule_id: None,
                enabled: true,
                latency: None,
                bandwidth: None,
                last_checked_at: None,
                last_check_error: None,
                direct_latency: None,
                hop_relay_ms: None,
                hop_landing_ms: None,
                hop_tunnel_ms: None,
            };
            repos::nodes::upsert(&tx, &node, &state.creds).map_err(|e| e.to_string())?;
            if u.node_id.is_none() {
                repos::upstreams::set_node_id(&tx, u.id, &node.id).map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
    } else {
        // 拆干净：连同节点一起删
        let mut conn = state.db.lock();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for u in &rows {
            if let Some(nid) = &u.node_id {
                repos::nodes::delete(&tx, nid).map_err(|e| e.to_string())?;
            }
            repos::upstreams::delete(&tx, u.id).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
    }

    emit("完成", "上游出口已就绪", 100);
    Ok(())
}
```

**身份材料从哪来**（`ensure_material`）：节点的 `public-key` 必须与写进配置的 `private_key` 配对，而客户端节点又不能在重建后变（变了就要重新导入）。做法是**首建生成、此后复用**——公钥一并存进 `material_json`，重建时不依赖任何推导命令：

```rust
/// 取该出口的身份材料。`material_json` 仍是占位 `{}`（首建）→ 用远端 `xray x25519`
/// 生成一套（含公钥）并落库；已有 → 直接反序列化复用。
/// 「添加出口」是纯本地操作（不需要 SSH），所以材料一律在这里补生成。
async fn ensure_material(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    state: &AppState,
    id: i64,
) -> Result<Material, String> {
    let existing_json = {
        let conn = state.db.lock();
        repos::upstreams::secrets(&conn, id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "出口不存在".to_string())?
            .1
    };
    if let Ok(m) = serde_json::from_str::<Material>(&existing_json) {
        return Ok(m);
    }
    let stdout = run_cmd_ok(sess, cancel, "配置", "xray x25519", CONFIG_TIMEOUT)
        .await?
        .stdout;
    let (private_key, public_key) = parse_x25519_keys(&stdout)?;
    let short_id: String = (0..8)
        .map(|_| format!("{:x}", rand::random::<u8>() % 16))
        .collect();
    let m = Material {
        uuid: Uuid::new_v4().to_string(),
        private_key,
        public_key,
        short_id,
    };
    let json = serde_json::to_string(&m).map_err(|e| e.to_string())?;
    {
        let conn = state.db.lock();
        repos::upstreams::set_material(&conn, id, &json).map_err(|e| e.to_string())?;
    }
    Ok(m)
}
```

> 占位 `"{}"` 反序列化成 `Material` 会因缺字段而失败，正好当作「尚未生成」的判据 —— 所以 Task 4 的 `add_upstream` 落库时写 `"{}"` 是刻意的，不是遗漏。

- [ ] **Step 5: 接线 dispatch**

`src-tauri/src/servers/deploy.rs` 的 `impl JobRunner for ProdRunner`（约 277 行）在 `JobRequest::Deploy(r) => ...` 之后加：

```rust
            JobRequest::Upstream(r) => {
                crate::servers::upstream::run_upstream_job(&state, r, &cancel, &emit).await
            }
```

> 若 match 是穷尽式的，编译器会在此前先报「non-exhaustive patterns」，正好当作 Step 2 的红。

- [ ] **Step 6: 运行测试与编译确认通过**

Run: `cd src-tauri && cargo test upstream && cargo clippy --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/servers/upstream.rs src-tauri/src/servers/upstream_tests.rs src-tauri/src/jobs/mod.rs src-tauri/src/servers/deploy.rs
git commit -m "feat(upstream): 部署/拆除执行器与 job 接线"
```

---

### Task 4: IPC 命令与前端 API

**Files:**
- Modify: `src-tauri/src/commands.rs`
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/ipc_tests.rs`
- Modify: `src/api/types.ts`
- Modify: `src/api/index.ts`

**Interfaces:**
- Consumes: `upstream::{UpstreamInput, validate, ...}`、`repos::upstreams`
- Produces（IPC 命令名）：
  - `list_upstreams_cmd(server_id) -> Vec<Upstream>`
  - `add_upstream_cmd(server_id, name, proto, host, port, username, password, listen_port, reality_dest) -> Upstream`
  - `remove_upstream_cmd(id) -> ()`（纯 DB；删除远端配置由调用方随后跑 `start_upstream_job_cmd`）
  - `start_upstream_job_cmd(server_id) -> i64`（返回 job_id）
  - 前端：`listUpstreams` / `addUpstream` / `removeUpstream` / `startUpstreamJob`

- [ ] **Step 1: 写失败的测试**

`src-tauri/src/ipc_tests.rs` 末尾追加（照该文件既有的 AppState 夹具写法）：

```rust
#[test]
fn add_upstream_cmd_rejects_duplicate_listen_port() {
    let (state, sid) = test_state_with_server("中转机-01", "relay");
    let input = |listen: i64| crate::servers::upstream::UpstreamInput {
        server_id: sid,
        name: "住宅01".into(),
        proto: "socks5".into(),
        host: "9.9.9.9".into(),
        port: 1080,
        username: "u".into(),
        password: "p".into(),
        listen_port: listen,
        reality_dest: "www.microsoft.com".into(),
    };
    let _ = crate::commands::add_upstream(&state, input(443)).expect("first ok");
    let err = crate::commands::add_upstream(&state, input(443)).expect_err("dup must fail");
    assert!(format!("{err}").contains("已被这台中转机上的其他出口占用"));
    assert!(crate::commands::add_upstream(&state, input(8443)).is_ok());
}

#[test]
fn add_upstream_rejects_listen_port_taken_by_wg_link() {
    let (state, sid) = test_state_with_server("中转机-01", "relay");
    // 复用 relay 测试里既有的规则夹具（relay_rules 有 10+ 个 NOT NULL 列，
    // 不要在 ipc_tests 里手搓第二套）；只需 relay_server_id=sid、wg_port=51820。
    insert_relay_rule_fixture(&state, sid, 51820);
    let err = crate::commands::add_upstream(
        &state,
        crate::servers::upstream::UpstreamInput {
            server_id: sid,
            name: "住宅01".into(),
            proto: "socks5".into(),
            host: "9.9.9.9".into(),
            port: 1080,
            username: "u".into(),
            password: "p".into(),
            listen_port: 51820,
            reality_dest: "www.microsoft.com".into(),
        },
    )
    .expect_err("撞 WG 端口必须拒绝");
    assert!(format!("{err}").contains("WireGuard 链路占用"));
}

#[test]
fn list_upstreams_exposes_no_password_field() {
    let (state, sid) = test_state_with_server("中转机-01", "relay");
    let u = crate::commands::add_upstream(
        &state,
        crate::servers::upstream::UpstreamInput {
            server_id: sid,
            name: "住宅01".into(),
            proto: "socks5".into(),
            host: "9.9.9.9".into(),
            port: 1080,
            username: "u".into(),
            password: "super-secret".into(),
            listen_port: 443,
            reality_dest: "www.microsoft.com".into(),
        },
    )
    .expect("add");
    let json = serde_json::to_string(&u).expect("json");
    assert!(!json.contains("super-secret"));
    assert!(!json.contains("password"));
}
```

> 若 `ipc_tests.rs` 没有 `test_state_with_server` 这类辅助，按该文件既有的夹具（建 AppState + 插一台服务器）抽一个小工具函数出来复用，不要新造第三套。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test upstream`
Expected: FAIL — `add_upstream` 不存在

- [ ] **Step 3: 实现命令内层函数**

`src-tauri/src/commands.rs` 追加：

```rust
/// 出口列表（纯 DB）。
pub fn list_upstreams(state: Arc<AppState>, server_id: i64) -> R<Vec<Upstream>> {
    let conn = state.db.lock();
    repos::upstreams::list_by_server(&conn, server_id).map_err(err)
}

/// 添加出口：校验 → 落库（密码 seal；材料留空占位，由部署 job 首次生成）。
pub fn add_upstream(
    state: Arc<AppState>,
    input: crate::servers::upstream::UpstreamInput,
) -> R<Upstream> {
    let existing = {
        let conn = state.db.lock();
        repos::upstreams::list_by_server(&conn, input.server_id).map_err(err)?
    };
    crate::servers::upstream::validate(&input, &existing).map_err(err)?;
    // 与既有中转链路的端口错开：WG 的 wg_port 与模式 A 的 forward_port 都占用这台
    // 中转机的公网端口，撞上表现为「链路莫名不通」而不是报错，所以提前拒绝。
    let reserved: Vec<i64> = crate::relay::list_relay_rules(&state)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.relay_server_id == input.server_id)
        .flat_map(|r| {
            let mut v = vec![r.wg_port];
            if let Some(f) = r.forward_port {
                v.push(f);
            }
            v
        })
        .collect();
    if reserved.contains(&input.listen_port) {
        return Err(AppError::Message(format!(
            "监听端口 {} 已被这台中转机上的 WireGuard 链路占用",
            input.listen_port
        )));
    }
    let row = Upstream {
        id: 0,
        server_id: input.server_id,
        name: crate::servers::clean_text(&input.name),
        proto: input.proto.clone(),
        host: crate::servers::clean_text(&input.host),
        port: input.port,
        username: input.username.clone(),
        listen_port: input.listen_port,
        reality_dest: crate::servers::clean_text(&input.reality_dest),
        node_id: None,
        egress_ip: None,
        last_check_error: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let sealed = crate::crypto::seal_param(&state.creds, &input.password);
    let conn = state.db.lock();
    let id = repos::upstreams::insert(&conn, &row, &sealed, "{}").map_err(err)?;
    Ok(Upstream { id, ..row })
}

/// 删除出口（纯 DB）。远端配置与节点由随后的 start_upstream_job 重渲染时清理。
pub fn remove_upstream(state: Arc<AppState>, id: i64) -> R<()> {
    let conn = state.db.lock();
    let exists = repos::upstreams::get(&conn, id).map_err(err)?.is_some();
    if !exists {
        return Err(AppError::Message("出口不存在".into()));
    }
    repos::upstreams::delete(&conn, id).map_err(err)
}

/// 提交部署/拆除任务（该中转机全部出口整份重渲染）。
pub fn start_upstream_job(state: Arc<AppState>, server_id: i64) -> R<i64> {
    {
        let conn = state.db.lock();
        repos::servers::get(&conn, server_id).map_err(err)?
            .ok_or_else(|| AppError::Message("服务器不存在".into()))?;
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "upstream", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::Upstream(crate::jobs::UpstreamReq {
            job_id,
            server_id,
        }))
        .map_err(AppError::Message)?;
    Ok(job_id)
}
```

`add_upstream` 里 `repos::nodes::delete_self_built` **不得出现**（见 Global Constraints）。

- [ ] **Step 4: 加 `_cmd` 包装与注册**

`src-tauri/src/commands.rs` 追加四个 `#[tauri::command]` 包装（照 `add_channel_cmd` 的扁平参数形态）：

```rust
#[tauri::command]
pub fn list_upstreams_cmd(state: tauri::State<'_, Arc<AppState>>, server_id: i64) -> R<Vec<Upstream>> {
    list_upstreams(state.inner().clone(), server_id)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn add_upstream_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    name: String,
    proto: String,
    host: String,
    port: i64,
    username: String,
    password: String,
    listen_port: i64,
    reality_dest: Option<String>,
) -> R<Upstream> {
    add_upstream(
        state.inner().clone(),
        crate::servers::upstream::UpstreamInput {
            server_id,
            name,
            proto,
            host,
            port,
            username,
            password,
            listen_port,
            reality_dest: reality_dest
                .unwrap_or_else(|| crate::servers::upstream::DEFAULT_REALITY_DEST.to_string()),
        },
    )
}

#[tauri::command]
pub fn remove_upstream_cmd(state: tauri::State<'_, Arc<AppState>>, id: i64) -> R<()> {
    remove_upstream(state.inner().clone(), id)
}

#[tauri::command]
pub fn start_upstream_job_cmd(state: tauri::State<'_, Arc<AppState>>, server_id: i64) -> R<i64> {
    start_upstream_job(state.inner().clone(), server_id)
}
```

在 `src-tauri/src/main.rs` 的 `generate_handler![...]` 加这四条（放在 `channel_switch_cmd` 之后）：

```rust
            commands::list_upstreams_cmd,
            commands::add_upstream_cmd,
            commands::remove_upstream_cmd,
            commands::start_upstream_job_cmd,
```

- [ ] **Step 5: 前端类型与 API**

`src/api/types.ts` 追加：

```ts
/** 上游出口（中转机上的住宅代理出口）。注意：**不含密码**——凭据不出后端。 */
export interface Upstream {
  id: number
  server_id: number
  name: string
  /** 'socks5' | 'http' */
  proto: string
  host: string
  port: number
  username: string
  listen_port: number
  reality_dest: string
  node_id: string | null
  egress_ip: string | null
  last_check_error: string | null
  created_at: string
}
```

`src/api/index.ts` 追加（照 `listChannels` 等既有写法）：

```ts
export function listUpstreams(serverId: number): Promise<Upstream[]> {
  return invoke('list_upstreams_cmd', { serverId })
}

export function addUpstream(input: {
  serverId: number
  name: string
  proto: string
  host: string
  port: number
  username: string
  password: string
  listenPort: number
  realityDest?: string
}): Promise<Upstream> {
  return invoke('add_upstream_cmd', { ...input })
}

export function removeUpstream(id: number): Promise<void> {
  return invoke('remove_upstream_cmd', { id })
}

export function startUpstreamJob(serverId: number): Promise<number> {
  return invoke('start_upstream_job_cmd', { serverId })
}
```

并把 `Upstream` 加进 `src/api/index.ts` 顶部的类型导入列表。

- [ ] **Step 6: 跑测试与类型检查**

Run: `cd src-tauri && cargo test upstream` 然后 `npx tsc --noEmit -p tsconfig.json`
Expected: 都 PASS

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/main.rs src-tauri/src/ipc_tests.rs src/api/types.ts src/api/index.ts
git commit -m "feat(upstream): 四条 IPC 命令与前端 API"
```

---

### Task 5: 服务器页的上游出口面板

**Files:**
- Create: `src/components/UpstreamPanel.tsx`
- Create: `src/components/UpstreamPanel.test.tsx`
- Modify: `src/views/Servers.tsx`（在展开面板里挂载）

**Interfaces:**
- Consumes: Task 4 的四个前端 API；`JobProgressPanel`（既有）
- Produces: `export default function UpstreamPanel({ serverId }: { serverId: number })`

- [ ] **Step 1: 写失败的组件测试**

创建 `src/components/UpstreamPanel.test.tsx`：

```tsx
// 上游出口面板测试：列表渲染、添加、http 选型的 UDP 提示、错误红字展示。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import UpstreamPanel from './UpstreamPanel'
import * as api from '../api'

vi.mock('../api', () => ({
  listUpstreams: vi.fn(),
  addUpstream: vi.fn(),
  removeUpstream: vi.fn(),
  startUpstreamJob: vi.fn(),
}))

const mocked = vi.mocked(api)

describe('UpstreamPanel', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    mocked.listUpstreams.mockResolvedValue([])
  })

  it('空列表提示', async () => {
    render(<UpstreamPanel serverId={1} />)
    expect(await screen.findByText(/还没有上游出口/)).toBeInTheDocument()
  })

  it('列出已有出口并展示出口 IP', async () => {
    mocked.listUpstreams.mockResolvedValue([
      {
        id: 1,
        server_id: 1,
        name: '住宅01',
        proto: 'socks5',
        host: '9.9.9.9',
        port: 1080,
        username: 'u',
        listen_port: 443,
        reality_dest: 'www.microsoft.com',
        node_id: 'n1',
        egress_ip: '5.5.5.5',
        last_check_error: null,
        created_at: 't',
      },
    ])
    render(<UpstreamPanel serverId={1} />)
    expect(await screen.findByText('住宅01')).toBeInTheDocument()
    expect(screen.getByText(/5\.5\.5\.5/)).toBeInTheDocument()
  })

  it('选 http 时提示不支持 UDP', async () => {
    const user = userEvent.setup()
    render(<UpstreamPanel serverId={1} />)
    await user.click(await screen.findByRole('button', { name: /添加出口/ }))
    await user.selectOptions(screen.getByLabelText('上游协议'), 'http')
    expect(screen.getByText(/HTTP 上游不支持 UDP/)).toBeInTheDocument()
  })

  it('添加成功后刷新并触发部署', async () => {
    const user = userEvent.setup()
    mocked.addUpstream.mockResolvedValue({
      id: 2,
      server_id: 1,
      name: '住宅02',
      proto: 'socks5',
      host: '8.8.8.8',
      port: 1080,
      username: '',
      listen_port: 8443,
      reality_dest: 'www.microsoft.com',
      node_id: null,
      egress_ip: null,
      last_check_error: null,
      created_at: 't',
    })
    mocked.startUpstreamJob.mockResolvedValue(7)
    render(<UpstreamPanel serverId={1} />)
    await user.click(await screen.findByRole('button', { name: /添加出口/ }))
    await user.type(screen.getByLabelText('出口名称'), '住宅02')
    await user.type(screen.getByLabelText('上游地址'), '8.8.8.8')
    await user.type(screen.getByLabelText('上游端口'), '1080')
    await user.type(screen.getByLabelText('监听端口'), '8443')
    await user.click(screen.getByRole('button', { name: /^保存并部署$/ }))
    expect(mocked.addUpstream).toHaveBeenCalled()
    expect(mocked.startUpstreamJob).toHaveBeenCalledWith(1)
  })

  it('失败原因红字展示', async () => {
    mocked.listUpstreams.mockResolvedValue([
      {
        id: 1,
        server_id: 1,
        name: '住宅01',
        proto: 'socks5',
        host: '9.9.9.9',
        port: 1080,
        username: 'u',
        listen_port: 443,
        reality_dest: 'www.microsoft.com',
        node_id: null,
        egress_ip: null,
        last_check_error: '上游代理超时',
        created_at: 't',
      },
    ])
    render(<UpstreamPanel serverId={1} />)
    expect(await screen.findByText(/上游代理超时/)).toBeInTheDocument()
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npx vitest run src/components/UpstreamPanel.test.tsx`
Expected: FAIL — 组件不存在

- [ ] **Step 3: 实现组件**

创建 `src/components/UpstreamPanel.tsx`。结构照 `ChannelPanel.tsx`（列表 + 内联表单 + 行级 busy + 错误红字），把 SSH 开关换成「部署」：

关键点（照抄 ChannelPanel 的组织方式，只需替换 API 与字段）：
- `useState` 持有 `upstreams` / `error` / `adding` / 表单字段
- `refresh()` = `listUpstreams(serverId)`
- 表单字段与 `aria-label` 必须与测试一致：`出口名称`、`上游协议`（`<select>`，选项 socks5/http）、`上游地址`、`上游端口`、`用户名`、`密码`、`监听端口`（默认 `443`）、`伪装域名`（默认 `www.microsoft.com`）
- 提交按钮文案 **`保存并部署`**：先 `addUpstream(...)` 再 `startUpstreamJob(serverId)`，然后 `refresh()`
- 「添加出口」按钮切换表单可见性
- 列表每行：`名称 · proto host:port · 监听 {listen_port}`，出口 IP 存在时展示 `出口 {egress_ip}`，`last_check_error` 存在时红字
- 每行「删除」：`removeUpstream(id)` 后 `startUpstreamJob(serverId)` 再 `refresh()`
- 选中 `http` 时在表单下方渲染 `<p>HTTP 上游不支持 UDP（CONNECT 只能承载 TCP）</p>`
- 空列表渲染 `还没有上游出口`

- [ ] **Step 4: 运行测试确认通过**

Run: `npx vitest run src/components/UpstreamPanel.test.tsx`
Expected: PASS（5 个用例）

- [ ] **Step 5: 挂到服务器页**

`src/views/Servers.tsx` 的展开面板里（`<ChannelPanel serverId={s.id} />` 附近）加：

```tsx
<UpstreamPanel serverId={s.id} />
```

并在文件顶部加 `import UpstreamPanel from '../components/UpstreamPanel'`。**不按 `kind` 过滤** —— 与 `ChannelPanel` 一致，少一条隐性规则（用户忘了标 relay 也能找到入口）。

- [ ] **Step 6: 全量前端门禁**

Run: `npm test` 然后 `npx tsc --noEmit -p tsconfig.json`
Expected: 全绿（既有用例不得回归）

- [ ] **Step 7: 提交**

```bash
git add src/components/UpstreamPanel.tsx src/components/UpstreamPanel.test.tsx src/views/Servers.tsx
git commit -m "feat(upstream): 服务器页上游出口面板"
```

---

### Task 6: 出口 IP 探测

**Files:**
- Modify: `src-tauri/src/speed/engine.rs`（`SpeedEngine` 加一个方法 + 两个实现）
- Modify: `src-tauri/src/speed/mod.rs`（`probe_egress_ip` 执行体）
- Modify: `src-tauri/src/commands.rs` + `src-tauri/src/main.rs`
- Modify: `src/api/index.ts`、`src/components/UpstreamPanel.tsx`

**Interfaces:**
- Consumes: `SpeedEngine::{put_proxy, delete_proxy}`、`sidecar::ensure_started`、`run_speed_job` 的装配方式
- Produces:
  - `SpeedEngine::fetch_text(&self, mixed_port: u16, url: &str, timeout: Duration, cancel: Arc<AtomicBool>) -> Result<String, String>`
  - `speed::probe_egress_ip(state: &AppState, upstream_id: i64) -> Result<String, String>`（返回出口 IP；失败写 `last_check_error` 后仍返回 Err）
  - IPC `probe_egress_ip_cmd(upstream_id) -> String`
  - 前端 `probeEgressIp(id)`

- [ ] **Step 1: 写失败的测试**

`src-tauri/src/speed/tests.rs` 末尾追加（照该文件 FakeEngine 的既有用法）：

```rust
#[test]
fn egress_probe_returns_trimmed_body() {
    let engine = FakeEngine::with_fetch_text("  1.2.3.4\n");
    let got = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(engine.fetch_text(0, "https://api.ipify.org", std::time::Duration::from_secs(5), std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))))
        .expect("ok");
    assert_eq!(got, "1.2.3.4");
}

#[test]
fn egress_probe_rejects_non_ip_body() {
    let engine = FakeEngine::with_fetch_text("<html>captive portal</html>");
    let got = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(engine.fetch_text(0, "https://api.ipify.org", std::time::Duration::from_secs(5), std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))));
    assert!(got.is_err(), "非 IP 正文不得当成出口 IP 落库");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test egress`
Expected: FAIL — `fetch_text` / `with_fetch_text` 不存在

- [ ] **Step 3: 给引擎加方法**

`src-tauri/src/speed/engine.rs`：

- `SpeedEngine` trait 加 `async fn fetch_text(&self, mixed_port: u16, url: &str, timeout: std::time::Duration, cancel: Arc<AtomicBool>) -> Result<String, String>;`
- `MihomoEngine` 实现：用既有 `self.http`（reqwest）发 GET，**经 `http://127.0.0.1:{mixed_port}` 代理**（与 `download_bytes` 同一条路径），返回响应体文本；超时/取消调用 `cancel` 标志的检查方式与 `download_bytes` 保持一致
- `FakeEngine` 加构造函数 `with_fetch_text(body: &str) -> Self` 与一个可注入的 `Result<String, String>`

- [ ] **Step 4: 实现探测执行体**

`src-tauri/src/speed/mod.rs` 追加：

```rust
/// 出口 IP 探测：经 mihomo 注入该出口写回的节点，请求 ipify 取回显。
/// **刻意不在中转机上 curl** —— 那要求把住宅代理密码塞进远端命令行
/// （/proc/<pid>/cmdline、shell 历史都会沾上），而中转机常是托管机器。
/// 经本地 mihomo 还顺带验证了**完整链路**（客户端→中转机→住宅代理）。
pub async fn probe_egress_ip(state: &AppState, upstream_id: i64) -> Result<String, String> {
    // 1 取出口行与其节点
    let (node_id, node) = {
        let conn = state.db.lock();
        let u = repos::upstreams::get(&conn, upstream_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "出口不存在".to_string())?;
        let nid = u.node_id.clone().ok_or_else(|| "该出口尚未部署（没有节点）".to_string())?;
        let n = repos::nodes::get(&conn, &nid, &state.creds)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "节点不存在".to_string())?;
        (nid, n)
    };

    // 2 注入 mihomo 并取回显
    let result = async {
        let (controller_port, mixed_port, token) = sidecar::ensure_started(state).await?;
        let engine: Arc<dyn SpeedEngine> = Arc::new(MihomoEngine::new(
            controller_port,
            mixed_port,
            token,
            sidecar::sidecar_config_path(state),
        ));
        let relays = crate::relay::list_relay_rules(state).unwrap_or_default();
        let servers = crate::db::repos::servers::list(&state.db.lock()).unwrap_or_default();
        let proxy = crate::config::node_proxy_json(&node, &node_id, &relays, &servers)
            .map_err(|e| e.to_string())?;
        engine.put_proxy(&node_id, proxy).await?;
        let body = engine
            .fetch_text(
                mixed_port,
                "https://api.ipify.org",
                std::time::Duration::from_secs(20),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .await;
        let _ = engine.delete_proxy(&node_id).await;
        let body = body?;
        let ip = body.trim().to_string();
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("出口返回的不是 IP：{}", ip.chars().take(80).collect::<String>()));
        }
        Ok(ip)
    }
    .await;

    // 3 落库：成功写 egress_ip 并清错误；失败写错误（脱敏），不影响部署结果
    {
        let conn = state.db.lock();
        match &result {
            Ok(ip) => {
                let _ = repos::upstreams::set_probe(&conn, upstream_id, Some(ip), None);
            }
            Err(e) => {
                let _ = repos::upstreams::set_probe(&conn, upstream_id, None, Some(e));
            }
        }
    }
    result
}
```

- [ ] **Step 5: 命令与前端接线**

- `src-tauri/src/commands.rs`：内层 `pub async fn probe_egress_ip(state: Arc<AppState>, upstream_id: i64) -> R<String>` 转调 `speed::probe_egress_ip`；`_cmd` 包装 `probe_egress_ip_cmd(state, upstream_id)`；`main.rs` 注册
- `src/api/index.ts`：`export function probeEgressIp(id: number): Promise<string> { return invoke('probe_egress_ip_cmd', { upstreamId: id }) }`
- `UpstreamPanel.tsx`：每行加「探测出口 IP」按钮，调用后 `refresh()`；`last_check_error` 红字沿用 Task 5 的渲染

- [ ] **Step 6: 全量门禁**

Run: `cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings` 然后 `npm test && npx tsc --noEmit -p tsconfig.json`
Expected: 全绿

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/speed/engine.rs src-tauri/src/speed/mod.rs src-tauri/src/speed/tests.rs src-tauri/src/commands.rs src-tauri/src/main.rs src/api/index.ts src/components/UpstreamPanel.tsx
git commit -m "feat(upstream): 出口 IP 探测（经本地 mihomo，不在中转机碰密码）"
```

---

## 验收（对应规格 §9）

实现完成后逐条走一遍，**每条都要有实机输出或测试输出为证**：

1. 在一台中转机上添加一条 socks5 住宅出口 → 部署成功 → 节点页出现一条 VLESS+REALITY 节点，`addr` = 中转机
2. 经该节点测速 → 延迟/带宽正常
3. 出口 IP 探测 → 显示的是**住宅代理的 IP**，不是中转机的
4. 同一台中转机再加第二条出口（http 形态）→ 两条节点同时可用、互不影响
5. 删掉第一条 → 第二条仍可用（**重点**：验证没用 `delete_self_built`）
6. 在中转机上走一次现有 deploy（部署／补装协议）→ 上游出口线路不受影响

第 5 条是本次实现最容易踩的坑，务必实机验证；第 6 条验证的是「独立实例」这个核心决定。
