//! 持有真实进程句柄核验身份；在 CreateProcess 时原子加入 kill-on-close Job。
//! Job 句柄不继承给子进程，服务被强杀时内核也会回收 mihomo。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;

pub struct ProcessRef {
    handle: OwnedHandle,
    pub pid: u32,
}

impl ProcessRef {
    /// 不存在与无权查询必须区分：后者不能成为终止另一个会话的依据。
    pub fn open(pid: u32, terminate: bool) -> io::Result<Option<Self>> {
        let access = PROCESS_QUERY_LIMITED_INFORMATION
            | PROCESS_SYNCHRONIZE
            | if terminate { PROCESS_TERMINATE } else { 0 };
        let raw = unsafe { OpenProcess(access, 0, pid) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(Self {
            handle: unsafe { OwnedHandle::from_raw_handle(raw) },
            pid,
        }))
    }

    pub fn alive(&self) -> io::Result<bool> {
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(true),
            WAIT_OBJECT_0 => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn created(&self) -> io::Result<u64> {
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        let ok = unsafe {
            GetProcessTimes(
                self.handle.as_raw_handle(),
                &mut times[0],
                &mut times[1],
                &mut times[2],
                &mut times[3],
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }

    pub fn image(&self) -> io::Result<String> {
        let mut buf = vec![0u16; 32768];
        let mut size = buf.len() as u32;
        if unsafe {
            QueryFullProcessImageNameW(self.handle.as_raw_handle(), 0, buf.as_mut_ptr(), &mut size)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(String::from_utf16_lossy(&buf[..size as usize]))
    }

    pub fn command_line(&self) -> io::Result<String> {
        use windows_sys::Wdk::System::Threading::{
            NtQueryInformationProcess, ProcessCommandLineInformation,
        };
        #[repr(C)]
        struct UnicodeString {
            length: u16,
            maximum: u16,
            buffer: *const u16,
        }
        let mut raw = vec![0usize; 32768];
        let mut needed = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                self.handle.as_raw_handle(),
                ProcessCommandLineInformation,
                raw.as_mut_ptr().cast(),
                (raw.len() * std::mem::size_of::<usize>()) as u32,
                &mut needed,
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "读取进程命令行失败: NTSTATUS {status:#x}"
            )));
        }
        let value = unsafe { &*raw.as_ptr().cast::<UnicodeString>() };
        let start = raw.as_ptr() as usize;
        let ptr = value.buffer as usize;
        let end = start + raw.len() * std::mem::size_of::<usize>();
        if ptr < start
            || ptr
                .checked_add(value.length as usize)
                .is_none_or(|p| p > end)
            || value.length % 2 != 0
        {
            return Err(io::Error::other("进程命令行越界"));
        }
        Ok(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(value.buffer, value.length as usize / 2)
        }))
    }

    pub fn terminate_and_wait(&self) -> io::Result<()> {
        if !self.alive()? {
            return Ok(());
        }
        if unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) } == 0 && self.alive()? {
            return Err(io::Error::last_os_error());
        }
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 5000) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "等待代理进程退出超时",
            )),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

pub struct ManagedChild {
    process: ProcessRef,
    // 子进程不继承此句柄，父进程死亡会关闭最后一个 Job 句柄。
    _job: OwnedHandle,
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut text: Vec<u16> = value.encode_wide().collect();
    if text.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "路径包含 NUL"));
    }
    text.push(0);
    Ok(text)
}

/// Windows CRT 参数转义；始终加引号，反斜线在引号前和末尾必须翻倍。
fn quote(value: &OsStr) -> io::Result<Vec<u16>> {
    let value = wide(value)?;
    let mut out = vec![34];
    let mut slashes = 0;
    for &ch in &value[..value.len() - 1] {
        if ch == 92 {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            92,
            if ch == 34 { slashes * 2 + 1 } else { slashes },
        ));
        out.push(ch);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(92, slashes * 2));
    out.push(34);
    Ok(out)
}

impl ManagedChild {
    pub fn spawn(exe: &Path, dir: &Path, args: &[&OsStr]) -> io::Result<Self> {
        let application = wide(exe.as_os_str())?;
        let directory = wide(dir.as_os_str())?;
        let mut command = quote(exe.as_os_str())?;
        for arg in args {
            command.push(32);
            command.extend(quote(arg)?);
        }
        command.push(0);
        unsafe {
            let raw_job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if raw_job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = OwnedHandle::from_raw_handle(raw_job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                raw_job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
            let attributes = buffer.as_mut_ptr().cast();
            if InitializeProcThreadAttributeList(attributes, 1, 0, &mut size) == 0 {
                return Err(io::Error::last_os_error());
            }
            struct Attributes(LPPROC_THREAD_ATTRIBUTE_LIST);
            impl Drop for Attributes {
                fn drop(&mut self) {
                    unsafe { DeleteProcThreadAttributeList(self.0) };
                }
            }
            let _attributes = Attributes(attributes);
            if UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                (&raw_job as *const windows_sys::Win32::Foundation::HANDLE).cast(),
                std::mem::size_of_val(&raw_job),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
            startup.lpAttributeList = attributes;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            // JOB_LIST 在创建进程时生效，没有 spawn 后再 Assign 的孤儿窗口。
            if CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                directory.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _thread = OwnedHandle::from_raw_handle(info.hThread);
            Ok(Self {
                process: ProcessRef {
                    handle: OwnedHandle::from_raw_handle(info.hProcess),
                    pid: info.dwProcessId,
                },
                _job: job,
            })
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.process.alive()? {
            return Ok(None);
        }
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.process.handle.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(ExitStatus::from_raw(code)))
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.process.terminate_and_wait()
    }

    /// 服务正常停止时解除「关 Job 即杀」：Job 句柄随服务进程退出而关闭，但子进程
    /// 要继续活着占住 mixed 端口。
    ///
    /// 已运行程序的环境变量是进程启动那一刻的死快照（浏览器/终端/Claude Code
    /// 都记着 `HTTP_PROXY=127.0.0.1:7890`）。服务重启若把 mihomo 连端口一起收走，
    /// 这些程序会整片 `ECONNREFUSED`。调用方应先把 mihomo 切到直连再调用本函数；
    /// 只对**正常停止**生效——进程被强杀时不会走到这里，Job 仍会回收子进程。
    pub fn keep_alive_on_close(&self) -> io::Result<()> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                self._job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        info.BasicLimitInformation.LimitFlags &= !JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                self._job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if unsafe { WaitForSingleObject(self.process.handle.as_raw_handle(), 5000) }
            != WAIT_OBJECT_0
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "等待子进程退出超时",
            ));
        }
        self.try_wait()?
            .ok_or_else(|| io::Error::other("子进程尚未退出"))
    }
    #[cfg(all(test, feature = "local-tests"))]
    pub fn id(&self) -> u32 {
        self.process.pid
    }
}
