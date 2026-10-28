//! Windows 原生网络与进程探测：网卡、监听端口、连接表及进程路径。
//! 孤儿进程恢复由 net::orphan 与 service::process 负责，使用完整路径和进程句柄核验归属。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 一枚网卡探测结果：判定它「还在不在」所需的最小字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterRow {
    pub name: String,
    /// Win32 `IF_OPER_STATUS`：1=Up 2=Down … 6=NotPresent。
    pub oper_status: i32,
}

/// Win32 `IF_OPER_STATUS::IfOperStatusNotPresent`：设备已不存在（只剩注册表残留）。
const IF_OPER_STATUS_NOT_PRESENT: i32 = 6;

/// 枚举结果 → 网卡名列表，只留**仍在**的：滤掉 `NotPresent` 的幻影条目，
/// 保留 `Down`（网卡在、只是没连上）。
///
/// 2026-09-19 实机：注册表残留了一条 `yunyi-tun0`（自家 wintun 设备名，设备已删除、
/// 无地址无路由、`netsh` 与 `Get-NetAdapter -IncludeHidden` 都看不到），枚举却仍把它
/// 报出来。接管闸门据此把「开启网络」拦死，提示「请先关闭其系统代理/TUN」——而用户
/// 没有任何可关闭的对象。别家的 `skyline-vpn-ethernet-*` 也留下过同款条目。
///
/// `Down` 不能一起滤：网卡插着、只是没插线（断开状态的以太网、未连接的 TAP）。
/// 滤掉它会漏报真实存在的 TUN——漏报是放行，误报是拦停，方向相反。
pub fn live_adapter_names(rows: &[AdapterRow]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.oper_status != IF_OPER_STATUS_NOT_PRESENT)
        .map(|r| r.name.clone())
        .collect()
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BUFFER_OVERFLOW, ERROR_INSUFFICIENT_BUFFER, HANDLE,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GetExtendedTcpTable, GetExtendedUdpTable, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST,
        IP_ADAPTER_ADDRESSES_LH, TCP_TABLE_OWNER_PID_ALL, TCP_TABLE_OWNER_PID_LISTENER,
        UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// AF_UNSPEC：IPv4/IPv6 两族网卡都要。
    const AF_UNSPEC: u32 = 0;
    /// AF_INET：监听端口表只查 IPv4（代理端点恒为 127.0.0.1）。
    const AF_INET: u32 = 2;
    /// AF_INET6：socket 快照要把 v6 也扫掉——只挡 v4 的检测器看不见 v6 直连。
    const AF_INET6: u32 = 23;
    /// NUL 结尾的 UTF-16 宽字符串 → String（非法代理对以 U+FFFD 兜底，不 panic）。
    ///
    /// # Safety
    /// `p` 必须指向以 NUL 结尾的可读 UTF-16 串。
    unsafe fn pwstr_to_string(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }

    /// 枚举本机网卡用的 flags：跳过的字段（单播/任播/组播/DNS）在不需要时省掉解析开销。
    ///
    /// **不含 `GAA_FLAG_INCLUDE_ALL_INTERFACES`(0x100)**——该公开标志枚举所有 NDIS 接口，会把
    /// 「设备已不存在、只剩注册表残留」的条目一并返回：2026-09-19 实测，带它枚举出 50 条
    /// （含自家 `yunyi-tun0` 与别家的 `skyline-vpn-ethernet-*`），不带它只剩 8 条真网卡。
    /// 它当初被当成 `Get-NetAdapter -IncludeHidden` 的等价物，而对照实测并不等价：
    /// 那些幻影条目 `-IncludeHidden` 同样看不到。要对齐的是 `-IncludeHidden`，不是它。
    pub fn adapter_flags() -> u32 {
        GAA_FLAG_SKIP_UNICAST
            | GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER
    }

    /// 枚举本机**仍在**的网卡友好名（幻影条目见 [`super::live_adapter_names`]）。
    pub fn adapter_names() -> Result<Vec<String>, String> {
        let flags = adapter_flags();
        // 先给一个够大的初值；不足时 API 会把所需大小写回 size 并返回 ERROR_BUFFER_OVERFLOW
        let mut size: u32 = 16 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            buf.resize(size as usize, 0);
            // SAFETY: buf 至少 size 字节可写；size 与 buf.len() 一致；reserved 允许为 null
            let ret = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC,
                    flags,
                    std::ptr::null::<c_void>(),
                    buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                    &mut size,
                )
            };
            if ret == ERROR_BUFFER_OVERFLOW {
                continue; // size 已被写成所需大小，重来一次
            }
            if ret != 0 {
                return Err(format!("枚举网卡失败（GetAdaptersAddresses={ret}）"));
            }
            break;
        }
        let mut rows = Vec::new();
        let mut cur = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: 内核在同一块 buf 内以 Next 串起链表，节点均在 buf 生命周期内有效
        unsafe {
            while !cur.is_null() {
                let name = pwstr_to_string((*cur).FriendlyName);
                if !name.is_empty() {
                    rows.push(super::AdapterRow {
                        name,
                        oper_status: (*cur).OperStatus,
                    });
                }
                cur = (*cur).Next;
            }
        }
        Ok(super::live_adapter_names(&rows))
    }

    /// 端口是否有**真实监听**。取代 `Get-NetTCPConnection -State Listen`（冷启动实测
    /// 3510ms，同样是 NetTCPIP 模块首次初始化）——旧实现特意不用 bind 探测，因为 Go 系
    /// 代理用 SO_REUSEADDR 在 Windows 上会「假空闲」放行第二次 bind，必须查真实监听表。
    /// 查询失败按「无监听」处理，与旧实现的 `.unwrap_or(false)` 一致。
    pub fn port_is_listening(port: u16) -> bool {
        let mut size: u32 = 16 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            buf.resize(size as usize, 0);
            // SAFETY: buf 至少 size 字节可写；size 与 buf.len() 一致；reserved 传 0
            let ret = unsafe {
                GetExtendedTcpTable(
                    buf.as_mut_ptr().cast::<c_void>(),
                    &mut size,
                    0,
                    AF_INET,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if ret == ERROR_INSUFFICIENT_BUFFER {
                continue; // size 已回填所需大小
            }
            if ret != 0 {
                return false;
            }
            break;
        }
        super::parse_listen_ports(&buf).contains(&port)
    }

    /// 监听指定 IPv4 端口的进程名，供 Tor 复用时核实 9050 的占用者。
    pub fn listener_image(port: u16) -> Result<Option<String>, String> {
        let table = read_table(AF_INET, TCP_TABLE_OWNER_PID_LISTENER, false)?;
        let pid = super::table_rows(&table, 24)
            .find(|row| super::port_at(row, 8) == port)
            .map(|row| super::u32_at(row, 20));
        let Some(pid) = pid else {
            return Ok(None);
        };
        Ok(process_images()?
            .into_iter()
            .find(|(id, _)| *id == pid)
            .map(|(_, image)| image))
    }

    /// 读一张 socket 表，返回原始字节交给纯函数 parser。
    ///
    /// 四张表共用这一处缓冲区协商：表在两次调用之间会变长（连接是活的），
    /// 只能按 `ERROR_INSUFFICIENT_BUFFER` 扩了再问一次。
    fn read_table(af: u32, class: i32, udp: bool) -> Result<Vec<u8>, String> {
        let mut size: u32 = 64 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            buf.resize(size as usize, 0);
            // SAFETY: buf 至少 size 字节可写；size 与 buf.len() 一致；reserved 传 0
            let ret = unsafe {
                if udp {
                    GetExtendedUdpTable(
                        buf.as_mut_ptr().cast::<c_void>(),
                        &mut size,
                        0,
                        af,
                        class,
                        0,
                    )
                } else {
                    GetExtendedTcpTable(
                        buf.as_mut_ptr().cast::<c_void>(),
                        &mut size,
                        0,
                        af,
                        class,
                        0,
                    )
                }
            };
            if ret == ERROR_INSUFFICIENT_BUFFER {
                // 表只会变长。size 没被回填成更大的值就是 API 异常，
                // 必须在这里断掉：采样任务每秒走一次，卡在死循环里等于检测器静默消失。
                if size as usize <= buf.len() {
                    return Err(format!("socket 表大小协商失败（{af}/{class}）"));
                }
                continue;
            }
            if ret != 0 {
                return Err(format!("枚举 socket 表失败（{af}/{class}，ret={ret}）"));
            }
            return Ok(buf);
        }
    }

    /// 整机 socket 快照：TCP/UDP × v4/v6 四张 `OWNER_PID` 表合并。
    ///
    /// 这是泄漏检测**唯一**能看见「没进 mihomo 的 socket」的地方——mihomo 的
    /// `/connections` 只列已经进了它的连接，漏掉的永远不会出现在那里（实测：
    /// codex 的连接在 `/connections` 里是 0 条，而它确实在直连公网）。
    ///
    /// 只留有对端的行：监听 socket 的对端恒为 `0.0.0.0:0`，UDP 行则压根没有对端
    /// 字段（`MIB_UDPROW_OWNER_PID` 表里就没有），留下会被分类器当成「连去 0.0.0.0」。
    /// 代价是 UDP 的泄漏这条检测器**看不见**——判不了就不报，交给 P2 的防火墙阻断，
    /// 好过拿半个事实报一个假泄漏。
    pub fn socket_snapshot() -> Result<Vec<super::SockRow>, String> {
        let mut out =
            super::parse_tcp_v4_table(&read_table(AF_INET, TCP_TABLE_OWNER_PID_ALL, false)?);
        out.extend(super::parse_tcp_v6_table(&read_table(
            AF_INET6,
            TCP_TABLE_OWNER_PID_ALL,
            false,
        )?));
        out.extend(super::parse_udp_v4_table(&read_table(
            AF_INET,
            UDP_TABLE_OWNER_PID,
            true,
        )?));
        out.extend(super::parse_udp_v6_table(&read_table(
            AF_INET6,
            UDP_TABLE_OWNER_PID,
            true,
        )?));
        out.retain(|r| r.remote_port != 0 && !r.remote_addr.is_unspecified());
        Ok(out)
    }

    /// pid → 镜像名（全量，不做任何筛选）。
    ///
    /// 分类器需要它把「这条 socket 是谁的」翻译成人看得懂的名字：mihomo 的
    /// `metadata.process` 在本机 16/16 条连接上都是空的，PROCESS 列只能从这儿来。
    /// 与 进程恢复探针 的差别只在于**不开进程句柄**——镜像名在快照里就有，
    /// 整机几百个进程各开一次 `OpenProcess` + `NtQuery` 是没必要的开销。
    pub fn process_images() -> Result<Vec<(u32, String)>, String> {
        // SAFETY: 仅取只读快照句柄；任何失败路径都在下方提前返回并关闭句柄
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap.is_null() || snap as isize == -1 {
            return Err("创建进程快照失败".to_string());
        }
        let mut out = Vec::new();
        // SAFETY: entry 已按 API 要求置 dwSize；两个 API 只写 entry 内部字段
        unsafe {
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    out.push((
                        entry.th32ProcessID,
                        pwstr_to_string(entry.szExeFile.as_ptr()),
                    ));
                    if Process32NextW(snap, &mut entry) == 0 {
                        break; // ERROR_NO_MORE_FILES：枚举结束
                    }
                }
            }
            CloseHandle(snap);
        }
        Ok(out)
    }

    /// 指定镜像名的进程的**绝对路径**，返回 `(镜像名, 绝对路径)`。
    ///
    /// 用来给防火墙的程序级规则取目标：`INetFwRule::ApplicationName` 要的是绝对路径，
    /// 而镜像名 → 路径的唯一可靠来源就是本机进程表。**只对名单里的名字开进程句柄**
    /// （与 进程恢复探针 只筛 `mihomo.exe` 同一取舍）：整机几百个进程各开一次
    /// `OpenProcess` + `QueryFullProcessImageNameW` 是没必要的开销。
    ///
    /// 取不到路径的进程直接**不进结果**：调用方把「没拿到路径」如实报成 partial，
    /// 而不是猜一个安装路径——猜错写出的规则永远匹配不上任何进程，表现是「界面说
    /// 已阻断、实际一条都没生效」，比明说没挡住难查得多。
    pub fn process_paths(names: &[String]) -> Result<Vec<(String, String)>, String> {
        // 名单是用户手打的，可能带路径/大小写不一：比对键与泄漏检测共用同一个
        // image_key（取文件名 + 小写），免得两边对「是不是同一个程序」有两套答案
        let wanted: Vec<String> = names
            .iter()
            .map(|n| super::super::connlog::image_key(n))
            .filter(|n| !n.is_empty())
            .collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        // SAFETY: 仅取只读快照句柄；任何失败路径都在下方提前返回（句柄随进程回收）
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap.is_null() || snap as isize == -1 {
            return Err("创建进程快照失败".to_string());
        }
        let mut out = Vec::new();
        // SAFETY: entry 已按 API 要求置 dwSize；两个 API 只写 entry 内部字段
        unsafe {
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    let image = pwstr_to_string(entry.szExeFile.as_ptr());
                    if wanted.contains(&image.to_ascii_lowercase()) {
                        if let Some(path) = image_path(entry.th32ProcessID) {
                            out.push((image, path));
                        }
                    }
                    if Process32NextW(snap, &mut entry) == 0 {
                        break; // ERROR_NO_MORE_FILES：枚举结束
                    }
                }
            }
            CloseHandle(snap);
        }
        Ok(out)
    }

    /// 取某进程的 exe 全路径。拿不到（权限不足、进程刚退出、系统进程）返回 None——
    /// 这是**尽力而为**的探测，调用方按「没拿到」如实上报。
    fn image_path(pid: u32) -> Option<String> {
        // SAFETY: 句柄关闭前只用于 QueryFullProcessImageNameW；null 已判
        unsafe {
            let h: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 32768];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            if ok == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows：无 WinINET/TUN 语义，返回空列表（与旧实现一致）。
    pub fn adapter_names() -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// 非 Windows：无 GetAdaptersAddresses 语义。
    pub fn adapter_flags() -> u32 {
        0
    }

    /// 非 Windows：无 IpHelper 表语义。返回**空列表**而不是错误——调用方要能区分
    /// 「枚举不出来」与「一条可疑的都没有」，把平台不支持说成后者才是撒谎。
    pub fn socket_snapshot() -> Result<Vec<super::SockRow>, String> {
        Ok(Vec::new())
    }

    /// 非 Windows：无 Toolhelp32 语义。
    pub fn process_images() -> Result<Vec<(u32, String)>, String> {
        Ok(Vec::new())
    }

    /// 非 Windows：无 Toolhelp32 语义。返回空列表 = 「一个路径都没拿到」，
    /// 调用方据此如实报 partial（而不是假装挡住了）。
    pub fn process_paths(_names: &[String]) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }

    /// 非 Windows：无 WinINET 监听表语义，恒判无监听（旧实现在非 Windows 走
    /// `dead_self_proxy` 的调用方本就只在 Windows 编译）。
    pub fn port_is_listening(_port: u16) -> bool {
        false
    }

    pub fn listener_image(_port: u16) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// 仅供测试断言（生产路径在 `imp::adapter_names` 内部直接用这份 flags）。
#[cfg(all(test, feature = "local-tests"))]
pub use imp::adapter_flags;
pub use imp::{
    adapter_names, listener_image, port_is_listening, process_images, process_paths,
    socket_snapshot,
};

/// socket 表行的协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "TCP",
            Proto::Udp => "UDP",
        }
    }
}

/// 地址族。**必须显式带着**而非从 `IpAddr` 反推：`::ffff:1.2.3.4` 这类 v4-mapped
/// 地址解出来是 v6，但 Windows 的 v6 表里它代表的就是一条 v4 连接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::V4 => "IPv4",
            Family::V6 => "IPv6",
        }
    }
}

/// TCP 连接状态。掐既有连接（`net::takeover`）只认
/// `Established`：`TIME_WAIT` 之类的残行不该被当成活的连接下手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    /// `MIB_TCP_STATE_ESTAB`(5)
    Established,
    /// CLOSED / TIME_WAIT / DELETE_TCB：无正在进行的数据连接，不进入当前泄漏分类。
    Closed,
    /// 其余 TCP 状态（LISTEN/SYN_SENT/FIN_WAIT/…）；保留正在尝试建立的连接。
    Other,
    /// 没有状态字段的表（UDP 行）
    Unknown,
}

/// 保留 SYN_SENT 等尝试连接；仅将明确关闭的状态标成 Closed。
fn tcp_state(raw: u32) -> TcpState {
    match raw {
        5 => TcpState::Established,
        1 | 11 | 12 => TcpState::Closed,
        _ => TcpState::Other,
    }
}

/// 一条 socket 表行。四张表（TCP/UDP × v4/v6）统一成同一形状：分类器只按
/// `(proto, local_port)` 与 mihomo 的连接 join，故协议与地址族必须是**数据**。
///
/// UDP 行**没有对端**（`MIB_UDPROW_OWNER_PID` 表里就没有这两个字段），故解析出来
/// 恒为 `0.0.0.0:0`——[`socket_snapshot`] 会把它们连同监听 socket 一起滤掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SockRow {
    pub pid: u32,
    pub proto: Proto,
    pub family: Family,
    pub state: TcpState,
    pub local_addr: IpAddr,
    pub local_port: u16,
    pub remote_addr: IpAddr,
    pub remote_port: u16,
}

/// 挨个切出表里的完整行（表头 `dwNumEntries` + 定长行）。表被截断时只给完整行，
/// 与 [`parse_listen_ports`] 同一防御式做法：宁可少报几条，也不能越界读。
fn table_rows(table: &[u8], row: usize) -> impl Iterator<Item = &[u8]> + '_ {
    let count = table
        .get(0..4)
        .map(|h| u32::from_ne_bytes([h[0], h[1], h[2], h[3]]) as usize)
        .unwrap_or(0);
    (0..count).map_while(move |i| {
        let base = 4 + i * row;
        table.get(base..base + row)
    })
}

/// 行内取 u32（本机字节序；这些字段都是普通整数，不是地址也不是端口）。
fn u32_at(row: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([row[off], row[off + 1], row[off + 2], row[off + 3]])
}

/// 行内取端口：DWORD 的**低 16 位、网络字节序**（与 [`parse_listen_ports`] 同一结论，
/// 2026-09 实测钉死；按本机字节序直接读会得到字节颠倒的端口号）。
fn port_at(row: &[u8], off: usize) -> u16 {
    u16::from_be(u16::from_ne_bytes([row[off], row[off + 1]]))
}

/// 行内取 IPv4：内存里就是四个八位组的网络字节序，按顺序读即是点分地址。
fn ipv4_at(row: &[u8], off: usize) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(
        row[off],
        row[off + 1],
        row[off + 2],
        row[off + 3],
    ))
}

/// 行内取 IPv6：16 字节已是对端顺序，直接搬。
fn ipv6_at(row: &[u8], off: usize) -> IpAddr {
    let mut b = [0u8; 16];
    b.copy_from_slice(&row[off..off + 16]);
    IpAddr::V6(Ipv6Addr::from(b))
}

/// 解析 `GetExtendedTcpTable(AF_INET, TCP_TABLE_OWNER_PID_ALL)`：24 字节/行
/// （`MIB_TCPROW_OWNER_PID`：state, localAddr, localPort, remoteAddr, remotePort, pid）。
pub fn parse_tcp_v4_table(table: &[u8]) -> Vec<SockRow> {
    table_rows(table, 24)
        .map(|r| SockRow {
            pid: u32_at(r, 20),
            proto: Proto::Tcp,
            family: Family::V4,
            state: tcp_state(u32_at(r, 0)),
            local_addr: ipv4_at(r, 4),
            local_port: port_at(r, 8),
            remote_addr: ipv4_at(r, 12),
            remote_port: port_at(r, 16),
        })
        .collect()
}

/// 解析 `GetExtendedTcpTable(AF_INET6, TCP_TABLE_OWNER_PID_ALL)`：56 字节/行
/// （`MIB_TCP6ROW_OWNER_PID`：localAddr[16], localScopeId, localPort,
/// remoteAddr[16], remoteScopeId, remotePort, state, pid）。scope id 丢弃——
/// 分类器只关心「是不是公网」。
pub fn parse_tcp_v6_table(table: &[u8]) -> Vec<SockRow> {
    table_rows(table, 56)
        .map(|r| SockRow {
            pid: u32_at(r, 52),
            proto: Proto::Tcp,
            family: Family::V6,
            state: tcp_state(u32_at(r, 48)),
            local_addr: ipv6_at(r, 0),
            local_port: port_at(r, 20),
            remote_addr: ipv6_at(r, 24),
            remote_port: port_at(r, 44),
        })
        .collect()
}

/// 解析 `GetExtendedUdpTable(AF_INET, UDP_TABLE_OWNER_PID)`：12 字节/行
/// （`MIB_UDPROW_OWNER_PID`：localAddr, localPort, pid）。**表里没有对端字段**，
/// 故对端恒为 `0.0.0.0:0`——UDP 的泄漏判定在本检测器里做不到，靠 P2 的防火墙阻断。
pub fn parse_udp_v4_table(table: &[u8]) -> Vec<SockRow> {
    table_rows(table, 12)
        .map(|r| SockRow {
            pid: u32_at(r, 8),
            proto: Proto::Udp,
            family: Family::V4,
            // UDP 表里没有状态字段
            state: TcpState::Unknown,
            local_addr: ipv4_at(r, 0),
            local_port: port_at(r, 4),
            remote_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            remote_port: 0,
        })
        .collect()
}

/// 解析 `GetExtendedUdpTable(AF_INET6, UDP_TABLE_OWNER_PID)`：28 字节/行
/// （`MIB_UDP6ROW_OWNER_PID`：localAddr[16], localScopeId, localPort, pid）。
pub fn parse_udp_v6_table(table: &[u8]) -> Vec<SockRow> {
    table_rows(table, 28)
        .map(|r| SockRow {
            pid: u32_at(r, 24),
            proto: Proto::Udp,
            family: Family::V6,
            // UDP 表里没有状态字段
            state: TcpState::Unknown,
            local_addr: ipv6_at(r, 0),
            local_port: port_at(r, 20),
            remote_addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            remote_port: 0,
        })
        .collect()
}

/// 解析 `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)` 的原始返回表。
/// 布局：`dwNumEntries: u32` 后紧跟 N 条 `MIB_TCPROW_OWNER_PID`（6×u32 = 24 字节），
/// 每行内偏移 8 处是 `dwLocalPort`，端口以**网络字节序**放在低 16 位。
/// 表被截断/长度不足时只解析完整可用的行（防御性，不 panic）。
pub fn parse_listen_ports(table: &[u8]) -> Vec<u16> {
    const ROW: usize = 24;
    const LOCAL_PORT_OFF: usize = 8;
    let Some(head) = table.get(0..4) else {
        return Vec::new();
    };
    let count = u32::from_ne_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = 4 + i * ROW;
        let Some(row) = table.get(base + LOCAL_PORT_OFF..base + LOCAL_PORT_OFF + 4) else {
            break; // 表被截断：已解析的行仍然有效
        };
        // 端口存于低 16 位、网络字节序
        let raw = u16::from_ne_bytes([row[0], row[1]]);
        out.push(u16::from_be(raw));
    }
    out
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/net/winprobe/mods.rs"
));
