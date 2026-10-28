//! 服务器测速域服务：阶段1（单次 SSH 端响应 + SSH curl 带宽）批量编排与阶段2
//! （服务器间 iperf3 吞吐 + 安装，iperf）。编排职责：按并发窗口分块并发测速
//! （JoinSet）、每台写库 + 事件（step='server'）、终态事件（step='完成'）、
//! 块间取消检查；单台失败不判任务失败，仅取消/前置失败才 Err。

use crate::db::repos;
use crate::domain::{JobProgress, Server, ServerSpeedReq};
use crate::jobs::EmitFn;
use crate::ssh::SshSession;
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub mod iperf;
pub mod phase1;

/// 服务器页只做一次端响应检测；精确链路延迟由节点测速提供。
pub const LATENCY_SAMPLES: u8 = 1;
/// 阶段1 带宽下载整体超时（curl --max-time 30 + 余量）。
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(45);
/// SSH 连接超时（连接+认证两阶段）。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 会话连接器抽象：生产走凭据解密 + 真实连接；测试注入已连接会话
/// （FakeSshServer 响应在虚拟时钟暂停下被饿死，测试必须真实时间驱动）。
#[async_trait::async_trait]
pub(crate) trait SshConnect: Send + Sync {
    async fn connect(&self, s: &Server) -> Result<SshSession, String>;
}

/// 生产连接器：凭据解密复用 servers::load_auth_parts（凭据不进日志/错误文本）。
/// 持有 db + 凭据服务而非整个 AppState——AppState 的这两个字段本就是 Arc，这样节点测速
/// （只拿得到 `&AppState`）也能构造同一个连接器，不必再为此传 Arc&lt;AppState&gt;。
pub(crate) struct RealConnect {
    pub db: Arc<crate::db::Db>,
    pub creds: Arc<crate::crypto::CredentialService>,
}

#[async_trait::async_trait]
impl SshConnect for RealConnect {
    async fn connect(&self, s: &Server) -> Result<SshSession, String> {
        let (auth, _) = crate::servers::load_auth_parts(&self.db, &self.creds, s.id)
            .map_err(|e| e.to_string())?;
        SshSession::connect(
            crate::domain::effective_host(s),
            s.port as u16,
            &s.user,
            &auth,
            CONNECT_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

/// 阶段1 批量测速入口（job 队列 runner 调用）：生产装配 RealConnect + 45s 带宽超时。
pub async fn run_server_speed_job(
    state: Arc<AppState>,
    req: ServerSpeedReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
) -> Result<(), String> {
    let connect: Arc<dyn SshConnect> = Arc::new(RealConnect {
        db: state.db.clone(),
        creds: state.creds.clone(),
    });
    run_server_speed_job_with(state, req, cancel, emit, connect, DOWNLOAD_TIMEOUT).await
}

/// 注入连接器与带宽超时的执行器本体：不变量测试用 FakeConnect + 短超时驱动。
/// 返回 Err 仅发生在取消/前置失败；单台失败不中断整体。
pub(crate) async fn run_server_speed_job_with(
    state: Arc<AppState>,
    req: ServerSpeedReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
    connect: Arc<dyn SshConnect>,
    curl_timeout: Duration,
) -> Result<(), String> {
    // 服务器装载：按 server_ids 顺序装载；缺失以「服务器不存在」结果占位
    let mut servers: Vec<(i64, Option<Server>)> = Vec::with_capacity(req.server_ids.len());
    {
        let conn = state.db.lock();
        for id in &req.server_ids {
            let s = repos::servers::get(&conn, *id).map_err(|e| format!("读取服务器失败: {e}"))?;
            servers.push((*id, s));
        }
    }
    let concurrency = req.concurrency.max(1) as usize;
    let mut done: i64 = 0;
    for chunk in servers.chunks(concurrency) {
        // 块间取消检查：置位后未开始的块全部跳过
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".to_string());
        }
        let mut set = tokio::task::JoinSet::new();
        for (id, srv) in chunk {
            // spawn 前拷贝为拥有值：async move 闭包不得捕获 chunk 引用（'static 需求）
            let id = *id;
            let srv = srv.clone();
            let state = state.clone();
            let connect = connect.clone();
            let cancel = cancel.clone();
            set.spawn(
                async move { test_server(state, connect, curl_timeout, id, srv, cancel).await },
            );
        }
        // 块内 select 轮询取消：在途 SSH 随 JoinSet drop 中止（future drop 关通道），
        // 否则挂起的 curl 会阻塞 join_next 使取消永远无法生效
        loop {
            tokio::select! {
                res = set.join_next() => {
                    match res {
                        Some(Ok((id, error))) => {
                            done += 1;
                            emit(JobProgress {
                                job_id: req.job_id,
                                kind: "server_speed".into(),
                                step: "server".into(),
                                message: id.to_string(),
                                progress: done,
                                error,
                            });
                        }
                        Some(Err(e)) => return Err(format!("测速任务异常: {e}")),
                        None => break,
                    }
                }
                _ = wait_cancel(cancel.clone()) => return Err("已取消".to_string()),
            }
        }
    }
    // 终态事件：全部完成（取消返回 Err 不发射终态，由 worker 补发 '结束'）
    emit(JobProgress {
        job_id: req.job_id,
        kind: "server_speed".into(),
        step: "完成".into(),
        message: String::new(),
        progress: 100,
        error: None,
    });
    Ok(())
}

/// 单台测速：连接 → 延迟 → 带宽（各自独立，延迟失败不阻断带宽）→ 写库。
/// 返回 (server_id, 单台综合错误或 None)；缺失/连接失败直接占位错误。
async fn test_server(
    state: Arc<AppState>,
    connect: Arc<dyn SshConnect>,
    curl_timeout: Duration,
    server_id: i64,
    srv: Option<Server>,
    _cancel: Arc<AtomicBool>,
) -> (i64, Option<String>) {
    let Some(srv) = srv else {
        return (server_id, Some("服务器不存在".to_string()));
    };
    let mut sess = match connect.connect(&srv).await {
        Ok(s) => s,
        Err(e) => {
            let _ = repos::servers::update_speed(&state.db.lock(), server_id, None, None, Some(&e));
            return (server_id, Some(e));
        }
    };
    // 延迟与带宽独立执行：任一成功即写成功值，error 记另一项失败原因
    let mut latency: Option<i64> = None;
    let mut bandwidth: Option<i64> = None;
    let mut errors: Vec<String> = Vec::new();
    match phase1::ssh_response_latency_ms(
        crate::domain::effective_host(&srv),
        srv.port as u16,
        LATENCY_SAMPLES,
    )
    .await
    {
        Ok(ms) => latency = Some(ms),
        Err(e) => errors.push(e),
    }
    // 下载目标与节点测速同源：一律经 download_url 拼，URL 只有这一个来源
    let speed_url =
        crate::speed::download_url(crate::speed::SPEED_TEST_ENDPOINT, crate::speed::SIZE_LIMIT);
    match phase1::curl_bandwidth(&mut sess, &speed_url, curl_timeout).await {
        Ok(b) => bandwidth = Some(b.round() as i64),
        Err(e) => errors.push(e),
    }
    let mut error = if errors.is_empty() {
        None
    } else {
        Some(errors.join("；"))
    };
    if let Err(e) = repos::servers::update_speed(
        &state.db.lock(),
        server_id,
        latency,
        bandwidth,
        error.as_deref(),
    ) {
        // 写库失败拼接保留原诊断
        error = Some(match error.take() {
            Some(orig) => format!("{orig}；写库失败: {e}"),
            None => format!("写库失败: {e}"),
        });
    }
    (server_id, error)
}

/// 轮询取消标志（100ms 间隔）；select 与任务完成竞争，置位即中止在途任务。
pub(crate) async fn wait_cancel(cancel: Arc<AtomicBool>) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/server_speed/mods.rs"
));
