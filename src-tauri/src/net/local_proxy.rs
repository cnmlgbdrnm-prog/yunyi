//! 应用自身流量的本地代理拨号。
//!
//! 背景：本应用只设 WinINET 系统代理，而 SSH（russh）与裸 TCP 探测完全不看
//! WinINET——「开启网络」对它们等于没开，直连路径不通的服务器（如走中国移动
//! 出境劣化路径的机器）因此永远连不上。
//!
//! 做法：开启网络时把 mihomo 的 mixed-port 登记到本模块，进程内所有出站裸 TCP
//! （SSH/部署/服务器测速的控制通道）改从这里拨号——对 mixed-port 发 HTTP CONNECT，
//! 打通后拿回的就是那条 TcpStream 本身，无需包装类型。好处是白拿一套 mihomo 规则
//! 路由：`GEOIP,CN,DIRECT` 让国内服务器自动直连、国外自动走节点，不需要自己写
//! 「直连失败再回退」的启发式。
//!
//! 回退语义（刻意收窄）：只有**本地代理拨不通**（端口过期/实例已退出/未监听）才
//! 回退直连；代理明确回了非 2xx 说明它活着并做了决定，错误如实上报不掩盖——否则
//! 「节点挂了」会被伪装成「直连也连不上」。

use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{OnceLock, RwLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 当前本地代理端口；0 = 不走代理（未开启网络）。
static PORT: AtomicU16 = AtomicU16::new(0);

/// 「自我托管」主机：上面跑着我们自己的节点（落地机 / 中转入口）。**对它们拨号
/// 直连优先**。
///
/// 经代理就是自指：SSH 到 A 时流量要先穿过装在 A 上的那个节点进隧道，而部署到 A
/// 的第 80% 步会重启 Xray——承载这次部署的隧道被它自己掐断，任务停在那里报
/// 「打开通道 执行失败(exit=0): Channel send error」（2026-09-20 真机）。
/// 直连还顺带解决了「节点挂了就够不着那台机器」——而那正是最需要 SSH 的时候。
///
/// 不直连优先的对象：**只有经代理才可达**的机器（如走劣化出境路径的 112）。它们是
/// 节点宿主之外的机器，不在本集合里，行为完全不变。集合里的机器若直连失败，仍回退
/// 代理——宁可冒自指的险，也不能够不着。
fn self_hosted() -> &'static RwLock<HashSet<String>> {
    static S: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(HashSet::new()))
}

/// 登记自我托管主机（开网络 / 配置变更时刷新，关网络清空）。只收主机名，不含端口。
/// 域名大小写不敏感，统一转小写比对。
pub(crate) fn set_self_hosted(hosts: impl IntoIterator<Item = String>) {
    let mut w = self_hosted().write().unwrap_or_else(|e| e.into_inner());
    w.clear();
    w.extend(hosts.into_iter().map(|h| h.to_ascii_lowercase()));
}

fn is_self_hosted(host: &str) -> bool {
    self_hosted()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&host.to_ascii_lowercase())
}

/// 握手响应头上限：防止异常代理无限吐头打爆内存。
const MAX_HEAD: usize = 8 * 1024;

/// 登记/清除本地代理端口（net/runtime 启停时调用）。
pub(crate) fn set(port: Option<u16>) {
    PORT.store(port.unwrap_or(0), Ordering::Relaxed);
}

/// 当前登记的本地代理端口；未开启网络为 None。
pub(crate) fn get() -> Option<u16> {
    match PORT.load(Ordering::Relaxed) {
        0 => None,
        p => Some(p),
    }
}

/// 拨号失败的两种性质——决定能不能回退直连。
enum Tunnel {
    /// 本地代理根本没连上/没应答：端口过期或实例已退出，回退直连
    Unreachable(io::Error),
    /// 代理回了非 2xx：它活着并做了决定，错误如实上报
    Rejected(String),
}

/// 按显式代理参数拨号（None = 直连）。代理参数外提是为了可测——全局登记表
/// 在并行测试里无法安全复位。
pub(crate) async fn dial(host: &str, port: u16, proxy: Option<u16>) -> io::Result<TcpStream> {
    dial_with(host, port, proxy, is_self_hosted(host)).await
}

/// 显式传「是否自我托管」的版本。判据外提的理由与 `proxy` 参数完全相同：
/// 全局登记表在并行测试里无法安全复位，而测试要能独立钉住两种拨号顺序。
pub(crate) async fn dial_with(
    host: &str,
    port: u16,
    proxy: Option<u16>,
    self_hosted: bool,
) -> io::Result<TcpStream> {
    let Some(proxy_port) = proxy else {
        return TcpStream::connect((host, port)).await;
    };
    // 自我托管主机（见 [`self_hosted`]）：**直连优先**。顺序反过来就是自指——流量先
    // 穿过装在这台机器上的节点，而针对它的部署/重启又会掐断那条节点。
    if self_hosted {
        return match TcpStream::connect((host, port)).await {
            Ok(s) => Ok(s),
            // 直连不成退回代理，两条失败原因都带上：单说直连超时会让用户以为
            // 「这台机器挂了」，而真相可能是「只有经代理才够得着，但代理也挂了」
            Err(direct_err) => match tunnel(proxy_port, host, port).await {
                Ok(s) => Ok(s),
                Err(Tunnel::Unreachable(proxy_err)) => Err(io::Error::new(
                    direct_err.kind(),
                    format!("{direct_err}（本地代理 :{proxy_port} 不可用：{proxy_err}）"),
                )),
                Err(Tunnel::Rejected(msg)) => Err(io::Error::other(format!(
                    "直连失败（{direct_err}），且{msg}"
                ))),
            },
        };
    }
    match tunnel(proxy_port, host, port).await {
        Ok(s) => Ok(s),
        // 回退直连。直连也失败时把代理侧的失败原因并进错误——否则「节点挂了」
        // 会被伪装成一次普通的直连超时，用户看着「网络明明开着」无从下手。
        Err(Tunnel::Unreachable(proxy_err)) => match TcpStream::connect((host, port)).await {
            Ok(s) => Ok(s),
            Err(direct_err) => Err(io::Error::new(
                direct_err.kind(),
                format!("{direct_err}（本地代理 :{proxy_port} 不可用：{proxy_err}）"),
            )),
        },
        Err(Tunnel::Rejected(msg)) => Err(io::Error::other(msg)),
    }
}

/// 对本地 mixed-port 发 HTTP CONNECT，打通后返回的 socket 本身即是裸隧道。
fn authority(host: &str, port: u16) -> String {
    // IPv6 字面量必须加方括号，否则 authority 里的冒号没法解析
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn tunnel(proxy_port: u16, host: &str, port: u16) -> Result<TcpStream, Tunnel> {
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .map_err(Tunnel::Unreachable)?;
    let authority = authority(host, port);
    let req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
    s.write_all(req.as_bytes())
        .await
        .map_err(Tunnel::Unreachable)?;

    // 逐字节读到 \r\n\r\n：**不能整块读再截断**。CONNECT 的 2xx 之后 socket 直接
    // 变裸隧道，而对端常在同一批字节里就发来首包——SSH 服务器连上就吐 banner
    // （SSH-2.0-...）。整块读会把 banner 一起吞掉，SSH 握手随即失败。
    let mut head: Vec<u8> = Vec::with_capacity(128);
    let mut one = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(Tunnel::Rejected(format!(
                "本地代理握手响应头超长（≥{MAX_HEAD} 字节），疑似非 HTTP 代理"
            )));
        }
        let n = s.read(&mut one).await.map_err(Tunnel::Unreachable)?;
        if n == 0 {
            return Err(Tunnel::Rejected(format!(
                "本地代理在 CONNECT {authority} 握手完成前关闭了连接"
            )));
        }
        head.push(one[0]);
    }

    let text = String::from_utf8_lossy(&head);
    let status_line = text.lines().next().unwrap_or_default().trim().to_string();
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2'));
    if !ok {
        return Err(Tunnel::Rejected(format!(
            "本地代理拒绝 CONNECT {authority}：{status_line}"
        )));
    }
    Ok(s)
}
