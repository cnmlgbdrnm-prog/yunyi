//! 三网延迟：地区表、目标、脚本片段、解析与分级。纯函数，无 IO。
use super::config::CarrierConfig;
use super::{
    CarrierBar, CarrierHealth, CarrierProbe, CarrierRegion, CarrierSample, CarrierStats,
    MonitorSample,
};

pub struct Region {
    pub code: &'static str,
    pub name: &'static str,
}

/// 31 个省级行政区，顺序按行政区划编码。台湾/香港/澳门没有三网目标，不收录。
pub const REGIONS: &[Region] = &[
    Region {
        code: "bj",
        name: "北京",
    },
    Region {
        code: "tj",
        name: "天津",
    },
    Region {
        code: "he",
        name: "河北",
    },
    Region {
        code: "sx",
        name: "山西",
    },
    Region {
        code: "nm",
        name: "内蒙古",
    },
    Region {
        code: "ln",
        name: "辽宁",
    },
    Region {
        code: "jl",
        name: "吉林",
    },
    Region {
        code: "hl",
        name: "黑龙江",
    },
    Region {
        code: "sh",
        name: "上海",
    },
    Region {
        code: "js",
        name: "江苏",
    },
    Region {
        code: "zj",
        name: "浙江",
    },
    Region {
        code: "ah",
        name: "安徽",
    },
    Region {
        code: "fj",
        name: "福建",
    },
    Region {
        code: "jx",
        name: "江西",
    },
    Region {
        code: "sd",
        name: "山东",
    },
    Region {
        code: "ha",
        name: "河南",
    },
    Region {
        code: "hb",
        name: "湖北",
    },
    Region {
        code: "hn",
        name: "湖南",
    },
    Region {
        code: "gd",
        name: "广东",
    },
    Region {
        code: "gx",
        name: "广西",
    },
    Region {
        code: "hi",
        name: "海南",
    },
    Region {
        code: "cq",
        name: "重庆",
    },
    Region {
        code: "sc",
        name: "四川",
    },
    Region {
        code: "gz",
        name: "贵州",
    },
    Region {
        code: "yn",
        name: "云南",
    },
    Region {
        code: "xz",
        name: "西藏",
    },
    Region {
        code: "sn",
        name: "陕西",
    },
    Region {
        code: "gs",
        name: "甘肃",
    },
    Region {
        code: "qh",
        name: "青海",
    },
    Region {
        code: "nx",
        name: "宁夏",
    },
    Region {
        code: "xj",
        name: "新疆",
    },
];

pub fn regions() -> Vec<CarrierRegion> {
    REGIONS
        .iter()
        .map(|r| CarrierRegion {
            code: r.code.into(),
            name: r.name.into(),
        })
        .collect()
}

pub fn region_name(code: &str) -> Option<&'static str> {
    REGIONS.iter().find(|r| r.code == code).map(|r| r.name)
}

pub fn target_hosts(region: &str) -> Result<[String; 3], String> {
    if region_name(region).is_none() {
        return Err(format!("未知的三网地区: {region}"));
    }
    Ok(["ct", "cu", "cm"].map(|c| format!("{region}-{c}-v4.ip.zstaticcdn.com")))
}

/// 后台并发跑 3 条 curl，与资源采集段的 sleep 1 重叠；结果写临时目录。
pub fn probe_setup(hosts: &[String; 3]) -> String {
    format!(
        "d=$(mktemp -d 2>/dev/null) || d=/tmp/.vpnctl-probe-$$\n\
         mkdir -p \"$d\"\n\
         _c(){{ curl -o /dev/null -s -w '%{{time_connect}}' --connect-timeout 3 --max-time 4 \"http://$1/\" 2>/dev/null; }}\n\
         _c '{}' > \"$d/ct\" 2>/dev/null &\n\
         _c '{}' > \"$d/cu\" 2>/dev/null &\n\
         _c '{}' > \"$d/cm\" 2>/dev/null &\n",
        hosts[0], hosts[1], hosts[2]
    )
}

/// 等三网探测结束并按 `##ct/##cu/##cm` 输出，最后清理临时目录。
pub fn probe_tail() -> &'static str {
    "wait\necho '##ct'; cat \"$d/ct\" 2>/dev/null\n\
     echo '##cu'; cat \"$d/cu\" 2>/dev/null\n\
     echo '##cm'; cat \"$d/cm\" 2>/dev/null\n\
     rm -rf \"$d\"\n"
}

pub fn parse(out: &str, region: &str) -> CarrierSample {
    let sections = super::parse::split_sections(out);
    CarrierSample {
        region: region.into(),
        ct: section_probe(sections.get("ct")),
        cu: section_probe(sections.get("cu")),
        cm: section_probe(sections.get("cm")),
    }
}

/// 段缺失 = 没探到（None）；段存在但值非正/不可解析 = 探了失败（ok=false）。
fn section_probe(section: Option<&String>) -> Option<CarrierProbe> {
    let section = section?;
    let secs = section.trim().parse::<f64>().ok().filter(|v| *v > 0.0);
    Some(match secs {
        Some(secs) => CarrierProbe {
            ok: true,
            ms: Some((secs * 10_000.0).round() / 10.0),
        },
        None => CarrierProbe {
            ok: false,
            ms: None,
        },
    })
}

/// 1–5 级：≤ levels[0] → 1，≤ levels[1] → 2，≤ levels[2] → 3，≤ levels[3] → 4，否则 5。
pub fn latency_level(ms: f64, levels: &[f64; 4]) -> u8 {
    match levels.iter().position(|t| ms <= *t) {
        Some(i) => i as u8 + 1,
        None => 5,
    }
}

pub fn loss_level(pct: f64, levels: &[f64; 4]) -> u8 {
    match levels.iter().position(|t| pct <= *t) {
        Some(i) => i as u8 + 1,
        None => 5,
    }
}

fn probe_of<'a>(c: &'a CarrierSample, key: &str) -> Option<&'a CarrierProbe> {
    match key {
        "ct" => c.ct.as_ref(),
        "cu" => c.cu.as_ref(),
        "cm" => c.cm.as_ref(),
        _ => None,
    }
}

/// 窗口聚合：只统计当前地区、真正探过的样本；SSH 失败留下的 NULL 不计入分母。
pub fn summarise(region: &str, samples: &[MonitorSample], cfg: &CarrierConfig) -> CarrierHealth {
    CarrierHealth {
        region: region.into(),
        region_name: region_name(region).unwrap_or(region).into(),
        bar_count: cfg.bars,
        carriers: ["ct", "cu", "cm"]
            .iter()
            .map(|key| summarise_one(key, region, samples, cfg))
            .collect(),
    }
}

fn summarise_one(
    key: &str,
    region: &str,
    samples: &[MonitorSample],
    cfg: &CarrierConfig,
) -> CarrierStats {
    let mut ok_count = 0usize;
    let mut fail_count = 0usize;
    let mut sum = 0.0f64;
    let mut bars: Vec<CarrierBar> = Vec::new();
    for s in samples {
        let Some(c) = &s.carrier else { continue };
        if c.region != region {
            continue;
        }
        let Some(p) = probe_of(c, key) else { continue };
        if p.ok {
            let ms = p.ms.unwrap_or(0.0);
            ok_count += 1;
            sum += ms;
            bars.push(CarrierBar {
                ts: s.ts.clone(),
                ok: true,
                latency_level: Some(latency_level(ms, &cfg.latency_levels_ms)),
            });
        } else {
            fail_count += 1;
            bars.push(CarrierBar {
                ts: s.ts.clone(),
                ok: false,
                latency_level: None,
            });
        }
    }
    let total = ok_count + fail_count;
    let avg_ms = (ok_count > 0).then(|| sum / ok_count as f64);
    let loss_pct = (total > 0).then(|| fail_count as f64 / total as f64 * 100.0);
    if bars.len() > cfg.bars {
        bars = bars.split_off(bars.len() - cfg.bars);
    }
    CarrierStats {
        key: key.into(),
        avg_ms,
        avg_level: avg_ms.map(|v| latency_level(v, &cfg.latency_levels_ms)),
        loss_pct,
        loss_level: loss_pct.map(|v| loss_level(v, &cfg.loss_levels_pct)),
        samples: total as u32,
        bars,
    }
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/carrier_tests.rs"
    ));
}
