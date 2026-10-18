//! 最小 MaxMind DB（`.mmdb`）读取器：只回答「这个 IP 属于哪个国家」。
//!
//! ## 为什么要有它（原方案里写着「判据不依赖 GeoIP」，现在改了）
//!
//! 因为不读它就会**制造假警报**。规则里写着 `GEOIP,CN,DIRECT` / `GEOSITE,cn,DIRECT`
//! ——国内目标直连是**我们自己定的行为**；而浏览器天生是「国内直连 + 国外走代理」的
//! 混合体，于是每访问一个国内站点就多一条「泄漏」。现场实测：用户报的 5 条里 4 条是
//! 国内 IP（上海移动 ×3、长沙电信 ×1，同一 /24 在 mihomo 里就是 `GeoIP(cn) → DIRECT`），
//! 只有 1 条 `msedge.exe → 135.234.174.40`（US）是真信号。假警报的代价不是噪音，是
//! 用户学会无视整个面板之后，真泄漏也一起被无视。
//!
//! 原方案反对的两条理由现在都不成立：
//!   1. 「Rust 侧没有 mmdb reader」——本文件就是；
//!   2. 「与 mihomo 内嵌库版本不一致会产生无法裁决的自相矛盾」——**读的是同一个文件**：
//!      `{data_dir}/net/country.mmdb` 就是 `geodata::GEO_FILES` 里 mihomo 自己加载的
//!      那份（实测 `mihomo -d <dir>` 只认目录根下这一个名字）。同源同文件，两边结论
//!      在构造上不可能打架。
//!
//! ## 为什么手写（不引 crate）
//!
//! 仓库既有约束：不新增依赖。而这里要的只是格式里极小的一撮——搜索树、元数据，
//! 以及 map/string/uint/pointer 四种值。一份 8MB 的只读数据文件不值得为它背一个依赖树
//! （`maxminddb` 那类 crate 还会带 `memmap`、`ipnetwork` 一串传递依赖）。
//!
//! ## 格式里两处反直觉的地方（都**实测纠正过**，别再想当然）
//!
//! 其一：搜索树与数据段之间有 **16 字节分隔符**，而记录值里的指针又**预扣了这 16**：
//!
//! ```text
//! 数据段起点   = 树长 + 16
//! 记录值       = node_count + 16 + 数据段内偏移   （故偏移 = 记录值 - node_count - 16）
//! 指针值       = 数据段内偏移                     （不预扣）
//! ```
//!
//! 两边都加这 16 会让**每一条**记录都偏移 16 字节：解出来的是垃圾——国内 IP 查成 `None`
//! （假警报照旧），另一些 IP 落到指针上被跟到别的记录（显示出错的国家码）。判断依据是
//! 拿真 `country.mmdb` 与 Python `maxminddb` 参考实现逐字段对齐：507 个 IP（v4/v6 混合）
//! 的完整记录，零处不符。
//!
//! 其二：**元数据里的指针相对「标记之后」**（`marker + 14`），不是相对标记本身。实测同一
//! 个指针值 122：按 `marker` 解出来是浮点垃圾，按 `marker + 14` 解出来是 `languages`
//! 列表里的 `"en"`（与 `description` 共享的那份字符串）。
//!
//! ## 失败方向
//!
//! 文件缺失/截断/格式不认识 → `None`（未知国家）→ 调用方维持原有判据（该报 Leak 照报）。
//! **绝不猜**：猜错的表现是把真泄漏说成「国内直连，属设计意图」，比假警报更糟。

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// 元数据标记（MaxMind DB 规范固定值；取文件里**最后**一次出现的位置）。
/// 长度 14 也是规范的一部分：元数据内容紧跟在它后面。
const METADATA_MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";
/// 搜索树与数据段之间的固定分隔符长度（见模块注释）。
const DATA_SECTION_SEPARATOR: usize = 16;
/// 指针/嵌套值的递归上限：畸形数据里的指针环不能把栈打爆。
const MAX_DEPTH: u8 = 32;

// ---------------------------------------------------------------- 库

/// 一份已解析的库。整个文件留在内存里，查询只做内存游走（不 mmap，免引 crate）。
pub struct CountryDb {
    bytes: Vec<u8>,
    node_count: usize,
    record_size: u16,
    ip_version: u16,
}

impl CountryDb {
    /// 从字节串建库。**纯函数**——测试造的金样就走这条路。
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let marker = rfind(&bytes, METADATA_MARKER)
            .ok_or_else(|| "不是 MaxMind DB：找不到元数据标记".to_string())?;
        // 元数据内容紧跟在标记之后（指针也以这里为基准，见模块注释）
        let meta_base = marker + METADATA_MARKER.len();
        let mut pos = meta_base;
        let meta = decode(&bytes, &mut pos, 0, meta_base).ok_or("元数据无法解码")?;
        // 主版本必须是 2：1 的树布局不同，按 2 去解只会读出垃圾
        if meta.uint("binary_format_major_version") != Some(2) {
            return Err("不是 MaxMind DB 2.x：主版本号对不上".to_string());
        }
        let node_count = meta.uint("node_count").ok_or("元数据缺少 node_count")? as usize;
        let record_size = meta.uint("record_size").ok_or("元数据缺少 record_size")?;
        let ip_version = meta.uint("ip_version").ok_or("元数据缺少 ip_version")?;
        if !matches!(record_size, 24 | 28 | 32) {
            return Err(format!("不支持的 record_size: {record_size}"));
        }
        if !matches!(ip_version, 4 | 6) {
            return Err(format!("不支持的 ip_version: {ip_version}"));
        }
        let db = Self {
            bytes,
            node_count,
            record_size: record_size as u16,
            ip_version: ip_version as u16,
        };
        // 树 + 分隔符必须整个落在文件里，且不能吃掉元数据：否则每次游走都是越界读
        if db.data_section_start() > marker {
            return Err("搜索树长度与文件实际大小不符".to_string());
        }
        Ok(db)
    }

    /// IP → 国家 ISO 码（大写）。查不到（库里没这条 / 记录里没有国家字段）给 `None`。
    pub fn country_of(&self, ip: IpAddr) -> Option<String> {
        iso_code_of(&self.lookup(ip)?)
    }

    fn node_byte_size(&self) -> usize {
        self.record_size as usize * 2 / 8
    }

    fn tree_len(&self) -> usize {
        self.node_count * self.node_byte_size()
    }

    /// 数据段起点 = 树长 + 16 字节分隔符（见模块注释）。
    fn data_section_start(&self) -> usize {
        self.tree_len() + DATA_SECTION_SEPARATOR
    }

    /// 走搜索树，返回终点的**值**。
    ///
    /// 记录值的语义：`< node_count` 还是节点、`== node_count` 空、`> node_count` 数据指针。
    fn lookup(&self, ip: IpAddr) -> Option<Val> {
        let record = self.lookup_record(ip)?;
        // 记录值 = node_count + 16 + 数据段内偏移（模块注释里那条「其一」）
        let offset = record.checked_sub(self.node_count + DATA_SECTION_SEPARATOR)?;
        let base = self.data_section_start();
        let mut pos = base.checked_add(offset)?;
        decode(&self.bytes, &mut pos, 0, base)
    }

    /// 沿地址的二进制位走树，返回终点的**记录值**。
    fn lookup_record(&self, ip: IpAddr) -> Option<usize> {
        // v4 地址在 v6 库里要走 **96 个 0 位**（规范：IPv4 子树挂在 `::/96` 下）——
        // 注意**不是** `to_ipv6_mapped()` 的 `::ffff:a.b.c.d`：那是 IPv4-mapped 前缀，
        // 只因为真实 GeoLite2 库里 `::ffff:0:0/96` 是 `::/96` 的**别名**（同一批节点
        // 被两个前缀共享）才碰巧也对。换一份没有别名的库就会查错，而查错的后果是
        // 「国内目标被判成外网」——正是要修的这个 bug。MaxMind 官方 reader 的做法也是
        // 先走 96 个 0 位拿到 `ipv4_start` 节点。
        //
        // 反过来，IPv4-mapped 的 v6 地址说的就是 v4，按 v4 查：Windows 双栈 socket
        // 表里满是这种形态（`::ffff:1.2.3.4`），不拆开就查不出国家。
        let ip = match ip {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(v6),
            },
            v4 => v4,
        };
        let octets: Vec<u8> = match (self.ip_version, ip) {
            (4, IpAddr::V4(v4)) => v4.octets().to_vec(),
            // v4 库查 v6 地址：这棵树里根本没有 v6 的空间，不猜
            (4, IpAddr::V6(_)) => return None,
            (_, IpAddr::V4(v4)) => {
                // `::a.b.c.d`（IPv4-compatible）：前 96 位全 0，后 32 位是地址
                let mut o = [0u8; 16];
                o[12..].copy_from_slice(&v4.octets());
                o.to_vec()
            }
            (_, IpAddr::V6(v6)) => v6.octets().to_vec(),
        };
        let mut node = 0usize;
        for byte in octets {
            for shift in (0..8).rev() {
                if node >= self.node_count {
                    return None; // 已经落在「空」或数据上，没有位可走了
                }
                let record = self.record_at(node, (byte >> shift) & 1)?;
                if record == self.node_count {
                    return None; // 空记录：这个前缀没有任何数据
                }
                if record > self.node_count {
                    return Some(record); // 数据指针：整段前缀共用这一条记录
                }
                node = record;
            }
        }
        None
    }

    /// 取节点 `node` 的第 `bit` 个记录（0 = 左，1 = 右）。
    ///
    /// 28 位记录是**交错**存的：中间那个字节的一半归左记录、一半归右记录
    /// （规范里唯一一处位级拼接，也是最容易写反的地方）。
    fn record_at(&self, node: usize, bit: u8) -> Option<usize> {
        let width = self.node_byte_size();
        let base = node * width;
        let b = self.bytes.get(base..base + width)?;
        Some(match (self.record_size, bit) {
            (24, 0) => u32::from_be_bytes([0, b[0], b[1], b[2]]) as usize,
            (24, _) => u32::from_be_bytes([0, b[3], b[4], b[5]]) as usize,
            (28, 0) => {
                (((b[3] >> 4) as usize) << 24)
                    | ((b[0] as usize) << 16)
                    | ((b[1] as usize) << 8)
                    | b[2] as usize
            }
            (28, _) => {
                (((b[3] & 0x0f) as usize) << 24)
                    | ((b[4] as usize) << 16)
                    | ((b[5] as usize) << 8)
                    | b[6] as usize
            }
            (32, 0) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize,
            (32, _) => u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize,
            _ => return None,
        })
    }
}

/// 从记录里挑国家码：`{"country":{"iso_code":"CN"}}` 是 GeoLite2 的形状；
/// 没有 `country` 时退到 `registered_country`；顶层 `iso_code` 是别的库的写法。
fn iso_code_of(val: &Val) -> Option<String> {
    for key in ["country", "registered_country"] {
        if let Some(code) = val.get(key).and_then(|c| c.str("iso_code")) {
            return Some(code.to_ascii_uppercase());
        }
    }
    val.str("iso_code").map(|c| c.to_ascii_uppercase())
}

// ---------------------------------------------------------------- 带缓存的查询句柄

/// 采样周期内复用的查询句柄：**文件只读一次**。
///
/// 8MB 的文件按每秒采样重读一次是不可接受的（每秒 8MB 磁盘 IO 只为查几十个 IP），
/// 故第一次查询时载入并常驻内存。删文件也不影响：库已经在内存里了。
///
/// 两种失败状态分开处理，因为**恢复方式不同**：
/// - 文件不在：geo 数据可能还没铺下来（`ensure_geodata` 在开启网络时跑），**下次再试**
///   ——代价只是一次 stat；
/// - 文件在但解不开：不再重试。否则一个损坏的文件会被每秒重读 8MB，而它自愈的唯一
///   现实途径是重新下载（那要等下一次开启网络，届时会新建一个句柄）。
pub struct CountryLookup {
    path: PathBuf,
    state: RwLock<State>,
}

enum State {
    Unknown,
    Loaded(Arc<CountryDb>),
    Broken,
}

impl CountryLookup {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: RwLock::new(State::Unknown),
        }
    }

    /// IP → 国家码。未知一律 `None`（调用方据此维持原判据，**绝不因为查不到就放宽**）。
    pub fn country_of(&self, ip: IpAddr) -> Option<String> {
        self.load()?.country_of(ip)
    }

    /// 取已载入的库；该载则载。读锁快路径不阻塞并发查询。
    fn load(&self) -> Option<Arc<CountryDb>> {
        if let State::Loaded(db) = &*crate::util::read_ignore_poison(&self.state) {
            return Some(db.clone());
        }
        let mut st = crate::util::write_ignore_poison(&self.state);
        // 双重检查：等锁期间可能已经被别的线程载好了
        if let State::Loaded(db) = &*st {
            return Some(db.clone());
        }
        if matches!(*st, State::Broken) {
            return None;
        }
        match std::fs::read(&self.path) {
            Ok(bytes) => match CountryDb::from_bytes(bytes) {
                Ok(db) => {
                    let db = Arc::new(db);
                    *st = State::Loaded(db.clone());
                    Some(db)
                }
                Err(_) => {
                    // 损坏：记下来，别再每秒重读 8MB
                    *st = State::Broken;
                    None
                }
            },
            // 不在：状态留在 Unknown，下次再试（它随时可能被 geodata 铺下来）
            Err(_) => None,
        }
    }
}

// ---------------------------------------------------------------- 值解码

/// 解码出来的值。只保留真的会用到的形状——country.mmdb 里就是嵌套的 map 和字符串。
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Map(HashMap<String, Val>),
    Str(String),
    Uint(u64),
    Int(i64),
    Bytes(Vec<u8>),
    Bool(bool),
    Array(Vec<Val>),
    Float(f64),
    /// 认识类型但用不到、或者解不出来的值（游标仍然停在正确位置）
    Other,
}

impl Val {
    fn get(&self, key: &str) -> Option<&Val> {
        match self {
            Val::Map(m) => m.get(key),
            _ => None,
        }
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Val::Str(s)) => Some(s),
            _ => None,
        }
    }

    fn uint(&self, key: &str) -> Option<u64> {
        match self.get(key) {
            Some(Val::Uint(n)) => Some(*n),
            _ => None,
        }
    }
}

/// 解一个指针：`(控制字节低 5 位, 后续字节)` → `(值, 消耗字节数)`。
///
/// 四种宽度都要支持（1/2/3/4 字节，后三种各带 2048 / 526336 的基址）：真实
/// `country.mmdb` 的数据段里四种都能扫到，只实现短的那种会在别的库上静默失效。
pub(crate) fn decode_pointer(size_bits: u8, rest: &[u8]) -> Option<(usize, usize)> {
    let high = (size_bits & 0x07) as usize;
    match (size_bits >> 3) & 0x03 {
        0 => Some(((high << 8) | *rest.first()? as usize, 1)),
        1 => {
            let b = rest.get(..2)?;
            Some((
                ((high << 16) | ((b[0] as usize) << 8) | b[1] as usize) + 2048,
                2,
            ))
        }
        2 => {
            let b = rest.get(..3)?;
            Some((
                ((high << 24) | ((b[0] as usize) << 16) | ((b[1] as usize) << 8) | b[2] as usize)
                    + 526336,
                3,
            ))
        }
        _ => {
            let b = rest.get(..4)?;
            Some((u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 4))
        }
    }
}

/// 解一个值。`pos` 停在值的下一个字节上；解不出来给 `None`。
///
/// `base` 是指针值的基准：数据段里的值以数据段起点为基准，元数据里的值以元数据内容
/// 起点为基准（实测结论，见模块注释）。`depth` 防指针环。
fn decode(bytes: &[u8], pos: &mut usize, depth: u8, base: usize) -> Option<Val> {
    if depth > MAX_DEPTH {
        return None;
    }
    let ctrl = *bytes.get(*pos)?;
    *pos += 1;
    let mut ty = ctrl >> 5;
    let size_bits = ctrl & 0x1f;
    if ty == 0 {
        // 扩展类型：真实类型 = 类型字节 + 7。**顺序是「类型字节在长度转义之前」**，
        // 写反了在纯 ASCII 的小文件上照样能跑，遇到 uint64/double 才炸。
        ty = bytes.get(*pos)? + 7;
        *pos += 1;
    }
    let size = match size_bits {
        n if n < 29 => n as usize,
        29 => {
            let n = 29 + *bytes.get(*pos)? as usize;
            *pos += 1;
            n
        }
        30 => {
            let b = bytes.get(*pos..*pos + 2)?;
            *pos += 2;
            285 + u16::from_be_bytes([b[0], b[1]]) as usize
        }
        _ => {
            let b = bytes.get(*pos..*pos + 3)?;
            *pos += 3;
            65821 + u32::from_be_bytes([0, b[0], b[1], b[2]]) as usize
        }
    };
    match ty {
        // 指针：值是数据段内偏移，**就地跟进去**（掉进环或者越界都当「不认识」，
        // 而不是让整份元数据/整条记录解码失败——我们不关心的字段不该拖垮关心的字段）
        1 => {
            let resolved =
                decode_pointer(size_bits, bytes.get(*pos..)?).and_then(|(value, used)| {
                    *pos += used;
                    let mut ptr = base.checked_add(value)?;
                    decode(bytes, &mut ptr, depth + 1, base)
                });
            // 解不开的指针当「不认识」：一个不关心的字段（如 languages 列表里的共享
            // 字符串）不该让整份元数据解不出来，那会连 node_count 都拿不到
            Some(resolved.unwrap_or(Val::Other))
        }
        2 => {
            let s = bytes.get(*pos..*pos + size)?;
            *pos += size;
            Some(Val::Str(String::from_utf8_lossy(s).into_owned()))
        }
        3 => {
            let b = bytes.get(*pos..*pos + 8)?;
            *pos += 8;
            Some(Val::Float(f64::from_be_bytes(b.try_into().ok()?)))
        }
        4 => {
            let b = bytes.get(*pos..*pos + size)?;
            *pos += size;
            Some(Val::Bytes(b.to_vec()))
        }
        5 | 6 | 9 | 10 => {
            let b = bytes.get(*pos..*pos + size)?;
            *pos += size;
            let mut n: u64 = 0;
            for byte in b {
                n = (n << 8) | *byte as u64;
            }
            Some(Val::Uint(n))
        }
        7 => {
            let mut m = HashMap::with_capacity(size);
            for _ in 0..size {
                let k = decode(bytes, pos, depth + 1, base)?;
                let val = decode(bytes, pos, depth + 1, base)?;
                let Val::Str(k) = k else { return None };
                m.insert(k, val);
            }
            Some(Val::Map(m))
        }
        8 => {
            let b = bytes.get(*pos..*pos + size)?;
            *pos += size;
            let mut n: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) {
                -1
            } else {
                0
            };
            for byte in b {
                n = (n << 8) | *byte as i64;
            }
            Some(Val::Int(n))
        }
        11 => {
            let mut a = Vec::with_capacity(size);
            for _ in 0..size {
                a.push(decode(bytes, pos, depth + 1, base)?);
            }
            Some(Val::Array(a))
        }
        14 => Some(Val::Bool(size != 0)),
        15 => {
            let b = bytes.get(*pos..*pos + 4)?;
            *pos += 4;
            Some(Val::Float(f32::from_be_bytes(b.try_into().ok()?) as f64))
        }
        // 不认识（含 12/13 这类缓存容器与结束标记）：长度未知，**停在这里**，
        // 而不是硬着头皮往下读一个字节数不对的值
        _ => None,
    }
}

/// 从后往前找子串（元数据标记取**最后**一次出现——数据段里出现同样的字节序列时，
/// 只有最后一次才是真标记）。
fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
}
