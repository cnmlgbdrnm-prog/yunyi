//! 命名管道客户端（主程序侧）。
//!
//! 每次调用新建一条连接、发一条请求、读一条响应、关闭——服务是无状态的命令
//! 循环，长连接只会多出一份闲置句柄要维护。
//!
//! **降级信号与错误的区分是本模块的核心契约**，分界线是「有没有收到响应」：
//!
//! - **收不到响应** → 带 [`ERR_SERVICE_UNAVAILABLE`] 前缀，调用方回退直接 spawn。
//!   涵盖未装、未启动，**以及服务把我们拒了**（身份校验不通过会直接断开，见
//!   service/pipe.rs）。被拒也归这一类的理由：此时硬失败会让「开启网络」整体
//!   不可用，而系统代理模式本来完全能工作——实测错误码 233（ERROR_PIPE_NOT_CONNECTED）
//!   就是这个形态。TUN 是否可用另由 `NetStatus.via_service` 表达，用户不会被蒙在鼓里。
//! - **收到响应但内容是失败，或响应看不懂** → 一律透出，不得降级——那是服务在工作
//!   却出了问题，降级会把它伪装成「没装服务」，永远查不出来。
//!
//! 输入输出：Request → Response；无状态。
//! 依赖：windows-sys、本模块 protocol。

use super::protocol::{decode_response, encode, Request, Response};

/// 服务不可用（未装 / 未启动 / 拒绝了我们）——调用方据此走降级路径。
pub const ERR_SERVICE_UNAVAILABLE: &str = "服务不可用";

/// 该错误是否代表「服务不可用、可降级」。
pub fn is_unavailable(msg: &str) -> bool {
    msg.starts_with(ERR_SERVICE_UNAVAILABLE)
}

/// 服务**答了话**、但答的是「不认识这条命令」——即服务是旧构建。
///
/// 这是 [`is_unavailable`] 之外的**第二种可降级情形**，且它不走 `Err` 通道：
/// 旧服务的 `decode` 失败会回一条 `Response::Failed`，所以调用方拿到的是 `Ok`。
/// 把它当普通失败会连累「开启网络」整体失败，而正确的处置与「服务不可达」一样——
/// 降级，并如实告诉用户「要让这个功能生效得重新安装服务」。
///
/// 判据是 `protocol::decode` 写下的固定前缀，不是措辞比对：改措辞就会同时改常量，
/// 有 `protocol_tests` 钉着。
pub fn is_unknown_command(msg: &str) -> bool {
    msg.starts_with(crate::service::protocol::ERR_UNKNOWN_COMMAND)
}

/// 统一的「服务不可用」错误：前缀供 [`is_unavailable`] 判定，
/// 后缀保留具体错误码与原因——降级是静默的，不留线索就没法排查。
fn unavailable(reason: &str) -> String {
    format!("{ERR_SERVICE_UNAVAILABLE}: {reason}")
}

pub fn call(req: &Request) -> Result<Response, String> {
    call_named(super::paths::PIPE_NAME, req)
}

/// 指定管道名的调用入口。存在的唯一理由是**可测**：测试要打一个不可能存在的
/// 管道名来验证降级路径，而直接打真实管道名会让测试依赖机器状态（装没装服务）。
pub fn call_named(pipe: &str, req: &Request) -> Result<Response, String> {
    #[cfg(windows)]
    {
        imp::call(pipe, req)
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe, req);
        Err(format!("{ERR_SERVICE_UNAVAILABLE}: 仅 Windows 支持"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    // GENERIC_READ/WRITE 在 Foundation 而非 FileSystem——windows-sys 的常量归类和
    // 直觉不一致，导入报错时先查一眼实际位置
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, OPEN_EXISTING,
    };

    /// 服务正忙（管道实例全被占用）——只有这一种错误值得重试，其余重试没用。
    /// 用字面量而非 windows-sys 常量，免得为一个数字再开一个 feature。
    const ERROR_PIPE_BUSY: u32 = 231;

    /// 响应上限：状态类响应只有几百字节，1 MB 是极大的余量。
    const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

    /// 撞上没有空闲实例时重试几次再放弃——服务是单线程循环，并发调用会撞上。
    const BUSY_RETRIES: u32 = 3;
    const BUSY_RETRY_MS: u64 = 100;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn open(pipe: &str) -> Result<Handle, String> {
        let name = wide(pipe);
        let mut last = 0u32;
        for _ in 0..BUSY_RETRIES {
            unsafe {
                let h = CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if h != INVALID_HANDLE_VALUE {
                    return Ok(Handle(h));
                }
                last = GetLastError();
            }
            if last != ERROR_PIPE_BUSY {
                break; // 只有「忙」值得重试，其余错误重试没用
            }
            std::thread::sleep(std::time::Duration::from_millis(BUSY_RETRY_MS));
        }
        // 连不上就是连不上——管它是没装（2）、全忙（231）还是被 DACL 挡了（5），
        // 对本进程而言都是「这个服务用不了」，都该降级（见模块头）
        Err(unavailable(&format!("连接管道失败（错误码 {last}）")))
    }

    pub fn call(pipe: &str, req: &Request) -> Result<Response, String> {
        let handle = open(pipe)?;
        let body = encode(req);
        let mut written = 0u32;
        unsafe {
            if WriteFile(
                handle.0,
                body.as_ptr() as *const _,
                body.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) == 0
            {
                // 刚连上就被断开：身份校验拒绝的形态（service/pipe.rs 先校验后读载荷）。
                // 这是「这个服务不让我们用」，不是「服务坏了」——降级。
                return Err(unavailable(&format!(
                    "写管道失败（错误码 {}）",
                    GetLastError()
                )));
            }
        }

        // 必须先读完响应再关句柄：消息模式管道会丢弃未读数据
        let mut buf = vec![0u8; MAX_RESPONSE_BYTES];
        let mut read = 0u32;
        unsafe {
            if ReadFile(
                handle.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
            {
                // 同上：拿不到响应就是「用不了」，可降级。
                // 真正的服务端故障会以 `Response::Failed` 的形式**答话**，那条不降级。
                return Err(unavailable(&format!(
                    "读管道失败（错误码 {}）",
                    GetLastError()
                )));
            }
        }
        let text = String::from_utf8(buf[..read as usize].to_vec())
            .map_err(|e| format!("响应不是 UTF-8: {e}"))?;
        decode_response(&text)
    }
}
