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
    if matches!(req, Request::RecoverNet | Request::Shutdown) || owner.is_some_and(|o| !o.2) {
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
        match clean_network() {
            Ok(()) => state.owner = None,
            Err(e) => eprintln!("[yunyi-service] 清理已退出界面的网络会话失败: {e}"),
        }
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
