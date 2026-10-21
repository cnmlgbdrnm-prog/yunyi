//! 中转链路状态的后端轮询：定时查一次已建成/已接管的链路，写入 AppState 缓存，
//! 再经 `relay://status` 推给前端。前端切到中转页只读缓存，不在挂载/切页时发 SSH。
//!
//! 扩展端口行（`forward_mode='extended'`）与锚点共用同一条隧道，**不参与轮询**：
//! 每个端口查一遍等于把同一组握手/字节数重复查 N 次。
//!
//! 轮询常驻而不是页面驱动：切页时不该重查，而页面关着时后端仍保有最近状态，
//! 重新打开即是熟的。单轮内的查询串行执行（`STATUS_SEM` 另有并发保护）。

use crate::domain::LinkStatus;
use crate::relay::refresh_link_status;
use crate::state::AppState;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 前端事件名：payload 为 `HashMap<rule_id, LinkStatus>`。
pub const EVENT: &str = "relay://status";
/// 轮询周期。状态是「最近握手/累计流量」，不需要秒级刷新；60s 足以发现断链，
/// 也不会让常驻后端每个周期都去开 SSH。
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

static STARTED: AtomicBool = AtomicBool::new(false);

fn error_status(error: &str) -> LinkStatus {
    LinkStatus {
        ok: false,
        relay_handshake_secs_ago: None,
        landing_handshake_secs_ago: None,
        relay_rx_bytes: 0,
        relay_tx_bytes: 0,
        landing_rx_bytes: 0,
        landing_tx_bytes: 0,
        checked_at: chrono::Utc::now().to_rfc3339(),
        error: Some(error.to_string()),
    }
}

/// 查一轮可用链路并替换缓存。返回本次完整快照（无可用链路时为空表）。
///
/// 单条查询失败不拖垮整轮：该行记为 `ok=false + error`，其余照常。
pub async fn poll_once(state: &Arc<AppState>) -> HashMap<i64, LinkStatus> {
    let rules = {
        let conn = state.db.lock();
        crate::db::repos::relay::list(&conn).unwrap_or_default()
    };
    let mut out = HashMap::new();
    for rule in rules.iter().filter(|r| {
        matches!(r.status.as_str(), "built" | "imported")
            && r.forward_mode != crate::relay::extend::EXTENDED_MODE
    }) {
        let status = match refresh_link_status(state, rule.id).await {
            Ok(st) => st,
            Err(e) => error_status(&e.to_string()),
        };
        out.insert(rule.id, status);
    }
    // 整份替换而不是逐条插入：已删除/已拆掉的链路随之从缓存里消失。
    *state.relay_statuses.lock().unwrap() = out.clone();
    out
}

/// 启动常驻轮询器（进程内只启动一次）。
pub fn spawn(app: AppHandle, state: Arc<AppState>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        // 避开启动高峰（配置重建、服务拉起），再来第一轮；前端此刻读到的是空表，
        // 会显示「状态未知」，下一轮事件补上——这是后端定时，不是页面挂载触发。
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            let snapshot = poll_once(&state).await;
            let _ = app.emit(EVENT, &snapshot);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}
