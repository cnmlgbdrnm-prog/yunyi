//! 服务自有目录的权限加固：建成**只有 SYSTEM 与管理员可写**，且不继承父目录的 ACE。
//!
//! 为什么需要：`%ProgramData%` 的默认继承给 `BUILTIN\Users` 授了 **Write**。这不是
//! 推断——实测（非提权账户 ASHURAFL\asus）确实能在 `%ProgramData%\yunyi` 下建出
//! 子目录。于是有预创建攻击：抢在服务之前把 `bin` 建出来，创建者作为 CREATOR OWNER
//! 拿到该目录的 FullControl，服务随后把 `mihomo.exe` 写进去，换掉它就是 SYSTEM 代码执行。
//!
//! 所以「这个目录归服务管」必须由 DACL 兑现，不能假设它默认成立。
//!
//! 与管道 DACL（pipe.rs）取向不同：那里要给 IU 留一条「进得来、由身份校验兜底」的
//! 路；服务自有的目录没有这个需要，对外一扇门都不开。
//!
//! 输入输出：`ensure(&Path) -> Result<(), String>`，幂等。
//! 依赖：windows-sys 的 CreateDirectoryW / SetFileSecurityW。

use std::path::Path;

use windows_sys::Win32::Foundation::{GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    SetFileSecurityW, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

/// 目录 DACL。`P` = 受保护（不继承父目录的 ACE），`OICI` = 子容器与子对象都继承。
///
/// 只授 SY（SYSTEM）与 BA（内建管理员）：服务以 SYSTEM 跑，这两个身份覆盖它的全部
/// 需要。多授一个身份，就多一个能把 `mihomo.exe` 换成自己的人。
///
/// **故意不写 `O:`**：新建时所有者落成创建者，而服务建就是 SYSTEM——同样不是
/// 普通用户改得动的。硬钉 `O:BA` 反而会让任何非 SYSTEM 的调用者（比如测试进程）
/// 直接拿到 `ERROR_INVALID_OWNER`，白白丢掉一层可用性。
pub(super) const DIR_SDDL: &str = "D:PAI(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)";

/// 连所有者一起钉成内建管理员。**只用于目录已经存在**的场合。
///
/// 为什么这一路必须夺所有者：Windows 给对象所有者保留隐式的 `WRITE_DAC`。如果这个
/// 目录是被人抢先建出来的，他只换 DACL 是赶不走的——他随时能改回来，再换掉里面
/// 待执行的程序。夺所有者要 `SeRestorePrivilege`，所以这一路只有以 SYSTEM 跑的
/// 服务做得成（见 [`ensure`] 的降级分支）。
pub(super) fn owner_pinned_sddl() -> String {
    format!("O:BA{DIR_SDDL}")
}

/// 从 SDDL 建出的安全描述符，析构时释放。
pub(super) struct Sd(PSECURITY_DESCRIPTOR);

impl Drop for Sd {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

pub(super) fn sd_from(sddl: &str) -> Result<Sd, String> {
    let w = wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        // 不能退回「不带安全属性」继续——那建出来的是继承来的宽松权限，
        // 故障会以「加固悄悄没生效」的形态出现，比直接失败难查得多
        return Err(format!(
            "安全描述符构造失败（{sddl}）: 错误码 {}",
            unsafe { GetLastError() }
        ));
    }
    Ok(Sd(sd))
}

/// 确保 `path` 存在，且其 DACL 就是 [`DIR_SDDL`]。
///
/// **不信继承**：目录已存在时（上一版建的、或被人抢先建的）照样重设一遍 DACL，
/// 以它当前的实际权限为准。设置失败即 Err——那一刻起这个目录里放的东西就不再是
/// 「只有管理员能换」，继续往里铺要执行的程序等于把提权口子留着。
///
/// 幂等：目录与权限都已经是目标状态时，两个分支都是空操作。
pub(super) fn ensure(path: &Path) -> Result<(), String> {
    let w = wide(&path.to_string_lossy());

    // 先建。带安全属性建，让「创建目录」与「设 DACL」落在同一个动作里，
    // 中间不留一个「已存在但权限还没设」的窗口。
    let create_sd = sd_from(DIR_SDDL)?;
    let created = unsafe {
        CreateDirectoryW(
            w.as_ptr(),
            &SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: create_sd.0,
                bInheritHandle: 0,
            },
        )
    };
    if created != 0 {
        return Ok(());
    }

    let err = unsafe { GetLastError() };
    if err != ERROR_ALREADY_EXISTS {
        return Err(format!("创建 {} 失败: 错误码 {err}", path.display()));
    }

    // 已存在：可能是上一版用继承权限建的，也可能被人抢先建的。连所有者一起重设。
    let sd = sd_from(&owner_pinned_sddl())?;
    let ok = unsafe {
        SetFileSecurityW(
            w.as_ptr(),
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            sd.0,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let owner_err = unsafe { GetLastError() };

    // 夺所有者要 SeRestorePrivilege。SYSTEM 持有它，但持有不等于默认启用。
    // 退一步只设 DACL：那已经堵住主路（普通用户往里写不了东西），残留的只是
    // 「抢先建目录的人作为所有者能改回来」这条窄路。这一层降级要出声，不静默。
    let dacl_sd = sd_from(DIR_SDDL)?;
    if unsafe { SetFileSecurityW(w.as_ptr(), DACL_SECURITY_INFORMATION, dacl_sd.0) } != 0 {
        eprintln!(
            "[yunyi-service] {} 的所有者没能钉到管理员（错误码 {owner_err}），本次只设了 DACL",
            path.display()
        );
        return Ok(());
    }
    Err(format!(
        "设置 {} 的权限失败: 错误码 {}。该目录必须只有管理员可写，\
         否则里面要执行的程序可以被替换掉",
        path.display(),
        unsafe { GetLastError() }
    ))
}

fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
