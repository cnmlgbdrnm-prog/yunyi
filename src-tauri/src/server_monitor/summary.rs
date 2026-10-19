//! 摘要装配：配置、服务器健康、活动告警、采集状态。
use super::config;
use super::store;
use super::{MonitorStatus, MonitorSummary, ServerHealth};
use crate::db::repos;
use crate::state::AppState;
use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;

pub fn expected_units(protocols: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if protocols
        .iter()
        .any(|p| matches!(p.as_str(), "vless" | "trojan" | "vmess"))
    {
        out.push("xray".to_string());
    }
    if protocols.iter().any(|p| p == "ss") {
        out.push("ssserver".to_string());
    }
    if protocols
        .iter()
        .any(|p| matches!(p.as_str(), "hysteria2" | "tuic"))
    {
        out.push("singbox".to_string());
    }
    out
}

pub fn build_summary(
    state: &Arc<AppState>,
    status: Option<&MonitorStatus>,
) -> Result<MonitorSummary, String> {
    let (config, config_error) = config::load(&state.settings);
    let (servers, active_alerts, mut latest, max_ts, wg_ids) = {
        let conn = state.db.lock();
        let servers = repos::servers::list(&conn).map_err(|e| e.to_string())?;
        let active = store::active_alerts(&conn)?;
        let mut latest = HashMap::new();
        for s in &servers {
            latest.insert(s.id, store::latest_sample(&conn, s.id)?);
        }
        let max_ts = store::max_ts(&conn)?;
        let wg_ids = store::wg_expected_server_ids(&conn)?;
        (servers, active, latest, max_ts, wg_ids)
    };

    let mut counts: HashMap<i64, (usize, usize)> = HashMap::new();
    for a in &active_alerts {
        let entry = counts.entry(a.server_id).or_insert((0, 0));
        entry.0 += 1;
        if a.acked_at.is_none() {
            entry.1 += 1;
        }
    }

    let now = Utc::now();
    let stale_after = Duration::seconds((config.interval_secs.saturating_mul(2) + 30) as i64);
    let local_errors = status
        .map(|s| {
            s.local_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();

    let mut health = Vec::with_capacity(servers.len());
    for s in servers {
        let (expected_services, deployment_known) =
            match crate::servers::deploy::deployed_protocols(state, s.id) {
                Ok(p) => (expected_units(&p), true),
                Err(_) => (Vec::new(), false),
            };
        let sample = latest.remove(&s.id).flatten();
        let (active_count, unacked_count) = counts.get(&s.id).copied().unwrap_or((0, 0));
        let stale = sample
            .as_ref()
            .and_then(|x| chrono::DateTime::parse_from_rfc3339(&x.ts).ok())
            .map(|t| now.signed_duration_since(t.with_timezone(&Utc)) > stale_after)
            .unwrap_or(true);
        health.push(ServerHealth {
            server_id: s.id,
            name: s.name,
            host: s.host,
            monitored: config::server_enabled(&config, s.id),
            online: sample.as_ref().map(|x| x.reachable),
            stale,
            last_sample_at: sample.as_ref().map(|x| x.ts.clone()),
            expected_services,
            deployment_known,
            wg_expected: wg_ids.contains(&s.id),
            sample,
            local_error: local_errors.get(&s.id).cloned(),
            active_alerts: active_count,
            unacked_alerts: unacked_count,
        });
    }

    Ok(MonitorSummary {
        config,
        config_error,
        last_round_at: max_ts,
        last_round_error: status.and_then(|s| {
            s.last_round_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }),
        collecting: status
            .map(|s| s.collecting.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false),
        servers: health,
        active_alerts,
    })
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/summary_tests.rs"
    ));
}
