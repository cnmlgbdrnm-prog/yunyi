//! IP → 最近解析过的域名：读 Windows DNS 客户端缓存。
//!
//! 目的只有一个：**直连行（没进 mihomo 的那些）原本拿不到域名**，八列里 DOMAIN 只能
//! 显示 `—`；有一条来路正当的名字总比空着强。但它是**尽力而为**，见文末的局限。
//!
//! ## 为什么是两个 API 拼起来（实测，不是设计偏好）
//!
//! `DnsGetCacheDataTable`（`dnsapi.dll` 的**未公开导出**，文档不全但没有替代品，
//! 微软自己二十年没动过它）只给**名字与类型**：本机实测 318 条缓存条目里
//! `wDataLength` **全是 0**，记录数据根本不在这张表里。所以地址得再问一次——
//! 拿表里的名字调 `DnsQuery_W`。
//!
//! 但 `DnsQuery_W` **必须带 cache-only**（`DNS_QUERY_NO_WIRE_QUERY`，值 16，头文件里
//! 还有个同值的别名 `DNS_QUERY_CACHE_ONLY`）：这个功能的存在意义就是找「漏出代理的
//! 流量」，而自己去发一次 DNS 查询等于**亲手制造一条漏出代理的流量**（DNS 泄漏），
//! 还是为了给泄漏检测器凑数据——荒唐。故顺带把 `DNS_QUERY_NO_MULTICAST` 也置上，
//! 连 LLMNR/mDNS 组播都不发：这条路径上**一个外部包都不许出去**。
//!
//! cache-only 的代价是**过期条目查不到**（实测返回 9701 = 记录不存在）。这不是缺陷，
//! 恰好是我们要的语义：能查到的就是新鲜的，查不到的本来也不会有活跃连接。
//!
//! ## 局限（界面必须如实显示，不许含糊）
//!
//! - **Chrome / Edge 默认自带解析器甚至 DoH**，它们的解析结果根本不进系统缓存，
//!   所以浏览器漏出去的那些连接**大概率仍然显示 `—`**。这是数据源的天花板，
//!   不是这里的 bug。
//! - 一个 IP 常被多个域名指向（CDN 边缘 IP 上能挂几十个站）。这里只能给一个代表，
//!   规则是「字典序最小」，纯粹为了**确定性**——两次采样显示同一个名字，界面不会
//!   每秒钟换一个域名让人以为看花了眼。
//! - **绝不做反向 DNS**（PTR）：慢、会被污染、解出来的还可能是完全不相干的站点名。
//!   拿不到就留空，界面渲染 `—`：**空着是诚实的，猜一个是错的。**

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// DNS 记录类型：A（IPv4）。常量抄自 windns.h（windows-sys 里也有同名常量）。
pub const DNS_TYPE_A: u16 = 1;
/// DNS 记录类型：AAAA（IPv6）。
pub const DNS_TYPE_AAAA: u16 = 28;

/// 一条 DNS 缓存记录：名字 + 类型 + 记录数据的**原始字节**。
///
/// 之所以把数据留成裸字节：解析（`ip_from_record`）要能脱离 FFI 单测，
/// 而 FFI 那侧本来也只拿得到一块没有类型信息的内存。
#[derive(Debug, Clone)]
pub struct RawRecord {
    pub name: String,
    pub wtype: u16,
    pub data: Vec<u8>,
}

/// 记录数据 → 地址。**只认 A/AAAA 且长度必须刚好**，其余一律 `None`。
///
/// 缓存表里还有 CNAME(5)、PTR(12)、ANY(255) 等等，它们的「数据」是名字不是地址；
/// 按 A 记录去解会解出一堆根本不存在的地址，然后这些地址会出现在面板的 DOMAIN 列里
/// ——宁可空着。
pub fn ip_from_record(wtype: u16, data: &[u8]) -> Option<IpAddr> {
    let ip = match (wtype, data.len()) {
        (DNS_TYPE_A, 4) => IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3])),
        (DNS_TYPE_AAAA, 16) => {
            let mut o = [0u8; 16];
            o.copy_from_slice(data);
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return None,
    };
    // 通配地址不是有效目标：hosts 里的 `0.0.0.0 ads.example.com` 会进缓存，
    // 而「解出一堆 0」也正是结构体偏移读串了的典型症状，一并挡掉
    if ip.is_unspecified() {
        return None;
    }
    Some(ip)
}

/// 缓存记录列表 → IP → 域名。
///
/// 同一个 IP 有多个名字时（CDN 常态）取**字典序最小**的那个：结果必须与输入顺序无关，
/// 否则同一份缓存两次采样会显示不同的域名。空名字/坏长度直接丢。
pub fn build_map(entries: &[RawRecord]) -> HashMap<IpAddr, String> {
    let mut by_name: Vec<(&str, IpAddr)> = entries
        .iter()
        .filter(|e| !e.name.trim().is_empty())
        .filter_map(|e| ip_from_record(e.wtype, &e.data).map(|ip| (e.name.as_str(), ip)))
        .collect();
    by_name.sort_unstable();
    let mut out: HashMap<IpAddr, String> = HashMap::with_capacity(by_name.len());
    for (name, ip) in by_name {
        out.entry(ip).or_insert_with(|| name.to_string());
    }
    out
}

/// 采样时调一次：把系统 DNS 缓存读成 IP → 域名。
///
/// 任何失败（非 Windows、dnsapi 抽风、缓存为空）都给**空表**：这个功能是锦上添花，
/// 它出问题不该让整轮采样失败，也不该让调用方拿到半真半假的数据。
pub fn read_cache() -> HashMap<IpAddr, String> {
    #[cfg(windows)]
    {
        imp::read()
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 没有这套缓存语义（也没有 DNS 客户端缓存服务）
        HashMap::new()
    }
}

#[cfg(windows)]
mod imp {
    use super::{build_map, RawRecord, DNS_TYPE_A, DNS_TYPE_AAAA};
    use std::collections::HashMap;
    use std::net::IpAddr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::NetworkManagement::Dns::{
        DnsFree, DnsFreeRecordList, DnsQuery_W, DNS_QUERY_NO_MULTICAST, DNS_QUERY_NO_WIRE_QUERY,
        DNS_RECORDA, DNS_RECORDW,
    };

    /// 单次读取最多看多少条缓存条目 / 每条多少条记录。
    /// 缓存表正常是几百条量级；这是**环形链表**（Windows 的缓存结构被损坏时真会出现）
    /// 的兜底——检测器宁可少读几条，也不能卡死采样任务。
    const MAX_ENTRIES: usize = 8192;
    const MAX_RECORDS: usize = 256;

    /// `DNS_CACHE_ENTRY`：`dnsapi.dll` 未公开导出 `DnsGetCacheDataTable` 的返回结构。
    ///
    /// **为什么敢照着抄**：这个结构自 Windows 2000 起没变过，是各语言 DNS 缓存查看
    /// 工具的公共知识；且本机实测过——318 条条目、名字全部可读、指针链无环。
    /// 字段与实测对照：`wType` 读出来是 1/28（A/AAAA）与 5/12/255（CNAME/PTR/ANY），
    /// 完全符合缓存内容，说明偏移没错位。
    #[repr(C)]
    struct DnsCacheEntry {
        p_next: *mut DnsCacheEntry,
        /// NUL 结尾的 UTF-16 名字（指向 dnsapi 自己的字符串池，**不归我们释放**）
        name: *mut u16,
        w_type: u16,
        /// 实测恒为 0：记录数据不在这张表里（见模块注释）
        #[allow(dead_code)]
        w_data_len: u16,
        dw_flags: u32,
    }

    #[link(name = "dnsapi")]
    extern "system" {
        /// 枚举 DNS 客户端缓存的名字与类型。成功返回非 0。
        /// 返回值指向 dnsapi 分配的一块内存（实测带 "LMEM" 堆标签，即 LocalAlloc），
        /// 但里面只有这张表——**记录数据要另用 `DnsQuery_W` 取**。
        fn DnsGetCacheDataTable(head: *mut *mut DnsCacheEntry) -> i32;
    }

    /// NUL 结尾的 UTF-16 串 → String（非法代理对以 U+FFFD 兜底，不 panic）。
    ///
    /// # Safety
    /// `p` 要么为空，要么指向以 NUL 结尾的可读 UTF-16 串。
    unsafe fn pwstr_to_string(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *p.add(len) != 0 && len < 1024 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }

    /// 查一个名字在当前**缓存**里的记录（绝不上网，见模块注释）。
    ///
    /// 返回记录数据本身（A 是 4 字节、AAAA 是 16 字节），而不是 `DNS_RECORDW`——
    /// 记录列表由 dnsapi 分配，必须在函数内释放掉，不能让它活到调用方手里。
    fn query_cached(name: &str, wtype: u16) -> Vec<Vec<u8>> {
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        wide.push(0);
        let mut list: *mut DNS_RECORDW = std::ptr::null_mut();
        // SAFETY: 名字是本函数的局部缓冲区（存活到调用返回）；`list` 由 dnsapi 分配并由
        // 下面的 DnsFree 释放；options 里没有任何会触碰网络的位。
        let rc = unsafe {
            DnsQuery_W(
                wide.as_ptr(),
                wtype,
                DNS_QUERY_NO_WIRE_QUERY | DNS_QUERY_NO_MULTICAST,
                std::ptr::null_mut(),
                // windows-sys 的签名声明成 DNS_RECORDA（窄字符版）；两者布局逐字段等价
                // （只有指针指向的元素类型不同），故这里直接转。
                &mut list as *mut *mut DNS_RECORDW as *mut *mut DNS_RECORDA,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 || list.is_null() {
            // 9701 = 记录不存在（缓存里没有/已过期）。这是**正常返回**，不是错误日志。
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cur = list;
        let mut guard = 0usize;
        while !cur.is_null() && guard < MAX_RECORDS {
            guard += 1;
            // SAFETY: 链上每个节点都由 dnsapi 分配、按 DNS_RECORDW 对齐且有效
            let rec = unsafe { &*cur };
            // 一次查询可能顺带返回 CNAME 链上的记录，只取类型对得上的
            if rec.wType == wtype {
                let data = match wtype {
                    DNS_TYPE_A => {
                        // A 记录：`DNS_A_DATA.IpAddress` 在内存里的 4 个字节**就是**网络序
                        // 地址，用 to_ne_bytes 原样取出（不要按整数再转一次大小端）
                        // SAFETY: wType == A 时 Data 联合体的活跃成员就是 A
                        unsafe { rec.Data.A.IpAddress }.to_ne_bytes().to_vec()
                    }
                    DNS_TYPE_AAAA => {
                        // SAFETY: wType == AAAA 时活跃成员是 AAAA，其 IP6_ADDRESS 是联合体
                        unsafe { rec.Data.AAAA.Ip6Address.IP6Byte }.to_vec()
                    }
                    _ => Vec::new(),
                };
                if !data.is_empty() {
                    out.push(data);
                }
            }
            cur = rec.pNext;
        }
        // SAFETY: `list` 是 DnsQuery_W 分配、尚未释放的记录链；DnsFreeRecordList 是
        // 官方指定的释放方式（旧名 DnsRecordListFree，两者等价）
        unsafe { DnsFree(list as *const core::ffi::c_void, DnsFreeRecordList) };
        out
    }

    pub(super) fn read() -> HashMap<IpAddr, String> {
        let mut head: *mut DnsCacheEntry = std::ptr::null_mut();
        // SAFETY: 只写一个 out 指针；失败时 head 保持空
        let ok = unsafe { DnsGetCacheDataTable(&mut head) };
        if ok == 0 || head.is_null() {
            return HashMap::new();
        }

        let mut names: Vec<(String, u16)> = Vec::new();
        let mut cur = head;
        let mut guard = 0usize;
        while !cur.is_null() && guard < MAX_ENTRIES {
            guard += 1;
            // SAFETY: 链上每个节点都由 dnsapi 分配且有效
            let entry = unsafe { &*cur };
            // 下一个节点要先捞出来：本轮的这块马上就被释放了
            let next = entry.p_next;
            // 只要 A/AAAA：其余类型（CNAME/PTR/ANY…）查出来也不是地址，白花一次调用
            if entry.w_type == DNS_TYPE_A || entry.w_type == DNS_TYPE_AAAA {
                // SAFETY: name 指向 dnsapi 字符串池里的 NUL 结尾 UTF-16 串
                let name = unsafe { pwstr_to_string(entry.name) };
                if !name.is_empty() {
                    names.push((name, entry.w_type));
                }
            }
            // **逐节点释放**：这张链表不是一整块内存，而是每个节点各自一次分配——
            // 本机实测：`HeapSize(GetProcessHeap(), 0, node)` 对每个节点都给得出大小
            // （首个 48 字节，其余 24），节点前的堆块标签是 `LMEM`（LocalAlloc）。
            // 搜到哪放到哪（先存 pNext 再放）→ 一次都不泄漏；不释放的话 318 条 × 24B
            // 每秒一次，开一天就是 GB 级的增长。
            // 只放节点、**不放 `name`**：名字指针在 dnsapi 自己的池子里（实测与节点
            // 相距好几 MB），不是我们的内存。
            // 释放约定没有官方文档（见模块注释），故这里是**锤出来的**：3000 轮
            // 「遍历 + 逐节点 LocalFree」后 `HeapValidate` 仍然干净。
            // SAFETY: 该节点由 LocalAlloc 分配、本函数不再使用它
            unsafe { LocalFree(cur as *mut core::ffi::c_void) };
            cur = next;
        }

        let mut entries: Vec<RawRecord> = Vec::with_capacity(names.len());
        for (name, wtype) in names {
            for data in query_cached(&name, wtype) {
                entries.push(RawRecord {
                    name: name.clone(),
                    wtype,
                    data,
                });
            }
        }
        build_map(&entries)
    }
}
