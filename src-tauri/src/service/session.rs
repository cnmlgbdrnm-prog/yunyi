//! 服务会话所有权：调用方 PID 取自经过身份校验的管道，并持有进程句柄防 PID 复用。
//! 恢复、启动、停止、失主清理串行化，旧界面仍活着时拒绝另一个界面抢占。

use super::process::ProcessRef;
use super::protocol::{Request, Response};
use std::sync::Mutex;

#[derive(Default)]
struct Session {
    owner: Option<ProcessRef>,
    shutting_down: bool,
}
static SESSION: Mutex<Session> = Mutex::new(Session {
    owner: None,
    shutting_down: false,
});

fn failure(message: impl ToString) -> Response {
    Response::Failed {
        message: message.to_string(),
    }
}

pub fn cleanup_legacy_orphans() -> Result<usize, String> {
    crate::net::orphan::cleanup(
        &[(
            super::paths::bin_dir().join("mihomo.exe"),
            super::paths::net_dir(),
        )],
        &[],
    )
}

fn clean_network() -> Result<(), String> {
    match super::mihomo::handle(&Request::StopNet) {
        Response::Stopped { .. } => {}
        Response::Failed { message } => return Err(message),
        _ => return Err("停止旧网络会话时收到未知响应".into()),
    }
    cleanup_legacy_orphans()?;
    super::mihomo::ensure_tun_gone()?;
    match super::mihomo::handle(&Request::UnblockDirect) {
        Response::Unblocked { .. } | Response::BlockDenied { .. } => Ok(()),
        Response::Failed { message } => Err(message),
        _ => Err("清理旧会话防火墙时收到未知响应".into()),
    }
}

/// 界面退出/关闭网络：保留 mihomo 但切直连，撤阻断，不碰进程。
///
/// 不能复用 [`clean_network`]：它第一步就是 `StopNet`。端口一停，环境变量里
/// 记着本机代理的已运行程序会立刻 `ECONNREFUSED`——那正是本函数要避免的故障。
///
/// 两件事**都尝试、合并报错**：切直连失败时防火墙规则仍然必须撤掉，否则
/// 新启动的直连程序会被残留规则挡住，故障面反而更大。失败时 mihomo 仍在原
/// 模式（通常是 VPN）继续服务已运行的程序，不会断网。
fn detach_network() -> Result<(), String> {
    let mut errors = Vec::new();

    match super::mihomo::handle(&Request::Detach) {
        Response::Detached => {}
        Response::Failed { message } => errors.push(message),
        _ => errors.push("保留直连代理时收到未知响应".into()),
    }
    // 刻意不调用 cleanup_legacy_orphans：它的目标是 bin_dir 下同参数的 mihomo，
    // 会把我们刚决定保留的这只一起杀掉。旧版孤儿等下次 StartNet 的 stop 再收。
    match super::mihomo::handle(&Request::UnblockDirect) {
        Response::Unblocked { .. } | Response::BlockDenied { .. } => {}
        Response::Failed { message } => errors.push(message),
        _ => errors.push("保留直连代理时清理防火墙收到未知响应".into()),
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

/// 可注入的归属判定：出生时间用于区分 PID 被复用的不同进程。
pub(super) fn owner_conflicts(owner: Option<(u32, u64, bool)>, caller: (u32, u64)) -> bool {
    owner.is_some_and(|(pid, born, alive)| alive && (pid, born) != caller)
}

pub fn handle(req: &Request, caller: ProcessRef) -> Response {
    let mut state = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    handle_with(
        &mut state,
        req,
        caller,
        clean_network,
        super::mihomo::handle,
    )
}

fn handle_with(
    state: &mut Session,
    req: &Request,
    caller: ProcessRef,
    cleanup: impl FnOnce() -> Result<(), String>,
    apply: impl FnOnce(&Request) -> Response,
) -> Response {
    let caller_pid = caller.pid;
    if state.shutting_down {
        return failure("网络服务正在退出，请稍后重试");
    }
    if matches!(req, Request::Status | Request::BlockStatus) {
        return apply(req);
    }
    let caller_birth = match caller.created() {
        Ok(t) => t,
        Err(e) => return failure(e),
    };
    let owner = match state
        .owner
        .as_ref()
        .map(|p| Ok::<_, std::io::Error>((p.pid, p.created()?, p.alive()?)))
        .transpose()
    {
        Ok(v) => v,
        Err(e) => return failure(format!("无法核验旧会话所属进程: {e}")),
    };
    if owner_conflicts(owner, (caller_pid, caller_birth)) {
        return failure("网络会话仍由另一个云驿窗口持有，请先关闭原窗口");
    }
    // 退出/关闭网络：只切直连并释放归属，绝不走 cleanup 的 StopNet。
    // 归属已在上面的冲突检查里核验过；即使旧界面刚死，也由这一条完成收尾。
    if matches!(req, Request::Detach) {
        let response = apply(req);
        state.owner = None;
        return response;
    }
    let stale_owner = owner.is_some_and(|o| !o.2);
    // RecoverNet 只在**确实有旧归属**时清理。归属为空说明上一轮退出已经留下
    // 一个直连 mihomo（见 `detach_network`），这里再 clean 一次会把它杀掉，
    // 已运行的程序又断网——而这次启动未必会马上开网络。
    if matches!(req, Request::Shutdown)
        || stale_owner
        || (matches!(req, Request::RecoverNet) && state.owner.is_some())
    {
        if let Err(e) = cleanup() {
            return failure(e);
        }
        state.owner = None;
    }
    match req {
        Request::RecoverNet => {
            state.owner = Some(caller);
            Response::Recovered
        }
        Request::Shutdown => {
            state.shutting_down = true;
            Response::ShuttingDown
        }
        Request::StartNet { .. } => {
            state.owner = Some(caller);
            apply(req)
        }
        _ => apply(req),
    }
}

/// 定时检查的是进程句柄，不靠 UI 心跳；界面卡顿不会被当成退出。
pub fn reap_owner() {
    let Ok(mut state) = SESSION.try_lock() else {
        return;
    };
    if state
        .owner
        .as_ref()
        .is_some_and(|p| matches!(p.alive(), Ok(false)))
    {
        // 界面崩溃/被强杀时同样保留端口：切直连后，那些环境变量里记着本机代理
        // 的程序不会因为主人没了而集体断网。
        if let Err(e) = detach_network() {
            eprintln!("[yunyi-service] 保留已退出界面的直连代理失败: {e}");
        }
        // 无论切直连是否成功都要释放归属：否则新界面会一直被告知「另一个窗口
        // 持有会话」。失败时 mihomo 仍在原模式（通常是 VPN）继续服务，不会断网。
        state.owner = None;
    }
}

pub fn shutting_down() -> bool {
    SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .shutting_down
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/service/session/mods.rs"
));
