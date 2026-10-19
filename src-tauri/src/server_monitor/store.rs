//! 监控 SQLite 读写：样本、告警、历史降采样与保留期清理。
//! 所有函数返回 Result<_, String>，命令层用 AppError::Message 统一收口。
use super::{MonitorMetric, MonitorPoint, MonitorSample};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;

const SAMPLE_COLS: &str = "server_id,ts,reachable,connect_ms,uptime_secs,load1,load5,load15,\
cpu_pct,cpu_cores,mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,bbr_algo,services_json,wg_json,error";

fn row_to_sample(r: &rusqlite::Row<'_>) -> rusqlite::Result<MonitorSample> {
    let services_json: Option<String> = r.get(15)?;
    let wg_json: Option<String> = r.get(16)?;
    Ok(MonitorSample {
        server_id: r.get(0)?,
        ts: r.get(1)?,
        reachable: r.get::<_, i64>(2)? != 0,
        connect_ms: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
        uptime_secs: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
        load1: r.get(5)?,
        load5: r.get(6)?,
        load15: r.get(7)?,
        cpu_pct: r.get(8)?,
        cpu_cores: r.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        mem_used_kb: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        mem_total_kb: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        disk_used_kb: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        disk_total_kb: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        bbr_algo: r.get(14)?,
        services: services_json.and_then(|s| serde_json::from_str(&s).ok()),
        wg_interfaces: wg_json.and_then(|s| serde_json::from_str(&s).ok()),
        error: r.get(17)?,
    })
}

pub fn insert_sample(conn: &Connection, s: &MonitorSample) -> Result<(), String> {
    let services_json = match &s.services {
        Some(m) => Some(serde_json::to_string(m).map_err(|e| e.to_string())?),
        None => None,
    };
    let wg_json = match &s.wg_interfaces {
        Some(v) => Some(serde_json::to_string(v).map_err(|e| e.to_string())?),
        None => None,
    };
    conn.execute(
        "INSERT INTO server_metric_samples
         (server_id,ts,reachable,connect_ms,uptime_secs,load1,load5,load15,cpu_pct,cpu_cores,
          mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,bbr_algo,services_json,wg_json,error)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![
            s.server_id,
            s.ts,
            s.reachable as i64,
            s.connect_ms.map(|v| v as i64),
            s.uptime_secs.map(|v| v as i64),
            s.load1,
            s.load5,
            s.load15,
            s.cpu_pct,
            s.cpu_cores.map(|v| v as i64),
            s.mem_used_kb.map(|v| v as i64),
            s.mem_total_kb.map(|v| v as i64),
            s.disk_used_kb.map(|v| v as i64),
            s.disk_total_kb.map(|v| v as i64),
            s.bbr_algo,
            services_json,
            wg_json,
            s.error,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn latest_sample(conn: &Connection, server_id: i64) -> Result<Option<MonitorSample>, String> {
    conn.query_row(
        &format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 ORDER BY ts DESC LIMIT 1"
        ),
        params![server_id],
        row_to_sample,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn recent_samples(
    conn: &Connection,
    server_id: i64,
    limit: usize,
) -> Result<Vec<MonitorSample>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 ORDER BY ts DESC LIMIT ?2"
        ))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id, limit as i64], row_to_sample)
        .map_err(|e| e.to_string())?;
    let mut out: Vec<MonitorSample> = rows
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())?;
    out.reverse();
    Ok(out)
}

pub fn last_reachable_sample(
    conn: &Connection,
    server_id: i64,
) -> Result<Option<MonitorSample>, String> {
    conn.query_row(
        &format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 AND reachable=1 ORDER BY ts DESC LIMIT 1"
        ),
        params![server_id],
        row_to_sample,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn last_nonempty_wg_sample(
    conn: &Connection,
    server_id: i64,
) -> Result<Option<MonitorSample>, String> {
    conn.query_row(
        &format!(
            "SELECT {SAMPLE_COLS} FROM server_metric_samples
             WHERE server_id=?1 AND reachable=1 AND wg_json IS NOT NULL AND wg_json != '[]'
             ORDER BY ts DESC LIMIT 1"
        ),
        params![server_id],
        row_to_sample,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn max_ts(conn: &Connection) -> Result<Option<String>, String> {
    conn.query_row("SELECT MAX(ts) FROM server_metric_samples", [], |r| {
        r.get::<_, Option<String>>(0)
    })
    .map_err(|e| e.to_string())
}

pub fn history(
    conn: &Connection,
    server_id: i64,
    metric: MonitorMetric,
    cutoff: &str,
    bucket_secs: i64,
) -> Result<Vec<MonitorPoint>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT ts,cpu_pct,mem_used_kb,mem_total_kb,disk_used_kb,disk_total_kb,load1,connect_ms
             FROM server_metric_samples
             WHERE server_id=?1 AND ts>=?2 AND reachable=1
             ORDER BY ts",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![server_id, cutoff], |r| {
            let ts: String = r.get(0)?;
            let value = match metric {
                MonitorMetric::CpuPct => r.get::<_, Option<f64>>(1)?,
                MonitorMetric::MemPct => {
                    let used: Option<i64> = r.get(2)?;
                    let total: Option<i64> = r.get(3)?;
                    match (used, total) {
                        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                        _ => None,
                    }
                }
                MonitorMetric::DiskPct => {
                    let used: Option<i64> = r.get(4)?;
                    let total: Option<i64> = r.get(5)?;
                    match (used, total) {
                        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                        _ => None,
                    }
                }
                MonitorMetric::Load1 => r.get::<_, Option<f64>>(6)?,
                MonitorMetric::ConnectMs => r
                    .get::<_, Option<i64>>(7)?
                    .map(|v| v as f64),
            };
            Ok((ts, value))
        })
        .map_err(|e| e.to_string())?;

    let bucket = bucket_secs.max(1);
    let mut buckets: std::collections::BTreeMap<i64, (String, f64, usize)> =
        std::collections::BTreeMap::new();
    for row in rows {
        let (ts, value) = row.map_err(|e| e.to_string())?;
        let Some(value) = value else { continue };
        let epoch = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let entry = buckets
            .entry(epoch / bucket)
            .or_insert_with(|| (ts.clone(), 0.0, 0));
        entry.1 += value;
        entry.2 += 1;
    }
    Ok(buckets
        .into_values()
        .map(|(ts, sum, n)| MonitorPoint {
            ts,
            value: sum / n as f64,
        })
        .collect())
}

pub fn prune(conn: &Connection, sample_cutoff: &str, alert_cutoff: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM server_metric_samples WHERE ts < ?1",
        params![sample_cutoff],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM server_alerts WHERE resolved_at IS NOT NULL AND resolved_at < ?1",
        params![alert_cutoff],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn wg_expected_server_ids(conn: &Connection) -> Result<HashSet<i64>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT relay_server_id, landing_server_id FROM relay_rules
             WHERE status IN ('built','imported')",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut set = HashSet::new();
    for row in rows {
        let (a, b) = row.map_err(|e| e.to_string())?;
        set.insert(a);
        set.insert(b);
    }
    Ok(set)
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/store_tests.rs"
    ));
}
