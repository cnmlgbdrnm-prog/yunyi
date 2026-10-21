//! Tauri 命令层：IPC 薄层。每个命令做参数校验后转调领域服务；
//! 内部函数以 Arc<AppState> 直参形式供测试直测，Tauri 壳函数只做 State 提取。

use crate::db::repos;
use crate::domain::{
    AdoptLinkInput, BuildLinkInput, Channel, ChannelStatus, ConnectionTest, CreateServerInput,
    DeployInput, DevicePeer, Job, LinkBuildReq, LinkStatus, LinkTeardownReq, Node, Privacy,
    ProbeResult, RelayRule, Server, ServerSpeedReq, SpeedTestReq, Subscription, ThroughputReq,
    ThroughputTest, UpdateServerInput, UpdateServerResult, Upstream,
};
use crate::relay;
use crate::serve;
use crate::servers;
use crate::servers::channels::{AddChannelInput, ServerPort};
use crate::state::AppState;
use crate::subs::SyncOutcome;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use uuid::Uuid;

/// 前端可见的统一错误。
///
/// **线上形状保持字符串**（`Serialize` 输出 `to_string()`）：前端 44 条命令全部经
/// `api/index.ts` 拿到它并当字符串用，改成 `{code,message}` 是无需求驱动的破坏性变更
/// ——当前没有任何一处 UI 需要按类别分支。分类只在 Rust 侧与将来的 api 层归一化有用。
/// 分类本身是为了让「校验失败」与「远端/内部失败」在 Rust 里可 `match`，而不是靠文案
/// 前缀猜（那是把文案当契约）。
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum AppError {
    /// 入参不合法（空名、格式不符等），进 IPC 前就该被拦住。
    #[error("{0}")]
    Validation(String),
    /// 目标行不存在（id 查不到、已被删）。
    #[error("{0}")]
    NotFound(String),
    /// 远端主机/服务返回的失败（SSH、部署、systemctl 等）。
    #[error("{0}")]
    Remote(String),
    /// 其余兜底（`err()` 的默认落点）。
    #[error("{0}")]
    Message(String),
}

impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

type R<T> = Result<T, AppError>;

/// 兜底：把任意 Display 错误收成 `Message`。pub(crate) 供测试直测这个落点。
pub(crate) fn err<E: std::fmt::Display>(e: E) -> AppError {
    AppError::Message(e.to_string())
}

/// 导出命令结果：保存路径 + 本次导出的 ssr 跳过计数（0 = 全部包含）。
/// IPC 层返回 Option<ExportOutcome>：None = 用户在保存对话框取消。
#[derive(Debug, Clone, Serialize)]
pub struct ExportOutcome {
    pub path: String,
    pub skipped_ssr: usize,
}

// ---------- 订阅 ----------

pub fn list_subscriptions(state: Arc<AppState>) -> R<Vec<Subscription>> {
    repos::subs::list(&state.db.lock()).map_err(err)
}

pub fn create_subscription(
    state: Arc<AppState>,
    name: String,
    url: String,
    enabled: bool,
    filter_json: String,
) -> R<Subscription> {
    if name.trim().is_empty() {
        return Err(AppError::Validation("订阅名不能为空".into()));
    }
    if url.trim().is_empty() {
        return Err(AppError::Validation("订阅 URL 不能为空".into()));
    }
    let sub = Subscription {
        id: Uuid::new_v4().simple().to_string(),
        name,
        url,
        enabled,
        last_sync_at: None,
        sync_interval: 86400,
        last_sync_error: None,
        filter_json,
    };
    repos::subs::insert(&state.db.lock(), &sub).map_err(err)?;
    Ok(sub)
}

pub async fn update_subscription(
    state: Arc<AppState>,
    id: String,
    name: String,
    url: String,
    enabled: bool,
    filter_json: String,
) -> R<Subscription> {
    if name.trim().is_empty() {
        return Err(AppError::Validation("订阅名不能为空".into()));
    }
    if url.trim().is_empty() {
        return Err(AppError::Validation("订阅 URL 不能为空".into()));
    }
    let updated = Subscription {
        id,
        name,
        url,
        enabled,
        last_sync_at: None,
        sync_interval: 86400,
        last_sync_error: None,
        filter_json,
    };
    repos::subs::update(&state.db.lock(), &updated).map_err(err)?;
    // 订阅状态（启用/禁用）直接决定哪些节点可见：除重建下发/导出用的缓存外，
    // 还必须让**正在跑的客户端**跟上——它手上是「开网络」那一刻渲出来的快照。
    rebuild_and_apply(&state).await?;
    Ok(updated)
}

pub async fn delete_subscription(state: Arc<AppState>, id: String) -> R<()> {
    {
        // 先删子行再删父行：外键 ON DELETE SET NULL 会先置空 sub_id，
        // 若先删订阅，显式清理将无法再按 sub_id 命中节点
        let conn = state.db.lock();
        conn.execute("DELETE FROM nodes WHERE sub_id=?1", rusqlite::params![&id])
            .map_err(err)?;
    }
    repos::subs::delete(&state.db.lock(), &id).map_err(err)?;
    // 删除订阅后立即重建缓存并热更新运行中的客户端，避免继续服务已删订阅的节点
    rebuild_and_apply(&state).await
}

pub async fn sync_subscription(state: Arc<AppState>, id: String) -> R<SyncOutcome> {
    let outcome = state.subs.sync(&id).await.map_err(err)?;
    // `sync` 内部已重建下发/导出缓存；这里补上**运行中客户端**的那一步。
    // 漏掉它的表现（2026-10-19 实测）：订阅同步出 105 个节点，界面上都能选，
    // 但跑着的 mihomo 还是开网络那一刻的 8 个——切到任意机场节点回
    // `HTTP 400 Selector update error: proxy not exist`。
    // 同步目前只有用户手动触发这一条路径（没有后台定时器）；将来若加定时同步，
    // 那条路径应直接调 `SubService::sync`，别把这里的「立即生效」带过去。
    crate::net::runtime::apply_config_change(&state)
        .await
        .map_err(err)?;
    Ok(outcome)
}

// ---------- 节点 ----------

pub fn list_nodes(state: Arc<AppState>) -> R<Vec<Node>> {
    // 与导出/分享/运行配置同一口径：停用订阅下的节点不在任何配置里，也不该出现在
    // 节点清单与出口下拉框里。否则用户能选中一个客户端根本不认识的代理名。
    state.subs.visible_nodes().map_err(err)
}

/// 用户显式改了配置之后：先重建合并配置缓存（导出/下发读到新数据），
/// 再让**正在运行**的本机上网客户端跟上（见 `net::runtime::apply_config_change`）。
///
/// 判据是「用户显式动作」而不是「这次改动涉不涉及订阅」：订阅的增删改与手动同步
/// 同样要立刻生效，否则界面读库能选到节点、客户端却只认识启动那一刻的快照
/// （切换回 `HTTP 400 Selector update error: proxy not exist`）。
/// 将来若加**后台定时同步**，那条路径应直接调 `SubService::sync`——没必要每同步一次
/// 就热重载一次用户正在用的客户端。
async fn rebuild_and_apply(state: &Arc<AppState>) -> R<()> {
    state.subs.rebuild_config().await.map_err(err)?;
    crate::net::runtime::apply_config_change(state)
        .await
        .map_err(AppError::Message)?;
    Ok(())
}

pub async fn set_node_enabled(state: Arc<AppState>, id: String, enabled: bool) -> R<()> {
    repos::nodes::update_enabled(&state.db.lock(), &id, enabled).map_err(err)?;
    // 开关直接影响合并配置与上网客户端（节点在不在客户端的代理表里），立即生效
    rebuild_and_apply(&state).await
}

pub async fn delete_node(state: Arc<AppState>, id: String) -> R<()> {
    repos::nodes::delete(&state.db.lock(), &id).map_err(err)?;
    // 删除后同样立即重建缓存并让客户端丢掉这条代理
    rebuild_and_apply(&state).await
}

/// 节点改名。名字不是装饰：它直接当 sing-box 的 tag 与 Clash 的 proxy 名、也是
/// proxy-groups 成员与「出口节点」选择的键（config/mod.rs:90、net/runtime.rs:310），
/// 所以空名与重名在写库前拒绝——否则会静默生成一份坏配置。
///
/// 自建节点改了就稳；订阅节点下次同步会按 `stable_node_id`（由名字派生）重建回原名，
/// 这是已接受的边界，不做墓碑表。
pub async fn rename_node(state: Arc<AppState>, id: String, name: String) -> R<()> {
    let name = servers::clean_text(&name);
    if name.is_empty() {
        return Err(AppError::Validation("节点名不能为空".into()));
    }
    if let Some(reason) = crate::config::proxy_name_conflict(&name) {
        return Err(AppError::Validation(format!(
            "节点名「{name}」不可用：{reason}"
        )));
    }
    // 必须在改库/热重载之前读：改完后「手动选择」可能已经回退到自动选择。
    let previous_selection = crate::net::runtime::manual_selection(&state).await;
    let (changed, old_effective, new_effective) = {
        let conn = state.db.lock();
        // 先判存在：repos::rename 对不存在的 id 返回 false，与「名字没变」不可区分
        let old_node = repos::nodes::get(&conn, &id, &state.creds)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("节点不存在".into()))?;
        let nodes = repos::nodes::list(&conn, &state.creds).map_err(err)?;
        let privacies = repos::privacies::list_all(&conn).map_err(err)?;
        let private = crate::servers::privacy::effective_privacy(&old_node.id, &privacies).is_some();
        let new_effective = if private {
            crate::servers::privacy::display_name(&name)
        } else {
            name.clone()
        };
        if nodes.iter().any(|n| n.id != id && n.name == name) {
            return Err(AppError::Message(format!("已存在同名节点：{name}")));
        }
        // 原始名不同也可能撞上代理名：隐私线的可见名是「原名 [隐私]」。
        if let Some(other) = nodes
            .iter()
            .find(|n| n.id != id && crate::config::client_name(n, &privacies) == new_effective)
        {
            return Err(AppError::Message(format!(
                "节点名与「{}」的生效名冲突：{new_effective}",
                other.name
            )));
        }
        let old_effective = crate::config::client_name(&old_node, &privacies);
        let changed = repos::nodes::rename(&conn, &id, &name, &state.creds).map_err(err)?;
        if changed {
            // 隐私线的代理显示名由 privacies.name 派生；不同步会让节点页与仪表盘
            // 各显示一个名字，且下次热重载仍用旧名。
            repos::privacies::rename_for_node(&conn, &id, &name).map_err(err)?;
        }
        (changed, old_effective, new_effective)
    };
    if changed {
        // 名字进合并配置（proxies 名/tag/分组成员），也要让运行中的客户端跟着改名——
        // 否则客户端还挂着旧名，而「手动选择」组里已经没有那个成员了
        rebuild_and_apply(&state).await?;
        // mihomo 按名字恢复选择；旧名消失时它会回退到组内第一项「自动选择」。
        // 改名不是「取消选择」，恢复成新名才是用户预期。
        if let Err(e) = crate::net::runtime::restore_manual_selection(
            &state,
            previous_selection.as_deref(),
            &old_effective,
            &new_effective,
        )
        .await
        {
            eprintln!("[vpn-ctl] 改名后恢复手动出口失败（保持现有出口）: {e}");
        }
    }
    Ok(())
}

/// 节点中转开关（切换 relay_rule_id）：挂上 = 选第一条「已建成」规则（imported/built，
/// 键自网元：隧道收敛且入站就绪）；未挂 = 恢复直连。无可用规则时报错不静默。
/// 开关只动配置（直连地址始终保留在节点行），撤销随开关零成本。
pub async fn toggle_node_relay(state: Arc<AppState>, id: String) -> R<()> {
    let node = {
        let db = state.db.lock();
        repos::nodes::get(&db, &id, &state.creds)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("节点不存在".into()))?
    };
    let new = match node.relay_rule_id {
        Some(_) => {
            // 关闭中转必须**连地址一起还原**：节点地址是被 set_route 改写成中转入口
            // 的，只清标记会停在「标记说直连、地址却指中转入口」的错位态——测速量到的
            // 仍是中转链路，而且此后没有入口能还原它（restore_nodes_direct 只从
            // 拆链/解除接管调用）。无 direct 记录（从未挂过中转）时 restore_direct
            // 返回 false 且不动地址，由下面那句 set_relay 清标记。
            repos::nodes::restore_direct(&state.db.lock(), &id, &state.creds).map_err(err)?;
            None
        }
        None => {
            let rules = crate::relay::list_relay_rules(&state).map_err(err)?;
            crate::relay::select_rule_for_node(node.port, &rules)
                .map(|r| Some(r.id))
                .ok_or_else(|| {
                    format!(
                        "该节点（端口 {}）暂无对应的中转入口：转发是端口对端口（当前规则只覆盖其落地端口），如需中转请先建对应转发",
                        node.port
                    )
                })
                .map_err(err)?
        }
    };
    repos::nodes::set_relay(&state.db.lock(), &id, new).map_err(err)?;
    // 挂载状态影响合并配置（导出订阅/上网客户端）：节点的生效地址整个换了一台机器，
    // 客户端不跟上就等于还指着旧地址（直连地址或旧中转入口）
    rebuild_and_apply(&state).await
}

// ---------- 配置 ----------

pub async fn get_merged_config(state: Arc<AppState>) -> R<String> {
    Ok(state.config.read().await.clone())
}

/// 导出公共实现：内容与跳过计数由调用方从缓存读出，这里只负责
/// 弹保存对话框（dialog 注入）、空配置守卫与写盘。对话框与写盘都是阻塞 IO，
/// 留在 blocking 线程完成；取消返回 None（取消不触发空配置报错）。
/// dialog 参数：生产传 [`native_save_dialog`]（rfd 原生模态对话框）；
/// 测试注入固定路径闭包，避免真实对话框阻塞测试进程。
/// guard：用户选定路径后、写盘前校验（如空配置拒绝导出）。
async fn save_export(
    content: String,
    skipped_ssr: usize,
    guard: impl FnOnce(&str) -> Result<(), AppError> + Send + 'static,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    let path = tokio::task::spawn_blocking(dialog).await.map_err(err)?;
    match path {
        Some(p) => {
            guard(&content)?;
            let path_str = p.display().to_string();
            tokio::task::spawn_blocking(move || std::fs::write(&p, content))
                .await
                .map_err(err)?
                .map_err(err)?;
            Ok(Some(ExportOutcome {
                path: path_str,
                skipped_ssr,
            }))
        }
        None => Ok(None),
    }
}

/// 生产保存对话框：rfd 原生模态对话框（由 save_export 在 blocking 线程执行）。
fn native_save_dialog(
    filter: &'static str,
    extensions: &'static [&'static str],
    default_name: &'static str,
) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter(filter, extensions)
        .set_file_name(default_name)
        .save_file()
}

/// 空配置守卫：导出缓存里没有任何可用节点时拒绝生成空壳文件——
/// 空 proxies 的 Clash YAML（MATCH 指向空组）与无节点出站的 sing-box
/// 配置在任何客户端都不可用，静默导出只会让用户拿到废文件。
const EXPORT_EMPTY_HINT: &str =
    "当前没有可导出的节点：请在订阅页导入并启用订阅，或在服务器页对落地机执行「部署」（部署成功的代理会自动进入导出）";
/// pub(crate)：分享（share::create_share）复用同一守卫，保证「空配置」在
/// 导出与分享两条路上给出**同一句**人话，而不是两处各写一份措辞。
pub(crate) fn ensure_exportable_has_nodes(yaml: &str) -> Result<(), AppError> {
    let v: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|e| AppError::Message(format!("导出配置解析失败: {e}")))?;
    let has = v
        .get("proxies")
        .and_then(|p| p.as_sequence())
        .map(|p| !p.is_empty())
        .unwrap_or(false);
    if !has {
        return Err(AppError::Message(EXPORT_EMPTY_HINT.into()));
    }
    Ok(())
}

/// sing-box 版空配置守卫：outbounds 中除两组基础设施（自动测速/自动选择）
/// 外没有任何节点出站即视为空。
fn ensure_singbox_has_nodes(json: &str) -> Result<(), AppError> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| AppError::Message(format!("导出配置解析失败: {e}")))?;
    let has = v["outbounds"]
        .as_array()
        .map(|outbounds| {
            outbounds.iter().any(|o| {
                let tag = o["tag"].as_str().unwrap_or("");
                tag != crate::config::singbox::SELECTOR_TAG
                    && tag != crate::config::singbox::URLTEST_TAG
            })
        })
        .unwrap_or(false);
    if !has {
        return Err(AppError::Message(EXPORT_EMPTY_HINT.into()));
    }
    Ok(())
}

/// 导出目标格式：两种格式的生成逻辑与 rebuild_config 完全相同，只是节点列表可裁剪。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportFormat {
    Yaml,
    Singbox,
}

/// 按选中 id 生成配置与跳过计数。`node_ids` 为空 = 全部可见节点（等价旧行为）。
///
/// **输入组合照抄 rebuild_config**（可见节点 + smart 设置 + 中转规则 + 服务器表），
/// 否则导出结果会与「开启网络」实际使用的配置漂移——那是最难发现的一类 bug。
pub(crate) async fn build_config_for(
    state: &Arc<AppState>,
    node_ids: &[String],
    format: ExportFormat,
) -> R<(String, usize)> {
    let mut nodes = state.subs.visible_nodes().map_err(err)?;
    if !node_ids.is_empty() {
        // 不在可见集合里的 id 静默忽略：调用方的清单来自同一个可见性口径，
        // 出现差异说明中途有改动，以导出时刻的库状态为准。
        // `enabled` 在这里显式再筛一次：build_* 内部本来也会筛，但写在这里才能让
        // 「清单列出的 = 导出写出的」这个契约一眼可读，不必去翻 builder 的实现。
        nodes.retain(|n| n.enabled && node_ids.iter().any(|id| id == &n.id));
    }
    match format {
        ExportFormat::Yaml => {
            let smart = repos::settings::get(&state.db.lock(), "export_smart_split")
                .map_err(err)?
                .map(|v| v == "1")
                .unwrap_or(true);
            let rules = crate::relay::list_relay_rules_inner(&state.db).map_err(err)?;
            let servers = repos::servers::list(&state.db.lock()).map_err(err)?;
            // 隐私也照抄：rebuild_config 已经带上它，这里不带就又漂了——导出的那条线
            // 会是 VPS 公网入口，而界面/运行配置里它是 .onion。
            // 语义是**一致**：开隐私的线在任何地方都是隐私线。代价是别的设备若没有
            // 本机 Tor（9050）就用不了它——那是 fail-closed，不是泄漏。
            let privacies = repos::privacies::list_all(&state.db.lock()).map_err(err)?;
            crate::config::build_yaml_with_privacy(&nodes, smart, &rules, &servers, &privacies)
                .map_err(err)
        }
        ExportFormat::Singbox => {
            let privacies = repos::privacies::list_all(&state.db.lock()).map_err(err)?;
            crate::config::singbox::build_singbox_with_privacy(&nodes, &privacies).map_err(err)
        }
    }
}

/// 导出候选清单：**只列导出时真会写进文件的那些**。
///
/// 两类节点的处理刻意不同：
/// - **未启用**（用户在「节点」页显式关掉的）：直接不列——它不是「导不出去」，
///   而是用户自己关的；列出来会让人以为选了 4 条、实际只导出 2 条。
/// - **ssr**（能力限制）：列出但标记不可导出并带理由，静默剔除正是「这条线怎么
///   传不上去」的观感来源。
pub fn list_exportable_nodes(state: Arc<AppState>) -> R<Vec<crate::domain::ExportableNode>> {
    Ok(state
        .subs
        .visible_nodes()
        .map_err(err)?
        .into_iter()
        .filter(|n| n.enabled)
        .map(|node| {
            let ssr = node.protocol == "ssr";
            crate::domain::ExportableNode {
                node,
                exportable: !ssr,
                skip_reason: ssr.then(|| "mihomo 与 sing-box 已移除 SSR 支持".to_string()),
            }
        })
        .collect())
}

/// 弹出保存对话框并写出合并 YAML；取消返回 None。
/// 返回携带本次导出的 ssr 跳过计数（ssr 在 mihomo 与 sing-box 均已移除支持，导出不包含）。
/// 无任何节点时拒绝导出（空壳在任何客户端都不可用）。
pub async fn export_yaml(
    state: Arc<AppState>,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    let yaml = state.config.read().await.clone();
    let skipped_ssr = state.skipped_ssr.load(Ordering::Relaxed);
    save_export(yaml, skipped_ssr, ensure_exportable_has_nodes, dialog).await
}

/// 弹出保存对话框并写出 sing-box config.json；取消返回 None。返回携带跳过计数。
/// 缓存构建失败标记置位时（见 rebuild_config 降级）报可读错误，
/// 不静默导出可能过期的旧配置；无任何节点时同样拒绝导出。
pub async fn export_singbox(
    state: Arc<AppState>,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    if let Some(reason) = state.config_singbox_error.read().await.clone() {
        return Err(AppError::Message(format!("sing-box 导出不可用：{reason}")));
    }
    let json = state.config_singbox.read().await.clone();
    let skipped_ssr = state.skipped_ssr_singbox.load(Ordering::Relaxed);
    save_export(json, skipped_ssr, ensure_singbox_has_nodes, dialog).await
}

/// 空选集守卫：空数组**不等于**「全部」，必须显式报错——否则用户以为筛过了，
/// 实际导出的是全量，而文件里多出来的线路不会被任何人发现。
fn ensure_selection_non_empty(node_ids: &[String]) -> Result<(), AppError> {
    if node_ids.is_empty() {
        return Err(AppError::Message("请至少选择一个节点".into()));
    }
    Ok(())
}

/// 导出选中节点为 YAML。生成逻辑与整份导出共用 `build_config_for`，只有节点列表不同。
pub async fn export_yaml_selected(
    state: Arc<AppState>,
    node_ids: Vec<String>,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    ensure_selection_non_empty(&node_ids)?;
    let (yaml, skipped_ssr) = build_config_for(&state, &node_ids, ExportFormat::Yaml).await?;
    save_export(yaml, skipped_ssr, ensure_exportable_has_nodes, dialog).await
}

/// 导出选中节点为 sing-box config.json。构建失败标记置位时同样不导出过期配置。
pub async fn export_singbox_selected(
    state: Arc<AppState>,
    node_ids: Vec<String>,
    dialog: impl FnOnce() -> Option<PathBuf> + Send + 'static,
) -> R<Option<ExportOutcome>> {
    ensure_selection_non_empty(&node_ids)?;
    if let Some(reason) = state.config_singbox_error.read().await.clone() {
        return Err(AppError::Message(format!("sing-box 导出不可用：{reason}")));
    }
    let (json, skipped_ssr) = build_config_for(&state, &node_ids, ExportFormat::Singbox).await?;
    save_export(json, skipped_ssr, ensure_singbox_has_nodes, dialog).await
}

// ---------- 订阅服务 ----------

pub async fn start_sub_server(state: Arc<AppState>, port: u16, token: String) -> R<u16> {
    let mut guard = state.sub_server.lock().await;
    if guard.is_some() {
        return Err(AppError::Message("订阅服务已在运行".into()));
    }
    let server = serve::start(state.config.clone(), token, port)
        .await
        .map_err(err)?;
    let actual = server.port;
    *guard = Some(server);
    Ok(actual)
}

pub async fn stop_sub_server(state: Arc<AppState>) -> R<()> {
    let mut guard = state.sub_server.lock().await;
    if let Some(server) = guard.take() {
        server.handle.abort();
        // 等待被中止的任务真正退出，监听端口随即释放，避免停止后立刻重启/探测到旧端口
        let _ = server.handle.await;
    }
    Ok(())
}

/// 查询订阅服务运行态：设置页挂载时读取初始状态，
/// 避免视图切换后本地态被重置为"已停止"而无法停止实际运行中的服务。
pub async fn get_sub_server_status(state: Arc<AppState>) -> R<bool> {
    Ok(state.sub_server.lock().await.is_some())
}

// ---------- 设置 ----------

pub fn get_settings(state: Arc<AppState>, keys: Vec<String>) -> R<HashMap<String, String>> {
    let mut out = HashMap::new();
    for k in keys {
        if let Some(v) = state.settings.get(&k).map_err(err)? {
            out.insert(k, v);
        }
    }
    Ok(out)
}

pub fn set_setting(state: Arc<AppState>, key: String, value: String) -> R<()> {
    state.settings.set(&key, &value).map_err(err)?;
    Ok(())
}

// ---------- 服务器 ----------

pub fn list_servers(state: Arc<AppState>) -> R<Vec<Server>> {
    servers::list_servers(&state).map_err(err)
}

pub fn create_server(state: Arc<AppState>, input: CreateServerInput) -> R<Server> {
    servers::create_server(&state, input).map_err(err)
}

/// 更新服务器。出口地址族（`egress_family`）变化且该机已有部署协议时，
/// 保存成功后**自动排一次重部署**——出口值是写进 VPS 侧配置的，不重部署就只是
/// 改了库里的一个数字，用户看到的仍是旧出口。
///
/// 失败语义：保存成功即成功，部署只是后续动作，提交失败放进 `deploy_error`
/// 由界面提示，绝不为了一次没排上的部署把用户刚改的资料回滚。
///
/// 不用 `install_supplement`：它要求「现有协议 ∪ 勾选协议」产生新增，纯出口
/// 变更（协议集合不变）会被它以 `merged == existing` 拒绝。
pub fn update_server(state: Arc<AppState>, input: UpdateServerInput) -> R<UpdateServerResult> {
    // 必须先读旧值：保存之后库里已是新值，再读就判断不出「出口是否变化」。
    let old_egress = {
        let conn = state.db.lock();
        repos::servers::get(&conn, input.id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?
            .egress_family
    };
    let server = servers::update_server(&state, input).map_err(err)?;
    let mut deploy_job_id = None;
    let mut deploy_error = None;
    if server.egress_family != old_egress {
        // 未部署过任何协议时不需要重部署：新值会在首次部署时自然写进配置。
        match servers::deploy::deployed_protocols(&state, server.id) {
            Ok(protocols) if !protocols.is_empty() => {
                let submitted = start_deploy(
                    state.clone(),
                    DeployInput {
                        server_id: server.id,
                        protocols,
                        // None = 让 deploy 从既有节点保留用户自定义的 REALITY
                        // 伪装域名，而不是回落默认值。
                        reality_dest: None,
                        enable_bbr: None,
                    },
                );
                match submitted {
                    Ok(job_id) => deploy_job_id = Some(job_id),
                    Err(e) => deploy_error = Some(e.to_string()),
                }
            }
            Ok(_) => {}
            Err(e) => deploy_error = Some(e),
        }
    }
    Ok(UpdateServerResult {
        server,
        deploy_job_id,
        deploy_error,
    })
}

/// 切换服务器地址族。整台服务器语义：SSH/测速管理地址与该机所有自建直连节点
/// 的 `addr` 在同一事务内一起切；中转节点的 `addr` 是入口地址，不在此列。
/// 切换后立即重建并下发配置，正在运行的上网客户端不用手动重连。
///
/// 首次切到 v6 且没有登记地址时自动探测（SSH 走原始 `host`）。探测最长要
/// 连接+命令两个超时，绝不能持数据库锁等待，所以先探测、后开事务。
pub async fn set_server_addr_family(state: Arc<AppState>, id: i64, family: String) -> R<Server> {
    if !matches!(family.as_str(), "v4" | "v6") {
        return Err(AppError::Validation("地址族只能是 v4 或 v6".into()));
    }
    let detected = if family == "v6" {
        let missing = {
            let conn = state.db.lock();
            repos::servers::get(&conn, id)
                .map_err(err)?
                .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?
                .host_v6
                .is_empty()
        };
        if missing {
            let found = crate::servers::ipv6_detect::detect_ipv6_addresses(&state, id)
                .await
                .map_err(|e| {
                    AppError::Validation(format!(
                        "自动探测 IPv6 失败：{e}；可在「编辑」里手动填写 IPv6 地址"
                    ))
                })?;
            Some(
                found
                    .into_iter()
                    .next()
                    .ok_or_else(|| AppError::Validation("服务器上没有发现全局 IPv6 地址".into()))?,
            )
        } else {
            None
        }
    } else {
        None
    };
    let updated = {
        let mut conn = state.db.lock();
        let s = repos::servers::get(&conn, id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
        let host_v6 = detected.unwrap_or_else(|| s.host_v6.clone());
        if family == "v6" && host_v6.is_empty() {
            return Err(AppError::Validation(
                "这台服务器还没有登记 IPv6 地址".into(),
            ));
        }
        let old = crate::domain::effective_host(&s).to_string();
        let new = if family == "v6" {
            host_v6.clone()
        } else {
            s.host.clone()
        };
        // 已是目标族且生效地址没变 → 不必重建；刚探测到的地址与已登记值相同时同理
        if s.addr_family == family && old == new {
            return Ok(s);
        }
        let tx = conn.transaction().map_err(err)?;
        // 探测结果与切换同事务落库：避免「地址已写、族没切」或反过来
        tx.execute(
            "UPDATE servers SET addr_family=?2, host_v6=?3 WHERE id=?1",
            rusqlite::params![id, &family, &host_v6],
        )
        .map_err(err)?;
        if old != new {
            crate::servers::sync_nodes_addr(&tx, &old, &new).map_err(err)?;
        }
        tx.commit().map_err(err)?;
        repos::servers::get(&conn, id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?
    };
    rebuild_and_apply(&state).await?;
    Ok(updated)
}

/// 只探测不切换：编辑表单的「自动探测」按钮用，返回候选地址（推荐值在首位）。
pub async fn detect_server_ipv6(state: Arc<AppState>, id: i64) -> R<Vec<String>> {
    crate::servers::ipv6_detect::detect_ipv6_addresses(&state, id)
        .await
        .map_err(err)
}

pub fn delete_server(state: Arc<AppState>, id: i64) -> R<()> {
    servers::delete_server(&state, id).map_err(err)
}

pub async fn test_server_connection(state: Arc<AppState>, id: i64) -> R<ConnectionTest> {
    servers::test_connection(&state, id).await.map_err(err)
}

pub async fn setup_passwordless(state: Arc<AppState>, id: i64) -> R<()> {
    servers::setup_passwordless(&state, id).await.map_err(err)
}

// ---------- 通道管理 ----------

pub fn list_channels(state: Arc<AppState>, server_id: i64) -> R<Vec<Channel>> {
    servers::channels::list_channels(&state, server_id).map_err(err)
}

pub fn add_channel(state: Arc<AppState>, input: AddChannelInput) -> R<Channel> {
    servers::channels::add_channel(&state, input).map_err(err)
}

pub fn remove_channel(state: Arc<AppState>, id: i64) -> R<()> {
    servers::channels::remove_channel(&state, id).map_err(err)
}

pub async fn scan_server_ports(state: Arc<AppState>, server_id: i64) -> R<Vec<ServerPort>> {
    servers::channels::scan_ports(&state, server_id)
        .await
        .map_err(err)
}

pub async fn channel_status(state: Arc<AppState>, id: i64) -> R<ChannelStatus> {
    servers::channels::channel_status(&state, id)
        .await
        .map_err(err)
}

pub async fn set_channel_enabled(state: Arc<AppState>, id: i64, enabled: bool) -> R<ChannelStatus> {
    servers::channels::set_channel_enabled(&state, id, enabled)
        .await
        .map_err(err)
}

// ---------- 上游出口（住宅代理） ----------

/// 出口列表（纯 DB）。
pub fn list_upstreams(state: Arc<AppState>, server_id: i64) -> R<Vec<Upstream>> {
    let conn = state.db.lock();
    repos::upstreams::list_by_server(&conn, server_id).map_err(err)
}

/// 这台机器上**除上游之外**已占用的端口，交给 `upstream::validate` 拒绝。
///
/// 两处来源都要，且都是在入口处拒绝而不是等远端报错：
///   ① deploy 管的协议入站——它不写在任何一处配置里，只体现在「该机节点的 `port`」上。
///      界面里监听端口**默认就预填 443**，而这正是协议入站最常用的端口：2026-09-21
///      实测撞上过一次，第二个 Xray 实例 bind 失败立刻退出，job 却报 done，节点表现成
///      「延迟测试失败: HTTP 503」（详见 `ports_in_use_elsewhere`）。
///   ② 中转链路的 `wg_port` / 模式 A 的 `forward_port`——撞上会表现为「链路莫名不通」
///      而不是报错。
fn upstream_reserved_ports(state: &AppState, server_id: i64) -> R<Vec<i64>> {
    // relay 那部分自己查库（内部加锁），必须在下面那个锁作用域之外算
    let mut ports: Vec<i64> = relay::list_relay_rules(state)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.relay_server_id == server_id)
        .flat_map(|r| {
            let mut v = vec![r.wg_port];
            if let Some(f) = r.forward_port {
                v.push(f);
            }
            v
        })
        .collect();
    let conn = state.db.lock();
    let server = repos::servers::get(&conn, server_id)
        .map_err(err)?
        .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
    let nodes: Vec<crate::domain::Node> = repos::nodes::list(&conn, &state.creds)
        .map_err(err)?
        .into_iter()
        .filter(|n| crate::domain::server_owns_addr(&server, &n.addr))
        .collect();
    let upstream_node_ids: Vec<String> = repos::upstreams::list_by_server(&conn, server_id)
        .map_err(err)?
        .into_iter()
        .filter_map(|u| u.node_id)
        .collect();
    ports.extend(servers::upstream::ports_in_use_elsewhere(
        &nodes,
        &upstream_node_ids,
    ));
    ports.sort_unstable();
    ports.dedup();
    Ok(ports)
}

/// 添加出口：校验 → 落库。密码 seal 入库；身份材料留占位 `{}`，
/// 由部署 job 首次跑时生成（生成需要 SSH 跑 `xray x25519`，这里没有连接）。
pub fn add_upstream(state: Arc<AppState>, input: servers::upstream::UpstreamInput) -> R<Upstream> {
    let existing = {
        let conn = state.db.lock();
        repos::upstreams::list_by_server(&conn, input.server_id).map_err(err)?
    };
    let reserved = upstream_reserved_ports(&state, input.server_id)?;
    // 端口撞了就**自动改用空闲端口**（首选能用则原样保留）：报错会让用户卡在「换个端口」
    // 上反复试，而挤掉已有占用更糟——那会把他正在用的线路弄断。挑中的值随返回值带回，
    // 界面据此提示「已被占用，已改用 X」。
    let taken: Vec<i64> = existing
        .iter()
        .map(|u| u.listen_port)
        .chain(reserved.iter().copied())
        .collect();
    let input = servers::upstream::UpstreamInput {
        listen_port: servers::upstream::pick_listen_port(input.listen_port, &taken),
        ..input
    };
    servers::upstream::validate(&input, &existing, &reserved).map_err(err)?;

    let row = Upstream {
        id: 0,
        server_id: input.server_id,
        name: servers::clean_text(&input.name),
        proto: input.proto.clone(),
        host: servers::clean_text(&input.host),
        port: input.port,
        username: input.username.clone(),
        listen_port: input.listen_port,
        reality_dest: servers::clean_text(&input.reality_dest),
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

/// 编辑出口（名称/协议/地址/端口/凭据/监听端口/伪装域名）。**只写库**：远端那份配置与
/// 写回的节点都由部署 job 重渲染（`upstream.rs` 尾部），所以改完**必须重部署才生效**
/// ——界面据此提示，别在这里假装已经切过去了。
///
/// **曾经只让改监听端口**，其余字段走「删掉重建」，理由写的是「改那些要重发身份材料」。
/// 那个理由对**上游端点与凭据**并不成立：换住宅代理的地址/端口/协议只是改 xray 的
/// 出站，入站那套 REALITY 身份与写回的节点都不动。代价却在用户身上现形了
/// （2026-09-22 真机）：对面把 socks5 端口发在 12324、HTTP 在 12323，用户填错一位，
/// 只能删掉重建——出口 id 3→4、写回的节点跟着重建、名称与分组全丢。
///
/// **名称也在这条编辑路径里**：它是写回节点名 `<服务器名>-<出口名>` 的一部分。
/// 保存只改自身行；重新部署时 job 按同一个 `node_id` 用新名字覆盖节点，节点身份与
/// 凭据不变。不要在这里顺手改 nodes 行——运行中的 mihomo 配置还是旧名字，提前改库
/// 只会让界面显示一个客户端并不认识的节点。
pub fn update_upstream(
    state: Arc<AppState>,
    id: i64,
    edit: servers::upstream::UpstreamEdit,
) -> R<Upstream> {
    let (row, existing) = {
        let conn = state.db.lock();
        let row = repos::upstreams::get(&conn, id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("出口不存在".into()))?;
        let existing = repos::upstreams::list_by_server(&conn, row.server_id).map_err(err)?;
        (row, existing)
    };
    let reserved = upstream_reserved_ports(&state, row.server_id)?;
    // 编辑场景的约定：被编辑行自己不算冲突，否则「改成原值」也会被判成撞车
    let others: Vec<Upstream> = existing.into_iter().filter(|u| u.id != id).collect();
    // 与新增同一套语义：撞了就自动改用空闲端口，不报错、也不挤掉别人
    let taken: Vec<i64> = others
        .iter()
        .map(|u| u.listen_port)
        .chain(reserved.iter().copied())
        .collect();
    let listen_port = servers::upstream::pick_listen_port(edit.listen_port, &taken);
    let input = servers::upstream::UpstreamInput {
        server_id: row.server_id,
        // 名称可改；重新部署时部署 job 按 node_id 用新名字覆盖写回的节点
        name: edit.name.clone(),
        proto: edit.proto.clone(),
        host: edit.host.clone(),
        port: edit.port,
        username: edit.username.clone(),
        password: edit.password.clone().unwrap_or_default(),
        listen_port,
        reality_dest: edit.reality_dest.clone(),
    };
    servers::upstream::validate(&input, &others, &reserved).map_err(err)?;
    let next = Upstream {
        name: servers::clean_text(&input.name),
        proto: input.proto.clone(),
        host: servers::clean_text(&input.host),
        port: input.port,
        username: input.username.clone(),
        listen_port,
        reality_dest: servers::clean_text(&input.reality_dest),
        ..row
    };
    // 留空 = 不动：空串与 None 都当「不改」，绝不用空值覆盖已存凭据
    let sealed = edit
        .password
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| crate::crypto::seal_param(&state.creds, p));
    repos::upstreams::update_fields(&state.db.lock(), &next, sealed.as_deref()).map_err(err)?;
    Ok(next)
}

/// 删除出口（纯 DB）。
///
/// **节点必须在这里一并删掉**：部署 job 是从 DB 现读出口集合来重渲染的，
/// 行删了之后 job 就再也拿不到 `node_id`，节点会变成永远清不掉的孤儿。
pub fn remove_upstream(state: Arc<AppState>, id: i64) -> R<()> {
    let conn = state.db.lock();
    let row = repos::upstreams::get(&conn, id)
        .map_err(err)?
        .ok_or_else(|| AppError::NotFound("出口不存在".into()))?;
    if let Some(node_id) = row.node_id.as_deref() {
        repos::nodes::delete(&conn, node_id).map_err(err)?;
    }
    repos::upstreams::delete(&conn, id).map_err(err)
}

/// 探测出口 IP：经本地 mihomo 走整条链路请求 ipify。失败原因落在出口行上（UI 红字）。
pub async fn probe_egress_ip(state: Arc<AppState>, upstream_id: i64) -> R<String> {
    crate::speed::probe_egress_ip(&state, upstream_id)
        .await
        .map_err(AppError::Message)
}

/// 提交部署/拆除任务：该中转机全部出口整份重渲染（DB 是唯一事实源，只带 server_id）。
pub fn start_upstream_job(state: Arc<AppState>, server_id: i64) -> R<i64> {
    {
        let conn = state.db.lock();
        repos::servers::get(&conn, server_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "upstream", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::Upstream(
            crate::jobs::UpstreamReq { job_id, server_id },
        ))
        .map_err(AppError::Message)?;
    Ok(job_id)
}

// ---------- 隐私保护（Tor Onion 开关） ----------

/// 这条线**能不能**开隐私保护。`Some(原因)` = 拒绝。
///
/// 只拒绝「这台机器上没有这条线」这一种：隐私保护靠的是**在这条线自己那台机器上**
/// 开隐藏服务，线不在这台机器上就无从谈起（订阅线路尤其——它的入口在别人的服务器上，
/// 在这里开 HiddenService 只会把本机一个毫不相干的端口暴露出去）。
///
/// **中转不是拒绝理由**：中转只是让入口换到中转机，隐藏服务跟着开过去就行
/// （见 [`crate::servers::privacy::resolve_target`]）。
fn privacy_refuse_reason(server: &Server, node: &Node) -> Option<String> {
    if node.source != "self-built" {
        return Some("只有本机部署的线路能开隐私保护：订阅线路的入口不在这台服务器上".into());
    }
    if !crate::domain::server_owns_addr(server, &node.addr) {
        return Some(format!(
            "这条线的入口是 {}，不在这台服务器上，开不了隐藏服务",
            node.addr
        ));
    }
    if node.port <= 0 {
        return Some("这条线没有可用的监听端口".into());
    }
    None
}

/// 打开隐私保护：在这条线自己那台机器上开一个 Tor 隐藏服务，转发到它**原本就在监听的
/// 端口**；客户端那条代理的 `server` 换成 `.onion`，端口与协议栈一个字不动。
///
/// 返回部署 job 的 id。**记录先落库、onion 地址等作业回填**是有意的：`onion_host` 为空时
/// [`crate::servers::privacy::effective_privacy`] 判为不生效，生成出来的配置与没开时逐字段
/// 相同。于是「点了开关但部署失败」的终态是「界面显示这条线开着、带红字错误，实际仍走
/// 直连」，而**不是**「以为在走 Tor，其实直连 VPS 公网 IP」。
pub fn enable_privacy(state: Arc<AppState>, server_id: i64, node_id: String) -> R<i64> {
    let (server, node, servers, rules) = {
        let conn = state.db.lock();
        let server = repos::servers::get(&conn, server_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
        let node = repos::nodes::get(&conn, &node_id, &state.creds)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("线路不存在".into()))?;
        // 入口判定要与生成侧**同源**：`node_relay_chain` / `effective_node` 就是
        // 客户端配置自己用的那两个函数，各写一份迟早对不上
        let servers = repos::servers::list(&conn).map_err(err)?;
        let rules = repos::relay::list(&conn).map_err(err)?;
        (server, node, servers, rules)
    };
    if let Some(reason) = privacy_refuse_reason(&server, &node) {
        return Err(AppError::Message(reason));
    }
    // 隐藏服务开在**入口那台机器**上：直连线是本机，forward 中转线是中转机。
    let target = crate::servers::privacy::resolve_target(&node, &server, &rules, &servers);

    // 已有记录就复用：失败留下的空记录尤其要复用——不复用的话用户每点一次开关
    // 就多出一行同名的开关，界面上全是幽灵条目。
    let privacy_id = {
        let conn = state.db.lock();
        match repos::privacies::get_by_node(&conn, &node.id).map_err(err)? {
            Some(p) if !p.onion_host.is_empty() && p.last_check_error.is_none() => {
                return Err(AppError::Message("这条线的隐私保护已经开着".into()))
            }
            Some(p) => p.id,
            None => {
                let row = Privacy {
                    id: 0,
                    server_id: target.hs_server_id,
                    node_id: node.id.clone(),
                    name: node.name.clone(),
                    port: target.listen_port,
                    fwd_host: target.fwd_host.clone(),
                    fwd_port: target.fwd_port,
                    tor_dir: String::new(),
                    onion_host: String::new(),
                    last_check_error: None,
                    created_at: chrono::Utc::now().to_rfc3339(),
                };
                let id = repos::privacies::insert(&conn, &row).map_err(err)?;
                let dir = crate::servers::privacy::hidden_service_dir(id);
                repos::privacies::set_tor_dir(&conn, id, &dir).map_err(err)?;
                id
            }
        }
    };

    // 作业连的是**隐藏服务那台机器**，不是用户点开关时所在的那台
    let job_id = repos::jobs::insert(&state.db.lock(), "privacy", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::Privacy(crate::jobs::PrivacyReq {
            job_id,
            server_id: target.hs_server_id,
            privacy_id,
        }))
        .map_err(AppError::Message)?;
    Ok(job_id)
}

/// 关闭隐私保护。返回 job id；确认已回滚的失败记录可以直接删行。
///
/// SSH 可在远端 Onion 已启动后断开，导致数据库没有地址且回滚未完成。
/// 只有明确记录「已回滚」才允许跳过远端清理；其余状态必须提交拆除作业。
pub fn disable_privacy(state: Arc<AppState>, privacy_id: i64) -> R<Option<i64>> {
    let row = {
        let conn = state.db.lock();
        repos::privacies::get(&conn, privacy_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("隐私开关不存在".into()))?
    };
    if row.onion_host.is_empty()
        && row
            .last_check_error
            .as_deref()
            .is_some_and(|error| error.starts_with("开启隐私保护失败（已回滚）"))
    {
        // 没生效过 → 生成侧本来就没改过任何东西，不必重建配置
        repos::privacies::delete(&state.db.lock(), privacy_id).map_err(err)?;
        return Ok(None);
    }

    let job_id = repos::jobs::insert(&state.db.lock(), "privacy", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::PrivacyRemove(
            crate::jobs::PrivacyRemoveReq {
                job_id,
                server_id: row.server_id,
                privacy_id,
            },
        ))
        .map_err(AppError::Message)?;
    Ok(Some(job_id))
}

/// 全部隐私开关。**含未生效的**：`onion_host` 为空 + `last_check_error` 正是界面要显示的
/// 「开着但没起来」状态，滤掉它们会让开关在界面上凭空变回「关」，与库里的事实相反。
pub fn list_privacies(state: Arc<AppState>) -> R<Vec<Privacy>> {
    repos::privacies::list_all(&state.db.lock()).map_err(err)
}

// ---------- 部署与任务 ----------

/// 提交部署任务：校验输入与服务器存在性 → 建 jobs 行（queued）→ 入队；
/// 入队失败时把 job 落 failed 并回传原因，避免排队错误被吞。
pub fn start_deploy(state: Arc<AppState>, input: DeployInput) -> R<i64> {
    if input.protocols.is_empty() {
        return Err(AppError::Message("未选择任何协议".into()));
    }
    for p in &input.protocols {
        if !crate::servers::deploy::is_supported_protocol(p) {
            return Err(AppError::Message(format!("不支持的协议: {p}")));
        }
    }
    {
        let conn = state.db.lock();
        repos::servers::get(&conn, input.server_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "deploy", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::Deploy(
            crate::jobs::DeployRequest {
                job_id,
                server_id: input.server_id,
                protocols: input.protocols,
                reality_dest: input.reality_dest,
                enable_bbr: input.enable_bbr.unwrap_or(false),
            },
        ))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            AppError::Message(e)
        })?;
    Ok(job_id)
}

/// 最近任务列表（按 id 倒序，上限 50）。
pub fn list_jobs(state: Arc<AppState>) -> R<Vec<Job>> {
    repos::jobs::list_recent(&state.db.lock()).map_err(err)
}

/// 补装协议（L2 补装/重跑）：现有已部署协议 ∪ 勾选协议 合并后提交一次 deploy job
/// （run_deploy 整体重写幂等：sing-box/xray 已装仅重配并 restart）。返回 job_id。
pub fn install_supplement(state: Arc<AppState>, server_id: i64, protocols: Vec<String>) -> R<i64> {
    if protocols.is_empty() {
        return Err(AppError::Message("未选择任何协议".into()));
    }
    for p in &protocols {
        if !crate::servers::deploy::is_supported_protocol(p) {
            return Err(AppError::Message(format!("不支持的协议: {p}")));
        }
    }
    {
        let conn = state.db.lock();
        repos::servers::get(&conn, server_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
    }
    let existing =
        crate::servers::deploy::deployed_protocols(&state, server_id).map_err(AppError::Message)?;
    let merged = crate::servers::deploy::merge_deploy_protocols(&existing, &protocols);
    if merged == existing {
        return Err(AppError::Message("所选协议均已部署".into()));
    }
    start_deploy(
        state,
        DeployInput {
            server_id,
            protocols: merged,
            reality_dest: None,
            enable_bbr: None,
        },
    )
}

/// 卸载协议：骨架实现——校验协议白名单/服务器存在/已部署后，
/// 返回 P2 里程碑提示（停止 unit + 摘除入站 + 重写摘要属下期流水线）。
pub fn remove_protocol(state: Arc<AppState>, server_id: i64, protocol: String) -> R<()> {
    if !crate::servers::deploy::is_supported_protocol(&protocol) {
        return Err(AppError::Message(format!("不支持的协议: {protocol}")));
    }
    {
        let conn = state.db.lock();
        repos::servers::get(&conn, server_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("服务器不存在".into()))?;
    }
    let current =
        crate::servers::deploy::deployed_protocols(&state, server_id).map_err(AppError::Message)?;
    if !current.contains(&protocol) {
        return Err(AppError::Message(format!("该服务器未部署协议 {protocol}")));
    }
    Err(AppError::Message(
        "协议卸载将在 P2 里程碑实现（停止 unit + 摘除入站 + 重写摘要）".into(),
    ))
}

/// 请求取消任务；job 不存在或已结束时返回错误。
pub fn cancel_job(state: Arc<AppState>, job_id: i64) -> R<()> {
    state.job_queue.cancel(job_id).map_err(AppError::Message)
}

// ---------- 中转链路 ----------

pub fn list_relay_rules(state: Arc<AppState>) -> R<Vec<RelayRule>> {
    relay::list_relay_rules(&state).map_err(err)
}

pub async fn probe_link(
    state: Arc<AppState>,
    relay_server_id: i64,
    landing_server_id: i64,
) -> R<ProbeResult> {
    relay::probe_link(&state, relay_server_id, landing_server_id)
        .await
        .map_err(err)
}

/// 建链输入校验：client_mode/action 取值、forward 模式 forward_port 必填、端口范围。
/// 与 runner 的 validate_build_input 同语义，命令层前置校验避免无效输入落 jobs 行。
fn validate_build_input(input: &BuildLinkInput) -> R<()> {
    if !matches!(input.client_mode.as_str(), "forward" | "peer") {
        return Err(AppError::Message(
            "client_mode 只能是 forward 或 peer".into(),
        ));
    }
    if input.client_mode == "forward" && input.forward_port.is_none() {
        return Err(AppError::Message(
            "forward 模式必须提供 forward_port".into(),
        ));
    }
    if !matches!(input.action.as_str(), "build" | "rebuild") {
        return Err(AppError::Message("action 只能是 build 或 rebuild".into()));
    }
    for p in [
        Some(input.wg_port),
        input.forward_port,
        Some(input.landing_port),
    ]
    .into_iter()
    .flatten()
    {
        if !(1..=65535).contains(&p) {
            return Err(AppError::Message("端口必须在 1-65535".into()));
        }
    }
    Ok(())
}

/// 提交建链任务：校验输入 → 建 jobs 行（kind='link'）→ 入队；
/// 入队失败时把 job 落 failed 并回传原因（沿用 start_deploy 模式）。
pub fn build_link(state: Arc<AppState>, input: BuildLinkInput) -> R<i64> {
    validate_build_input(&input)?;
    let job_id = repos::jobs::insert(&state.db.lock(), "link", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::LinkBuild(LinkBuildReq {
            job_id,
            name: input.name,
            relay_server_id: input.relay_server_id,
            landing_server_id: input.landing_server_id,
            client_mode: input.client_mode,
            wg_port: input.wg_port,
            forward_port: input.forward_port,
            landing_port: input.landing_port,
            converged: input.converged,
            action: input.action,
            forward_mode: input.forward_mode,
        }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            AppError::Message(e)
        })?;
    Ok(job_id)
}

pub async fn adopt_link(state: Arc<AppState>, input: AdoptLinkInput) -> R<RelayRule> {
    relay::adopt_link(&state, input).await.map_err(err)
}

/// 提交拆链任务：规则存在（NotFound 直接报错）→ 建 jobs 行（kind='link'）→ 入队。
/// action ∈ 'unregister'（解除登记）| 'full'（真实拆除）。
pub fn teardown_link(state: Arc<AppState>, rule_id: i64, action: String) -> R<i64> {
    if !matches!(action.as_str(), "unregister" | "full") {
        return Err(AppError::Message("未知拆链动作".into()));
    }
    {
        let conn = state.db.lock();
        repos::relay::get(&conn, rule_id)
            .map_err(err)?
            .ok_or_else(|| AppError::NotFound("链路不存在".into()))?;
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "link", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::LinkTeardown(LinkTeardownReq {
            job_id,
            rule_id,
            action,
        }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            AppError::Message(e)
        })?;
    Ok(job_id)
}

pub fn delete_link_rule(state: Arc<AppState>, rule_id: i64) -> R<()> {
    relay::delete_link_rule(&state, rule_id).map_err(err)
}

// ---------- 测速 ----------

/// 提交测速任务：校验 ids 非空（去重保序）、并发 1..=5、节点存在（任一缺失 → Err 列名）
/// → 建 jobs 行（kind='speed'）→ 入队；入队失败时把 job 落 failed 并回传原因（沿用
/// start_deploy 模式）。
pub fn speed_test(state: Arc<AppState>, node_ids: Vec<String>, concurrency: u8) -> R<i64> {
    if node_ids.is_empty() {
        return Err(AppError::Message("请选择至少一个节点".into()));
    }
    if !(1..=5).contains(&concurrency) {
        return Err(AppError::Message("并发数必须在 1-5".into()));
    }
    // 去重保序：同一节点只测一次，事件顺序按首次出现
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<String> = node_ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect();
    {
        let conn = state.db.lock();
        for id in &ids {
            if repos::nodes::get(&conn, id, &state.creds)
                .map_err(err)?
                .is_none()
            {
                return Err(AppError::NotFound(format!("节点不存在: {id}")));
            }
        }
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "speed", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::SpeedTest(SpeedTestReq {
            job_id,
            node_ids: ids,
            concurrency,
        }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            AppError::Message(e)
        })?;
    Ok(job_id)
}

/// 提交服务器测速任务：校验 ids 非空（去重保序）、并发 1..=5、服务器全部存在
/// （任一缺失 → Err 列名）→ 建 jobs 行（kind='server_speed'）→ 入队；入队失败时
/// 把 job 落 failed 并回传原因（沿用 start_deploy 模式）。
pub fn server_speed(state: Arc<AppState>, server_ids: Vec<i64>, concurrency: u8) -> R<i64> {
    if server_ids.is_empty() {
        return Err(AppError::Message("请选择至少一个服务器".into()));
    }
    if !(1..=5).contains(&concurrency) {
        return Err(AppError::Message("并发数必须在 1-5".into()));
    }
    // 去重保序：同一服务器只测一次，事件顺序按首次出现
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<i64> = server_ids
        .into_iter()
        .filter(|id| seen.insert(*id))
        .collect();
    {
        let conn = state.db.lock();
        for id in &ids {
            if repos::servers::get(&conn, *id).map_err(err)?.is_none() {
                return Err(AppError::NotFound(format!("服务器不存在: {id}")));
            }
        }
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "server_speed", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::ServerSpeed(ServerSpeedReq {
            job_id,
            server_ids: ids,
            concurrency,
        }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            AppError::Message(e)
        })?;
    Ok(job_id)
}

/// 提交服务器间吞吐对测任务：from!=to、两服务器存在、seconds 5..=30 →
/// 建 jobs 行（kind='server_speed'）→ 入队；入队失败回写 failed。
pub fn server_throughput(
    state: Arc<AppState>,
    from_server_id: i64,
    to_server_id: i64,
    seconds: u8,
) -> R<i64> {
    if from_server_id == to_server_id {
        return Err(AppError::Message("源与目标不能是同一服务器".into()));
    }
    if !(5..=30).contains(&seconds) {
        return Err(AppError::Message("对测时长必须在 5-30 秒".into()));
    }
    {
        let conn = state.db.lock();
        if repos::servers::get(&conn, from_server_id)
            .map_err(err)?
            .is_none()
        {
            return Err(AppError::NotFound(format!(
                "服务器不存在: {from_server_id}"
            )));
        }
        if repos::servers::get(&conn, to_server_id)
            .map_err(err)?
            .is_none()
        {
            return Err(AppError::NotFound(format!("服务器不存在: {to_server_id}")));
        }
    }
    let job_id = repos::jobs::insert(&state.db.lock(), "server_speed", "queued").map_err(err)?;
    state
        .job_queue
        .submit(crate::jobs::JobRequest::Throughput(ThroughputReq {
            job_id,
            from_server_id,
            to_server_id,
            seconds,
        }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            AppError::Message(e)
        })?;
    Ok(job_id)
}

/// 一键安装 iperf3（非 job，UI busy 态等返回）：委托 iperf 域服务。
pub async fn install_iperf3(state: Arc<AppState>, server_id: i64) -> R<String> {
    crate::server_speed::iperf::install_iperf3_on(&state, server_id)
        .await
        .map_err(err)
}

/// 历史对测结果（最新 50 条在前，前端展示最近一条）。
pub fn list_throughput(state: Arc<AppState>) -> R<Vec<ThroughputTest>> {
    repos::throughput::list(&state.db.lock()).map_err(err)
}

/// 检测服务器当前拥塞控制算法（惰性触发：仅行内按钮调用，页面加载绝不自动 SSH）。
pub async fn check_bbr(state: Arc<AppState>, server_id: i64) -> R<String> {
    let (auth, s) = servers::load_auth(&state, server_id).map_err(err)?;
    let mut sess = crate::ssh::SshSession::connect(
        crate::domain::effective_host(&s),
        s.port as u16,
        &s.user,
        &auth,
        servers::CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| AppError::Remote(e.to_string()))?;
    crate::server_bbr::check_bbr(&mut sess)
        .await
        .map_err(AppError::Remote)
}

/// 一键开启 BBR（非 job，UI busy 态等待）：幂等脚本 + 回读验证。
pub async fn enable_bbr(state: Arc<AppState>, server_id: i64) -> R<String> {
    let (auth, s) = servers::load_auth(&state, server_id).map_err(err)?;
    let mut sess = crate::ssh::SshSession::connect(
        crate::domain::effective_host(&s),
        s.port as u16,
        &s.user,
        &auth,
        servers::CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| AppError::Remote(e.to_string()))?;
    crate::server_bbr::enable_bbr(&mut sess)
        .await
        .map_err(AppError::Remote)
}

pub async fn refresh_link_status(state: Arc<AppState>, rule_id: i64) -> R<LinkStatus> {
    relay::refresh_link_status(&state, rule_id)
        .await
        .map_err(err)
}

pub fn list_device_peers(state: Arc<AppState>, rule_id: i64) -> R<Vec<DevicePeer>> {
    relay::list_device_peers(&state, rule_id).map_err(err)
}

pub fn add_device_peer(state: Arc<AppState>, rule_id: i64, name: String) -> R<i64> {
    relay::add_device_peer(&state, rule_id, name).map_err(err)
}

pub fn remove_device_peer(state: Arc<AppState>, peer_id: i64) -> R<i64> {
    relay::remove_device_peer(&state, peer_id).map_err(err)
}

pub async fn get_peer_config(state: Arc<AppState>, peer_id: i64) -> R<String> {
    relay::get_peer_config(&state, peer_id).await.map_err(err)
}

// Tauri 壳（IPC 适配层）拆到 ipc.rs：壳函数只做 State 提取，业务逻辑仍在上面。
// 重导出保证 `crate::commands::xxx_cmd` 路径与 generate_handler! 不变。
mod ipc;
pub use ipc::*;
mod monitor;
pub use monitor::*;
