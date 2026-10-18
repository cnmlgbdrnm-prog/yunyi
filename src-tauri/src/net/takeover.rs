//! 接管「开启网络**之前**就已建立」的直连：把它们强制断开，逼程序用代理重连。
//!
//! 为什么需要这一步：系统代理只是个「新连接往哪儿建」的设置，对**已经建立**的
//! socket 零影响——四元组定死了，改 HKCU 也迁移不过去。于是「关闭网络 → 开启网络」
//! 之间的那段窗口（关应用时界面进程会撤掉系统代理）里，浏览器建立的直连会一直直连
//! 下去：在连接日志里表现为「泄漏」，而它的正确处置（重启浏览器）与真泄漏（查分流）
//! 完全是两回事。2026-09-21 实测：一条 chrome → Google 的直连建于 20:56:19，
//! 而本次实例的服务进程启动于 20:56:43——出生在重启空档，之后 142 分钟没重建过。
//!
//! 判据与时机：**在代理生效之前**取一次 TCP 表快照（见 `runtime::start`），快照里
//! 同时满足下面几条的行才掐：
//!
//! 1. IPv4 + TCP + ESTABLISHED——`SetTcpEntry` 只有 IPv4（没有 `SetTcp6Entry`），
//!    UDP 表连状态字段都没有；
//! 2. 目标非回环非私网——**这条同时排掉了「代理起来之后新建的连接」**：它们一律
//!    指向 `127.0.0.1:<mixed_port>`。于是「先快照、后掐」不需要查 socket 创建时间
//!    （`GetExtendedTcpTable` 压根不带这个字段），也没有时序竞态；
//! 3. 目标不是国内——那些本来就该直连（`GEOIP,CN,DIRECT`），掐了纯亏；
//! 4. 目标不是我们自己的节点端点——那是 mihomo 自己的隧道出站，掐了等于自断网；
//! 5. 属主不是 mihomo、不是云驿自己——云驿自己有 SSH 会话（读分享、部署服务器）。
//!
//! 判据只需要「这条连接长什么样」：开启前 mihomo 还没起来，`connlog` 那套
//! 「join `/connections`」的判据在这个时刻根本用不了。
//!
//! 边界（有意不做）：TUN 模式不调用本模块——TUN 在网络层抓包，既有连接本来就会被
//! 接管；IPv6 掐不掉（如实计入「没掐成」）；UDP 不在检测器视野内（`winprobe` 的
//! UDP 行没有对端），交给会话级阻断规则。

use crate::net::connlog::{image_key, is_local_or_private, CountryLookup};
use crate::net::winprobe::{Family, Proto, SockRow, TcpState};
use crate::service::protocol::StaleConn;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};

/// 一次最多掐多少条。
///
/// 上限是**防御性**的（照 `runtime::MAX_CLOSE_PER_SWITCH = 200` 的先例）：正常场景
/// 个位数。超限不静默——`plan` 会把丢弃条数回给调用方。
pub const MAX_TAKEOVER: usize = 200;

/// 判据所需的上下文。**每一项都能在代理生效之前取到**（这是本模块成立的前提）。
pub struct PreNet<'a> {
    /// pid → 镜像名（`winprobe::process_images()`；比对前统一走 `image_key` 归一化）
    pub images: &'a HashMap<u32, String>,
    /// 不该动的自家进程镜像名（mihomo、云驿自己）
    pub exempt: &'a HashSet<String>,
    /// 自家节点端点 → 节点名（`connlog::own_endpoints`）
    pub own: &'a HashMap<(IpAddr, u16), String>,
    /// 国家查询器。查不到（`None`）**不放行**——与连接日志同一条取舍：拿不准时
    /// 宁可少掐一条，也不去动一条可能本来就该直连的连接。
    pub country: &'a dyn CountryLookup,
}

/// 一条连接该不该掐（`plan` 的内核，单测直接钉它）。
pub fn should_take_over(row: &SockRow, pre: &PreNet) -> bool {
    // 掐得动的只有 IPv4 TCP 的已建立连接
    if row.proto != Proto::Tcp || row.family != Family::V4 || row.state != TcpState::Established {
        return false;
    }
    // 内网/回环：局域网连接本就直连；代理起来后新建的连接也全落在这里
    if is_local_or_private(row.remote_addr) {
        return false;
    }
    // 连到我们自己的节点 = mihomo 自己的隧道出站，掐了自断网
    if pre.own.contains_key(&(row.remote_addr, row.remote_port)) {
        return false;
    }
    // 自家进程：云驿自己的 SSH 会话、mihomo 的隧道出站
    if let Some(img) = pre.images.get(&row.pid) {
        if pre.exempt.contains(&image_key(img)) {
            return false;
        }
    }
    // 国内目标本来就该直连；**查不到国家也不掐**——那多半是 geo 库还没就绪，
    // 此时「不掐」的表现是面板上留着一条泄漏（看得见、有 `degraded_geo` 可解释），
    // 而「掐」的代价是误断一条本来就该直连的连接（看不见）。与连接日志同一条
    // 取舍：拿不准时不动手。
    !matches!(
        pre.country.country_of(row.remote_addr).as_deref(),
        Some("CN") | None
    )
}

/// 快照 → 待掐清单（线格式，可直接发给服务）。返回 `(清单, 因超限丢弃的条数)`。
pub fn plan(snapshot: &[SockRow], pre: &PreNet) -> (Vec<StaleConn>, usize) {
    let all: Vec<&SockRow> = snapshot
        .iter()
        .filter(|r| should_take_over(r, pre))
        .collect();
    let keep = all.len().min(MAX_TAKEOVER);
    let list = all[..keep].iter().map(|r| to_wire(r)).collect();
    (list, all.len() - keep)
}

/// 快照行 → 线格式。地址用点分字符串（线格式人肉可读是这个协议一贯的取舍）。
fn to_wire(r: &SockRow) -> StaleConn {
    StaleConn {
        local_addr: r.local_addr.to_string(),
        local_port: r.local_port,
        remote_addr: r.remote_addr.to_string(),
        remote_port: r.remote_port,
        pid: r.pid,
    }
}

/// 已解析、准备下手的一条连接：IPv4 四元组 + 属主 pid。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub local: Ipv4Addr,
    pub local_port: u16,
    pub remote: Ipv4Addr,
    pub remote_port: u16,
    pub pid: u32,
}

/// 线格式 → [`Target`]。地址必须是**点分 IPv4**：解析不出来就拒——`SetTcpEntry`
/// 只有 IPv4（没有 `SetTcp6Entry`），v6 条目在这儿就被挡在提权边界之外。
pub fn parse_wire(c: &StaleConn) -> Option<Target> {
    Some(Target {
        local: c.local_addr.parse().ok()?,
        local_port: c.local_port,
        remote: c.remote_addr.parse().ok()?,
        remote_port: c.remote_port,
        pid: c.pid,
    })
}

/// 服务侧下手前的复查（提权边界上的两道闸）。返回 `(可掐的, 跳过的条数)`。
///
/// 第一道闸是**时效**：快照取自「代理生效前」，到真正下手之间隔着启动流程（实测最
/// 长十几秒），四元组在这段时间里可能被复用——端口回到池子里再被别的连接拿走。
/// 要求「四元组 + 属主 pid + 仍是 ESTABLISHED」三样都对得上，属主是最后那道闸：
/// 宁可少掐一条，不误杀一条。
///
/// 第二道闸是**边界**：目标是回环/私网的一律不碰。这是服务端的防御性校验，与
/// [`should_take_over`] 里那条判据**故意重复**——那条是策略（主程序说该不该掐），
/// 这条是安全（即使主程序算错，提权进程也不越界），两者失效方式不同。
pub fn recheck(requested: &[Target], now: &[SockRow]) -> (Vec<Target>, usize) {
    let mut ok = Vec::new();
    let mut skipped = 0usize;
    for t in requested {
        if is_local_or_private(IpAddr::V4(t.remote)) {
            skipped += 1;
            continue;
        }
        let alive = now.iter().any(|r| {
            r.proto == Proto::Tcp
                && r.state == TcpState::Established
                && r.pid == t.pid
                && r.local_port == t.local_port
                && r.remote_port == t.remote_port
                && r.local_addr == IpAddr::V4(t.local)
                && r.remote_addr == IpAddr::V4(t.remote)
        });
        if alive {
            ok.push(*t);
        } else {
            skipped += 1;
        }
    }
    (ok, skipped)
}

/// 强制断开一条连接。内核对它置 `DELETE_TCB`：对端收到 RST，程序下次重连时才会
/// 重新读系统代理，于是走代理。
pub fn reset_one(t: &Target) -> Result<(), String> {
    let code = imp::reset(t);
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "SetTcpEntry 失败（win32 错误码 {code}）: {}:{} → {}:{}",
            t.local, t.local_port, t.remote, t.remote_port
        ))
    }
}

#[cfg(windows)]
mod imp {
    use super::Target;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        SetTcpEntry, MIB_TCPROW_LH, MIB_TCPROW_LH_0, MIB_TCP_STATE_DELETE_TCB,
    };

    /// 返回 win32 错误码（0 = 成功）。需要管理员令牌，故只可能由服务侧调用。
    pub fn reset(t: &Target) -> u32 {
        let row = MIB_TCPROW_LH {
            // union 只写不读：写进去的 dwState 就是这次调用的动作语义
            Anonymous: MIB_TCPROW_LH_0 {
                dwState: MIB_TCP_STATE_DELETE_TCB as u32,
            },
            // 字节序照 **MIB 表自己的约定**（与 `winprobe` 解析时同源反向，那边有
            // 实测钉着）：地址在内存里就是网络序，故取 `from_ne_bytes`；端口存在
            // DWORD 的低 16 位、网络序，故取 `u32::from(p.to_be())`。
            dwLocalAddr: u32::from_ne_bytes(t.local.octets()),
            dwLocalPort: u32::from(t.local_port.to_be()),
            dwRemoteAddr: u32::from_ne_bytes(t.remote.octets()),
            dwRemotePort: u32::from(t.remote_port.to_be()),
        };
        unsafe { SetTcpEntry(&row) }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Target;

    /// 非 Windows：没有 iphlpapi。返回 `ERROR_NOT_SUPPORTED`(50)——与「内核拒绝」
    /// 同一个形状，调用方本来就把失败当成「这条没掐成」而继续。
    pub fn reset(_t: &Target) -> u32 {
        50
    }
}
