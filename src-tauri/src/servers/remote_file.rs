//! 远端文件读写原语：单往返读取（区分「不存在」与「读失败」）与原子写回。
//!
//! 从 deploy.rs 拆出：该文件已顶到单文件 1500 行上限，而这两个原语同时被
//! relay::build / relay::peers / servers::privacy 复用，本就不只属于部署管道。

use crate::servers::deploy::{run_cmd, run_cmd_ok};
use crate::ssh::SshSession;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Duration;

/// 同进程内原子写的临时文件名序号（配合进程 id，跨进程也不撞车）。
static TEMP_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 远端文件读取结果：区分「文件不存在」（正常）与「读取失败」（异常）。
///
/// 为什么需要这个类型：本仓多处把「读失败」与「文件不存在」当成同一件事
/// （`relay/build.rs` 的 `read_conf` 把非零退出归成空串、`servers/privacy.rs` 的
/// `cat … 2>/dev/null || true`），后果是读失败时按「首次部署」处理——覆盖用户
/// 既有配置、轮换本不该动的密钥，且表面上什么都看不出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteFile {
    /// 文件不存在（`[ -e ]` 为假）。
    Absent,
    /// 文件存在，内容逐字带回（**空文件是 `Content("")`**，与 Absent 不同：
    /// 前者要保留其余段合并，后者才整份渲染）。
    Content(String),
}

impl RemoteFile {
    /// 内容视图：`Absent` 视作空串。
    ///
    /// 只用在「按内容决定取舍」的**下游**函数（渲染/合并/解析私钥——它们本来就把
    /// 空配置当首建处理）。**判断「文件在不在」必须显式 `matches!(.., Absent)`**：
    /// 这个访问器存在的意义只是别让下游把两个变体各写一遍，不是抹平二者差异。
    pub(crate) fn content(&self) -> &str {
        match self {
            RemoteFile::Absent => "",
            RemoteFile::Content(c) => c,
        }
    }
}

/// 读远端文件：存在则 cat（exit 0），不存在则 exit 42，其它非零 = 读失败。
///
/// **单次往返**同时取回内容与存在性，不必「先 test 再 cat」跑两条命令（也就没有
/// 两次读取之间文件被改动的窗口）。`[ -e ]` 为真但 cat 失败（权限/IO）→ cat 非零
/// → 落 Err，正是我们要的语义。
pub(crate) async fn read_remote_file(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    path: &str,
    timeout: Duration,
) -> Result<RemoteFile, String> {
    let script = format!("if [ -e '{path}' ]; then cat '{path}'; else exit 42; fi");
    let out = run_cmd(sess, cancel, step, &script, timeout).await?;
    match out.exit_code {
        0 => Ok(RemoteFile::Content(out.stdout)),
        42 => Ok(RemoteFile::Absent),
        code => {
            let tail = crate::util::stderr_tail(&out.stderr);
            Err(format!(
                "读取 {path} 失败(exit={code}): {}",
                if tail.trim().is_empty() {
                    "远端无错误输出"
                } else {
                    tail.trim_end()
                }
            ))
        }
    }
}

/// 远端写文件：mkdir -p 父目录 + 临时文件 + chmod + `mv -f` 原子替换。
/// 路径为常量白名单，故无注入面（内容整体 base64 后单引号包裹）。
/// pub(crate)：relay::build 收敛写回与 peers 下发复用。
///
/// **原子性**：原实现 `> '{path}'` 是 truncate-then-stream，取消/超时/断连
/// （都是常态）会留下截断的配置，服务起不来而不报原因。
///
/// **权限**：按路径判定，**不加参数**——本批缺陷的形态就是「某条路径上忘了应用」，
/// 可选参数等于给未来的调用点留同一个坑。`wg0.conf` 内含 PrivateKey，首次写入按
/// umask 会落 644（`mkdir -p` 还会把 /etc/wireguard 建成 755），而 `wg-quick`
/// 自己都会警告 world accessible。
///
/// 顺带一提：`chmod` 不能省。改成 `> tmp && mv` 之后，tmp 的模式会**替换**掉原文件
/// 的模式，省掉 chmod 反而会把本来就是 600 的 wg0.conf 降级成 644。
pub(crate) async fn write_remote_file(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    remote_path: &str,
    content: &str,
    timeout: Duration,
) -> Result<(), String> {
    let b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        content.as_bytes(),
    );
    let parent = remote_path.rsplit_once('/').map(|(p, _)| p).unwrap_or(".");
    let (mode, dir_mode) = remote_perms(remote_path);
    // 临时文件名必须每次不同：固定 `.new` 时，两次并发写同一路径会一胜一败，
    // 败的那次 `mv` 报 `cannot stat '…new'`——而最终文件其实已经被对方换好，
    // 表面看像“服务器上的分享读不到”，实际只是一次同路径写入撞车。列举分享
    // （每次进分享页都会重写 share_summary.py）与后台刷新重叠时就会踩到。
    let tmp_path = format!(
        "{remote_path}.new.{}.{}",
        std::process::id(),
        TEMP_WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut cmd = format!("mkdir -p '{parent}' && ");
    if let Some(dm) = dir_mode {
        // 显式 chmod 而非 `mkdir -p -m`：后者只对**新建**目录生效，已存在却是 755 的
        // /etc/wireguard 它修不了——而恰恰是这种情况最需要修。
        // 顺序必须在 `mkdir -p` **之后**：目录还不存在时 chmod 会失败，`&&` 链直接
        // 短路，首次写 wg0.conf 一个字节都写不进去。
        cmd.push_str(&format!("chmod {dm:o} '{parent}' && "));
    }
    cmd.push_str(&format!(
        "echo '{b64}' | base64 -d > '{tmp_path}' \
         && chmod {mode:o} '{tmp_path}' \
         && mv -f '{tmp_path}' '{remote_path}'"
    ));
    run_cmd_ok(sess, cancel, step, &cmd, timeout).await?;
    Ok(())
}

/// 远端文件权限策略：路径 → (文件 mode, 父目录 mode)。
///
/// 新增敏感文件时**必须**在这里登记，否则落 umask 默认（通常 644）。
fn remote_perms(path: &str) -> (u32, Option<u32>) {
    if path == crate::relay::WG_CONF_PATH || path == crate::relay::WG_BAK_PATH {
        (0o600, Some(0o700))
    } else {
        (0o644, None)
    }
}
