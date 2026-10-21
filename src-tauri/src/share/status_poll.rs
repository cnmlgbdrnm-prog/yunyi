//! 分享状态的后端轮询：定时逐台读取服务器上的分享摘要与证书状态，写入
//! AppState 缓存，再经 `share://status` 推给前端。
//!
//! 前端切到分享页只读缓存，不再在挂载/切页时触发 `list_shares` 的全量 SSH
//! （实测一次 ~14 秒）。手动「刷新」仍会即时采集并写回缓存。
//!
//! 周期取 120s 而不是 60s：一轮要连所有服务器，比中转链路的两次状态查询重
//! 得多；用量/配额展示对 2 分钟的新鲜度足够。

use crate::share::model::ShareSnapshot;
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 前端事件名：payload 为整份 [`ShareSnapshot`]。
pub const EVENT: &str = "share://status";
pub const POLL_INTERVAL: Duration = Duration::from_secs(120);

static STARTED: AtomicBool = AtomicBool::new(false);

/// 当前缓存快照；尚未完成第一轮时返回默认值（`checked_at = None`）。
pub fn cached(state: &AppState) -> ShareSnapshot {
    state.share_snapshot.lock().unwrap().clone().unwrap_or_default()
}

/// 采集一轮并替换缓存。失败时保留上一轮快照（不把界面推成空列表）。
pub async fn poll_once(state: &Arc<AppState>) -> ShareSnapshot {
    match crate::share::install::refresh_share_snapshot(state).await {
        Ok(snap) => {
            *state.share_snapshot.lock().unwrap() = Some(snap.clone());
            snap
        }
        Err(_) => cached(state),
    }
}

/// 启动常驻轮询器（进程内只启动一次）。
pub fn spawn(app: AppHandle, state: Arc<AppState>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        // 避开启动高峰；前端此刻读到的是空快照（显示读取中），第一轮完成后推送。
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            let snapshot = poll_once(&state).await;
            let _ = app.emit(EVENT, &snapshot);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}
