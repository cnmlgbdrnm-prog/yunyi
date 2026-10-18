//! 订阅同步服务：拉取 → 识别格式 → 解析 → 过滤 → 事务入库 → 刷新合并配置。
//! 失败语义：拉取或解析失败时保留旧节点、记录 last_sync_error，绝不半途清库。
//! Clash 来源节点将原始 YAML 片段（clash_raw）并入 params_json 入库，配置生成时原样输出。

use crate::db::{repos, Db};
use crate::domain::{Node, Subscription};
use crate::subs::parse::{detect, parse, ParsedNode};
pub use crate::subs::{SubError, SyncOutcome};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// 构建订阅拉取 HTTP 客户端：连接与整体请求超时兜底，
/// 防止卡死订阅源（永不响应的 TCP/HTTP 端点）永久挂起同步线程。
pub fn http_client(connect_timeout: Duration, timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .build()
        .expect("构建 HTTP 客户端失败")
}

/// 订阅同步服务。
pub struct SubscriptionService {
    db: Arc<Db>,
    client: reqwest::Client,
    /// 合并配置缓存（YAML 字符串），订阅服务与导出读取
    config: Arc<RwLock<String>>,
    /// 最近一次重建跳过的 ssr 节点数（mihomo 不支持 SSR；与 config 缓存同生命周期，
    /// AppState 共享同一计数器，导出/UI 读取）
    skipped_ssr: Arc<AtomicUsize>,
    /// sing-box config.json 缓存（第二导出出口），与 config 同生命周期
    config_singbox: Arc<RwLock<String>>,
    /// sing-box 缓存重建时跳过的 ssr 节点数（sing-box 自 1.6.0 移除 SSR 支持；
    /// 与 config_singbox 同生命周期，AppState 共享同一计数器，导出读取）
    skipped_ssr_singbox: Arc<AtomicUsize>,
    /// sing-box 构建失败标记（Some = 上次重建失败原因，缓存可能过期）；
    /// 与 config_singbox 同生命周期，AppState 共享同一容器，导出命令读
    config_singbox_error: Arc<RwLock<Option<String>>>,
    /// 凭据加密服务：节点参数（含密码/UUID）入库前加密
    creds: Arc<crate::crypto::CredentialService>,
    /// 每订阅在飞守卫：同一订阅并发同步直接拒绝，
    /// 防止旧同步的失败状态覆盖新同步结果（守卫只在插入/移除瞬间持有锁）
    in_flight: tokio::sync::Mutex<HashSet<String>>,
}

impl SubscriptionService {
    #[allow(clippy::too_many_arguments)] // 构造器聚合全部服务依赖（db/HTTP/缓存/计数），语义直接不打包
    pub fn new(
        db: Arc<Db>,
        client: reqwest::Client,
        config: Arc<RwLock<String>>,
        skipped_ssr: Arc<AtomicUsize>,
        config_singbox: Arc<RwLock<String>>,
        skipped_ssr_singbox: Arc<AtomicUsize>,
        config_singbox_error: Arc<RwLock<Option<String>>>,
        creds: Arc<crate::crypto::CredentialService>,
    ) -> Self {
        Self {
            db,
            client,
            config,
            skipped_ssr,
            config_singbox,
            skipped_ssr_singbox,
            config_singbox_error,
            creds,
            in_flight: tokio::sync::Mutex::new(HashSet::new()),
        }
    }

    /// 同步单个订阅；成功后刷新合并配置，失败时保留旧节点并记录 last_sync_error。
    /// 同一订阅已在同步中时拒绝并发重复调用（该订阅正在同步中）。
    pub async fn sync(&self, id: &str) -> Result<SyncOutcome, SubError> {
        {
            let mut inflight = self.in_flight.lock().await;
            if !inflight.insert(id.to_string()) {
                return Err(SubError::FetchFailed("该订阅正在同步中".into()));
            }
        }
        let outcome = self.sync_impl(id).await;
        self.in_flight.lock().await.remove(id);
        outcome
    }

    /// 同步主流程（含订阅存在性检查），调用前必须已持有在飞守卫。
    async fn sync_impl(&self, id: &str) -> Result<SyncOutcome, SubError> {
        let sub = {
            let conn = self.db.lock();
            let subs =
                repos::subs::list(&conn).map_err(|e| SubError::FetchFailed(e.to_string()))?;
            subs.into_iter().find(|s| s.id == id)
        }
        .ok_or_else(|| SubError::FetchFailed("订阅不存在".into()))?;

        let outcome = self.sync_inner(&sub).await;
        if let Err(e) = &outcome {
            // 失败语义：同步失败必须留痕，旧节点保持原样
            self.record_sync_error(&sub.id, e)?;
        }
        outcome
    }

    /// 同步主流程：拉取 → 识别 → 解析 → 过滤 → 事务入库 → 刷新合并配置。
    async fn sync_inner(&self, sub: &Subscription) -> Result<SyncOutcome, SubError> {
        let body = self.fetch(sub).await?;
        let format = detect(&body)?;
        let parsed = parse(&body, format)?;
        let filtered = apply_filters(sub, parsed);
        let outcome = self.store_nodes(sub, filtered)?;
        self.rebuild_config().await?;
        Ok(outcome)
    }

    /// 把失败原因写入订阅状态；自身失败时不静默吞掉。
    fn record_sync_error(&self, sub_id: &str, err: &SubError) -> Result<(), SubError> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE subscriptions SET last_sync_error=?1 WHERE id=?2",
            rusqlite::params![err.to_string(), sub_id],
        )
        .map_err(|e| SubError::FetchFailed(format!("记录同步失败原因失败: {e}")))?;
        Ok(())
    }

    /// 拉取订阅内容；非 2xx 视为失败。
    async fn fetch(&self, sub: &Subscription) -> Result<Vec<u8>, SubError> {
        let resp = self
            .client
            .get(&sub.url)
            .send()
            .await
            .map_err(|e| SubError::FetchFailed(describe_reqwest_error(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SubError::FetchFailed(format!(
                "订阅源返回 HTTP {}",
                status.as_u16()
            )));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| SubError::FetchFailed(describe_reqwest_error(&e)))
    }

    /// 事务入库：清掉已下架节点，upsert 新节点；成功后更新订阅状态。
    fn store_nodes(
        &self,
        sub: &Subscription,
        parsed: Vec<ParsedNode>,
    ) -> Result<SyncOutcome, SubError> {
        let mut conn = self.db.lock();
        let tx = conn
            .transaction()
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let mut upserted = 0usize;
        let mut keep_ids = Vec::with_capacity(parsed.len());
        for p in parsed {
            let id = stable_node_id(sub, &p);
            let mut params = p.params.clone();
            if let Some(raw) = &p.clash_raw {
                // Clash 来源：raw YAML 片段以字符串值并入 params_json，配置生成时原样输出
                params.insert("clash_raw".into(), serde_json::Value::String(raw.clone()));
            }
            let node = Node {
                id: id.clone(),
                name: p.name,
                source: "import".into(),
                protocol: p.protocol,
                addr: p.addr,
                port: p.port,
                params_json: serde_json::to_string(&params).unwrap_or_else(|_| "{}".into()),
                group_name: "默认分组".into(),
                sub_id: Some(sub.id.clone()),
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
            repos::nodes::upsert(&tx, &node, &self.creds)
                .map_err(|e| SubError::FetchFailed(e.to_string()))?;
            keep_ids.push(id);
            upserted += 1;
        }
        let removed = repos::nodes::delete_for_sub_except(&tx, &sub.id, &keep_ids)
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE subscriptions SET last_sync_at=?1, last_sync_error=NULL WHERE id=?2",
            rusqlite::params![now, sub.id],
        )
        .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        tx.commit()
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        Ok(SyncOutcome { upserted, removed })
    }

    /// 刷新合并配置与 ssr 跳过计数：仅启用订阅下的节点进入（手工/自建节点不受
    /// 订阅状态影响）；ssr 节点由 build_yaml/build_singbox 剔除并计数，
    /// 同步写入两个共享计数器（Clash 与 sing-box 各自计各自的 raw 判定）。
    /// sing-box 构建失败时降级：yaml 主链路照常，错误原因写入
    /// config_singbox_error 标记且旧缓存保留——一行特殊节点（如 clash_raw
    /// type: wireguard）不得使订阅同步整体失败，也不得静默导出过期配置。
    /// 「哪些节点进配置」的唯一口径：**其订阅启用**；`sub_id=None`（自建/手工）恒可见。
    /// 注意节点自身的 `enabled` 不在这里过滤——`build_yaml`/`build_singbox` 内部各自
    /// 还会按 `n.enabled` 再筛一次并排序，两处保持一致。rebuild_config 与导出选中共用
    /// 本函数，避免「清单」与「导出内容」两个口径漂移。
    pub fn visible_nodes(&self) -> Result<Vec<Node>, SubError> {
        let nodes = repos::nodes::list(&self.db.lock(), &self.creds)
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let enabled_subs: HashSet<String> = repos::subs::list(&self.db.lock())
            .map_err(|e| SubError::FetchFailed(e.to_string()))?
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.id)
            .collect();
        Ok(nodes
            .into_iter()
            .filter(|n| match &n.sub_id {
                Some(sid) => enabled_subs.contains(sid),
                None => true,
            })
            .collect())
    }

    pub async fn rebuild_config(&self) -> Result<(), SubError> {
        let visible: Vec<Node> = self.visible_nodes()?;
        // 智能分流开关：设置表 export_smart_split（"1" 开，"0" 关，缺省视为开）
        let smart = repos::settings::get(&self.db.lock(), "export_smart_split")
            .map_err(|e| SubError::FetchFailed(e.to_string()))?
            .map(|v| v == "1")
            .unwrap_or(true);
        // 中转规则/服务器表：挂规则节点在导出为经中转机的地址（与上网客户端同口径）
        let relay_rules = crate::relay::list_relay_rules_inner(&self.db)
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let relay_servers = repos::servers::list(&self.db.lock())
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        // 隐私开关：与 relay 规则同一时机读，走**带隐私**的那个构造函数。
        // 用不带隐私的会产出「界面显示已开启、配置里却是 VPS 公网入口」的配置——
        // 这正是 build_yaml_with_privacy 的文档注释警告的那一类偏差。
        let privacies = repos::privacies::list_all(&self.db.lock())
            .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        let (yaml, skipped_ssr) = crate::config::build_yaml_with_privacy(
            &visible,
            smart,
            &relay_rules,
            &relay_servers,
            &privacies,
        )
        .map_err(|e| SubError::FetchFailed(e.to_string()))?;
        *self.config.write().await = yaml;
        self.skipped_ssr.store(skipped_ssr, Ordering::Relaxed);
        match crate::config::singbox::build_singbox_with_privacy(&visible, &privacies) {
            Ok((json, skipped_ssr_singbox)) => {
                *self.config_singbox.write().await = json;
                self.skipped_ssr_singbox
                    .store(skipped_ssr_singbox, Ordering::Relaxed);
                *self.config_singbox_error.write().await = None;
            }
            Err(e) => {
                // 降级：只记标记，不清缓存/计数（防双缓存分裂），不阻断 sync
                *self.config_singbox_error.write().await = Some(e.to_string());
            }
        }
        Ok(())
    }
}

/// reqwest 的 Display 不含 cause 链（超时等细节在其 source 中），
/// 拼接完整错误链，让用户/日志可诊断且消息可读。
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        msg.push_str(": ");
        msg.push_str(&c.to_string());
        cause = c.source();
    }
    msg
}

/// 按订阅过滤规则取舍节点：include 为空则全收；命中 include 且不命中 exclude 才保留。
fn apply_filters(sub: &Subscription, parsed: Vec<ParsedNode>) -> Vec<ParsedNode> {
    let filters: serde_json::Value = serde_json::from_str(&sub.filter_json)
        .unwrap_or_else(|_| serde_json::json!({"include":[],"exclude":[]}));
    let include: Vec<String> = filters
        .get("include")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let exclude: Vec<String> = filters
        .get("exclude")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let inc_re: Vec<Regex> = include.iter().filter_map(|p| Regex::new(p).ok()).collect();
    let exc_re: Vec<Regex> = exclude.iter().filter_map(|p| Regex::new(p).ok()).collect();
    parsed
        .into_iter()
        .filter(|n| {
            let kept = inc_re.is_empty() || inc_re.iter().any(|r| r.is_match(&n.name));
            kept && !exc_re.iter().any(|r| r.is_match(&n.name))
        })
        .collect()
}

/// 稳定节点 ID：订阅内按 URI/片段哈希，重同步不重复入库。
fn stable_node_id(sub: &Subscription, p: &ParsedNode) -> String {
    let canonical = format!("{}|{}|{}|{}|{}", sub.id, p.protocol, p.addr, p.port, p.name);
    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(&digest[..8])
}
