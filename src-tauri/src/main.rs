// 模块说明：桌面应用二进制入口。声明 commands（Tauri 命令层）、config（配置生成）、
// crypto（凭据加密）、db（数据库层）、domain（领域类型）、jobs（job 队列）、
// relay（中转链路服务）、serve（本地订阅 HTTP 服务）、server_bbr（服务器 BBR
// 检测与一键开启）、server_speed（服务器测速：阶段1 延迟/带宽批量编排与阶段2
// iperf3 对测/安装）、servers（服务器服务）、service（提权服务宿主：SCM 派发、
// 命名管道 IPC、mihomo 生命周期）、speed（节点测速：sidecar 生命周期与
// 测速引擎）、ssh（SSH 适配器）、subs（订阅解析）、settings（设置服务）与 state
// （应用状态装配）模块；
// main() 启动 Tauri，注册 updater 自更新插件（minisign 校验 GitHub Releases 签名），
// 在 setup 阶段打开数据目录、创建数据库、装配 AppState 后注入 managed state
// （含打包内置 mihomo 资源路径），并以 invoke_handler 注册全部 Tauri 命令
// 供前端调用；ExitRequested 时停止 sidecar 子进程。
// 输入输出：无输入参数；正常退出码 0，Tauri 构建失败时 panic。
// 依赖：tauri；本文件声明的 commands、config、crypto、db、domain、jobs、relay、serve、
// server_bbr、server_speed、servers、speed、ssh、subs、settings、state 模块；
// commands_tests、domain_tests、settings_tests 仅在 cfg(test) + local-tests 特性下编译
// （测试源码在仓库根 tests/rust，不随仓库上传）。
// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod boot_heal;
mod commands;
mod config;
mod crypto;
mod db;
mod domain;
mod jobs;
mod net;
mod relay;
mod serve;
mod server_bbr;
mod server_monitor;
mod server_speed;
mod servers;
mod service;
mod settings;
mod share;
mod speed;
mod ssh;
mod state;
mod subs;
mod sysproxy;
mod tor;
mod util;

use crate::state::AppState;
use std::sync::Arc;
use tauri::{Emitter, Manager};

/// 仪表盘顶部告警条的键。
///
/// **Dashboard 只读这一个键**（Dashboard.tsx:197 的 `getSettings(['startup_config_error', …])`），
/// 所以「重建失败」与「凭据加密降级」两类告警都写在这里：另起一个键就必须动前端，
/// 而这条通路的全部意义就是**让用户看见**。键名是历史遗留（原先只放重建失败），
/// 实际语义是「启动阶段需要用户知道的坏消息」。
const STARTUP_ALERT_KEY: &str = "startup_config_error";

/// 已报告过的降级条目行数（只报增量用；前端不读这个键）。
const DEGRADATIONS_REPORTED_KEY: &str = "crypto_degradations_reported_lines";

/// 组装并写入顶部告警条（键见 [`STARTUP_ALERT_KEY`]）。
///
/// **必须在 DB 锁外调用**：它自己会读写 settings（每次内部取一次锁），持锁调用必死锁。
/// main 的 setup 天然满足——那时还没有任何命令在跑。
pub(crate) fn apply_startup_alert(
    settings: &crate::settings::SettingsService,
    data_dir: &std::path::Path,
    rebuild_error: Option<String>,
) -> Result<(), String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(e) = rebuild_error {
        parts.push(e);
    }
    if let Some(notice) = crypto_downgrade_notice(settings, data_dir)? {
        parts.push(notice);
    }
    // 一切正常就是空串——等于清掉上次的旧告警（重启成功不该继续显示历史失败）
    settings.set(STARTUP_ALERT_KEY, &parts.join("\n"))
}

/// 凭据加密降级的告警文案（#13）：只报**上次启动之后新增**的条目，没有新增返回 None。
///
/// 为什么只报增量：日志只追加，问题修好之后旧行还在；每次都全量报就等于横幅永久挂着，
/// 而永久挂着的告警没人看，还会把同一条横幅上的「配置重建失败」一起拖成背景噪声。
/// 日志文件本身不删不改——它是排查用的原始记录，不是待办清单。
///
/// 为什么按行数而不是字节偏移记进度：偏移会落进 UTF-8 字符中间（原因里有中文），行数不会。
/// 文件被外部清空或轮转时行数变小，这里顺手把计数回退，否则以后的新条目会被永远当成
/// 「已经报过」——正是「留痕没人盯」的另一种写法。
///
/// 一个说清楚的代价：降级发生在运行期，而这里只在启动时读一次，所以用户是**下一次启动**
/// 才看到提示。要当场提示就得在降级那一刻写库，而那一刻正持着 DB 锁（`open_param` 是从
/// `row_to_node` 里被调的），写不得。
fn crypto_downgrade_notice(
    settings: &crate::settings::SettingsService,
    data_dir: &std::path::Path,
) -> Result<Option<String>, String> {
    let path = data_dir.join(crate::crypto::DEGRADATION_LOG_NAME);
    let Ok(content) = std::fs::read_to_string(&path) else {
        // 没有日志＝没降级过。读不出来也当作没有：这是告警，不是业务，不能反过来挡住启动
        return Ok(None);
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let reported: usize = settings
        .get(DEGRADATIONS_REPORTED_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if lines.len() <= reported {
        if reported != lines.len() {
            settings.set(DEGRADATIONS_REPORTED_KEY, &lines.len().to_string())?;
        }
        return Ok(None);
    }
    let fresh = &lines[reported..];
    // 行格式：`RFC3339<TAB>原因`（见 crypto::append_degradation_log）
    let latest = fresh.last().copied().unwrap_or_default();
    let (at, reason) = latest.split_once('\t').unwrap_or(("", latest));
    settings.set(DEGRADATIONS_REPORTED_KEY, &lines.len().to_string())?;
    // 时间按秒显示：日志里是 RFC3339 全精度（含纳秒），直接摆到用户面前是一串噪声，
    // 而这条文本的全部目的就是让人一眼看懂。解析不了就原样用（不至于因为它丢告警）。
    let shown_at = chrono::DateTime::parse_from_rfc3339(at)
        .map(|t| {
            t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .to_string()
        })
        .unwrap_or_else(|_| at.to_string());
    let when = if at.is_empty() {
        String::new()
    } else {
        format!("（最近 {shown_at}）")
    };
    Ok(Some(format!(
        "凭据加密降级 {} 条{}：{reason}——这些节点的参数可能读不出来，详情见 {}",
        fresh.len(),
        when,
        path.display()
    )))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 服务宿主分流必须在 Tauri 之前：SCM 拉起进程时期望它立刻进入
    // StartServiceCtrlDispatcher，晚一步就会报「服务未及时响应启动请求」。
    if service::paths::is_service_invocation(&args) {
        #[cfg(windows)]
        service::host::run();
        return;
    }

    // 安装/卸载同样要在 Tauri 之前接住：主程序用 ShellExecute(runas) 拉起**本
    // exe** 的这两个标志去过 UAC，晚一步就会先弹出一个 GUI 窗口再装服务。
    #[cfg(windows)]
    {
        // 登录兜底必须同样在 Tauri 之前接住：它要在“应用还没被打开”的窗口里
        // 修复系统重启留下的死代理指针，不能因此拉起一个 GUI/WebView。
        if args.iter().any(|a| a == "--boot-heal") {
            if let Err(e) = boot_heal::run() {
                // 打包后是 GUI 子系统，stderr 不可见；详细痕迹在 boot-heal.log。
                eprintln!("[vpn-ctl] 登录兜底失败: {e}");
            }
            return;
        }
        if args.iter().any(|a| a == "--install-service") {
            match service::install::install() {
                Ok(()) => println!("服务安装完成"),
                Err(e) => {
                    eprintln!("服务安装失败: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        if args.iter().any(|a| a == "--uninstall-service") {
            // 卸载时清扫阻断规则（四重清扫之四）。**必须放在 uninstall 之前**：
            // 服务一旦从 SCM 删掉，就再没有谁能替我们把规则撤下来，而残留的
            // BLOCK 规则是真会挡住用户网络的。此刻这个进程本身就是提权的，
            // 直接清，不必绕服务。
            match crate::net::firewall::clear() {
                Ok(n) if n > 0 => println!("已清除 {n} 条残留的会话阻断规则"),
                Ok(_) => {}
                Err(e) => eprintln!(
                    "清除残留阻断规则失败（可到 wf.msc 手工删 yunyi-vpn- 开头的规则）: {e}"
                ),
            }
            match service::install::uninstall() {
                Ok(()) => println!("服务已卸载"),
                Err(e) => {
                    eprintln!("服务卸载失败: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let data_dir = app.path().app_data_dir().expect("app data dir");
            std::fs::create_dir_all(&data_dir).expect("create data dir");
            // 主机密钥 TOFU 指纹库（#11）：进程级设置一次，之后所有 SSH 连接都按它校验。
            // 必须在任何 connect 之前——不设就等于「不校验」（测试进程的默认态）。
            crate::ssh::set_known_hosts_path(data_dir.join("known_hosts"));
            // 凭据加密降级痕迹落盘（#13）：解不开的密文会被原样返回，上游只看到 JSON
            // 解析失败——痕迹必须落在数据目录里，否则打包后（无控制台）没人看得见。
            crate::crypto::set_downgrade_log_path(
                data_dir.join(crate::crypto::DEGRADATION_LOG_NAME),
            );
            let state =
                crate::state::AppState::new(&data_dir.join("vpn-ctl.db")).expect("build app state");
            // 启动时从持久化节点重建合并配置缓存：重启后订阅服务与导出立即可用，无需先同步
            let rebuild_error = match tauri::async_runtime::block_on(state.subs.rebuild_config()) {
                Ok(()) => None,
                Err(e) => {
                    eprintln!("[vpn-ctl] 启动时重建合并配置失败，缓存保持为空: {e}");
                    // 失败只进 stderr 在打包后不可见，写入设置表让仪表盘顶部可展示告警
                    Some(format!("启动时重建合并配置失败: {e}"))
                }
            };
            // 顶部告警条：一次写好全部内容，全好就是空串（等于清掉上次的旧告警）
            if let Err(e) = apply_startup_alert(&state.settings, &data_dir, rebuild_error) {
                eprintln!("[vpn-ctl] 记录启动告警到设置表失败: {e}");
            }
            // job 进度事件 → 前端 `job://progress`
            let handle = app.handle().clone();
            let emit: crate::jobs::EmitFn = Arc::new(move |p| {
                let _ = handle.emit("job://progress", &p);
            });
            state.job_queue.set_emit(emit);
            // 连接日志快照 → 前端 `net://conn_log`（采样在 runtime::start 里随网络开启而启动）
            let conn_handle = app.handle().clone();
            let conn_emit: crate::net::connlog::ConnEmitFn = Arc::new(move |s| {
                let _ = conn_handle.emit("net://conn_log", &s);
            });
            state.conn_sampler.set_emit(conn_emit);
            // 打包内置 mihomo 资源路径注入（未打包/未下载时置 None，不 panic）
            let resource = app
                .path()
                .resolve("binaries/mihomo.exe", tauri::path::BaseDirectory::Resource);
            if let Ok(path) = resource {
                *state.sidecar_resource.write().unwrap() = Some(path);
            }
            // 打包内置 tor.exe 资源路径注入（隐私线的本机那一段）。未打包/未下载时置
            // None——`tor::ensure_binary` 会给出「先跑 npm run fetch:tor」的中文错误，
            // 而不是在这里 panic。
            let tor_resource = app
                .path()
                .resolve("binaries/tor/tor.exe", tauri::path::BaseDirectory::Resource);
            if let Ok(path) = tor_resource {
                *state.tor_resource.write().unwrap() = Some(path);
            }
            // 拉起提权服务。未装/未启动都无所谓——net::runtime 会回退直接 spawn，
            // 服务是**增益不是依赖**。失败只记一行日志，绝不阻断启动。
            #[cfg(windows)]
            if let Err(e) = crate::service::install::start_service() {
                eprintln!("[vpn-ctl] 提权服务未就绪（开启网络将回退直接起 mihomo）: {e}");
            }
            // 强杀后的上网恢复：界面被强杀时系统代理会停在「指向已无人监听的本机端口」
            // 的僵尸态（服务只负责杀 mihomo、撤规则——HKCU 只有界面写得动）。放在**启动**
            // 而不是只放在「开启网络」里：强杀后重开应用，浏览器立刻能上网，不必再点一次
            // 开启。判据与开启路径同源：只清死指针，别的客户端的真接管绝不碰。
            if let Err(e) = crate::net::sysproxy_set::heal_dead_system_proxy() {
                eprintln!("[vpn-ctl] 启动自愈残留系统代理失败: {e}");
            }
            // 注册登录兜底：重启/强杀会跳过所有退出清理，而系统代理与环境变量的
            // 注册表值会活过重启；只有登录后有人把它们指向的本机端口重新拉起来，
            // “应用没启动”的那段窗口才不会整片 ECONNREFUSED。失败只记一行——
            // 应用启动时的自愈仍然有效，不把这件事变成启动阻断。
            #[cfg(windows)]
            if let Err(e) = crate::boot_heal::ensure_registered() {
                eprintln!("[vpn-ctl] 注册登录兜底失败: {e}");
            }
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_upstreams_cmd,
            commands::add_upstream_cmd,
            commands::update_upstream_cmd,
            commands::remove_upstream_cmd,
            commands::start_upstream_job_cmd,
            commands::probe_egress_ip_cmd,
            commands::enable_privacy_cmd,
            commands::disable_privacy_cmd,
            commands::list_privacies_cmd,
            commands::list_subscriptions_cmd,
            commands::create_subscription_cmd,
            commands::update_subscription_cmd,
            commands::delete_subscription_cmd,
            commands::sync_subscription_cmd,
            commands::list_nodes_cmd,
            commands::set_node_enabled_cmd,
            commands::toggle_node_relay_cmd,
            commands::change_reality_dest_cmd,
            commands::delete_node_cmd,
            commands::rename_node_cmd,
            commands::get_merged_config_cmd,
            commands::export_yaml_cmd,
            commands::export_singbox_cmd,
            commands::list_exportable_nodes_cmd,
            commands::export_yaml_selected_cmd,
            commands::export_singbox_selected_cmd,
            commands::list_shares_cmd,
            commands::create_share_cmd,
            commands::delete_share_cmd,
            commands::start_sub_server_cmd,
            commands::stop_sub_server_cmd,
            commands::get_sub_server_status_cmd,
            commands::get_settings_cmd,
            commands::set_setting_cmd,
            commands::list_servers_cmd,
            commands::create_server_cmd,
            commands::update_server_cmd,
            commands::set_server_addr_family_cmd,
            commands::detect_server_ipv6_cmd,
            commands::delete_server_cmd,
            commands::test_server_connection_cmd,
            commands::setup_passwordless_cmd,
            commands::list_channels_cmd,
            commands::add_channel_cmd,
            commands::remove_channel_cmd,
            commands::channel_status_cmd,
            commands::channel_switch_cmd,
            commands::scan_server_ports_cmd,
            commands::start_deploy_cmd,
            commands::deployed_protocols_cmd,
            commands::deployed_protocol_details_cmd,
            commands::install_supplement_cmd,
            commands::remove_protocol_cmd,
            commands::net_status_cmd,
            commands::net_repair_proxy_cmd,
            commands::net_start_cmd,
            commands::net_set_tun_cmd,
            commands::net_service_state_cmd,
            commands::net_service_install_cmd,
            commands::net_service_uninstall_cmd,
            commands::net_firewall_clear_cmd,
            commands::net_stop_cmd,
            commands::net_select_cmd,
            commands::net_current_cmd,
            commands::net_conn_log_cmd,
            commands::list_jobs_cmd,
            commands::cancel_job_cmd,
            commands::list_relay_rules_cmd,
            commands::probe_link_cmd,
            commands::build_link_cmd,
            commands::adopt_link_cmd,
            commands::teardown_link_cmd,
            commands::readopt_link_cmd,
            commands::delete_link_rule_cmd,
            commands::refresh_link_status_cmd,
            commands::list_device_peers_cmd,
            commands::add_device_peer_cmd,
            commands::remove_device_peer_cmd,
            commands::get_peer_config_cmd,
            commands::speed_test_cmd,
            commands::server_speed_cmd,
            commands::server_throughput_cmd,
            commands::install_iperf3_cmd,
            commands::list_throughput_cmd,
            commands::check_bbr_cmd,
            commands::enable_bbr_cmd,
            sysproxy::get_system_proxy_cmd,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // 退出即停 sidecar；本机上网客户端则**保留端口、切直连**：
            // 已运行的程序（浏览器/终端/Claude Code…）环境变量里还记着这个代理，
            // 杀了它就会整片断网（见 net::runtime::stop_for_exit 的根因说明）
            if let tauri::RunEvent::ExitRequested { .. } = event {
                let state = app.state::<Arc<AppState>>();
                tauri::async_runtime::block_on(crate::speed::stop_all(&state));
                tauri::async_runtime::block_on(crate::net::runtime::stop_all(&state));
                // 双保险：`net::runtime::stop` 已经会收 Tor，但槽位为空时它走不到那句
                // （例如网络压根没起来、Tor 却已在 bootstrap）。这里再收一次——
                // `stop_all` 是幂等的，且复用了用户自己 9050 的 Tor 不会被杀。
                tauri::async_runtime::block_on(crate::tor::stop_all(&state));
                // 刻意不发 Shutdown：服务与它的 mihomo 要留在后台切直连，否则
                // 环境变量里记着本机代理的程序会立刻断网。`stop_all` 已经把端点
                // 切到直连并释放了会话归属；服务在下次开启网络、`sc stop` 或
                // 重启时正常回收。
            }
        });
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/root_mods.rs"
));
