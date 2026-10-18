//! 跨模块共享的进程辅助。
//!
//! 打包后的应用是 GUI 子系统进程（main.rs `windows_subsystem="windows"`，
//! 无控制台）；此时 spawn console 子进程（powershell/tasklist/mihomo/icacls）
//! 会为每个子进程新建 cmd 黑窗口。统一加 `CREATE_NO_WINDOW` 抑制。
//! 开发态 `cargo run` 父进程自带控制台，该标志无副作用。

use std::process::Command;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// 给子进程加 CREATE_NO_WINDOW（0x0800_0000），返回原命令便于链式。
pub fn quiet(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = &cmd;
    cmd
}

/// 远端 shell 单引号转义：包裹 + 内部 `'` 转 `'\''`（POSIX 标准拼法）。
///
/// 任何**来自数据库/远端输出**的片段进命令前都要过这里。原先它只存在于
/// `server_speed/iperf.rs`，于是同一个仓里一半的拼接带引号、另一半裸拼
/// （relay 的接口名、servers 的地址之类）——本模块是那个「共用处」。
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 远端 stderr 的尾部（最多 500 字符，按字符不按字节截，避免切碎多字节）。
///
/// 报错要带远端原因，但 stderr 可能很长（apt/curl 的进度条）——只留尾部，
/// 原因通常在那里。原先 deploy.rs 与 relay/build.rs 各有一份。
pub(crate) fn stderr_tail(s: &str) -> String {
    s.chars()
        .rev()
        .take(500)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// 取锁并忽略毒化：持锁线程 panic 不该让整个应用连锁失效。
///
/// Rust 的 `Mutex` 在意的是「数据可能处于不一致状态」，而本仓这些锁护着的都是
/// 要么可重算、要么下一句就整体覆盖的状态（DB 连接、任务表、连接日志快照），
/// 继续用远比让**之后每个**碰该锁的命令都跟着 panic 划算。本项目已有
/// 「async 中 panic 无法 unwind → 进程 abort」的实测记录（见 jobs/mod.rs 的 worker
/// 说明），所以「一次 panic 变成全应用慢慢死掉」不是假设。
///
/// 仓内一半锁位（service/mihomo.rs、net/local_proxy.rs、service/session.rs 等）
/// 本来就是这么写的，本组函数是把另一半对齐过来，不是新语义。
pub(crate) fn lock_ignore_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `RwLock` 读侧同 [`lock_ignore_poison`]（net/mmdb.rs 的 `state.read()`）。
pub(crate) fn read_ignore_poison<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

/// `RwLock` 写侧同 [`lock_ignore_poison`]（net/mmdb.rs 的 `state.write()`）。
pub(crate) fn write_ignore_poison<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/util/mods.rs"
));
