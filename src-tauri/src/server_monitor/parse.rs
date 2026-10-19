//! 采集脚本输出解析：纯函数、无 IO；所有缺失/异常都落到 Option，不把空值当 0。
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedProbe {
    pub uptime_secs: Option<u64>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub cpu_pct: Option<f64>,
    pub cpu_cores: Option<u32>,
    pub mem_used_kb: Option<u64>,
    pub mem_total_kb: Option<u64>,
    pub disk_used_kb: Option<u64>,
    pub disk_total_kb: Option<u64>,
    pub bbr_algo: Option<String>,
    pub services: Option<BTreeMap<String, String>>,
    pub wg_interfaces: Option<Vec<String>>,
    pub warning: Option<String>,
}

pub fn parse_probe_output(out: &str) -> ParsedProbe {
    let sections = split_sections(out);
    let mut p = ParsedProbe::default();

    p.uptime_secs = sections
        .get("uptime")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.max(0.0) as u64);

    if let Some(load) = sections.get("load") {
        let vals: Vec<f64> = load
            .split_whitespace()
            .take(3)
            .filter_map(|v| v.parse().ok())
            .collect();
        if vals.len() == 3 {
            p.load1 = Some(vals[0]);
            p.load5 = Some(vals[1]);
            p.load15 = Some(vals[2]);
        }
    }

    p.cpu_pct = cpu_pct(sections.get("cpu1"), sections.get("cpu2"));

    if let Some(mem) = sections.get("mem") {
        p.mem_total_kb = mem_kb(mem, "MemTotal:");
        p.mem_used_kb = match (p.mem_total_kb, mem_kb(mem, "MemAvailable:")) {
            (Some(total), Some(avail)) => Some(total.saturating_sub(avail)),
            _ => None,
        };
    }

    if let Some(disk) = sections.get("disk") {
        if let Some(line) = disk
            .lines()
            .find(|l| l.split_whitespace().last() == Some("/"))
        {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 6 {
                p.disk_total_kb = f[1].parse().ok();
                p.disk_used_kb = f[2].parse().ok();
            }
        }
    }

    p.cpu_cores = sections
        .get("cores")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0);

    p.bbr_algo = sections
        .get("bbr")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if let Some(svc) = sections.get("svc") {
        let lines: Vec<&str> = svc
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if !lines.is_empty() {
            let mut map = BTreeMap::new();
            for (i, unit) in ["xray", "ssserver", "singbox"].iter().enumerate() {
                let state = lines.get(i).copied().unwrap_or("unknown");
                map.insert((*unit).to_string(), normalize_service(state));
            }
            p.services = Some(map);
        }
    }

    let rc = sections
        .get("wg_rc")
        .and_then(|s| s.trim().parse::<i32>().ok());
    p.wg_interfaces = match (rc, sections.get("wg")) {
        (Some(0), Some(s)) => {
            let mut ifaces: Vec<String> = s
                .split_whitespace()
                .map(str::to_string)
                .collect();
            ifaces.sort();
            ifaces.dedup();
            Some(ifaces)
        }
        _ => None,
    };

    if !sections.contains_key("done") {
        p.warning = Some("采集输出不完整".into());
    }
    p
}

fn split_sections(out: &str) -> HashMap<&str, String> {
    let mut map: HashMap<&str, String> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("##") {
            let name = name.trim();
            map.entry(name).or_default();
            current = Some(name);
        } else if let Some(name) = current {
            let entry = map.entry(name).or_default();
            entry.push_str(line);
            entry.push('\n');
        }
    }
    map
}

fn cpu_pct(s1: Option<&String>, s2: Option<&String>) -> Option<f64> {
    let a = parse_cpu_line(s1?)?;
    let b = parse_cpu_line(s2?)?;
    let total_a: u64 = a.iter().sum();
    let total_b: u64 = b.iter().sum();
    let idle_a = a[3] + a[4];
    let idle_b = b[3] + b[4];
    let dt = total_b.checked_sub(total_a)?;
    let di = idle_b.checked_sub(idle_a)?;
    if dt == 0 {
        return None;
    }
    Some((100.0 * (dt.saturating_sub(di)) as f64 / dt as f64).clamp(0.0, 100.0))
}

/// 8 个字段：user nice system idle iowait irq softirq steal；guest 已计入 user/nice，排除。
fn parse_cpu_line(s: &str) -> Option<[u64; 8]> {
    let vals: Vec<u64> = s
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() != 8 {
        return None;
    }
    Some([vals[0], vals[1], vals[2], vals[3], vals[4], vals[5], vals[6], vals[7]])
}

fn mem_kb(mem: &str, key: &str) -> Option<u64> {
    mem.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn normalize_service(s: &str) -> String {
    match s.trim() {
        "active" => "active",
        "inactive" => "inactive",
        "failed" => "failed",
        "activating" => "activating",
        "deactivating" => "deactivating",
        "reloading" => "reloading",
        _ => "unknown",
    }
    .to_string()
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/parse_tests.rs"
    ));
}
