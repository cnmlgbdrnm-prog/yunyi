//! 提权服务宿主：向 SCM 注册派发器，跑命令循环直到收到停止请求。
//!
//! 服务与应用是**同一个 exe**：SCM 注册的命令行是 `<exe> --service`，由 main.rs
//! 的入口分流接住。理由见 service/mod.rs。
//!
//! 输入输出：`run()` 无参、无返回，阻塞到服务停止。
//! 依赖：windows-service（SCM）、本模块的 pipe 服务端。

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::paths::SERVICE_NAME;

define_windows_service!(ffi_service_main, service_main);

/// 服务入口（被 SCM 在后台线程调用）。
fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        eprintln!("[yunyi-service] 运行失败: {e}");
    }
}

fn status_for(state: ServiceState, accepted: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn run_service() -> windows_service::Result<()> {
    // 停止信号经 channel 传给命令循环
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // Interrogate 是「你还活着吗」，必须答 NoError；
            // 其余控件没实现就必须答 NotImplemented，否则 SCM 会认为服务接受了它
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

    status_handle.set_service_status(status_for(
        ServiceState::Running,
        ServiceControlAccept::STOP,
    ))?;

    // 这里**刻意不清理** mihomo：服务正常停止时会留下一个直连实例占着 mixed
    // 端口（见 `mihomo::preserve_on_service_stop`），启动期按镜像 + `-d` 清扫会
    // 把它连同用户已运行程序的环境变量指针一起打断。真正的回收推迟到
    // `StartNet`（mihomo::start）或需要完整清理的 `RecoverNet`；在那之前这只
    // 直连 mihomo 继续给旧进程供网。

    // 服务启动时清扫阻断规则（四重清扫之三）：服务活着的每一刻，主程序都可能被
    // 强杀，那一瞬间留下的规则没有任何人去撤。服务起来了就意味着「上一段会话已经
    // 结束」，此时还挂着的自家规则必然是残留——先撤干净再开始接命令。
    //
    // 放在命令循环**之前**：反过来的话，头几条命令会在一个还没扫过的规则集上跑。
    match super::mihomo::handle(&super::protocol::Request::UnblockDirect) {
        super::protocol::Response::Unblocked { removed } if removed > 0 => {
            eprintln!("[yunyi-service] 启动清扫：撤掉上次会话残留的 {removed} 条阻断规则");
        }
        super::protocol::Response::Failed { message } => {
            eprintln!("[yunyi-service] 启动清扫阻断规则失败（不影响上网）: {message}");
        }
        _ => {}
    }

    // 命令循环**必须放独立线程**：`ConnectNamedPipe` 在阻塞模式下无限期等待，
    // 循环里没有任何可观察的退出点，主线程没法「一边接受连接一边等停止信号」。
    //
    // 早先的写法是在管道循环里每轮 `try_recv` 一次停止信号，指望 200ms 的
    // `nDefaultTimeOut` 把 `ConnectNamedPipe` 叫醒——但那个值**只对客户端的
    // `WaitNamedPipe` 生效**。实测：`sc stop` 返回成功、服务却永远 RUNNING，
    // 再 stop 就是 1061，最后只能提权 taskkill。
    std::thread::spawn(super::pipe::serve_until);

    loop {
        match shutdown_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        super::session::reap_owner();
        if super::session::shutting_down() {
            break;
        }
    }

    // 正常停止**不杀 mihomo**：先切直连，再解除 Job 的「关句柄即杀」，让它继续
    // 占住 mixed 端口。服务重启时，用户已运行程序的环境变量仍指向该端口；杀了
    // 它们就整片 `ECONNREFUSED`（本机实测过）。下次 StartNet 会精确回收它。
    super::mihomo::preserve_on_service_stop();

    status_handle.set_service_status(status_for(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;

    Ok(())
}

/// 进程以服务模式启动时的入口（main.rs 分流到这里）。
pub fn run() {
    if let Err(e) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        eprintln!("[yunyi-service] 无法启动服务派发器: {e}");
        eprintln!("[提示] 本进程必须由 SCM 拉起（sc start {SERVICE_NAME}），不能手工运行");
    }
}
