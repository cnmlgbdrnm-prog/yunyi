//! 登录兜底：系统重启/强杀后，用户级代理指针可能仍指向本机端口，而代理进程已经
//! 随上次会话消失。这个模式在 Tauri 初始化之前运行：检测到死指针后，在指针指向的
//! 端口拉起一个最小 direct 配置的 mihomo；拉不起来则清掉死指针。
//!
//! 为什么必须放在登录而不是应用启动：HKCU 的 WinINET 与用户环境变量只有用户进程
//! 写得动；服务以 SYSTEM 运行，够不到当前用户注册表，而且服务刻意保持手动启动。
//! 应用启动时的自愈（`net::sysproxy_set::heal_dead_system_proxy`）只能救“应用已经
//! 被打开”之后的进程；开机到应用启动之间才是死指针真正伤人的窗口。
//!
//! 为什么只拉起直连而不是恢复 VPN：登录时节点/网络未必就绪，开 VPN 也应当由用户
//! 决定。这里只保证“指针指向的端口有人听”，已运行/新启动的程序不会再
//! `ECONNREFUSED`；真正的 VPN 仍由用户打开云驿后开启。
//!
//! 输入输出：`run()` 读注册表/端口，必要时 spawn mihomo；`ensure_registered()` 写
//! HKCU Run 键。两者都不初始化 Tauri，也不显示窗口。
//! 依赖：std::process、winreg、chrono、net::sysproxy_set、net::winprobe。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

/// `HKCU\...\Run`：登录时由 explorer 拉起本 exe 的 `--boot-heal` 模式。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "YunyiBootHeal";
/// 与 Tauri `app_data_dir` 同源的目录名（%APPDATA%\com.vpnctl.app）。
const APP_DIR: &str = "com.vpnctl.app";
/// 等 mihomo 监听的上限：正常几百毫秒；慢盘/杀软扫描时留足余量。
const LISTEN_TIMEOUT: Duration = Duration::from_secs(5);
const LISTEN_INTERVAL: Duration = Duration::from_millis(100);

/// 登录时执行：有死代理指针就在原端口拉起 direct mihomo，否则静默退出。
pub fn run() -> Result<(), String> {
    let data_dir = data_dir()?;
    let ports = crate::net::sysproxy_set::dead_self_proxy_ports()?;
    if ports.is_empty() {
        return Ok(());
    }
    let port = ports[0];
    append_log(
        &data_dir,
        &format!("发现指向本机死端口的代理指针 {ports:?}，优先在 {port} 拉起直连兜底"),
    );
    match start_direct(&data_dir, port) {
        Ok(pid) => {
            append_log(
                &data_dir,
                &format!("直连兜底已在 127.0.0.1:{port} 就绪（mihomo pid {pid}）"),
            );
            Ok(())
        }
        Err(e) => {
            // 拉不起来时至少清掉死指针：新进程会直连，不会再撞 ECONNREFUSED。
            // 注意这里不能无条件清：heal 自己会复查端口，若刚才有别的程序接管了
            // 该端口，它不会误删用户正在用的代理。
            append_log(
                &data_dir,
                &format!("直连兜底启动失败: {e}；回退为清除死指针"),
            );
            crate::net::sysproxy_set::heal_dead_system_proxy()
        }
    }
}

/// 让 `--boot-heal` 在下次登录时跑。值随 exe 路径变化而更新（开发构建/安装位置
/// 变了也不会指向旧路径）；路径含空格/中文，必须加引号。
pub fn ensure_registered() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("取自身路径失败: {e}"))?;
    let cmd = format!("\"{}\" --boot-heal", exe.display());
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_SET_VALUE)
        .map_err(|e| format!("打开 Run 键失败: {e}"))?;
    let current: Option<String> = key.get_value(RUN_VALUE).ok();
    if current.as_deref() != Some(cmd.as_str()) {
        key.set_value(RUN_VALUE, &cmd)
            .map_err(|e| format!("写 Run 键失败: {e}"))?;
    }
    Ok(())
}

/// 数据目录与 Tauri 的 `app_data_dir` 同源：`%APPDATA%\com.vpnctl.app`。
fn data_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("APPDATA")
        .ok_or_else(|| "APPDATA 未设置，无法定位云驿数据目录".to_string())?;
    Ok(PathBuf::from(base).join(APP_DIR))
}

/// mihomo 的候选位置，按“这次会话真的能用”的顺序：
/// 1. 数据目录 net/ ——上次开网络时 sidecar 已铺好的那份；
/// 2. 数据目录 sidecar/ ——测速 sidecar 用过的同镜像副本；
/// 3. exe 同目录 binaries/ ——打包安装后的资源位置；
/// 4. 开发构建的 src-tauri/binaries/。
fn find_mihomo(data_dir: &Path) -> Option<PathBuf> {
    let mut candidates = vec![
        data_dir.join("net").join("mihomo.exe"),
        data_dir.join("sidecar").join("mihomo.exe"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("binaries").join("mihomo.exe"));
        }
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join("mihomo.exe"),
    );
    candidates.into_iter().find(|p| p.is_file())
}

/// 在 `port` 上拉起最小 direct 配置。只用 mixed-port，不建 TUN、不读节点/geo：
/// 登录兜底只需要“端口有人听 + 流量直连”。
fn start_direct(data_dir: &Path, port: u16) -> Result<u32, String> {
    let net_dir = data_dir.join("net");
    std::fs::create_dir_all(&net_dir)
        .map_err(|e| format!("创建 {} 失败: {e}", net_dir.display()))?;
    let exe = find_mihomo(data_dir).ok_or_else(|| {
        "找不到 mihomo.exe（数据目录、exe 同目录与开发资源目录都不存在）".to_string()
    })?;
    // 覆盖 net/config.yaml 是有意的：下次“开启网络”会按数据库重新生成整份配置；
    // 而这份最小配置只服务重启后的直连窗口。`-d` 指向 data_dir/net 也是故意的——
    // 应用启动时 `kill_own_orphans` 正是按“net 目录 + 该镜像名”回收自己的实例。
    std::fs::write(net_dir.join("config.yaml"), direct_config_yaml(port))
        .map_err(|e| format!("写入直连配置失败: {e}"))?;
    let mut child = crate::util::quiet(Command::new(&exe))
        .arg("-d")
        .arg(&net_dir)
        .current_dir(&net_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 mihomo 失败: {e}"))?;
    let pid = child.id();
    for _ in 0..(LISTEN_TIMEOUT.as_millis() / LISTEN_INTERVAL.as_millis()) {
        if crate::net::winprobe::port_is_listening(port) {
            // 丢掉句柄不杀进程：本兜底进程马上退出，mihomo 要继续占着端口。
            // std::process::Child 的 Drop 只关句柄，不终止子进程。
            drop(child);
            return Ok(pid);
        }
        std::thread::sleep(LISTEN_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "mihomo 未能在 {}s 内监听 127.0.0.1:{port}",
        LISTEN_TIMEOUT.as_secs()
    ))
}

/// 与 `net::runtime::direct_config_yaml` 共用同一份口径：退出兜底与登录兜底拉起
/// 的必须是同一种最小配置，否则两条路径的行为会悄悄漂移。
fn direct_config_yaml(port: u16) -> String {
    crate::net::runtime::direct_config_yaml(port)
}

/// 登录项没有控制台（release 是 GUI 子系统），失败只能落盘。日志按需创建，
/// 写不进去也不阻断兜底。
fn append_log(data_dir: &Path, message: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(data_dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("boot-heal.log"))
    else {
        return;
    };
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(file, "[{now}] {message}");
}

// 测试源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
// 放进子模块是为了隔离测试自己的 use：直接 include! 会和本模块的 PathBuf/Duration
// 导入重名（E0252）。
#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/boot_heal_tests.rs"
    ));
}
