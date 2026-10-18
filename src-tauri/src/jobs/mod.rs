//! job 队列：单 worker 串行执行部署/建链/拆链/设备 peer 任务。job 生命周期落 jobs 表
//! （queued→running→done/failed/cancelled），进度经 EmitFn 回传；
//! 取消为协作式：请求置位 AtomicBool，runner 步骤间检查。
//! worker 在首次 submit 时惰性启动。**别假设 submit 一定来自 async 上下文**：
//! 同步命令（`start_upstream_job`）是在主线程上调 submit 的，那里没有 Tokio 运行时，
//! 无条件 `tokio::spawn` 会 panic 且跨不过事件循环的 FFI 边界 → 进程 abort
//! （见 `ensure_worker` 里的实测记录）。启动路径按运行时有无分流。
//! 构造时把遗留 queued/running 行标为 failed（应用重启中断），并把遗留
//! building 的链路行标为 failed（崩溃/强杀后解锁接管/重建/拆链/删除路径）。

use crate::db::repos;
use crate::db::Db;
use crate::domain::{
    JobProgress, LinkBuildReq, LinkTeardownReq, PeerAddReq, PeerRemoveReq, ServerSpeedReq,
    SpeedTestReq, ThroughputReq,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::sync::mpsc;

pub type EmitFn = Arc<dyn Fn(JobProgress) + Send + Sync>;

/// 部署任务请求（jobs 行已由 start_deploy 建立）。protocols 为白名单内协议,
/// 每协议端口由部署流水线按 default_port + 占用偏移自动分配。
#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub job_id: i64,
    pub server_id: i64,
    pub protocols: Vec<String>,
    pub reality_dest: Option<String>,
    /// 部署成功后是否顺手检测并开启 BBR（失败不影响部署结论）
    pub enable_bbr: bool,
}

/// 上游出口部署/拆除请求：重渲染该中转机专用 Xray 实例的整份配置并写回节点。
/// 出口集合以 DB 为唯一事实源，故请求只带 server_id（新增/删除出口后都跑同一个 job）。
#[derive(Debug, Clone)]
pub struct UpstreamReq {
    pub job_id: i64,
    pub server_id: i64,
}

/// 隐私入口（Tor Onion）部署/重建请求。
///
/// 只带 `server_id` 与 `privacy_id`：隐私记录本身由命令层先落库（凭据与 loopback
/// 端口在那一步已定），作业只负责「把这条记录变成远端事实」——所以首建与重建
/// 跑的是同一条路径，天然幂等。
#[derive(Debug, Clone)]
pub struct PrivacyReq {
    pub job_id: i64,
    pub server_id: i64,
    pub privacy_id: i64,
}

/// 隐私入口拆除请求：清 torrc 段、重渲染掉该入站、删 Onion 身份目录与节点。
#[derive(Debug, Clone)]
pub struct PrivacyRemoveReq {
    pub job_id: i64,
    pub server_id: i64,
    pub privacy_id: i64,
}

/// 任务请求（jobs 行已由命令层建立并落 kind）。
#[derive(Debug, Clone)]
pub enum JobRequest {
    Deploy(DeployRequest),
    Upstream(UpstreamReq),
    Privacy(PrivacyReq),
    PrivacyRemove(PrivacyRemoveReq),
    LinkBuild(LinkBuildReq),
    LinkTeardown(LinkTeardownReq),
    PeerAdd(PeerAddReq),
    PeerRemove(PeerRemoveReq),
    SpeedTest(SpeedTestReq),
    ServerSpeed(ServerSpeedReq),
    Throughput(ThroughputReq),
}

impl JobRequest {
    pub fn job_id(&self) -> i64 {
        match self {
            JobRequest::Deploy(r) => r.job_id,
            JobRequest::Upstream(r) => r.job_id,
            JobRequest::Privacy(r) => r.job_id,
            JobRequest::PrivacyRemove(r) => r.job_id,
            JobRequest::LinkBuild(r) => r.job_id,
            JobRequest::LinkTeardown(r) => r.job_id,
            JobRequest::PeerAdd(r) => r.job_id,
            JobRequest::PeerRemove(r) => r.job_id,
            JobRequest::SpeedTest(r) => r.job_id,
            JobRequest::ServerSpeed(r) => r.job_id,
            JobRequest::Throughput(r) => r.job_id,
        }
    }

    /// 事件 kind：与 jobs 表 kind 列一致（deploy/upstream/link/peer/speed/server_speed）。
    pub fn kind(&self) -> &'static str {
        match self {
            JobRequest::Deploy(_) => "deploy",
            JobRequest::Upstream(_) => "upstream",
            JobRequest::Privacy(_) | JobRequest::PrivacyRemove(_) => "privacy",
            JobRequest::LinkBuild(_) | JobRequest::LinkTeardown(_) => "link",
            JobRequest::PeerAdd(_) | JobRequest::PeerRemove(_) => "peer",
            JobRequest::SpeedTest(_) => "speed",
            JobRequest::ServerSpeed(_) | JobRequest::Throughput(_) => "server_speed",
        }
    }
}

/// 任务执行器：SSH 全序列 + 进度发射；错误文本决定 job 落 failed。
#[async_trait::async_trait]
pub trait JobRunner: Send + Sync {
    async fn run(
        &self,
        req: JobRequest,
        cancel: Arc<AtomicBool>,
        emit: EmitFn,
    ) -> Result<(), String>;
}

/// 队列内部消息：Job 携带提交时注册的取消标志（queued 期间取消同样生效）。
enum QueueMsg {
    Job(JobRequest, Arc<AtomicBool>),
}

/// 队列句柄：submit/cancel 由命令层调用，worker 事件经 EmitFn 回传。
pub struct JobQueue {
    tx: mpsc::UnboundedSender<QueueMsg>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<QueueMsg>>>,
    cancels: Arc<Mutex<HashMap<i64, Arc<AtomicBool>>>>,
    emit: Arc<RwLock<EmitFn>>,
    db: Arc<Db>,
    runner: Arc<dyn JobRunner>,
    worker: OnceLock<()>,
}

impl JobQueue {
    /// 构造并执行启动恢复（遗留 queued/running → failed，遗留 building 链路行 → failed）；
    /// worker 首次 submit 时启动。
    pub fn new(db: Arc<Db>, runner: Arc<dyn JobRunner>) -> Self {
        if let Err(e) = repos::jobs::mark_interrupted(&db.lock()) {
            eprintln!("[vpn-ctl] 启动恢复失败: {e}");
        }
        // 启动对账（与 jobs 同位置）：遗留 building 的链路行置 failed，解锁
        // 「接管/重建/拆链/删除」全部拒绝路径（应用崩溃/强杀后无恢复入口）
        if let Err(e) = repos::relay::mark_building_failed(&db.lock()) {
            eprintln!("[vpn-ctl] 链路 building 对账失败: {e}");
        }
        let (tx, rx) = mpsc::unbounded_channel::<QueueMsg>();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            emit: Arc::new(RwLock::new(Arc::new(|_| {}))),
            db,
            runner,
            worker: OnceLock::new(),
        }
    }

    /// 设置生产事件发射器（main setup 中绑定 AppHandle）。
    pub fn set_emit(&self, emit: EmitFn) {
        *crate::util::write_ignore_poison(&self.emit) = emit;
    }

    /// 提交任务（jobs 行须已存在且为 queued）。
    pub fn submit(&self, req: JobRequest) -> Result<(), String> {
        // 取消标志随提交注册：排队期间取消立即生效，worker 启动后 runner 首步即见
        let flag = Arc::new(AtomicBool::new(false));
        let job_id = req.job_id();
        crate::util::lock_ignore_poison(&self.cancels).insert(job_id, flag.clone());
        if self.tx.send(QueueMsg::Job(req, flag)).is_err() {
            // 发送失败（通道已关闭）时回收标志，避免残留让 cancel 误报成功
            crate::util::lock_ignore_poison(&self.cancels).remove(&job_id);
            return Err("任务队列已关闭".to_string());
        }
        self.ensure_worker();
        Ok(())
    }

    /// 请求取消；job 不存在或已结束时返回错误。
    pub fn cancel(&self, job_id: i64) -> Result<(), String> {
        let flag = crate::util::lock_ignore_poison(&self.cancels)
            .get(&job_id)
            .cloned()
            .ok_or_else(|| "任务不存在或已结束".to_string())?;
        flag.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// 惰性启动单 worker（OnceLock 保证并发首次提交也只启动一个）。
    fn ensure_worker(&self) {
        self.worker.get_or_init(|| {
            let mut rx = crate::util::lock_ignore_poison(&self.rx)
                .take()
                .expect("worker 只启动一次");
            let worker_cancels = self.cancels.clone();
            let worker_db = self.db.clone();
            let runner = self.runner.clone();
            let worker_emit = self.emit.clone();
            let worker = async move {
                while let Some(msg) = rx.recv().await {
                    let QueueMsg::Job(req, flag) = msg;
                    let job_id = req.job_id();
                    let kind = req.kind();
                    let _ = repos::jobs::update(&worker_db.lock(), job_id, "running", 5, None);
                    // 包装 emit：记录最后进度，供取消路径保留
                    let last = Arc::new(std::sync::atomic::AtomicI64::new(5));
                    let last2 = last.clone();
                    let base_emit = crate::util::read_ignore_poison(&worker_emit).clone();
                    let emit: EmitFn = Arc::new(move |mut p: JobProgress| {
                        if p.progress > last2.load(Ordering::SeqCst) {
                            last2.store(p.progress, Ordering::SeqCst);
                        }
                        // speed/server_speed 事件经 error 字段传递单台失败原因（前端按
                        // message=node_id 定位）；其余 kind 的业务事件不携带 error
                        // （错误只出现在终态事件）
                        if p.kind != "speed" && p.kind != "server_speed" {
                            p.error = None;
                        }
                        base_emit(p);
                    });
                    let result = runner.run(req.clone(), flag.clone(), emit).await;
                    // 取消语义：标志置位且 runner 以错误返回才落 cancelled；
                    // runner 成功（工作已完成）即使标志已置也落 done，取消不覆盖成功。
                    let (status, error) = if flag.load(Ordering::SeqCst) && result.is_err() {
                        ("cancelled", Some("已取消".to_string()))
                    } else {
                        match result {
                            Ok(()) => ("done", None),
                            Err(e) => ("failed", Some(e)),
                        }
                    };
                    // done 落 100；取消/失败保持最后进度（UI 进度条语义）
                    let progress = if status == "done" {
                        100
                    } else {
                        last.load(Ordering::SeqCst)
                    };
                    let _ = repos::jobs::update(
                        &worker_db.lock(),
                        job_id,
                        status,
                        progress,
                        error.as_deref(),
                    );
                    // 失败/取消补发终态事件，UI 依赖 error 字段展示
                    if let Some(err_text) = error {
                        let base_emit = crate::util::read_ignore_poison(&worker_emit).clone();
                        base_emit(JobProgress {
                            job_id,
                            kind: kind.to_string(),
                            step: "结束".into(),
                            message: String::new(),
                            progress,
                            error: Some(err_text),
                        });
                    } else if last.load(Ordering::SeqCst) < 100 {
                        // 成功同样要补终态：runner 自己没走到 100 的 kind（拆链停在 90）
                        // 库里会落 done/100，前端却永远停在「进行中」——事件流是前端唯一
                        // 的完成依据，不能只靠库里的状态自说自话。已达 100 的不重复发。
                        let base_emit = crate::util::read_ignore_poison(&worker_emit).clone();
                        base_emit(JobProgress {
                            job_id,
                            kind: kind.to_string(),
                            step: "完成".into(),
                            message: String::new(),
                            progress: 100,
                            error: None,
                        });
                    }
                    crate::util::lock_ignore_poison(&worker_cancels).remove(&job_id);
                }
            };
            // **不能无条件 `tokio::spawn`**：`submit` 也来自主线程上的**同步**命令
            // （`start_upstream_job` 就是），而主线程跑的是 Tauri 的消息循环、没有
            // Tokio 运行时——在那里 spawn 会 panic「there is no reactor running」，
            // 且它发生在事件循环的 FFI 边界上，panic 无法 unwind → **整个进程 abort**。
            // 2026-09-21 实测：重启应用后点「上游部署」，应用当场消失，系统代理指针
            // 留在死端口上（用户全窗口断网）。模块头那句「submit 只来自 async 上下文」
            // 是错的——别再按那个假设写。
            // 在运行时里（async 命令、测试）用当前运行时；不在就交给 Tauri 的全局
            // 运行时（`get_or_init` 惰性建，任何线程可调），两者是同一个运行时。
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(worker);
            } else {
                tauri::async_runtime::spawn(worker);
            }
        });
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/jobs/mods.rs"
));
