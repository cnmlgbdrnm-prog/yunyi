//! 应用状态装配：聚合 DB、凭据、订阅服务、合并配置、job 队列与订阅服务句柄，
//! 由 main 启动时构建并以 Tauri managed state 注入命令层。

use crate::crypto::{CredentialService, KeyringStore};
use crate::db::Db;
use crate::serve::SubServer;
use crate::settings::SettingsService;
use crate::subs::service::SubscriptionService;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// 全局应用状态。
pub struct AppState {
    pub db: Arc<Db>,
    pub creds: Arc<CredentialService>,
    /// 订阅服务改为 Arc：既供命令层直接调用，也克隆给部署 runner
    pub subs: Arc<SubscriptionService>,
    pub config: Arc<RwLock<String>>,
    /// 最近一次合并配置重建时跳过的 ssr 节点数（mihomo 不支持 SSR；导出结果提示用）。
    /// 与 SubscriptionService 共享同一计数器：rebuild 写，export_yaml 读。
    pub skipped_ssr: Arc<AtomicUsize>,
    /// sing-box config.json 缓存（第二导出出口），与 config 同生命周期
    pub config_singbox: Arc<RwLock<String>>,
    /// sing-box 缓存重建时跳过的 ssr 节点数（sing-box 自 1.6.0 移除 SSR 支持；
    /// export_singbox 结果提示用）。与 SubscriptionService 共享同一计数器。
    pub skipped_ssr_singbox: Arc<AtomicUsize>,
    /// sing-box 构建失败标记：上次重建失败的可读原因（Some = 缓存可能过期，
    /// export_singbox 必须报错而非静默导出旧配置）；重建成功时清空。
    /// 与 SubscriptionService 共享同一容器：rebuild 写，export_singbox 读。
    pub config_singbox_error: Arc<RwLock<Option<String>>>,
    pub sub_server: Mutex<Option<SubServer>>,
    pub settings: SettingsService,
    pub job_queue: crate::jobs::JobQueue,
    /// mihomo sidecar（P4 测速）：懒启动、App 退出时停；None = 未运行
    pub sidecar: tokio::sync::Mutex<Option<crate::speed::sidecar::RunningSidecar>>,
    /// 本机上网客户端（独立 mihomo 实例 + 系统代理）；None = 未开启。App 退出时停
    pub net: tokio::sync::Mutex<Option<crate::net::runtime::RunningNet>>,
    /// 运行时连接日志采样器（第 7 项泄漏检测）：随网络开启而起的后台任务，
    /// 秒级采样并推事件。**必须常驻**而不是面板驱动——连接短命，面板不开就漏，
    /// 而漏掉的恰恰是这个功能要找的东西。
    pub conn_sampler: crate::net::connlog::ConnSampler,
    /// 打包内置 mihomo.exe 的资源路径（main setup 注入；未打包/未下载为 None）
    pub sidecar_resource: std::sync::RwLock<Option<std::path::PathBuf>>,
    /// 本机 Tor（隐私保护链路的客户端那一段）：懒启动、App 退出时停；None = 未运行。
    ///
    /// **锁序**：本槽位在持有期间还会去读 `net` 的混合端口（Tor 借道它出网），
    /// 所以 `tor → net` 是允许的方向；反过来（在 `net` 的锁里启 Tor）会死锁，
    /// 只有「网络起来之后」的代码才允许调 [`crate::tor::ensure_started`]。
    pub tor: tokio::sync::Mutex<Option<crate::tor::RunningTor>>,
    /// 打包内置 tor.exe 的资源路径（main setup 注入；未打包/未下载为 None）
    pub tor_resource: std::sync::RwLock<Option<std::path::PathBuf>>,
    /// Tor bootstrap 跟踪任务「在飞」标记（[`crate::tor::spawn_bootstrap`] 幂等用）。
    /// 重复点「开启网络」不该把 bootstrap 从头再来一遍。
    pub tor_watch: std::sync::atomic::AtomicBool,
    /// 指向自身的弱引用（装配期注入）。
    ///
    /// 为什么需要：后台任务要 `'static` 才能 spawn，而它要碰 `net`/`tor` 两把锁、
    /// 只能靠 `Arc<AppState>`。调用点（`net::runtime`）手里只有 `&AppState`，
    /// 拿不到 Arc——与 `job_queue` 的 `state_slot` 是同一个问题的两种解法
    /// （那边是 OnceLock<Arc>，这边**必须是 Weak**：强引用会让 AppState 永不释放）。
    pub self_ref: std::sync::OnceLock<std::sync::Weak<AppState>>,
    /// 数据目录：sidecar 工作目录 `{data_dir}/sidecar` 的父级，由 db_path 派生
    pub data_dir: std::path::PathBuf,
    /// 服务器健康监控调度器；应用退出时 stop。None = 尚未启动。
    pub monitor: std::sync::Mutex<Option<crate::server_monitor::scheduler::MonitorHandle>>,
}

impl AppState {
    /// 打开数据目录中的数据库并装配全部服务（生产：KeyringStore）。
    pub fn new(db_path: &std::path::Path) -> Result<Arc<Self>, String> {
        Self::new_with_store(db_path, Arc::new(KeyringStore))
    }

    /// 可注入 KeyStore 的装配入口：测试用 MemoryStore，避免触碰真实凭据管理器。
    pub fn new_with_store(
        db_path: &std::path::Path,
        store: Arc<dyn crate::crypto::KeyStore>,
    ) -> Result<Arc<Self>, String> {
        let db = Arc::new(Db::open(db_path).map_err(|e| e.to_string())?);
        let creds = Arc::new(CredentialService::new(store).map_err(|e| e.to_string())?);
        let config = Arc::new(RwLock::new(String::new()));
        let skipped_ssr = Arc::new(AtomicUsize::new(0));
        let config_singbox = Arc::new(RwLock::new(String::new()));
        let skipped_ssr_singbox = Arc::new(AtomicUsize::new(0));
        let config_singbox_error = Arc::new(RwLock::new(None));
        let settings = SettingsService::new(db.clone())?;
        // 存量明文节点参数一次性迁移为加密（幂等；失败不阻断启动，下次启动重试）
        let _ = crate::db::repos::nodes::migrate_params(&db.lock(), &creds);
        // 存量「地址被改指中转入口」的节点还原直连地址（幂等；同上，失败不阻断启动）。
        // 与 migrate_params 同理放在装配期：配置生成、测速、导出都读 nodes.addr，
        // 治晚了就有一段时间量的是错的链路。
        let _ = crate::db::repos::nodes::heal_direct_addresses(&db.lock(), &creds);
        // 存量重名节点消歧（幂等；同上，失败不阻断启动，下次启动重试）。
        // 代理名是 mihomo 配置里的全局键：升级前入库的重名节点会让整份配置被
        // `proxy xxx is the duplicate name` 拒绝——表现就是「开启网络」永远失败。
        let _ = crate::db::repos::nodes::ensure_unique_names(&db.lock(), &creds);
        // 生产超时：连接 15s、整体 30s，卡死端点不得永久挂起同步
        let client = crate::subs::service::http_client(
            std::time::Duration::from_secs(15),
            std::time::Duration::from_secs(30),
        );
        let subs = Arc::new(SubscriptionService::new(
            db.clone(),
            client.clone(),
            config.clone(),
            skipped_ssr.clone(),
            config_singbox.clone(),
            skipped_ssr_singbox.clone(),
            config_singbox_error.clone(),
            creds.clone(),
        ));
        // 两阶段装配：ProdRunner 需要 Arc<AppState>，而 AppState 需要 ProdRunner。
        // state 经 OnceLock 延迟注入（worker 惰性启动，首次 submit 前必已 set）。
        let state_slot: Arc<std::sync::OnceLock<Arc<Self>>> = Arc::new(std::sync::OnceLock::new());
        let job_queue = crate::jobs::JobQueue::new(
            db.clone(),
            crate::servers::deploy::prod_runner(
                state_slot.clone(),
                client,
                crate::servers::deploy::SS_GH_API_BASE.to_string(),
                crate::servers::deploy::SINGBOX_GH_API_BASE.to_string(),
            ),
        );
        let state = Arc::new(Self {
            db,
            creds,
            subs,
            config,
            skipped_ssr,
            config_singbox,
            skipped_ssr_singbox,
            config_singbox_error,
            sub_server: Mutex::new(None),
            settings,
            job_queue,
            sidecar: tokio::sync::Mutex::new(None),
            net: tokio::sync::Mutex::new(None),
            tor: tokio::sync::Mutex::new(None),
            conn_sampler: crate::net::connlog::ConnSampler::new(),
            sidecar_resource: std::sync::RwLock::new(None),
            tor_resource: std::sync::RwLock::new(None),
            tor_watch: std::sync::atomic::AtomicBool::new(false),
            self_ref: std::sync::OnceLock::new(),
            data_dir: db_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf(),
            monitor: std::sync::Mutex::new(None),
        });
        // 构造期无并发读取（worker 未启动），set 必成功
        let _ = state_slot.set(state.clone());
        // 自引用弱指针：后台任务用（见字段注释）。同样在构造期设置，set 必成功。
        let _ = state.self_ref.set(Arc::downgrade(&state));
        Ok(state)
    }

    /// 拿回自身的 `Arc`（后台任务用）。装配完成前返回 None。
    pub fn shared(&self) -> Option<Arc<Self>> {
        self.self_ref.get().and_then(|w| w.upgrade())
    }
}
