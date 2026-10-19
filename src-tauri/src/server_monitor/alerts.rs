//! 告警状态机：纯函数，不碰 DB/SSH。输入样本序列与活动告警，输出 Open/Update/Resolve。
use super::{sanitize_text, MonitorAlert, MonitorSample, Thresholds, TRANSITION_SAMPLES};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Offline,
    Cpu,
    Memory,
    Disk,
    Load,
    Service,
    Wireguard,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Load => "load",
            Self::Service => "service",
            Self::Wireguard => "wireguard",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlertTransition {
    Open {
        kind: AlertKind,
        severity: Severity,
        message: String,
        value: Option<String>,
    },
    Update {
        id: i64,
        severity: Severity,
        message: String,
        value: Option<String>,
        clear_ack: bool,
    },
    Resolve {
        id: i64,
    },
}

pub struct AlertInput<'a> {
    pub server_name: &'a str,
    pub current: &'a MonitorSample,
    pub recent: &'a [MonitorSample],
    pub last_reachable: Option<&'a MonitorSample>,
    pub wg_baseline: Option<&'a [String]>,
    pub active: &'a [MonitorAlert],
    pub thresholds: &'a Thresholds,
    pub expected_services: &'a [String],
    pub deployment_known: bool,
    pub wg_expected: bool,
    pub all_unreachable_guard: bool,
}

pub fn evaluate(input: &AlertInput<'_>) -> Vec<AlertTransition> {
    let mut out = Vec::new();
    if !input.all_unreachable_guard {
        eval_offline(input, &mut out);
    }
    eval_cpu(input, &mut out);
    eval_memory(input, &mut out);
    eval_disk(input, &mut out);
    eval_load(input, &mut out);
    eval_service(input, &mut out);
    eval_wireguard(input, &mut out);
    out
}

#[derive(Clone, Copy)]
enum Compare {
    /// 违反条件 `v >= t`；恢复 `v < t`（CPU、内存）。
    Ge,
    /// 违反条件 `v > t`；恢复 `v <= t`（负载）。
    Gt,
}

impl Compare {
    fn breach(self, v: f64, t: f64) -> bool {
        match self {
            Self::Ge => v >= t,
            Self::Gt => v > t,
        }
    }

    fn clear(self, v: f64, t: f64) -> bool {
        match self {
            Self::Ge => v < t,
            Self::Gt => v <= t,
        }
    }
}

fn active_for<'a>(input: &'a AlertInput<'_>, kind: AlertKind) -> Option<&'a MonitorAlert> {
    input
        .active
        .iter()
        .find(|a| a.kind == kind.as_str() && a.resolved_at.is_none())
}

fn eval_offline(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let kind = AlertKind::Offline;
    let active = active_for(input, kind);
    if !input.current.reachable {
        let streak = count_back(true, input.recent, |s| Some(!s.reachable));
        let value = input.current.error.as_deref().map(sanitize_text);
        let message = format!(
            "SSH 连续 {} 轮不可达：{}",
            streak,
            value.as_deref().unwrap_or("连接失败")
        );
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Critical,
                message,
                value,
                clear_ack: false,
            }),
            None if streak >= input.thresholds.offline_samples => out.push(AlertTransition::Open {
                kind,
                severity: Severity::Critical,
                message,
                value,
            }),
            None => {}
        }
    } else if let Some(a) = active {
        let streak = count_back(true, input.recent, |s| Some(s.reachable));
        if streak >= input.thresholds.clear_samples {
            out.push(AlertTransition::Resolve { id: a.id });
        }
    }
}

fn eval_numeric(
    input: &AlertInput<'_>,
    kind: AlertKind,
    value: Option<f64>,
    threshold: f64,
    compare: Compare,
    out: &mut Vec<AlertTransition>,
) {
    let Some(v) = value else { return };
    let active = active_for(input, kind);
    let label = match kind {
        AlertKind::Cpu => "CPU",
        AlertKind::Memory => "内存",
        AlertKind::Load => "负载",
        _ => "指标",
    };
    let value_text = |v: f64| -> String {
        if kind == AlertKind::Load {
            format!("{v:.2}")
        } else {
            format!("{v:.1}%")
        }
    };

    if compare.breach(v, threshold) {
        let streak = count_back(true, input.recent, |s| {
            metric_of(s, kind).map(|x| compare.breach(x, threshold))
        });
        let message = if kind == AlertKind::Load {
            format!(
                "1 分钟负载 {v:.2} 超过 {} 核 × {:.1}（阈值 {threshold:.1}），连续 {streak} 轮",
                input.current.cpu_cores.unwrap_or(0),
                input.thresholds.load_per_core,
            )
        } else {
            format!("{label}连续 {streak} 轮高于 {threshold:.0}%（最近 {v:.1}%）")
        };
        let value_text = Some(value_text(v));
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Warning,
                message,
                value: value_text,
                clear_ack: false,
            }),
            None if streak >= input.thresholds.breach_samples => out.push(AlertTransition::Open {
                kind,
                severity: Severity::Warning,
                message,
                value: value_text,
            }),
            None => {}
        }
    } else if let Some(a) = active {
        let clear = count_back(true, input.recent, |s| {
            metric_of(s, kind).map(|x| compare.clear(x, threshold))
        });
        if clear >= input.thresholds.clear_samples {
            out.push(AlertTransition::Resolve { id: a.id });
        } else {
            let message = if kind == AlertKind::Load {
                format!("1 分钟负载 {v:.2}，不高于阈值 {threshold:.1}")
            } else {
                format!("{label} {v:.1}%，低于阈值 {threshold:.0}%")
            };
            out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Warning,
                message,
                value: Some(value_text(v)),
                clear_ack: false,
            });
        }
    }
}

fn eval_cpu(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    eval_numeric(
        input,
        AlertKind::Cpu,
        input.current.cpu_pct,
        input.thresholds.cpu_pct,
        Compare::Ge,
        out,
    );
}

fn metric_of(s: &MonitorSample, kind: AlertKind) -> Option<f64> {
    match kind {
        AlertKind::Cpu => s.cpu_pct,
        AlertKind::Memory => match (s.mem_used_kb, s.mem_total_kb) {
            (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
            _ => None,
        },
        AlertKind::Load => s.load1,
        _ => None,
    }
}

fn eval_memory(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let value = match (input.current.mem_used_kb, input.current.mem_total_kb) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
        _ => None,
    };
    eval_numeric(
        input,
        AlertKind::Memory,
        value,
        input.thresholds.mem_pct,
        Compare::Ge,
        out,
    );
}

fn eval_disk(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let Some(v) = metric_of_percent(input.current.disk_used_kb, input.current.disk_total_kb) else {
        return;
    };
    let active = active_for(input, AlertKind::Disk);
    let warning = input.thresholds.disk_pct;
    let critical = input.thresholds.disk_critical_pct;
    if v >= warning {
        let severity = if v >= critical {
            Severity::Critical
        } else {
            Severity::Warning
        };
        let message = if severity == Severity::Critical {
            format!("磁盘使用率 {v:.1}% 超过严重阈值 {critical:.0}%（警告阈值 {warning:.0}%）")
        } else {
            format!("磁盘使用率 {v:.1}% 超过警告阈值 {warning:.0}%")
        };
        let value = Some(format!("{v:.1}%"));
        match active {
            Some(a) => {
                let was_critical = a.severity == "critical";
                out.push(AlertTransition::Update {
                    id: a.id,
                    severity,
                    message,
                    value,
                    clear_ack: severity == Severity::Critical && !was_critical,
                });
            }
            None => out.push(AlertTransition::Open {
                kind: AlertKind::Disk,
                severity,
                message,
                value,
            }),
        }
    } else if let Some(a) = active {
        let clear = count_back(true, input.recent, |s| {
            metric_of_percent(s.disk_used_kb, s.disk_total_kb).map(|x| x < warning)
        });
        if clear >= input.thresholds.clear_samples {
            out.push(AlertTransition::Resolve { id: a.id });
        } else {
            out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Warning,
                message: format!("磁盘使用率 {v:.1}%，低于警告阈值 {warning:.0}%"),
                value: Some(format!("{v:.1}%")),
                clear_ack: false,
            });
        }
    }
}

fn metric_of_percent(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    match (used, total) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
        _ => None,
    }
}

fn eval_load(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    let (Some(load), Some(cores)) = (input.current.load1, input.current.cpu_cores) else {
        return;
    };
    let threshold = cores as f64 * input.thresholds.load_per_core;
    eval_numeric(
        input,
        AlertKind::Load,
        Some(load),
        threshold,
        Compare::Gt,
        out,
    );
}

fn eval_service(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    if !input.deployment_known || input.expected_services.is_empty() {
        return;
    }
    let Some(cur) = &input.current.services else {
        return;
    };
    let active = active_for(input, AlertKind::Service);
    let down = input
        .expected_services
        .iter()
        .any(|u| matches!(cur.get(u).map(String::as_str), Some("inactive" | "failed")));
    let all_active = input
        .expected_services
        .iter()
        .all(|u| cur.get(u).map(String::as_str) == Some("active"));

    if down {
        let message = service_message(input.expected_services, cur);
        let value = Some(down_value(input.expected_services, cur));
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Critical,
                message,
                value,
                clear_ack: false,
            }),
            None => {
                let first_failed = input.last_reachable.is_none()
                    && input
                        .expected_services
                        .iter()
                        .any(|u| cur.get(u).map(String::as_str) == Some("failed"));
                if first_failed {
                    out.push(AlertTransition::Open {
                        kind: AlertKind::Service,
                        severity: Severity::Critical,
                        message,
                        value,
                    });
                    return;
                }
                let baseline_active = input.last_reachable.is_some_and(|s| {
                    s.services.as_ref().is_some_and(|m| {
                        input
                            .expected_services
                            .iter()
                            .all(|u| m.get(u).map(String::as_str) == Some("active"))
                    })
                });
                if baseline_active && service_down_streak(input) >= TRANSITION_SAMPLES {
                    out.push(AlertTransition::Open {
                        kind: AlertKind::Service,
                        severity: Severity::Critical,
                        message,
                        value,
                    });
                }
            }
        }
    } else if all_active {
        if let Some(a) = active {
            if service_healthy_streak(input) >= TRANSITION_SAMPLES {
                out.push(AlertTransition::Resolve { id: a.id });
            }
        }
    }
}

fn service_message(expected: &[String], states: &BTreeMap<String, String>) -> String {
    let parts: Vec<String> = expected
        .iter()
        .map(|u| {
            format!(
                "{u} {}",
                states.get(u).map(String::as_str).unwrap_or("unknown")
            )
        })
        .collect();
    format!("服务异常：{}", parts.join("；"))
}

fn down_value(expected: &[String], states: &BTreeMap<String, String>) -> String {
    expected
        .iter()
        .filter(|u| {
            matches!(
                states.get(*u).map(String::as_str),
                Some("inactive" | "failed")
            )
        })
        .map(|u| {
            format!(
                "{u}={}",
                states.get(u).map(String::as_str).unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn service_down_streak(input: &AlertInput<'_>) -> u32 {
    let mut n = 0;
    for s in input.recent.iter().rev() {
        if !s.reachable {
            continue;
        }
        let Some(m) = &s.services else { continue };
        if input
            .expected_services
            .iter()
            .any(|u| matches!(m.get(u).map(String::as_str), Some("inactive" | "failed")))
        {
            n += 1;
        } else if input
            .expected_services
            .iter()
            .all(|u| m.get(u).map(String::as_str) == Some("active"))
        {
            break;
        }
    }
    n + 1 // current is down
}

fn service_healthy_streak(input: &AlertInput<'_>) -> u32 {
    let mut n = 0;
    for s in input.recent.iter().rev() {
        if !s.reachable {
            continue;
        }
        let Some(m) = &s.services else { continue };
        if input
            .expected_services
            .iter()
            .all(|u| m.get(u).map(String::as_str) == Some("active"))
        {
            n += 1;
        } else if input
            .expected_services
            .iter()
            .any(|u| matches!(m.get(u).map(String::as_str), Some("inactive" | "failed")))
        {
            break;
        }
    }
    n + 1 // current is all active
}

fn eval_wireguard(input: &AlertInput<'_>, out: &mut Vec<AlertTransition>) {
    if !input.wg_expected {
        return;
    }
    let Some(cur) = &input.current.wg_interfaces else {
        return;
    };
    let active = active_for(input, AlertKind::Wireguard);
    if cur.is_empty() {
        let Some(base) = input.wg_baseline.filter(|b| !b.is_empty()) else {
            return;
        };
        let message = format!("WireGuard 接口全部消失（基线：{}）", base.join(", "));
        let value = Some(format!("无接口（基线 {}）", base.join(", ")));
        match active {
            Some(a) => out.push(AlertTransition::Update {
                id: a.id,
                severity: Severity::Critical,
                message,
                value,
                clear_ack: false,
            }),
            None => {
                let streak = count_back(true, input.recent, |s| {
                    s.wg_interfaces.as_ref().map(|v| v.is_empty())
                });
                if streak >= TRANSITION_SAMPLES {
                    out.push(AlertTransition::Open {
                        kind: AlertKind::Wireguard,
                        severity: Severity::Critical,
                        message,
                        value,
                    });
                }
            }
        }
    } else if let Some(a) = active {
        let streak = count_back(true, input.recent, |s| {
            s.wg_interfaces.as_ref().map(|v| !v.is_empty())
        });
        if streak >= TRANSITION_SAMPLES {
            out.push(AlertTransition::Resolve { id: a.id });
        }
    }
}

/// 从 recent 末尾（最新）向前数连续样本，返回计数（含 current）。
/// `pred` 返回 `None` 表示该样本指标未知：既不计数也不清零（规格 6.1 的冻结规则）。
fn count_back(
    current_matches: bool,
    recent: &[MonitorSample],
    pred: impl Fn(&MonitorSample) -> Option<bool>,
) -> u32 {
    if !current_matches {
        return 0;
    }
    let mut n = 0;
    for s in recent.iter().rev() {
        match pred(s) {
            Some(true) => n += 1,
            Some(false) => break,
            None => continue,
        }
    }
    n + 1
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/alert_tests.rs"
    ));
}
