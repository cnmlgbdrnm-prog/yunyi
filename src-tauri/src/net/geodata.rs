//! 分流规则数据：geoip.dat / geosite.dat 自动确保（首次开启前下载到 data_dir/net）。
//! 下载失败 → 降级（客户端 config 跳过 GEOSITE/GEOIP CN 规则，仅 LAN/PRIVATE 直连）。
//! base 参数化：生产用 DEFAULT_GEODATA_BASE，测试用 wiremock uri。

use crate::state::AppState;

/// 数据目录子目录名（与 runtime::NET_DIR 同目录：data_dir/net）。
pub const GEO_DIR: &str = "net";
/// 国家库文件名。**同一个文件两个消费者**：mihomo 用它判 `GEOIP,CN`，
/// 连接日志用它判「这条直连是不是国内目标」（`net::connlog`）。
pub const MMDB_FILE: &str = "country.mmdb";
/// 三个 geo 数据文件名（存在性即 geo_present 判据）。
/// 实测（mihomo v1.19.10）：country.mmdb 只认「-d 目录根」，启动缺它会联网下载
/// geoip.metadb 卡 12-15s 后 fatal；故三件套全部落到 net 目录根。
pub const GEO_FILES: [&str; 3] = ["geoip.dat", "geosite.dat", MMDB_FILE];
/// 生产数据源（MetaCubeX/meta-rules-dat 官方 release，数据集与 Clash Verge 同源）。
pub const DEFAULT_GEODATA_BASE: &str =
    "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest";

fn geo_dir(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join(GEO_DIR)
}

/// 候选资产路径（与 sidecar::candidate_paths 同模式）：dev/构建机 src-tauri/binaries/geo/
/// 携带真实数据文件（npm 打包时并入资源）；生产安装目录无此目录时回退网络下载。
/// 返回 (源路径, 目标文件名) 对；文件不存在时调用方跳过。
pub fn candidate_geo_paths() -> Vec<(std::path::PathBuf, &'static str)> {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("geo");
    GEO_FILES.iter().map(|f| (base.join(f), *f)).collect()
}

/// 两个数据文件都存在于 data_dir/net → true（config 生成据此下发 GEOSITE/GEOIP CN 规则）。
pub fn geo_present(state: &AppState) -> bool {
    GEO_FILES.iter().all(|f| geo_dir(state).join(f).is_file())
}

/// `country.mmdb` 的落盘路径（连接日志采样器启动时读它一次）。
pub fn mmdb_path(state: &AppState) -> std::path::PathBuf {
    geo_dir(state).join(MMDB_FILE)
}

/// 生产路径：默认数据源 + 系统代理跟随客户端由调用方构造（http_client 自行跟随）。
pub async fn ensure_geodata(state: &AppState, client: reqwest::Client) -> bool {
    ensure_geodata_with_base(state, client, DEFAULT_GEODATA_BASE, candidate_geo_paths()).await
}

/// 逐个下载；任一失败即返回 false（降级，不重试——首次失败下次开启再试）。
/// candidates 由调用方注入（生产 = candidate_geo_paths，测试 = 空列表以隔离
/// dev 机器上真实数据文件对下载失败路径的影响）。
pub async fn ensure_geodata_with_base(
    state: &AppState,
    client: reqwest::Client,
    base: &str,
    candidates: Vec<(std::path::PathBuf, &'static str)>,
) -> bool {
    if geo_present(state) {
        return true;
    }
    let dir = geo_dir(state);
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    // 候选资产优先：dev/构建机携带真实 geo 文件（离线也能就位，且 mihomo 不会因
    // 缺 country.mmdb 在启动期联网下载卡 5s+）
    let mut all_copied = true;
    for (src, name) in candidates {
        if src.is_file() && std::fs::copy(&src, dir.join(name)).is_err() {
            all_copied = false;
            break;
        }
    }
    if all_copied && geo_present(state) {
        return true;
    }
    // 回退：网络下载（base 参数化；任一失败即降级 false，不重试）
    for f in GEO_FILES {
        let url = format!("{base}/{f}");
        let bytes = match client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(r) => match r.bytes().await {
                Ok(b) => b,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        if std::fs::write(dir.join(f), &bytes).is_err() {
            return false;
        }
    }
    geo_present(state)
}
