//! 服务器 BBR 检测与一键开启：检测走 `sysctl -n net.ipv4.tcp_congestion_control`
//! 回读当前算法；开启走幂等脚本（modprobe tcp_bbr → sysctl 双参数 → 幂等追加
//! /etc/sysctl.conf → 回读验证）。modprobe 失败（内核无 tcp_bbr 模块）→
//! 「内核不支持 BBR」可读错误。惰性触发：页面加载绝不自动 SSH，仅行内按钮调用。
//! 脚本每步幂等（grep -q 守卫），重复开启不重复追加配置文件。

use crate::ssh::SshSession;
use std::time::Duration;

/// 检测当前拥塞控制算法：`sysctl -n net.ipv4.tcp_congestion_control`（15s 超时）。
/// 非零退出/空输出 → 可读错误带原因。
pub async fn check_bbr(session: &mut SshSession) -> Result<String, String> {
    let out = session
        .run(
            "检测 BBR",
            "sysctl -n net.ipv4.tcp_congestion_control",
            Duration::from_secs(15),
        )
        .await
        .map_err(|e| e.to_string())?;
    let algo = out.stdout.trim();
    if out.exit_code != 0 || algo.is_empty() {
        return Err(format!(
            "读取拥塞控制算法失败(exit={}): {}",
            out.exit_code,
            out.stderr.trim_end()
        ));
    }
    Ok(algo.to_string())
}

/// 算法可用性守卫模式串：`tcp_available_congestion_control` 是**算法名**空间
/// （内容如 `reno cubic bbr`，与 check_bbr 读出的 "bbr" 同一命名），非模块名。
/// -w 词边界防误配 bbr2 之类；同时覆盖内建（CONFIG_TCP_CONG_BBR=y，
/// /proc/modules 无行）与已加载模块；modprobe 兜底覆盖模块化发行版。
/// /proc/modules 双端锚定不可用（行含大小/引用数字段；且内建无行）。
/// 脚本与测试共用本模式串（纯字符串断言 == 本模式，防再次写错算法名/模块名）。
macro_rules! bbr_available_guard {
    () => {
        "grep -qw bbr /proc/sys/net/ipv4/tcp_available_congestion_control"
    };
}
#[cfg(all(test, feature = "local-tests"))]
pub const BBR_AVAILABLE_GUARD: &str = bbr_available_guard!();

/// 一键开启 BBR 的幂等脚本：先确认/加载 tcp_bbr 内核模块（失败 → NO_BBR_MODULE），
/// sysctl 双参数立即生效，/etc/sysctl.conf 幂等追加（重启后保留），末尾回读验证。
/// 每步 stderr 重定向，仅把结果与失败标记输出到 stdout。
pub const BBR_ENABLE_SCRIPT: &str = concat!(
    "\n",
    bbr_available_guard!(),
    " 2>/dev/null || modprobe tcp_bbr 2>/dev/null || { echo NO_BBR_MODULE; exit 1; }\n",
    "sysctl -w net.core.default_qdisc=fq >/dev/null\n",
    "sysctl -w net.ipv4.tcp_congestion_control=bbr >/dev/null\n",
    "grep -q 'net.core.default_qdisc' /etc/sysctl.conf 2>/dev/null || echo 'net.core.default_qdisc=fq' >> /etc/sysctl.conf\n",
    "grep -q 'net.ipv4.tcp_congestion_control' /etc/sysctl.conf 2>/dev/null || echo 'net.ipv4.tcp_congestion_control=bbr' >> /etc/sysctl.conf\n",
    "sysctl -n net.ipv4.tcp_congestion_control\n",
);

/// 一键开启 BBR：执行幂等脚本（30s 超时），脚本尾行回读当前算法。
/// NO_BBR_MODULE → 「内核不支持 BBR」；非零 → Err 带 stderr；成功 → 「已开启 bbr」。
pub async fn enable_bbr(session: &mut SshSession) -> Result<String, String> {
    let out = session
        .run("开启 BBR", BBR_ENABLE_SCRIPT, Duration::from_secs(30))
        .await
        .map_err(|e| e.to_string())?;
    let algo = out.stdout.trim();
    if algo.contains("NO_BBR_MODULE") {
        return Err("内核不支持 BBR（modprobe tcp_bbr 失败）".to_string());
    }
    if out.exit_code != 0 || algo.is_empty() {
        return Err(format!(
            "开启 BBR 失败(exit={}): {}",
            out.exit_code,
            out.stderr.trim_end()
        ));
    }
    if algo != "bbr" {
        return Err(format!("开启 BBR 后回读算法异常: {algo}"));
    }
    Ok("已开启 bbr".to_string())
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/server_bbr/mods.rs"
));
