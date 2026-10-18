//! 服务的安装 / 卸载 / 启停。
//!
//! 安装要管理员权限（这是**唯一**一次 UAC）：注册服务 + 设成手动启动 + 把服务
//! DACL 改成允许交互用户启停。「之后开关 TUN 永不弹 UAC」全靠最后那一步。
//!
//! 启动类型必须是 OnDemand（手动）——绝不注册开机自启，服务跟应用生命周期走。
//!
//! 输入输出：三个幂等操作 + 两个启停函数，返回 Result<(), String>。
//! 依赖：windows-service（SCM）、std::process（sc.exe 改 DACL）。

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::service::paths::SERVICE_NAME;

/// 服务描述，显示在 services.msc 里。
const DISPLAY_NAME: &str = "云驿网络服务";
const DESCRIPTION: &str = "为云驿提供 TUN 全局模式所需的提权通道（拉起/停止 mihomo）";

/// 服务 DACL：System 与管理员全权，**交互用户**可启停与查询，但**不可改配置**。
///
/// 权限位：CC=查询配置 LC=查询状态 SW=枚举依赖 RP=启动 WP=停止
/// DT=暂停继续 LO=询问 CR=自定义控制 RC=读控制。
///
/// 授给 IU（交互用户）而非「当前用户 SID」是个取舍：拿 SID 要再拉一串 Windows
/// API，而**真正的安全边界不在 DACL**——是管道的「调用方 exe 路径必须与服务
/// 自身一致」那条校验。IU 能做的只有「启停这个服务」，改不了它的配置、换不了
/// 它的可执行文件。放行这一层换来的是「装一次 UAC，之后永久免打扰」。
const SERVICE_SDDL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPDTLOCRRC;;;IU)";

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(|e| format!("连接服务管理器失败（需要管理员权限？）: {e}"))
}

/// 本 exe 的绝对路径——服务要跑的就是它，带 `--service` 参数。
fn own_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("取自身路径失败: {e}"))
}

/// 安装服务。已存在视为成功（幂等）。
pub fn install() -> Result<(), String> {
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        // 手动启动：**不开机自启**，由应用在启动时拉起
        start_type: ServiceStartType::OnDemand,
        error_control: ServiceErrorControl::Normal,
        executable_path: own_exe()?,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // None = 以 LocalSystem 运行
        account_password: None,
    };

    let mgr = manager(ServiceManagerAccess::CREATE_SERVICE)?;
    match mgr.create_service(&info, ServiceAccess::CHANGE_CONFIG) {
        Ok(service) => {
            // 描述是可选的，失败不影响可用性
            let _ = service.set_description(DESCRIPTION);
        }
        Err(e) if is_already_exists(&e) => { /* 幂等：继续去补 DACL */ }
        Err(e) => return Err(format!("注册服务失败: {e}")),
    }

    set_service_acl()
}

/// 用 sc.exe 设 DACL。windows-service 没有暴露 sdset，而这一步是「免 UAC」的
/// 唯一来源，不能省。
fn set_service_acl() -> Result<(), String> {
    // 同样要静默：打包版是 GUI 子系统进程（无控制台），裸 spawn sc.exe 会闪一个黑框。
    // 本函数只在「安装服务」那一下跑，但用户看到的就是装个服务黑框一闪
    let out = crate::util::quiet(Command::new("sc.exe"))
        .args(["sdset", SERVICE_NAME, SERVICE_SDDL])
        .output()
        .map_err(|e| format!("调用 sc.exe 失败: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "设置服务权限失败（之后每次启停都会弹 UAC）: {} {}",
            stdout.trim(),
            stderr.trim()
        ));
    }
    Ok(())
}

/// 卸载服务：先停（失败无所谓）再删。未安装视为成功（幂等）。
pub fn uninstall() -> Result<(), String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = match mgr.open_service(SERVICE_NAME, ServiceAccess::DELETE | ServiceAccess::STOP)
    {
        Ok(s) => s,
        Err(_) => return Ok(()), // 没装过 → 已经是目标状态
    };
    let _ = service.stop();
    service.delete().map_err(|e| format!("删除服务失败: {e}"))
}

// 注：没有 `is_installed()`。它曾在这里，但当前没有任何调用方——「服务可不可用」
// 这件事主程序是从管道调用成不成功直接得出的（见 net/runtime.rs 的降级分支），
// 多一个独立的探测器只会多一条可能与事实不符的判据。等 TUN 开关真的需要区分
// 「没装」与「装了没起」时再加，那时它才知道自己该回答什么。

/// 拉起服务（应用启动时调用）。未安装 → Err，调用方据此走降级。
pub fn start_service() -> Result<(), String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(SERVICE_NAME, ServiceAccess::START)
        .map_err(|e| format!("打开服务失败（可能未安装）: {e}"))?;
    match service.start(&[] as &[&std::ffi::OsStr]) {
        Ok(()) => Ok(()),
        // 已在运行 → 幂等成功，不是错误
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1056) => Ok(()),
        Err(e) => Err(format!("启动服务失败: {e}")),
    }
}

/// `ERROR_SERVICE_EXISTS`(1073) / `ERROR_SERVICE_MARKED_FOR_DELETE`(1072)。
fn is_already_exists(e: &windows_service::Error) -> bool {
    matches!(
        e,
        windows_service::Error::Winapi(io) if matches!(io.raw_os_error(), Some(1073) | Some(1072))
    )
}

/// 服务的安装与运行状态：`(已安装, 运行中)`。
///
/// 查询**不需要管理员权限**——SCM 的 CONNECT 与 QUERY_STATUS 对交互用户开放。
///
/// 为什么现在才加（此前刻意不加，见上方注释）：设置页要区分「没装」与「装了没起」，
/// 两者该给的入口不同——没装给「安装」，装了没起多半是被人手工停了，该给的是
/// 「启动」或「卸载」。只靠管道通不通分不出这两者。
pub fn query_state() -> (bool, bool) {
    let Ok(mgr) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return (false, false);
    };
    let Ok(svc) = mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) else {
        return (false, false);
    };
    let running = svc
        .query_status()
        .map(|s| s.current_state == ServiceState::Running)
        .unwrap_or(false);
    (true, running)
}

/// 以管理员身份拉起本 exe 的提权分支（`--install-service` / `--uninstall-service`）。
///
/// 这是**全程唯一一次 UAC**：装完之后服务 DACL 允许交互用户启停，开关 TUN 再不弹窗。
/// 用 `ShellExecuteW` + `"runas"` 而不是自建提权——这是 Win32 唯一受支持的方式，
/// 失败时的返回值也比自建方案可读。
///
/// **不等它退出**：UAC 窗口是模态的，等下去会把主程序的主线程卡住。调用方靠轮询
/// [`query_state`] 看结果，而不是假定成功。
pub fn run_elevated(flag: &str) -> Result<(), String> {
    let exe = own_exe()?;
    let op = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(flag);
    let rc = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 的返回值是 HINSTANCE（指针宽度），它用「≤ 32 即失败」表示错误——
    // 16 位 Windows 的遗留约定，但至今有效。最常见的是 5：用户点了「否」。
    let code = rc as isize;
    if code <= 32 {
        return Err(format!(
            "拉起提权助手失败（ShellExecuteW 返回 {code}）。若刚才在 UAC 窗口点了「否」，就是它"
        ));
    }
    Ok(())
}

/// `SW_SHOWNORMAL`：正常显示窗口。写常量而不是再开一个 windows-sys feature——
/// 提权助手是个立刻退出的控制台进程，窗口怎么显示无关紧要。
const SW_SHOWNORMAL: i32 = 1;

fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
