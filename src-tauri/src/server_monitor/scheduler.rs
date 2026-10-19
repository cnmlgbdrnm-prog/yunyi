//! 调度器：启动/停止、定时轮次、并发采集、全不可达守卫、事件发射。
use super::alerts::{self, AlertInput};
use super::config::{self, MonitorConfig};
use super::parse::parse_probe_output;
use super::probe::{MonitorProbe, MonitorTarget, ProbeOutcome, RealProbe};
use super::store;
use super::summary;
use super::{sanitize_text, MonitorSample, MonitorStatus, MonitorSummary};
use crate::state::AppState;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tauri::async_runtime::JoinHandle;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinSet;

pub type MonitorEmit = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

const FIRST_ROUND_DELAY: Duration = Duration::from_secs(3);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const RECENT_SAMPLES: usize = 10;
const PRUNE_INTERVAL: chrono::Duration = chrono::Duration::hours(1);

pub struct MonitorShared {
    pub collect: tokio::sync::Mutex<()>,
    /// 摘要/命令层共享的运行态（收集状态、上一轮错误、本地凭据错误）。
    pub status: Arc<MonitorStatus>,
    pub wake: Notify,
    pub shutdown: watch::Sender<bool>,
    pub emit: MonitorEmit,
    pub last_prune: std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>,
}

impl MonitorShared {
    pub fn new(emit: MonitorEmit) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            collect: tokio::sync::Mutex::new(()),
            status: Arc::new(MonitorStatus::default()),
            wake: Notify::new(),
            shutdown,
            emit,
            last_prune: std::sync::Mutex::new(None),
        }
    }
}

pub struct MonitorHandle {
    pub shared: Arc<MonitorShared>,
    pub join: JoinHandle<()>,
}

pub fn start(state: &Arc<AppState>, emit: MonitorEmit) -> Result<(), String> {
    let mut slot = state.monitor.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Err("监控调度器已在运行".into());
    }
    let shared = Arc::new(MonitorShared::new(emit));
    let weak: Weak<AppState> = Arc::downgrade(state);
    let loop_shared = shared.clone();
    let join = tauri::async_runtime::spawn(scheduler_loop(weak, loop_shared));
    *slot = Some(MonitorHandle { shared, join });
    Ok(())
}

pub async fn stop(state: &Arc<AppState>) {
    let handle = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(handle) = handle else { return };
    let _ = handle.shared.shutdown.send_replace(true);
    handle.shared.wake.notify_waiters();
    let abort = handle.join.inner().abort_handle();
    if tokio::time::timeout(STOP_TIMEOUT, handle.join)
        .await
        .is_err()
    {
        abort.abort();
    }
}

pub async fn collect_now(state: &Arc<AppState>) -> Result<MonitorSummary, String> {
    let shared = state
        .monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.shared.clone())
        .ok_or_else(|| "监控调度器尚未启动".to_string())?;
    let _permit = shared
        .collect
        .try_lock()
        .map_err(|_| "正在采集，请稍候".to_string())?;
    shared
        .status
        .local_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let probe: Arc<dyn MonitorProbe> = Arc::new(RealProbe {
        db: state.db.clone(),
        creds: state.creds.clone(),
    });
    let result = run_round(state, &shared, probe).await;
    emit_tick(state, &shared);
    result?;
    summary::build_summary(state, Some(&shared.status))
}

async fn scheduler_loop(state: Weak<AppState>, shared: Arc<MonitorShared>) {
    let mut shutdown = shared.shutdown.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(FIRST_ROUND_DELAY) => {}
        _ = shared.wake.notified() => {}
        _ = shutdown.changed() => return,
    }
    let mut last_round_end: Option<Instant> = None;
    let mut force_round = false;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let Some(state) = state.upgrade() else { break };
        let (cfg, _) = config::load(&state.settings);
        if !cfg.enabled {
            tokio::select! {
                _ = shared.wake.notified() => { force_round = true; continue; }
                _ = shutdown.changed() => break,
            }
        }
        if let Some(last) = last_round_end {
            if !force_round {
                let due = last + Duration::from_secs(cfg.interval_secs);
                let now = Instant::now();
                if due > now {
                    tokio::select! {
                        _ = tokio::time::sleep(due - now) => {}
                        _ = shared.wake.notified() => continue,
                        _ = shutdown.changed() => break,
                    }
                }
            }
        }
        let (cfg_now, _) = config::load(&state.settings);
        if !cfg_now.enabled {
            continue;
        }
        force_round = false;
        let probe: Arc<dyn MonitorProbe> = Arc::new(RealProbe {
            db: state.db.clone(),
            creds: state.creds.clone(),
        });
        if let Ok(_permit) = shared.collect.try_lock() {
            if let Err(e) = run_round(&state, &shared, probe).await {
                *shared
                    .status
                    .last_round_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(e);
            }
            emit_tick(&state, &shared);
        }
        last_round_end = Some(Instant::now());
    }
}

/// 调用方必须已持有 `shared.collect` 锁；本函数不重复加锁。
pub(crate) async fn run_round(
    state: &Arc<AppState>,
    shared: &Arc<MonitorShared>,
    probe: Arc<dyn MonitorProbe>,
) -> Result<(), String> {
    let result = run_round_inner(state, shared, probe).await;
    let message = match &result {
        Ok(warning) => warning.clone(),
        Err(e) => Some(e.clone()),
    };
    *shared
        .status
        .last_round_error
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = message;
    result.map(|_| ())
}

async fn run_round_inner(
    state: &Arc<AppState>,
    shared: &Arc<MonitorShared>,
    probe: Arc<dyn MonitorProbe>,
) -> Result<Option<String>, String> {
    let (cfg, _) = config::load(&state.settings);
    let targets = collect_targets(state, &cfg)?;
    if targets.is_empty() {
        return Ok(Some("没有启用监控的服务器".into()));
    }
    shared
        .status
        .local_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let _guard = CollectingGuard::new(shared);

    let sem = Arc::new(Semaphore::new(cfg.concurrency));
    let mut set = JoinSet::new();
    for target in targets.iter().cloned() {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| format!("采集并发控制失败: {e}"))?;
        let probe = probe.clone();
        set.spawn(async move {
            let _permit = permit;
            (target.server.id, probe.probe(&target).await)
        });
    }
    let mut outcomes: HashMap<i64, ProbeOutcome> = HashMap::new();
    while let Some(joined) = set.join_next().await {
        let (id, outcome) = joined.map_err(|e| format!("采集任务失败: {e}"))?;
        outcomes.insert(id, outcome);
    }

    // 本地凭据/DB 错误先收集再判守卫：守卫轮也要让 UI 看到“这台是本地错误”。
    let mut local_errors = HashMap::new();
    for (id, outcome) in &outcomes {
        if let ProbeOutcome::LocalError { error } = outcome {
            local_errors.insert(*id, sanitize_text(error));
        }
    }
    *shared
        .status
        .local_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = local_errors;

    let probed = outcomes
        .values()
        .filter(|o| !matches!(o, ProbeOutcome::LocalError { .. }))
        .count();
    let unreachable = outcomes
        .values()
        .filter(|o| matches!(o, ProbeOutcome::Unreachable { .. }))
        .count();
    if probed >= 2 && unreachable == probed {
        return Ok(Some("所有服务器均不可达，疑似本机网络异常".into()));
    }

    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut samples = Vec::new();
    for target in &targets {
        let Some(outcome) = outcomes.remove(&target.server.id) else {
            continue;
        };
        match outcome {
            ProbeOutcome::LocalError { .. } => {}
            ProbeOutcome::Unreachable { error } => samples.push(MonitorSample {
                server_id: target.server.id,
                ts: ts.clone(),
                reachable: false,
                error: Some(sanitize_text(&error)),
                ..Default::default()
            }),
            ProbeOutcome::Connected {
                connect_ms,
                stdout,
                command_error,
            } => {
                let parsed = parse_probe_output(&stdout);
                let error = command_error
                    .map(|e| sanitize_text(&e))
                    .or(parsed.warning.clone());
                samples.push(MonitorSample {
                    server_id: target.server.id,
                    ts: ts.clone(),
                    reachable: true,
                    connect_ms: Some(connect_ms),
                    uptime_secs: parsed.uptime_secs,
                    load1: parsed.load1,
                    load5: parsed.load5,
                    load15: parsed.load15,
                    cpu_pct: parsed.cpu_pct,
                    cpu_cores: parsed.cpu_cores,
                    mem_used_kb: parsed.mem_used_kb,
                    mem_total_kb: parsed.mem_total_kb,
                    disk_used_kb: parsed.disk_used_kb,
                    disk_total_kb: parsed.disk_total_kb,
                    bbr_algo: parsed.bbr_algo,
                    services: parsed.services,
                    wg_interfaces: parsed.wg_interfaces,
                    error,
                });
            }
        }
    }
    let target_map: HashMap<i64, &MonitorTarget> =
        targets.iter().map(|t| (t.server.id, t)).collect();
    let mut conn = state.db.lock();
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for sample in &samples {
        let target = target_map
            .get(&sample.server_id)
            .ok_or_else(|| format!("缺少服务器 {} 的采集目标", sample.server_id))?;
        let thresholds = config::effective_thresholds(&cfg, sample.server_id);
        let recent = store::recent_samples(&tx, sample.server_id, RECENT_SAMPLES)?;
        let last_reachable = store::last_reachable_sample(&tx, sample.server_id)?;
        let wg_baseline =
            store::last_nonempty_wg_sample(&tx, sample.server_id)?.and_then(|s| s.wg_interfaces);
        let active = store::active_alerts_for_server(&tx, sample.server_id)?;
        let transitions = alerts::evaluate(&AlertInput {
            server_name: &target.server.name,
            current: sample,
            recent: &recent,
            last_reachable: last_reachable.as_ref(),
            wg_baseline: wg_baseline.as_deref(),
            active: &active,
            thresholds: &thresholds,
            expected_services: &target.expected_services,
            deployment_known: target.deployment_known,
            wg_expected: target.wg_expected,
            all_unreachable_guard: false,
        });
        store::insert_sample(&tx, sample)?;
        store::apply_transitions(&tx, sample.server_id, &sample.ts, &transitions)?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    // 必须先放掉 DB 锁：maybe_prune 会再次 lock，std::sync::Mutex 不可重入。
    drop(conn);

    maybe_prune(&state, shared)?;
    Ok(None)
}

fn collect_targets(
    state: &Arc<AppState>,
    cfg: &MonitorConfig,
) -> Result<Vec<MonitorTarget>, String> {
    let servers = {
        let conn = state.db.lock();
        crate::db::repos::servers::list(&conn).map_err(|e| e.to_string())?
    };
    let wg_ids = {
        let conn = state.db.lock();
        store::wg_expected_server_ids(&conn)?
    };
    let mut out = Vec::new();
    for server in servers {
        if !config::server_enabled(cfg, server.id) {
            continue;
        }
        let (expected_services, deployment_known) =
            match crate::servers::deploy::deployed_protocols(state, server.id) {
                Ok(p) => (summary::expected_units(&p), true),
                Err(_) => (Vec::new(), false),
            };
        out.push(MonitorTarget {
            wg_expected: wg_ids.contains(&server.id),
            deployment_known,
            expected_services,
            server,
        });
    }
    Ok(out)
}

fn maybe_prune(state: &Arc<AppState>, shared: &Arc<MonitorShared>) -> Result<(), String> {
    let now = chrono::Utc::now();
    let mut last = shared.last_prune.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| now - t < PRUNE_INTERVAL) {
        return Ok(());
    }
    let sample_cutoff = (now - chrono::Duration::days(super::SAMPLE_RETENTION_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let alert_cutoff = (now - chrono::Duration::days(super::ALERT_RETENTION_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    store::prune(&state.db.lock(), &sample_cutoff, &alert_cutoff)?;
    *last = Some(now);
    Ok(())
}

fn emit_tick(state: &Arc<AppState>, shared: &Arc<MonitorShared>) {
    match summary::build_summary(state, Some(&shared.status)) {
        Ok(s) => {
            if let Ok(v) = serde_json::to_value(s) {
                (shared.emit)("monitor://tick", v);
            }
        }
        Err(e) => eprintln!("[vpn-ctl] 生成监控摘要失败: {e}"),
    }
}

struct CollectingGuard<'a> {
    shared: &'a MonitorShared,
}

impl<'a> CollectingGuard<'a> {
    fn new(shared: &'a MonitorShared) -> Self {
        shared.status.collecting.store(true, Ordering::SeqCst);
        (shared.emit)("monitor://collecting", json!({ "collecting": true }));
        Self { shared }
    }
}

impl Drop for CollectingGuard<'_> {
    fn drop(&mut self) {
        self.shared.status.collecting.store(false, Ordering::SeqCst);
        (self.shared.emit)("monitor://collecting", json!({ "collecting": false }));
    }
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/scheduler_tests.rs"
    ));
}
