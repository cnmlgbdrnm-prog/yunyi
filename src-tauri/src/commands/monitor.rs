//! 服务器监控命令层：参数校验 + 领域调用，Tauri 壳在 ipc.rs。
use super::{err, AppError, R};
use crate::server_monitor::{config, store, summary, MonitorMetric};
pub use crate::server_monitor::{MonitorAlert, MonitorConfig, MonitorPoint, MonitorSummary};
use crate::state::AppState;
use std::collections::HashSet;
use std::sync::Arc;

pub fn monitor_summary(state: Arc<AppState>) -> R<MonitorSummary> {
    let status = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.shared.status.clone());
    summary::build_summary(&state, status.as_deref()).map_err(AppError::Message)
}

pub async fn monitor_collect(state: Arc<AppState>) -> R<MonitorSummary> {
    crate::server_monitor::collect_now(&state)
        .await
        .map_err(AppError::Message)
}

pub fn monitor_history(
    state: Arc<AppState>,
    server_id: i64,
    metric: String,
    range_secs: i64,
) -> R<Vec<MonitorPoint>> {
    let metric = MonitorMetric::parse(&metric).map_err(AppError::Validation)?;
    if !matches!(range_secs, 3600 | 86400 | 604800) {
        return Err(AppError::Validation(
            "时间范围必须是 1 小时、24 小时或 7 天".into(),
        ));
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(range_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let bucket = (range_secs / 360).max(1);
    let conn = state.db.lock();
    store::history(&conn, server_id, metric, &cutoff, bucket).map_err(AppError::Message)
}

pub fn monitor_alerts(
    state: Arc<AppState>,
    include_resolved: bool,
    limit: u32,
) -> R<Vec<MonitorAlert>> {
    if !(1..=500).contains(&limit) {
        return Err(AppError::Validation("limit 必须在 1–500 之间".into()));
    }
    let conn = state.db.lock();
    store::alerts(&conn, include_resolved, limit as i64).map_err(AppError::Message)
}

pub fn monitor_config(state: Arc<AppState>) -> R<MonitorConfig> {
    Ok(config::load(&state.settings).0)
}

pub fn monitor_set_config(state: Arc<AppState>, mut cfg: MonitorConfig) -> R<MonitorConfig> {
    let ids: HashSet<i64> = {
        let conn = state.db.lock();
        crate::db::repos::servers::list(&conn)
            .map_err(err)?
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    cfg.servers.retain(|k, _| {
        k.parse::<i64>()
            .map(|id| ids.contains(&id))
            .unwrap_or(false)
    });
    let saved = config::save(&state.settings, &cfg).map_err(AppError::Validation)?;
    let shared = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.shared.clone());
    if let Some(shared) = shared {
        shared.wake.notify_one();
    }
    Ok(saved)
}

pub fn monitor_ack(state: Arc<AppState>, alert_id: i64) -> R<()> {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let conn = state.db.lock();
    let updated = store::ack(&conn, alert_id, &ts).map_err(AppError::Message)?;
    if !updated {
        return Err(AppError::NotFound("告警不存在或已恢复".into()));
    }
    Ok(())
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/commands_tests.rs"
    ));
}
