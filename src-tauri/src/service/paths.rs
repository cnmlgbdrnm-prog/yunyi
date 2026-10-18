//! 服务的固定路径与入口标志。
//!
//! 服务以 SYSTEM 运行，用户数据目录未必可达，故配置与网卡数据一律放在
//! `%ProgramData%\yunyi`（该目录默认只允许管理员写，正好与「配置由服务落盘」
//! 的设计吻合）。
//!
//! 输入输出：纯函数/常量，无 IO。
//! 依赖：std::env。

use std::path::PathBuf;

/// SCM 里的服务名（`sc query yunyi-service` 用的就是它）。
pub const SERVICE_NAME: &str = "yunyi-service";

/// 命名管道全名。
pub const PIPE_NAME: &str = r"\\.\pipe\yunyi-service";

/// 命令行标志：带它 = 本次进程是被 SCM 拉起来的服务宿主，不是 GUI。
pub const SERVICE_FLAG: &str = "--service";

/// 服务固定根：`%ProgramData%\yunyi`。
pub fn root() -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
    PathBuf::from(base).join("yunyi")
}

/// mihomo 的工作目录（配置、geo 数据都在这）。
pub fn net_dir() -> PathBuf {
    root().join("net")
}

/// 服务自有的可执行文件目录——mihomo 的副本落在这，也从这跑。
///
/// 单独一个目录而不是并进 net_dir：net_dir 是「mihomo 的数据」，这里是
/// 「服务要执行的程序」。两者信任级相同（都只在管理员可写处），但混在一起
/// 会让「服务跑的东西都在 bin/ 下」这条规则不好讲。
pub fn bin_dir() -> PathBuf {
    root().join("bin")
}

/// 服务落盘的 mihomo 配置路径。
pub fn config_path() -> PathBuf {
    net_dir().join("config.yaml")
}

/// 本次进程是否以服务模式启动。
///
/// 用**精确相等**而非 `contains`/`starts_with`——后者会让 `--service-name` 这类
/// 前缀相似的参数误命中，把一次普通启动变成服务宿主，GUI 直接不出来。
pub fn is_service_invocation(args: &[String]) -> bool {
    args.iter().any(|a| a == SERVICE_FLAG)
}
