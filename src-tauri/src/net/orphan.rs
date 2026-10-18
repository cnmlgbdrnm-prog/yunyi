//! 旧版本遗留进程恢复：完整参数与路径匹配，仍有活父进程的一律保留。
//!
//! 返回 Err 只有两种情形——枚举本身失败，或**确证**有存活父进程持有它（那条提示是
//! 可行动的：去关掉那个实例）。其余核验失败（拒绝访问、读不到创建时间、PID 被复用后
//! 认不出）一律跳过 + 一行日志：**核验不了不等于别人在用**，拿它拦停「开启网络」
//! 等于让用户为一个自己无从下手的理由卡死。

/// 解析 Windows 的引号/反斜线规则，不用空格切分或命令行子串判断归属。
pub fn arguments(command: &str) -> Option<Vec<String>> {
    let mut chars = command.chars().peekable();
    let mut args = Vec::new();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&ch) = chars.peek() {
            if !quoted && (ch == ' ' || ch == '\t') {
                break;
            }
            let mut slashes = 0;
            while chars.peek() == Some(&'\\') {
                chars.next();
                slashes += 1;
            }
            if chars.peek() == Some(&'"') {
                arg.extend(std::iter::repeat_n('\\', slashes / 2));
                chars.next();
                if slashes % 2 == 1 {
                    arg.push('"');
                } else if quoted && chars.peek() == Some(&'"') {
                    chars.next();
                    arg.push('"');
                } else {
                    quoted = !quoted;
                }
            } else {
                arg.extend(std::iter::repeat_n('\\', slashes));
                if let Some(&next) = chars.peek() {
                    if !quoted && (next == ' ' || next == '\t') {
                        break;
                    }
                    arg.push(next);
                    chars.next();
                }
            }
        }
        if quoted {
            return None;
        }
        args.push(arg);
    }
    Some(args)
}

pub fn normalized_path(path: &str) -> Option<String> {
    let path = path.replace('/', "\\").to_lowercase();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    // 当前产生的路径为绝对盘符或 UNC；不猜测相对路径的工作目录。
    let absolute = path.as_bytes().get(1..3) == Some(b":\\") || path.starts_with(r"\\");
    if !absolute {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() <= 1 {
                    return None;
                }
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    Some(format!(
        "{}{}",
        if path.starts_with(r"\\") { r"\\" } else { "" },
        parts.join("\\")
    ))
}

/// 父进程核验结论：决定这只自家 mihomo 能不能回收。
///
/// 三态而不是布尔：**「确证有活父进程」与「核验不了」是两件事**。前者说明另一实例
/// 正在用它，报错是可行动的（用户能去关那个实例）；后者只说明我们没资格动它，
/// 拿它去拦停「开启网络」等于让用户为一个自己无从下手的理由卡死。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentVerdict {
    /// 父进程已不在（或该 PID 已被晚于子进程创建的进程复用）→ 可以回收
    Reap,
    /// 确证父进程仍活着且早于子进程 → 不动，且要把原因报给用户
    Blocked,
    /// 核验不了（拒绝访问、读不到创建时间等）→ 不动，但不阻断（记一行日志）
    Unverifiable,
}

/// 纯函数：`parent` 是「打开父进程并读出（是否存活、创建时间）」的结果，
/// `Ok(None)` = 该 PID 已不存在。抽成纯函数是为了能穷举四类输入，不必真的
/// 去构造一个拒绝访问的父进程。
pub fn parent_verdict(
    parent: &Result<Option<(bool, u64)>, String>,
    child_created: u64,
) -> ParentVerdict {
    match parent {
        Ok(None) => ParentVerdict::Reap,
        Ok(Some((alive, created))) if *alive && *created <= child_created => ParentVerdict::Blocked,
        Ok(Some(_)) => ParentVerdict::Reap,
        Err(_) => ParentVerdict::Unverifiable,
    }
}

pub fn command_matches(command: &str, directory: &str) -> bool {
    let Some(args) = arguments(command) else {
        return false;
    };
    if args
        .first()
        .and_then(|v| v.rsplit(['\\', '/']).next())
        .is_none_or(|v| !v.eq_ignore_ascii_case("mihomo.exe"))
    {
        return false;
    }
    if args
        .iter()
        .filter(|a| *a == "-d" || a.starts_with("-d="))
        .count()
        != 1
    {
        return false;
    }
    let values: Vec<_> = args
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            if a == "-d" {
                args.get(i + 1).map(String::as_str)
            } else {
                a.strip_prefix("-d=")
            }
        })
        .collect();
    let expected = normalized_path(directory);
    values.len() == 1 && expected.is_some() && normalized_path(values[0]) == expected
}

/// 回收 `targets` 里镜像与 `-d` 都对得上的自家 mihomo，返回实际清掉的条数。
///
/// `exclude` 里的 PID 直接跳过（本实例正在使用的子进程）。错误契约见模块头。
#[cfg(windows)]
pub fn cleanup(
    targets: &[(std::path::PathBuf, std::path::PathBuf)],
    exclude: &[u32],
) -> Result<usize, String> {
    use crate::service::process::ProcessRef;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    fn snapshot() -> Result<Vec<(u32, u32)>, String> {
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw.is_null() || raw as isize == -1 {
            return Err(format!(
                "枚举残留进程失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let snap = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut rows = Vec::new();
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut more = unsafe { Process32FirstW(snap.as_raw_handle(), &mut entry) };
        while more != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("mihomo.exe")
            {
                rows.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }
            more = unsafe { Process32NextW(snap.as_raw_handle(), &mut entry) };
        }
        Ok(rows)
    }
    let mut removed = 0;
    for (pid, _) in snapshot()? {
        if exclude.contains(&pid) {
            continue;
        }
        // 先只读身份，第三方或无权查看的 SYSTEM 进程不要求终止权限。
        let Ok(Some(identity)) = ProcessRef::open(pid, false) else {
            continue;
        };
        if !identity.alive().map_err(|e| e.to_string())? {
            continue;
        }
        let Ok(image) = identity.image() else {
            continue;
        };
        let Some((_, dir)) = targets
            .iter()
            .find(|(exe, _)| normalized_path(&exe.to_string_lossy()) == normalized_path(&image))
        else {
            continue;
        };
        // 身份核验失败一律**跳过**（不动这只进程，也不阻断）：核验不了只说明我们
        // 没资格动它，不是「别人正在用」的证据——拿它拦停「开启网络」等于让用户
        // 为一个自己无从下手的理由卡死。
        let Ok(command) = identity.command_line() else {
            eprintln!("[orphan] 跳过无法核验命令行的进程 {pid}");
            continue;
        };
        if !command_matches(&command, &dir.to_string_lossy()) {
            continue;
        }
        let Ok(created) = identity.created() else {
            eprintln!("[orphan] 跳过无法核验创建时间的进程 {pid}");
            continue;
        };
        // 已持有进程句柄，再取一次父 PID，避免第一次快照后 PID 已被复用。
        let Some((_, parent_pid)) = snapshot()?.into_iter().find(|r| r.0 == pid) else {
            continue;
        };
        let parent = ProcessRef::open(parent_pid, false)
            .map_err(|e| e.to_string())
            .and_then(|p| match p {
                None => Ok(None),
                Some(p) => Ok(Some((
                    p.alive().map_err(|e| e.to_string())?,
                    p.created().map_err(|e| e.to_string())?,
                ))),
            });
        match parent_verdict(&parent, created) {
            ParentVerdict::Reap => {}
            ParentVerdict::Blocked => {
                return Err(format!(
                    "代理进程 {pid} 仍由存活进程 {parent_pid} 持有，请先关闭原实例"
                ))
            }
            ParentVerdict::Unverifiable => {
                eprintln!("[orphan] 跳过无法核验父进程的自家进程 {pid}（不动它，也不阻断开启）");
                continue;
            }
        }
        let Ok(Some(process)) = ProcessRef::open(pid, true) else {
            continue;
        };
        if process.created().ok() != Some(created) {
            continue; // 句柄到手之间 PID 已被复用，换来的不是原来那只
        }
        if let Err(e) = process.terminate_and_wait() {
            // 确证是自家孤儿却杀不掉（权限/保护）：记一行，不阻断——它占的端口由
            // 候选端口扫描兜底，用户没有第二个能做的动作。
            eprintln!("[orphan] 清理自家孤儿进程 {pid} 失败: {e}");
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(not(windows))]
pub fn cleanup(
    _targets: &[(std::path::PathBuf, std::path::PathBuf)],
    _exclude: &[u32],
) -> Result<usize, String> {
    Ok(0)
}
