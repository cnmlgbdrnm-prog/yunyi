//! 命名管道服务端。
//!
//! 威胁模型：命名管道**任何本地进程都能连**，而本服务以 SYSTEM 运行。若不校验
//! 调用方，任意本地进程都能指挥 SYSTEM 起进程——这是教科书级的本地提权。
//! 故每次连接先取客户端 PID（GetNamedPipeClientProcessId），比对其 exe 路径
//! 是否与服务自身相同。
//!
//! 为什么「同一个 exe」是足够的判据：服务与应用是同一个二进制（见 service/mod.rs），
//! 攻击者要伪造得像，就得在本机放一份路径一致的副本——那需要管理员权限才能写到
//! 安装目录，而有了管理员权限本来就不需要提权。
//!
//! 输入输出：`serve_until` 在独立线程里阻塞接受连接（见 host.rs）。
//! 依赖：windows-sys（管道 + 进程镜像名 + 安全描述符）、本模块 protocol 与 mihomo。

/// 单条请求的最大字节数。配置 YAML 可能上百 KB，留足余量但必须有上限，
/// 否则一个不出数据的客户端能把服务内存吃光。
#[cfg(windows)]
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// `CreateNamedPipeW` 的 `nDefaultTimeOut`。**只对客户端的 `WaitNamedPipe` 生效**，
/// 对服务端的 `ConnectNamedPipe` 无效——后者在阻塞模式下无限期等待。
/// 保留一个值只是让参数有意义；别指望它能把 `ConnectNamedPipe` 叫醒
/// （那个错误的想法曾让服务收到停止请求后永远停不下来，见 host.rs）。
#[cfg(windows)]
const PIPE_DEFAULT_TIMEOUT_MS: u32 = 200;

/// 管道的 DACL：System 与管理员全权，**交互用户可读写**。
///
/// 必须显式给。安全属性传 NULL 时管道继承创建者（SYSTEM）的默认 DACL——只有
/// SY 与 BA，**连主程序自己都连不上**。实测报 `Access is denied`，而主程序把
/// 错误码 5 归为「连接失败」而非「服务不可用」（见 client.rs），于是不降级、
/// 「开启网络」整体失败；设计里真正的安全边界（比对调用方 exe 路径）连执行
/// 的机会都没有，成了死代码。
///
/// 授给 IU 而非某个具体 SID：与安装时给 IU 授服务启停是同一个取舍（见 install.rs
/// 的 SERVICE_SDDL）——DACL 只负责把人放行到身份校验那一步，判断在那里做。
#[cfg(windows)]
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

#[cfg(windows)]
pub use imp::serve_until;
// 身份校验只在服务端内部与测试里用（管道循环自己调用），生产侧无需外露
#[cfg(all(test, feature = "local-tests", windows))]
pub use imp::current_exe_matches;

#[cfg(not(windows))]
pub fn serve_until() {}

#[cfg(not(windows))]
pub fn current_exe_matches(_pid: u32) -> Result<bool, String> {
    Err("命名管道仅 Windows 可用".to_string())
}

#[cfg(windows)]
mod imp {
    use super::{MAX_REQUEST_BYTES, PIPE_DEFAULT_TIMEOUT_MS, PIPE_SDDL};
    use crate::service::protocol::{decode, encode_response, Response};
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    /// `ERROR_PIPE_CONNECTED`：客户端在 `ConnectNamedPipe` 之前就连上了。
    /// 这是**成功**而非失败——当成失败会让服务随机漏掉连接。
    const ERROR_PIPE_CONNECTED: u32 = 535;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// 从 SDDL 建好的安全属性；析构时释放内部分配的安全描述符。
    ///
    /// `CreateNamedPipeW` 会把安全描述符**拷进管道对象**，故建完即可释放。
    struct PipeSecurity {
        attrs: SECURITY_ATTRIBUTES,
        descriptor: PSECURITY_DESCRIPTOR,
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            if !self.descriptor.is_null() {
                unsafe { LocalFree(self.descriptor as HLOCAL) };
            }
        }
    }

    /// 按 `PIPE_SDDL` 建安全属性。SDDL 非法时返回 None——那时**不能**退回 NULL
    /// 安全属性：那会建出一条谁都连不上的管道，故障会以「开启网络莫名其妙失败」
    /// 的形态出现，极难定位。宁可在这里明确停住。
    fn pipe_security() -> Option<PipeSecurity> {
        let sddl = wide(PIPE_SDDL);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            return None;
        }
        Some(PipeSecurity {
            attrs: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
            descriptor,
        })
    }

    /// 管道句柄的 RAII 包装：任何提前 return 都要断开并关闭，否则实例泄漏，
    /// 泄漏够多之后 `CreateNamedPipeW` 会直接失败。
    struct PipeHandle(HANDLE);

    impl Drop for PipeHandle {
        fn drop(&mut self) {
            unsafe {
                DisconnectNamedPipe(self.0);
                CloseHandle(self.0);
            }
        }
    }

    /// 身份核验和会话归属共享同一进程句柄，避免核验后再按 PID 重开带来的复用窗口。
    fn authenticated_client(
        pid: u32,
    ) -> Result<Option<crate::service::process::ProcessRef>, String> {
        let caller = crate::service::process::ProcessRef::open(pid, false)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "调用方已退出".to_string())?;
        let mine = std::env::current_exe().map_err(|e| e.to_string())?;
        let actual = caller.image().map_err(|e| e.to_string())?;
        Ok(
            (crate::net::orphan::normalized_path(&mine.to_string_lossy())
                == crate::net::orphan::normalized_path(&actual))
            .then_some(caller),
        )
    }

    #[cfg(all(test, feature = "local-tests"))]
    pub fn current_exe_matches(pid: u32) -> Result<bool, String> {
        authenticated_client(pid).map(|caller| caller.is_some())
    }

    fn connect(handle: HANDLE) -> bool {
        unsafe {
            if ConnectNamedPipe(handle, std::ptr::null_mut()) != 0 {
                return true;
            }
            GetLastError() == ERROR_PIPE_CONNECTED
        }
    }

    fn read_request(handle: HANDLE) -> Result<String, String> {
        let mut buf = vec![0u8; MAX_REQUEST_BYTES];
        let mut read = 0u32;
        unsafe {
            let ok = ReadFile(
                handle,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!("读管道失败: 错误码 {}", GetLastError()));
            }
        }
        String::from_utf8(buf[..read as usize].to_vec()).map_err(|e| format!("请求不是 UTF-8: {e}"))
    }

    fn write_response(handle: HANDLE, body: &str) -> Result<(), String> {
        let mut written = 0u32;
        unsafe {
            let ok = WriteFile(
                handle,
                body.as_ptr() as *const _,
                body.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(format!("写管道失败: 错误码 {}", GetLastError()));
            }
        }
        Ok(())
    }

    /// 接受连接、分发请求。**永不返回**——停止走的是「进程退出」，见 host.rs。
    ///
    /// `ConnectNamedPipe` 在阻塞模式下会无限期等待，本循环没有任何可观察的退出点；
    /// 之所以能这样写，是因为 host.rs 把它放在独立线程上，停止时主线程直接上报
    /// STOPPED 让进程结束，管道句柄由内核回收。
    pub fn serve_until() {
        let Some(security) = pipe_security() else {
            eprintln!("[yunyi-service] 构造管道安全描述符失败，服务不可用");
            return;
        };
        loop {
            unsafe {
                // 消息模式（PIPE_TYPE_MESSAGE + PIPE_READMODE_MESSAGE）：每次
                // ReadFile 正好返回一条消息，省掉自己做长度前缀分帧；字节模式下
                // 必须自己分帧，否则上百 KB 的配置会被截断
                // 参数顺序别记混：openMode（访问方向）与 pipeMode（分帧方式）是两个
                // 独立参数，合并成一个会静默建出行为不对的管道
                let handle = CreateNamedPipeW(
                    wide(crate::service::paths::PIPE_NAME).as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    MAX_REQUEST_BYTES as u32,
                    MAX_REQUEST_BYTES as u32,
                    PIPE_DEFAULT_TIMEOUT_MS,
                    &security.attrs,
                );
                if handle == INVALID_HANDLE_VALUE {
                    eprintln!("[yunyi-service] 创建管道失败: {}", GetLastError());
                    return;
                }
                let handle = PipeHandle(handle);

                if !connect(handle.0) {
                    continue; // 没人连上（被打断），下一轮
                }

                // 先校验身份、再读载荷：反过来的话攻击者只要不发货就能占住线程
                let mut pid = 0u32;
                if GetNamedPipeClientProcessId(handle.0, &mut pid) == 0 {
                    continue;
                }
                let caller = match authenticated_client(pid) {
                    Ok(Some(caller)) => caller,
                    // 身份不符：直接断开、**不回任何响应**，不给探测者反馈
                    Ok(None) => continue,
                    Err(e) => {
                        eprintln!("[yunyi-service] 身份校验失败(pid={pid}): {e}");
                        continue;
                    }
                };

                let raw = match read_request(handle.0) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[yunyi-service] {e}");
                        continue;
                    }
                };
                let resp = match decode(&raw) {
                    Ok(req) => crate::service::session::handle(&req, caller),
                    Err(e) => Response::Failed { message: e },
                };
                let _ = write_response(handle.0, &encode_response(&resp));
                // PipeHandle 的 Drop 负责 Disconnect + Close
            }
        }
    }
}
