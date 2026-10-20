//! 数据仓库层：各表 CRUD。参数一律用 rusqlite 占位符，禁止字符串拼接 SQL。

use crate::domain::{Channel, Node, NodeSpeed, Privacy, Subscription, Upstream};
use rusqlite::{params, params_from_iter, Connection};

pub mod subs {
    use super::*;

    pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Subscription>> {
        let mut stmt = conn.prepare(
            "SELECT id,name,url,enabled,last_sync_at,sync_interval,last_sync_error,filter_json
             FROM subscriptions ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Subscription {
                id: r.get(0)?,
                name: r.get(1)?,
                url: r.get(2)?,
                enabled: r.get::<_, i64>(3)? != 0,
                last_sync_at: r.get(4)?,
                sync_interval: r.get(5)?,
                last_sync_error: r.get(6)?,
                filter_json: r.get(7)?,
            })
        })?;
        rows.collect()
    }

    pub fn insert(conn: &Connection, s: &Subscription) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO subscriptions (id,name,url,enabled,last_sync_at,sync_interval,last_sync_error,filter_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![s.id, s.name, s.url, s.enabled as i64, s.last_sync_at, s.sync_interval, s.last_sync_error, s.filter_json],
        )?;
        Ok(())
    }

    pub fn update(conn: &Connection, s: &Subscription) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE subscriptions SET name=?2,url=?3,enabled=?4,sync_interval=?5,filter_json=?6 WHERE id=?1",
            params![s.id, s.name, s.url, s.enabled as i64, s.sync_interval, s.filter_json],
        )?;
        Ok(())
    }

    pub fn delete(conn: &Connection, id: &str) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM subscriptions WHERE id=?1", params![id])?;
        Ok(())
    }
}

pub mod nodes {
    use super::*;

    /// nodes 的读取列清单：`row_to_node` 的取值索引与该顺序**逐位对应**，所以只此一份。
    /// 之前 4 处 SELECT 各写各的列清单，v9 加 direct_latency 时漏改其中两处（表现为
    /// `InvalidColumnIndex(15)`）——收口成常量，加列只需改这里和 row_to_node。
    const NODE_COLS: &str = "id,name,source,protocol,addr,port,params_json,group_name,sub_id,relay_rule_id,enabled,latency,bandwidth,last_checked_at,last_check_error,direct_latency,hop_relay_ms,hop_landing_ms,hop_tunnel_ms";

    pub fn list(
        conn: &Connection,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<Vec<Node>> {
        let sql = format!("SELECT {NODE_COLS} FROM nodes ORDER BY name");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| row_to_node(r, creds))?;
        rows.collect()
    }

    /// 存在则更新（保留 latency/bandwidth/last_checked_at，测速数据不因订阅刷新丢失），不存在则插入。
    /// 写库前参数加密（seal_param）；读侧经 row_to_node 解密（legacy 明文兼容）。
    pub fn upsert(
        conn: &Connection,
        n: &Node,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<()> {
        let params_json = crate::crypto::seal_param(creds, &n.params_json);
        conn.execute(
            "INSERT INTO nodes (id,name,source,protocol,addr,port,params_json,group_name,sub_id,relay_rule_id,enabled,latency,bandwidth,last_checked_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name, source=excluded.source, protocol=excluded.protocol,
               addr=excluded.addr, port=excluded.port, params_json=excluded.params_json,
               group_name=excluded.group_name, sub_id=excluded.sub_id,
               relay_rule_id=excluded.relay_rule_id, enabled=excluded.enabled,
               latency=COALESCE(excluded.latency, nodes.latency),
               bandwidth=COALESCE(excluded.bandwidth, nodes.bandwidth),
               last_checked_at=COALESCE(excluded.last_checked_at, nodes.last_checked_at)",
            params![n.id, n.name, n.source, n.protocol, n.addr, n.port, params_json, n.group_name, n.sub_id, n.relay_rule_id, n.enabled as i64, n.latency, n.bandwidth, n.last_checked_at],
        )?;
        Ok(())
    }

    pub fn update_enabled(conn: &Connection, id: &str, enabled: bool) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE nodes SET enabled=?2 WHERE id=?1",
            params![id, enabled as i64],
        )?;
        Ok(())
    }

    pub fn delete(conn: &Connection, id: &str) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM nodes WHERE id=?1", params![id])?;
        Ok(())
    }

    /// 单条节点改名。**必须同时改写 clash_raw 片段里的 name**：`config::node_mapping`
    /// （config/mod.rs:215）对 raw 片段自带 name 时原样输出、忽略 `n.name`，而
    /// proxy-groups 成员与 sing-box tag 取的都是 `n.name` —— 只改一边，分组就指向一个
    /// 不存在于 proxies 的成员，整份配置静默失效。
    ///
    /// 返回名字是否真的变了：false（节点不存在 / 名字未变）时上层跳过配置重建。
    pub fn rename(
        conn: &Connection,
        id: &str,
        name: &str,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<bool> {
        let cur_name: Option<String> = conn
            .query_row("SELECT name FROM nodes WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(cur_name) = cur_name else {
            return Ok(false);
        };
        if cur_name == name {
            return Ok(false);
        }
        let stored: String = conn.query_row(
            "SELECT params_json FROM nodes WHERE id=?1",
            params![id],
            |r| r.get(0),
        )?;
        conn.execute("UPDATE nodes SET name=?2 WHERE id=?1", params![id, name])?;
        if let Some(new_json) =
            rewrite_clash_raw_name(&crate::crypto::open_param(creds, &stored), name)
        {
            conn.execute(
                "UPDATE nodes SET params_json=?2 WHERE id=?1",
                params![id, crate::crypto::seal_param(creds, &new_json)],
            )?;
        }
        Ok(true)
    }

    /// 把 params_json 里 clash_raw 片段的 name 改成 `new_name`。
    /// 没有 clash_raw、片段非映射、或片段已经是该名字 → None（无需回写）。
    pub(crate) fn rewrite_clash_raw_name(params_json: &str, new_name: &str) -> Option<String> {
        let mut p: serde_json::Value = serde_json::from_str(params_json).ok()?;
        let raw = p.get("clash_raw")?.as_str()?;
        p["clash_raw"] = serde_json::Value::String(rewrite_raw_yaml_name(raw, new_name)?);
        serde_json::to_string(&p).ok()
    }

    /// 把 clash_raw 的**片段 YAML** 里的 name 改成 `new_name`，返回改写后的片段。
    /// 片段非映射或已经是该名字 → None（无需回写）。片段原本没有 name 键也照样
    /// 写入：既然节点有名字，proxies 就该输出它。
    ///
    /// 导入路径（`subs::service::store_nodes`）与 [`rename`] 共用这一份：消歧只改
    /// 数据库名而不同步 raw 片段，会让 `proxies` 里仍是旧名、而 `proxy-groups`
    /// 成员用的是新名——mihomo 要么报重复名，要么分组指向不存在的成员。
    pub(crate) fn rewrite_raw_yaml_name(raw: &str, new_name: &str) -> Option<String> {
        let mut frag: serde_yaml::Value = serde_yaml::from_str(raw).ok()?;
        let map = frag.as_mapping_mut()?;
        let key = serde_yaml::Value::String("name".into());
        if map.get(&key).and_then(|v| v.as_str()) == Some(new_name) {
            return None;
        }
        map.insert(key, serde_yaml::Value::String(new_name.into()));
        serde_yaml::to_string(&frag).ok()
    }

    /// 存量重名自愈：节点名是 mihomo 的代理名/分组引用键，也是 sing-box 的 tag，
    /// 必须全局唯一；重名会让整份客户端配置被 mihomo 拒绝（`duplicate name`）。
    ///
    /// 为什么装配期还要再做一次：导入去重只能拦住**新**数据，升级前入库的重名节点
    /// 仍会在第一次「开启网络」时炸掉。按 `list` 的稳定顺序（`ORDER BY name`）保留
    /// 第一个，其余追加 ` #2`、` #3`……；改名同时经 [`rename`] 改写 clash_raw 片段，
    /// 否则 proxies 与分组又会分叉。
    ///
    /// 候选名先与**全部现有名**比对（而不是只与已处理过的），避免把 `A` 改名成
    /// 另一个节点本来就叫的 `A #2`。幂等：没有重名时零写入，返回 0。
    pub fn ensure_unique_names(
        conn: &Connection,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<usize> {
        // 隐私节点的配置代理名是 `privacies.name + " [隐私]"`。先把旧快照同步成
        // 节点当前名，下面才能按「配置里真正出现的名字」判重。
        crate::db::repos::privacies::sync_names(conn)?;
        let nodes = list(conn, creds)?;
        let privacies = crate::db::repos::privacies::list_all(conn)?;
        let mut existing: std::collections::HashSet<String> =
            nodes.iter().map(|n| n.name.clone()).collect();
        let mut seen_raw = std::collections::HashSet::new();
        let mut seen_effective = std::collections::HashSet::new();
        let mut healed = 0usize;
        for n in nodes {
            let private = crate::servers::privacy::effective_privacy(&n.id, &privacies).is_some();
            let effective = if private {
                crate::servers::privacy::display_name(&n.name)
            } else {
                n.name.clone()
            };
            // 保留名即使不重也必须改：分组名/内置代理名会让 mihomo 拒绝整份配置。
            // 原始名重复与「生效名重复」是两回事：普通节点 `A [隐私]` 与开了隐私的
            // `A` 原始名不同，但配置里都叫 `A [隐私]`。
            let reserved = crate::config::is_reserved_proxy_name(&n.name);
            let raw_dup = !seen_raw.insert(n.name.clone());
            let effective_dup = !seen_effective.insert(effective);
            if !reserved && !raw_dup && !effective_dup {
                continue;
            }
            let mut k = 2u32;
            loop {
                let candidate = format!("{} #{k}", n.name);
                let candidate_effective = if private {
                    crate::servers::privacy::display_name(&candidate)
                } else {
                    candidate.clone()
                };
                if !existing.contains(&candidate) && !seen_effective.contains(&candidate_effective)
                {
                    rename(conn, &n.id, &candidate, creds)?;
                    if private {
                        crate::db::repos::privacies::rename_for_node(conn, &n.id, &candidate)?;
                    }
                    existing.insert(candidate);
                    seen_effective.insert(candidate_effective);
                    healed += 1;
                    break;
                }
                k += 1;
            }
        }
        Ok(healed)
    }

    /// 按 id 查单行（测速执行器装载节点用）。
    pub fn get(
        conn: &Connection,
        id: &str,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<Option<Node>> {
        let sql = format!("SELECT {NODE_COLS} FROM nodes WHERE id=?1");
        conn.query_row(&sql, params![id], |r| row_to_node(r, creds))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
    }

    /// 测速写回：整行指标与失败原因一次更新，last_checked_at 每次尝试都刷新（成功时 error 置 NULL）。
    /// **每次尝试整行覆盖**（含清空）：否则会留下上一轮的对比值/分跳值配上这一轮的新错误。
    pub fn update_speed(conn: &Connection, id: &str, m: &NodeSpeed) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE nodes SET latency=?2, direct_latency=?3, hop_relay_ms=?4, hop_landing_ms=?5,
               hop_tunnel_ms=?6, bandwidth=?7, last_checked_at=?8, last_check_error=?9 WHERE id=?1",
            params![
                id,
                m.latency,
                m.direct_latency,
                m.hop_relay_ms,
                m.hop_landing_ms,
                m.hop_tunnel_ms,
                m.bandwidth,
                chrono::Utc::now().to_rfc3339(),
                m.error
            ],
        )?;
        Ok(())
    }

    /// 某落地机上的全部自建节点（整机转发按「落地机 + 原端口号」逐端口规划时用）。
    /// 判据是**直连地址**（`domain::direct_addr`）而不是 `addr`：改指过的节点 addr
    /// 已换成中转入口。params_json 是加密列，SQL 匹配不到，只能读出来在 Rust 侧判
    /// ——自建节点量小（一台机器几条），全量读无碍。
    pub fn find_self_built_by_host(
        conn: &Connection,
        host: &str,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<Vec<Node>> {
        let sql = format!("SELECT {NODE_COLS} FROM nodes WHERE source='self-built'");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| row_to_node(r, creds))?;
        let all: Vec<Node> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(all
            .into_iter()
            .filter(|n| crate::domain::direct_addr(n) == host)
            .collect())
    }

    /// 删除同一地址同协议的自建节点旧行（重复部署幂等：只保留最新一次写回）。
    pub fn delete_self_built(
        conn: &Connection,
        protocol: &str,
        addr: &str,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "DELETE FROM nodes WHERE source='self-built' AND protocol=?1 AND addr=?2",
            params![protocol, addr],
        )
    }

    /// 删除某订阅下不在保留集合中的节点（订阅刷新时清理已下架节点）。
    pub fn delete_for_sub_except(
        conn: &Connection,
        sub_id: &str,
        keep_ids: &[String],
    ) -> rusqlite::Result<usize> {
        let placeholders = keep_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "DELETE FROM nodes WHERE sub_id=?1 AND id NOT IN ({})",
            placeholders
        );
        let mut params_vec = Vec::with_capacity(keep_ids.len() + 1);
        params_vec.push(sub_id.to_string());
        params_vec.extend(keep_ids.iter().cloned());
        conn.execute(&sql, params_from_iter(params_vec))
    }

    fn row_to_node(
        r: &rusqlite::Row<'_>,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<Node> {
        Ok(Node {
            id: r.get(0)?,
            name: r.get(1)?,
            source: r.get(2)?,
            protocol: r.get(3)?,
            addr: r.get(4)?,
            port: r.get(5)?,
            // 列存储为 seal_param 密文（legacy 明文经 open_param 兼容读）
            params_json: crate::crypto::open_param(creds, &r.get::<_, String>(6)?),
            group_name: r.get(7)?,
            sub_id: r.get(8)?,
            relay_rule_id: r.get(9)?,
            enabled: r.get::<_, i64>(10)? != 0,
            latency: r.get(11)?,
            bandwidth: r.get(12)?,
            last_checked_at: r.get(13)?,
            last_check_error: r.get(14)?,
            direct_latency: r.get(15)?,
            hop_relay_ms: r.get(16)?,
            hop_landing_ms: r.get(17)?,
            hop_tunnel_ms: r.get(18)?,
        })
    }

    /// 节点改指：addr/port 指向中转入口，原直连地址存 params_json.direct（保留既有键）。
    /// params_json 读写均走 seal/open（与 upsert/row_to_node 同加密口径）。
    pub fn set_route(
        conn: &Connection,
        node_id: &str,
        addr: &str,
        port: i64,
        rule_id: Option<i64>,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<()> {
        let stored: String = conn.query_row(
            "SELECT params_json FROM nodes WHERE id=?1",
            params![node_id],
            |r| r.get(0),
        )?;
        let params_json = crate::crypto::open_param(creds, &stored);
        let mut p: serde_json::Value =
            serde_json::from_str(&params_json).unwrap_or(serde_json::json!({}));
        let direct = serde_json::json!({
            "addr": conn.query_row("SELECT addr FROM nodes WHERE id=?1", params![node_id], |r| r.get::<_, String>(0))?,
            "port": conn.query_row("SELECT port FROM nodes WHERE id=?1", params![node_id], |r| r.get::<_, i64>(0))?,
        });
        if p.get("direct").is_none() {
            p["direct"] = direct;
        }
        let new_json = serde_json::to_string(&p).unwrap_or_else(|_| "{}".into());
        conn.execute(
            "UPDATE nodes SET addr=?2, port=?3, params_json=?4, relay_rule_id=?5 WHERE id=?1",
            params![
                node_id,
                addr,
                port,
                crate::crypto::seal_param(creds, &new_json),
                rule_id
            ],
        )?;
        Ok(())
    }

    /// 「仅换伪装域名」的 sni 回写：该服务器名下的 vless/trojan 节点（params_json
    /// 含字符串 sni 字段 = REALITY）统一替换为 dest；返回改写条数。读写同样加密口径。
    pub fn update_reality_sni(
        conn: &Connection,
        host: &str,
        dest: &str,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<usize> {
        let mut stmt = conn.prepare(
            "SELECT id, params_json FROM nodes WHERE addr=?1 AND protocol IN ('vless','trojan')",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![host], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut count = 0;
        for (id, json) in rows {
            let plain = crate::crypto::open_param(creds, &json);
            if let Some(new) = crate::servers::reality_dest::rewrite_sni_in_params(&plain, dest) {
                conn.execute(
                    "UPDATE nodes SET params_json=?1 WHERE id=?2",
                    params![crate::crypto::seal_param(creds, &new), id],
                )?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// 一次性迁移历史明文 params_json → 加密（POSIX：enc:v1 前缀的行跳过；
    /// 空值/非 JSON 脏行跳过不动）。幂等可重入；启动时调用，失败不阻断
    /// （明文仍在，下次启动重试）。返回迁移行数。
    pub fn migrate_params(
        conn: &Connection,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<i64> {
        let mut stmt = conn.prepare("SELECT id, params_json FROM nodes")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut n: i64 = 0;
        for (id, stored) in rows {
            if stored.starts_with(crate::crypto::PARAM_PREFIX) || stored.trim().is_empty() {
                continue;
            }
            if serde_json::from_str::<serde_json::Value>(&stored).is_err() {
                continue;
            }
            conn.execute(
                "UPDATE nodes SET params_json=?1 WHERE id=?2",
                params![crate::crypto::seal_param(creds, &stored), id],
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// 节点中转开关（relay_rule_id）：Some = 经该规则中转，None = 直连。
    ///
    /// **只动标记，不动地址**。挂过中转的节点地址已被 [`set_route`] 改写成中转入口，
    /// 解除挂载要连地址一起还原，用 [`restore_direct`]——单用本函数会留下
    /// 「标记说直连、地址指中转入口」的错位态。
    pub fn set_relay(conn: &Connection, id: &str, rule_id: Option<i64>) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE nodes SET relay_rule_id=?1 WHERE id=?2",
            params![rule_id, id],
        )?;
        Ok(())
    }

    /// 单节点回指直连：把 addr/port 写回 `params_json.direct` 记的原值，并清
    /// relay_rule_id（走 [`set_route`]，`direct` 记录因此不会丢，可以反复挂/解）。
    ///
    /// 返回是否真的还原了地址。**无 `direct` 记录 = 从未挂过中转**——此时不动地址，
    /// 调用方若要解标记自行调 [`set_relay`]。
    pub fn restore_direct(
        conn: &Connection,
        id: &str,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<bool> {
        let Some(node) = get(conn, id, creds)? else {
            return Ok(false);
        };
        let Ok(p) = serde_json::from_str::<serde_json::Value>(&node.params_json) else {
            return Ok(false);
        };
        let Some(direct) = p.get("direct") else {
            return Ok(false);
        };
        let addr = direct.get("addr").and_then(|v| v.as_str()).unwrap_or("");
        let port = direct.get("port").and_then(|v| v.as_i64()).unwrap_or(0);
        if addr.is_empty() || port == 0 {
            return Ok(false);
        }
        set_route(conn, id, addr, port, None, creds)?;
        Ok(true)
    }

    /// 启动时一次性治好「地址被改指中转入口」的历史遗留节点（幂等，返回治好几行）。
    ///
    /// 2026-09-20 之前三处挂载（建链写回、接管落地节点、补挂中转）都用 [`set_route`]
    /// 连 addr 一起改写，违反「数据始终存直连地址」的不变量；而解除挂载只清标记，
    /// 直连地址就此丢失。症状是**节点标着直连、地址却指中转入口**——测速量到的仍是
    /// 中转链路，且此后没有任何入口能还原它（拆链只认 `relay_rule_id`）。
    ///
    /// 现在挂载只落 `relay_rule_id`、不再动地址，所以 **addr/port 与 `params_json.direct`
    /// 不符 = 旧数据**，一律还原成 direct 记的原值。`relay_rule_id` **原样保留**：
    /// 用户选的是「走不走中转」，被写坏的只是地址，重建配置时 [`effective_node`] 会
    /// 按规则重算入口。
    ///
    /// [`effective_node`]: crate::config::effective_node
    pub fn heal_direct_addresses(
        conn: &Connection,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<i64> {
        let mut stmt =
            conn.prepare("SELECT id, addr, port, params_json, relay_rule_id FROM nodes")?;
        let rows: Vec<(String, String, i64, String, Option<i64>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut healed = 0i64;
        for (id, addr, port, stored, relay_rule_id) in rows {
            let plain = crate::crypto::open_param(creds, &stored);
            let Ok(p) = serde_json::from_str::<serde_json::Value>(&plain) else {
                continue;
            };
            let Some(direct) = p.get("direct") else {
                continue;
            };
            let d_addr = direct.get("addr").and_then(|v| v.as_str()).unwrap_or("");
            let d_port = direct.get("port").and_then(|v| v.as_i64()).unwrap_or(0);
            if d_addr.is_empty() || d_port == 0 || (d_addr == addr && d_port == port) {
                continue;
            }
            set_route(conn, &id, d_addr, d_port, relay_rule_id, creds)?;
            healed += 1;
        }
        Ok(healed)
    }

    /// 落地机自建节点：按 P2 写回时的 addr/port 精确匹配。
    pub fn find_for_landing(
        conn: &Connection,
        host: &str,
        port: i64,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<Vec<Node>> {
        let sql = format!(
            "SELECT {NODE_COLS} FROM nodes WHERE source='self-built' AND addr=?1 AND port=?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![host, port], |r| row_to_node(r, creds))?;
        rows.collect()
    }

    /// 按 relay_rule_id 查询改指后的节点（拆链恢复直连与收敛协议判定用）。
    pub fn find_by_rule(
        conn: &Connection,
        rule_id: i64,
        creds: &crate::crypto::CredentialService,
    ) -> rusqlite::Result<Vec<Node>> {
        let sql = format!("SELECT {NODE_COLS} FROM nodes WHERE relay_rule_id=?1");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![rule_id], |r| row_to_node(r, creds))?;
        rows.collect()
    }
}

pub mod relay {
    use super::*;
    use crate::domain::RelayRule;

    pub fn insert(conn: &Connection, r: &RelayRule) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO relay_rules (name,relay_server_id,landing_server_id,client_mode,wg_port,forward_port,landing_port,subnet,relay_wg_ip,landing_wg_ip,relay_pubkey,landing_pubkey,converged,status,forward_mode) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![r.name, r.relay_server_id, r.landing_server_id, r.client_mode, r.wg_port, r.forward_port, r.landing_port, r.subnet, r.relay_wg_ip, r.landing_wg_ip, r.relay_pubkey, r.landing_pubkey, r.converged as i64, r.status, r.forward_mode],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<RelayRule> {
        Ok(RelayRule {
            id: row.get(0)?,
            name: row.get(1)?,
            relay_server_id: row.get(2)?,
            landing_server_id: row.get(3)?,
            client_mode: row.get(4)?,
            wg_port: row.get(5)?,
            forward_port: row.get(6)?,
            landing_port: row.get(7)?,
            subnet: row.get(8)?,
            relay_wg_ip: row.get(9)?,
            landing_wg_ip: row.get(10)?,
            relay_pubkey: row.get(11)?,
            landing_pubkey: row.get(12)?,
            converged: row.get::<_, i64>(13)? != 0,
            status: row.get(14)?,
            forward_mode: row.get(15)?,
        })
    }

    const COLS: &str = "id,name,relay_server_id,landing_server_id,client_mode,wg_port,forward_port,landing_port,subnet,relay_wg_ip,landing_wg_ip,relay_pubkey,landing_pubkey,converged,status,forward_mode";

    /// 转发模式回写（重建成整机转发时改这条链路的语义）。
    pub fn set_forward_mode(conn: &Connection, id: i64, mode: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE relay_rules SET forward_mode=?2 WHERE id=?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// 重建时按请求改写语义字段（模式与端口）：行必须与本次建链一致，否则 UI 与
    /// 客户端会按旧值渲染——实测按模式 A 重建后行里还写着 `peer`，节点路由目标
    /// 与「模式 B 不转发端口」的提示就都是错的。
    pub fn set_mode_and_ports(
        conn: &Connection,
        id: i64,
        client_mode: &str,
        forward_port: Option<i64>,
        landing_port: i64,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE relay_rules SET client_mode=?2, forward_port=?3, landing_port=?4 WHERE id=?1",
            params![id, client_mode, forward_port, landing_port],
        )?;
        Ok(())
    }

    /// 建链写回：补存两端公钥（导入态或建链成功时由探测/构建结果提供）。
    pub fn set_pubkeys(
        conn: &Connection,
        id: i64,
        relay_pubkey: Option<&str>,
        landing_pubkey: Option<&str>,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE relay_rules SET relay_pubkey=?2, landing_pubkey=?3 WHERE id=?1",
            params![id, relay_pubkey, landing_pubkey],
        )?;
        Ok(())
    }

    /// P5.1：写回真实 WG 接口地址。COALESCE 语义：None 字段保持原值
    /// （readopt 读不到不置 NULL；refresh 只更新读到且不符的侧）。
    pub fn set_wg_ips(
        conn: &Connection,
        id: i64,
        relay_wg_ip: Option<&str>,
        landing_wg_ip: Option<&str>,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE relay_rules
             SET relay_wg_ip=COALESCE(?2, relay_wg_ip), landing_wg_ip=COALESCE(?3, landing_wg_ip)
             WHERE id=?1",
            params![id, relay_wg_ip, landing_wg_ip],
        )?;
        Ok(())
    }

    pub fn list(conn: &Connection) -> rusqlite::Result<Vec<RelayRule>> {
        let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM relay_rules ORDER BY id"))?;
        let rows = stmt.query_map([], row_to_rule)?;
        rows.collect()
    }

    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<RelayRule>> {
        let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM relay_rules WHERE id=?1"))?;
        let mut rows = stmt.query_map(params![id], row_to_rule)?;
        rows.next().transpose()
    }

    pub fn update_status(conn: &Connection, id: i64, status: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE relay_rules SET status=?2 WHERE id=?1",
            params![id, status],
        )?;
        Ok(())
    }

    pub fn set_converged(conn: &Connection, id: i64, converged: bool) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE relay_rules SET converged=?2 WHERE id=?1",
            params![id, converged as i64],
        )?;
        Ok(())
    }

    /// 删除规则行（网段随之释放）：先清设备 peer 行（FK 依赖，避免删除被锁死），再删规则。
    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM device_peers WHERE rule_id=?1", params![id])?;
        conn.execute("DELETE FROM relay_rules WHERE id=?1", params![id])?;
        Ok(())
    }

    /// 启动对账：遗留 status='building' 的行置 failed（应用崩溃/强杀后解锁
    /// 「接管/重建/拆链/删除」拒绝路径；failed 可拆链清理半成品）。
    pub fn mark_building_failed(conn: &Connection) -> rusqlite::Result<usize> {
        conn.execute(
            "UPDATE relay_rules SET status='failed' WHERE status='building'",
            [],
        )
    }

    /// 按 (落地机, 落地端口) 查链路。
    ///
    /// **一台落地机上可以有多条链路**——一个服务一条（真机：同一台 ashura 上
    /// ss 走 8388、xray 走 8444，各有一条 DNAT 与一条链路）。落地端口就是它们的
    /// 身份：节点挂载（`select_rule_for_node`）、部署补挂（`attach_deployed_nodes`）、
    /// 路由目标全按它匹配。因此判重必须带上落地端口——只按落地机判会把
    /// 「同一台机器上的第二个服务」误判成「已存在链路」而拦死。
    pub fn find_by_landing_port(
        conn: &Connection,
        landing_id: i64,
        landing_port: i64,
    ) -> rusqlite::Result<Option<RelayRule>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM relay_rules
             WHERE landing_server_id=?1 AND landing_port=?2 AND status!='not-built'"
        ))?;
        let mut rows = stmt.query_map(params![landing_id, landing_port], row_to_rule)?;
        rows.next().transpose()
    }

    /// 该落地机上是否有**本工具建立的**链路（status='built'）。
    /// 工具建的链路独占落地机的 `wg0.conf`（整份渲染覆盖写），所以一台落地机
    /// 只能建一条；接管来的 imported 链路不写服务器配置，不受此限——多服务共存
    /// 正落在它们身上。
    pub fn has_built_link(conn: &Connection, landing_id: i64) -> rusqlite::Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM relay_rules WHERE landing_server_id=?1 AND status='built'",
            params![landing_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// 按服务器对查询链路（双向：a 为中转 b 为落地，或反之），仅 built/imported。
    /// 对测目标地址选择用（审查 Minor 2：反向对测同样走 WG 隧道，方向由调用侧
    /// 按 rule.relay_server_id 判断取对应 wg_ip）。
    pub fn find_by_pair(conn: &Connection, a: i64, b: i64) -> rusqlite::Result<Option<RelayRule>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM relay_rules
             WHERE status IN ('built','imported')
               AND ((relay_server_id=?1 AND landing_server_id=?2)
                    OR (relay_server_id=?2 AND landing_server_id=?1))
             LIMIT 1"
        ))?;
        let mut rows = stmt.query_map(params![a, b], row_to_rule)?;
        rows.next().transpose()
    }

    /// 网段是否被**别的隧道**占用。
    ///
    /// 同一 (中转, 落地) 对上的多条链路本来就走同一条隧道，网段相同是正常的
    /// （真机：ss 与 xray 两条链路共用 10.66.66.0/30）——按「网段出现过就算占用」
    /// 判会把同隧道的第二条服务挡在「网段已被占用」上。真正要防的是**跨隧道**
    /// 重网段：两份 AllowedIPs 会互相覆盖，路由打到哪条全看谁后写。
    pub fn subnet_taken_elsewhere(
        conn: &Connection,
        subnet: &str,
        relay_id: i64,
        landing_id: i64,
    ) -> rusqlite::Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM relay_rules WHERE subnet=?1
               AND NOT (relay_server_id=?2 AND landing_server_id=?3)",
            params![subnet, relay_id, landing_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

pub mod device_peers {
    use super::*;
    use crate::domain::DevicePeerRow;

    pub fn insert(conn: &Connection, p: &DevicePeerRow) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO device_peers (rule_id,name,public_key,private_key,psk,wg_ip,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![p.rule_id, p.name, p.public_key, p.private_key_b64, p.psk_b64, p.wg_ip, p.created_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_by_rule(conn: &Connection, rule_id: i64) -> rusqlite::Result<Vec<DevicePeerRow>> {
        let mut stmt = conn.prepare(
            "SELECT id,rule_id,name,public_key,private_key,psk,wg_ip,created_at FROM device_peers WHERE rule_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![rule_id], |row| {
            Ok(DevicePeerRow {
                id: row.get(0)?,
                rule_id: row.get(1)?,
                name: row.get(2)?,
                public_key: row.get(3)?,
                private_key_b64: row.get(4)?,
                psk_b64: row.get(5)?,
                wg_ip: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<DevicePeerRow>> {
        let mut stmt = conn.prepare(
            "SELECT id,rule_id,name,public_key,private_key,psk,wg_ip,created_at FROM device_peers WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(DevicePeerRow {
                id: row.get(0)?,
                rule_id: row.get(1)?,
                name: row.get(2)?,
                public_key: row.get(3)?,
                private_key_b64: row.get(4)?,
                psk_b64: row.get(5)?,
                wg_ip: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM device_peers WHERE id=?1", params![id])?;
        Ok(())
    }

    /// 当前规则内已分配的最大 wg_ip（末段最大值）；空表返回 None，调用方从 .3 起分配。
    /// 取末段整数最大值后按 10.9.{n/254}.{n} 重建 —— 与 SQL 侧 MAX 语义一致，
    /// 不依赖 SQLite 3.54+ 的 instr 负索引语法（bundled 版本为 3.46）。
    pub fn max_wg_ip(conn: &Connection, rule_id: i64) -> rusqlite::Result<Option<String>> {
        let mut stmt = conn.prepare("SELECT wg_ip FROM device_peers WHERE rule_id=?1")?;
        let rows = stmt.query_map(params![rule_id], |r| r.get::<_, String>(0))?;
        let mut max_octet: Option<i64> = None;
        for row in rows {
            let ip = row?;
            if let Some(last) = ip.rsplit('.').next().and_then(|s| s.parse::<i64>().ok()) {
                max_octet = Some(max_octet.map_or(last, |m| m.max(last)));
            }
        }
        Ok(max_octet.map(|n| format!("10.9.{}.{}", (n / 254) % 254, n)))
    }
}

pub mod settings {
    use super::*;

    pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![key],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    pub fn set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO settings (key,value) VALUES (?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

pub mod credentials {
    use super::*;
    pub fn insert(conn: &Connection, id: &str, blob: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO credentials (id,encrypted_blob) VALUES (?1,?2)",
            params![id, blob],
        )?;
        Ok(())
    }
    pub fn delete(conn: &Connection, id: &str) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM credentials WHERE id=?1", params![id])?;
        Ok(())
    }
    pub fn get_blob(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT encrypted_blob FROM credentials WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }
}

pub mod servers {
    use super::*;
    use crate::domain::Server;

    const COLS: &str = "id,name,host,port,user,auth_kind,kind,system,notes,created_at,latency,bandwidth,last_checked_at,last_check_error,host_v6,addr_family,egress_family,share_domain";

    pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Server>> {
        let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM servers ORDER BY name"))?;
        let rows = stmt.query_map([], row_to_server)?;
        rows.collect()
    }

    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Server>> {
        conn.query_row(
            &format!("SELECT {COLS} FROM servers WHERE id=?1"),
            params![id],
            row_to_server,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    pub fn insert(conn: &Connection, s: &Server, auth_ref: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO servers (name,host,host_v6,addr_family,egress_family,share_domain,port,user,auth_kind,auth_ref,kind,system,notes,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![s.name, s.host, s.host_v6, s.addr_family, s.egress_family, s.share_domain, s.port, s.user, s.auth_kind, auth_ref, s.kind, s.system, s.notes, s.created_at],
        )?;
        Ok(())
    }

    pub fn update(conn: &Connection, s: &Server) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE servers SET name=?2,host=?3,host_v6=?4,addr_family=?5,egress_family=?6,share_domain=?7,port=?8,user=?9,auth_kind=?10,kind=?11,notes=?12 WHERE id=?1",
            params![s.id, s.name, s.host, s.host_v6, s.addr_family, s.egress_family, s.share_domain, s.port, s.user, s.auth_kind, s.kind, s.notes],
        )?;
        Ok(())
    }

    pub fn get_auth_ref(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT auth_ref FROM servers WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    /// 免密切换：认证方式改为 key 并指向新凭据。
    pub fn set_auth_kind_ref(
        conn: &Connection,
        id: i64,
        auth_kind: &str,
        auth_ref: &str,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE servers SET auth_kind=?2, auth_ref=?3 WHERE id=?1",
            params![id, auth_kind, auth_ref],
        )?;
        Ok(())
    }

    /// 连通测试成功后回填探测到的系统。
    pub fn set_system(conn: &Connection, id: i64, system: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE servers SET system=?2 WHERE id=?1",
            params![id, system],
        )?;
        Ok(())
    }

    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM servers WHERE id=?1", params![id])?;
        Ok(())
    }

    /// 关联检查：deploys、relay_rules、channels 或 upstreams 引用该服务器即拒绝删除。
    pub fn has_dependents(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
        let d: i64 = conn.query_row(
            "SELECT COUNT(*) FROM deploys WHERE server_id=?1",
            params![id],
            |r| r.get(0),
        )?;
        let r: i64 = conn.query_row(
            "SELECT COUNT(*) FROM relay_rules WHERE relay_server_id=?1 OR landing_server_id=?1",
            params![id],
            |r| r.get(0),
        )?;
        let c: i64 = conn.query_row(
            "SELECT COUNT(*) FROM channels WHERE server_id=?1",
            params![id],
            |r| r.get(0),
        )?;
        let u: i64 = conn.query_row(
            "SELECT COUNT(*) FROM upstreams WHERE server_id=?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(d + r + c + u > 0)
    }

    /// 测速写回：指标与失败原因一次更新，last_checked_at 每次尝试都刷新（成功时 error 置 NULL）。
    /// 语义照 nodes::update_speed（P4 模式）。
    pub fn update_speed(
        conn: &Connection,
        id: i64,
        latency: Option<i64>,
        bandwidth: Option<i64>,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE servers SET latency=?2, bandwidth=?3, last_checked_at=?4, last_check_error=?5 WHERE id=?1",
            params![id, latency, bandwidth, chrono::Utc::now().to_rfc3339(), error],
        )?;
        Ok(())
    }

    fn row_to_server(r: &rusqlite::Row<'_>) -> rusqlite::Result<Server> {
        Ok(Server {
            id: r.get(0)?,
            name: r.get(1)?,
            host: r.get(2)?,
            port: r.get(3)?,
            user: r.get(4)?,
            auth_kind: r.get(5)?,
            kind: r.get(6)?,
            system: r.get(7)?,
            notes: r.get(8)?,
            created_at: r.get(9)?,
            latency: r.get(10)?,
            bandwidth: r.get(11)?,
            last_checked_at: r.get(12)?,
            last_check_error: r.get(13)?,
            host_v6: r.get(14)?,
            addr_family: r.get(15)?,
            egress_family: r.get(16)?,
            share_domain: r.get(17)?,
        })
    }
}

/// 服务器间吞吐历史（一次对测一行；成功 bps、失败 error 含时间）。
pub mod throughput {
    use super::*;
    use crate::domain::ThroughputTest;

    pub fn insert(
        conn: &Connection,
        from_id: i64,
        to_id: i64,
        bps: Option<i64>,
        error: Option<&str>,
        full_output: Option<&str>,
    ) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO server_throughput (from_server_id,to_server_id,bits_per_second,checked_at,error,full_output)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                from_id,
                to_id,
                bps,
                chrono::Utc::now().to_rfc3339(),
                error,
                full_output
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 最近 50 条，最新在前（前端展示最近一次）。
    pub fn list(conn: &Connection) -> rusqlite::Result<Vec<ThroughputTest>> {
        let mut stmt = conn.prepare(
            "SELECT id,from_server_id,to_server_id,bits_per_second,checked_at,error,full_output
             FROM server_throughput ORDER BY id DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ThroughputTest {
                id: r.get(0)?,
                from_server_id: r.get(1)?,
                to_server_id: r.get(2)?,
                bits_per_second: r.get(3)?,
                checked_at: r.get(4)?,
                error: r.get(5)?,
                full_output: r.get(6)?,
            })
        })?;
        rows.collect()
    }
}

pub mod jobs {
    use super::*;
    use crate::domain::Job;

    pub fn insert(conn: &Connection, kind: &str, status: &str) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO jobs (kind,status,progress,error,created_at) VALUES (?1,?2,0,NULL,?3)",
            params![kind, status, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    // 预留：job 详情命令使用；当前仅测试直用
    #[allow(dead_code)]
    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Job>> {
        conn.query_row(
            "SELECT id,kind,status,progress,error,created_at FROM jobs WHERE id=?1",
            params![id],
            row_to_job,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    pub fn update(
        conn: &Connection,
        id: i64,
        status: &str,
        progress: i64,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE jobs SET status=?2, progress=?3, error=?4 WHERE id=?1",
            params![id, status, progress, error],
        )?;
        Ok(())
    }

    /// 最近 50 条，倒序。
    pub fn list_recent(conn: &Connection) -> rusqlite::Result<Vec<Job>> {
        let mut stmt = conn.prepare(
            "SELECT id,kind,status,progress,error,created_at FROM jobs ORDER BY id DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], row_to_job)?;
        rows.collect()
    }

    /// 启动恢复：遗留 queued/running 全部标 failed。
    pub fn mark_interrupted(conn: &Connection) -> rusqlite::Result<usize> {
        conn.execute(
            "UPDATE jobs SET status='failed', error='应用重启中断' WHERE status IN ('queued','running')",
            [],
        )
    }

    fn row_to_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
        Ok(Job {
            id: r.get(0)?,
            kind: r.get(1)?,
            status: r.get(2)?,
            progress: r.get(3)?,
            error: r.get(4)?,
            created_at: r.get(5)?,
        })
    }
}

pub mod deploys {
    use super::*;
    pub fn insert(
        conn: &Connection,
        server_id: i64,
        kind: &str,
        config_json: &str,
        status: &str,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO deploys (server_id,kind,config_json,deployed_at,status) VALUES (?1,?2,?3,?4,?5)",
            params![server_id, kind, config_json, chrono::Utc::now().to_rfc3339(), status],
        )?;
        Ok(())
    }
}

pub mod channels {
    use super::*;

    const COLS: &str = "id,server_id,name,port,protocol,created_at";

    fn row_to_channel(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
        Ok(Channel {
            id: r.get(0)?,
            server_id: r.get(1)?,
            name: r.get(2)?,
            port: r.get(3)?,
            protocol: r.get(4)?,
            created_at: r.get(5)?,
        })
    }

    /// 单台服务器的通道列表（按端口排序，输出确定性）。
    pub fn list_by_server(conn: &Connection, server_id: i64) -> rusqlite::Result<Vec<Channel>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM channels WHERE server_id=?1 ORDER BY port"
        ))?;
        let rows = stmt.query_map(params![server_id], row_to_channel)?;
        rows.collect()
    }

    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Channel>> {
        conn.query_row(
            &format!("SELECT {COLS} FROM channels WHERE id=?1"),
            params![id],
            row_to_channel,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    pub fn insert(conn: &Connection, c: &Channel) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO channels (server_id,name,port,protocol,created_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![c.server_id, c.name, c.port, c.protocol, c.created_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM channels WHERE id=?1", params![id])?;
        Ok(())
    }
}

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
                u.server_id,
                u.name,
                u.proto,
                u.host,
                u.port,
                u.username,
                password_sealed,
                u.listen_port,
                u.reality_dest,
                material_json,
                u.node_id,
                u.egress_ip,
                u.last_check_error,
                u.created_at
            ],
        )?;
        Ok(conn.last_insert_rowid())
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

    /// 改可编辑字段（协议/地址/端口/用户名/监听端口/伪装域名）。**只写库**：远端那份
    /// 配置与写回的节点都由部署 job 重渲染，所以改完必须重部署才生效。
    /// 调用前必须过 [`crate::servers::upstream::validate`]。
    ///
    /// `password_sealed = None` 表示**不动密码**——界面留空即此意。凭据经 IPC 不回显，
    /// 表单拿不到原值，把「没填」当成空串写回去等于把已存的口令擦掉。
    pub fn update_fields(
        conn: &Connection,
        u: &Upstream,
        password_sealed: Option<&str>,
    ) -> rusqlite::Result<()> {
        match password_sealed {
            Some(sealed) => {
                conn.execute(
                    "UPDATE upstreams SET proto=?2,host=?3,port=?4,username=?5,listen_port=?6,\
                     reality_dest=?7,password=?8 WHERE id=?1",
                    params![
                        u.id,
                        u.proto,
                        u.host,
                        u.port,
                        u.username,
                        u.listen_port,
                        u.reality_dest,
                        sealed
                    ],
                )?;
            }
            None => {
                conn.execute(
                    "UPDATE upstreams SET proto=?2,host=?3,port=?4,username=?5,listen_port=?6,\
                     reality_dest=?7 WHERE id=?1",
                    params![
                        u.id,
                        u.proto,
                        u.host,
                        u.port,
                        u.username,
                        u.listen_port,
                        u.reality_dest
                    ],
                )?;
            }
        }
        Ok(())
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

/// 隐私保护（Tor Onion 入口）的持久化。**没有凭据列**——开关版不新建入站，
/// 也就没有属于它自己的凭据（这是相对早期设计的一处简化）。
pub mod privacies {
    use super::*;

    /// 读取列清单：与 `row_to_privacy` 的取值索引逐位对应（照 nodes 那条教训收口成常量）。
    const COLS: &str = "id,server_id,node_id,name,port,fwd_host,fwd_port,tor_dir,onion_host,\
                        last_check_error,created_at";

    fn row_to_privacy(r: &rusqlite::Row) -> rusqlite::Result<Privacy> {
        Ok(Privacy {
            id: r.get(0)?,
            server_id: r.get(1)?,
            node_id: r.get(2)?,
            name: r.get(3)?,
            port: r.get(4)?,
            fwd_host: r.get(5)?,
            fwd_port: r.get(6)?,
            tor_dir: r.get(7)?,
            onion_host: r.get(8)?,
            last_check_error: r.get(9)?,
            created_at: r.get(10)?,
        })
    }

    pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Privacy>> {
        conn.query_row(
            &format!("SELECT {COLS} FROM privacies WHERE id=?1"),
            params![id],
            row_to_privacy,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    /// 按被保护的节点定位开关。**这是开关的唯一入口**：节点上有没有它，
    /// 就代表那条线走不走 Tor。不按 protocol+addr 定位——同机多条线的协议与
    /// 地址相同，会互相误判。
    pub fn get_by_node(conn: &Connection, node_id: &str) -> rusqlite::Result<Option<Privacy>> {
        conn.query_row(
            &format!("SELECT {COLS} FROM privacies WHERE node_id=?1"),
            params![node_id],
            row_to_privacy,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    /// 一次取全量：生成客户端配置时要按 node_id 查「这条线开没开隐私」，
    /// 逐节点查库会变成 N 次加锁。
    pub fn list_all(conn: &Connection) -> rusqlite::Result<Vec<Privacy>> {
        let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM privacies ORDER BY id"))?;
        let rows = stmt.query_map([], row_to_privacy)?;
        rows.collect()
    }

    /// 插入一条隐私入口（= 打开某条线的开关）。无凭据参数：开关版不新建入站。
    pub fn insert(conn: &Connection, p: &Privacy) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO privacies (server_id,node_id,name,port,fwd_host,fwd_port,tor_dir,\
             onion_host,last_check_error,created_at)\
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                p.server_id,
                p.node_id,
                p.name,
                p.port,
                p.fwd_host,
                p.fwd_port,
                p.tor_dir,
                p.onion_host,
                p.last_check_error,
                p.created_at
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 节点改名时同步显示名。
    ///
    /// 隐私线的客户端代理名由 `Privacy.name + NODE_SUFFIX` 派生；不同步的话
    /// 节点页显示新名、仪表盘出口却仍是旧名，而且下一次开关/重建会继续用旧名。
    pub fn rename_for_node(conn: &Connection, node_id: &str, name: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE privacies SET name=?2 WHERE node_id=?1",
            params![node_id, name],
        )?;
        Ok(())
    }

    /// 回读到的 `.onion` 写回（部署 job 最后一步；中断则保持空串）。
    pub fn set_onion(conn: &Connection, id: i64, onion: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE privacies SET onion_host=?2 WHERE id=?1",
            params![id, onion],
        )?;
        Ok(())
    }

    /// 把隐私记录的显示名同步为节点当前名（幂等，返回改动行数）。
    ///
    /// 隐私节点的代理名由 `privacies.name + " [隐私]"` 派生；旧版本改名时没有
    /// 同步这里，升级后会留下「节点页叫新名、客户端代理叫旧名」的分叉，节点改名
    /// 后的出口恢复也会按错误的新名去选。装配期跑一次把存量拉齐。
    pub fn sync_names(conn: &Connection) -> rusqlite::Result<usize> {
        conn.execute(
            "UPDATE privacies SET name=(SELECT name FROM nodes WHERE nodes.id=privacies.node_id)
             WHERE EXISTS (SELECT 1 FROM nodes WHERE nodes.id=privacies.node_id)
               AND name <> (SELECT name FROM nodes WHERE nodes.id=privacies.node_id)",
            [],
        )
    }

    /// 写回本条独享的 HiddenServiceDir。`tor_dir` 是行 id 的函数
    /// （[`crate::servers::privacy::hidden_service_dir`]），插入时 id 还没生成，
    /// 所以由命令层补一次——让库里的每一行都自洽，而不是靠「空则回退」去猜。
    pub fn set_tor_dir(conn: &Connection, id: i64, dir: &str) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE privacies SET tor_dir=?2 WHERE id=?1",
            params![id, dir],
        )?;
        Ok(())
    }

    /// 探测/部署结果落库：成功时 error 置 NULL（照 upstreams::set_probe 的同款语义）。
    pub fn set_error(conn: &Connection, id: i64, err: Option<&str>) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE privacies SET last_check_error=?2 WHERE id=?1",
            params![id, err],
        )?;
        Ok(())
    }

    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM privacies WHERE id=?1", params![id])?;
        Ok(())
    }
}
