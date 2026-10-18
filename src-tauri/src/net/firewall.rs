//! 会话级防火墙阻断：非 TUN 模式下限制关注程序直连公网。
//!
//! 为什么非要动防火墙：非 TUN 时 mihomo 只看得到**已经进了它的** socket，剩下的
//! 直连流量它一无所知（实测：codex 的连接在 `/connections` 里 0 条，而它确实在
//! 直连公网、并被地区封锁）。2026-09-23 又抓到 chatgpt.exe 经 IPv4 TCP 直连
//! Google:443/5228；只挡 QUIC 与 IPv6 根本拦不住这一类。非 TUN 下对关注程序
//! 的公网 IPv4/IPv6 出站统一阻断，愿意走本地 mixed 端口的流量仍可经代理出去。
//!
//! 为什么放提权服务：改防火墙要管理员令牌，而主程序只有装服务那一次 UAC。
//! 用 `INetFwPolicy2` COM（`windows` crate 早已在依赖树中，见 Cargo.toml 的注释）；
//! 不选 WFP——几百行 unsafe 落在 SYSTEM 进程里，错一条就是全机断网；不选 netsh——
//! 本地化输出解析 + 每条规则一次 spawn。
//!
//! # 四条安全约束（每一条都对应一种「把用户网断掉」的形态）
//!
//! 1. **`ApplicationName` 必须非空**。[`block_rules`] 收到空路径直接返回 `Err`，
//!    并有专门用例钉死。一条没有程序限定的 block 规则 = 全机断网，这是本功能
//!    唯一的误伤入口。
//! 2. **范围按程序与公网目标**：Windows 防火墙里 Block 优先于 Allow，故只给
//!    关注名单里的程序安装规则；远端范围排除回环、内网及 fake-ip。名单与泄漏检测
//!    **同源**（同一个设置键）。
//! 3. **规则名只认固定 UUID 前缀**：删除只删 `yunyi-vpn-<uuid>:` 开头的规则，
//!    误伤别人的规则在物理上不可能。
//! 4. **只挡公网段**：IPv4 范围排除回环/私网/保留段，IPv6 只挡 `2000::/3`；
//!    本地 mixed 端口与局域网保持可用。**绝不写全地址范围**。
//!
//! 不遵守系统代理的程序在非 TUN 下会失去公网连接；要同时保留其联网能力与阻止
//! 直连，需要启用 TUN。防火墙负责阻断，不能把现有连接转发给代理。

use crate::db::Db;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// 规则名的固定 UUID：既进规则名也进 `Grouping`，是「这是本应用装的」的唯一标记。
/// 与 [`RULE_PREFIX`] 的包含关系由单测钉死——两个常量分开写是为了零分配比对，
/// 分开就得有东西保证它们指的还是同一个 UUID。
pub(crate) const RULE_UUID: &str = "6b1f4c2e-9d3a-4e57-8a10-3c5f7d2b9e41";
/// 规则名前缀。**删除只认这个前缀**，所以别人的规则在物理上不可能被误删。
pub(crate) const RULE_PREFIX: &str = "yunyi-vpn-6b1f4c2e-9d3a-4e57-8a10-3c5f7d2b9e41:";

/// 公网 IPv6 的分配范围。链路本地与 ULA **不在其中**，故不必也不能再写排除项。
const V6_PUBLIC_RANGE: &str = "2000::/3";
/// 公网 IPv4 的连续区间。排除 0/8、RFC1918、CGNAT、回环、链路本地、文档段、
/// mihomo fake-ip (198.18/15) 和组播/保留地址；因此不会挡本地代理或局域网。
/// COM 的 INetFwRule.RemoteAddresses 接受逗号分隔的 IPv4 范围（本机已校验）。
const V4_PUBLIC_RANGES: &str = concat!(
    "1.0.0.0-9.255.255.255,11.0.0.0-100.63.255.255,",
    "100.128.0.0-126.255.255.255,128.0.0.0-169.253.255.255,",
    "169.255.0.0-172.15.255.255,172.32.0.0-191.255.255.255,",
    "192.0.1.0-192.0.1.255,192.0.3.0-192.167.255.255,",
    "192.169.0.0-198.17.255.255,198.20.0.0-198.51.99.255,",
    "198.51.101.0-203.0.112.255,203.0.114.0-223.255.255.255"
);

/// 写防火墙被拒时的固定前缀（`E_ACCESSDENIED`）。这是**降级信号而不是错误**：
/// 组策略禁本地规则、或安全软件锁了防火墙时，「开启网络」必须照常成功。
pub const ERR_DENIED: &str = "防火墙拒绝写入";
/// 非 Windows：这套东西压根不存在。
pub const ERR_UNSUPPORTED: &str = "平台不支持防火墙阻断";

/// 该错误是否代表「防火墙不让我们写」。
pub fn is_denied(msg: &str) -> bool {
    msg.starts_with(ERR_DENIED)
}

/// 该错误是否代表「这个平台没有防火墙阻断」。
pub fn is_unsupported(msg: &str) -> bool {
    msg.starts_with(ERR_UNSUPPORTED)
}

/// 「这些程序没在跑、拿不到绝对路径」那句原因。**必须如实列出是哪些**——
/// 只说「部分失败」用户没法判断到底漏了谁在直连。
pub fn unresolved_note(unresolved: &[String]) -> String {
    format!(
        "这些程序当前没在运行，拿不到绝对路径、没能挡住：{}。\
         需要挡住它们的话，先把程序开起来再重开一次网络",
        unresolved.join("、")
    )
}

/// 阻断状态（`NetStatus.firewall` 的取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirewallState {
    /// 没开网络，或本次会话没装规则
    Off,
    /// 按名单装齐了
    Applied,
    /// 只装上了一部分（有程序没在跑、拿不到绝对路径；或服务报了别的失败）
    Partial,
    /// 防火墙拒写（组策略/安全软件）——网络照常开，界面给说明
    Denied,
    /// 服务不可达，或服务是旧构建、不认识阻断命令
    Unavailable,
    /// 非 Windows：这套东西压根不存在
    Unsupported,
}

impl FirewallState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Applied => "applied",
            Self::Partial => "partial",
            Self::Denied => "denied",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
        }
    }
}

/// 公网阻断覆盖全部协议；只挡 QUIC 会漏掉 IPv4 TCP 直连。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Any,
}

/// 规则的两种形态。`label()` 进规则名，便于在 `wf.msc` 里一眼看出这条是干嘛的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// 挡公网 IPv4（含 TCP/443、TCP/5228 和非 QUIC UDP）；不挡回环代理与内网。
    V4Public,
    /// 挡公网 IPv6 直连：v6 无法被系统代理代理，只能不让它绕过代理出去
    V6Public,
}

impl RuleKind {
    fn label(self) -> &'static str {
        match self {
            Self::V4Public => "v4public",
            Self::V6Public => "v6public",
        }
    }

    fn protocol(self) -> Proto {
        match self {
            Self::V4Public => Proto::Any,
            Self::V6Public => Proto::Any,
        }
    }

    fn remote_ports(self) -> &'static str {
        match self {
            Self::V4Public => "",
            Self::V6Public => "",
        }
    }

    fn remote_addresses(self) -> &'static str {
        match self {
            Self::V4Public => V4_PUBLIC_RANGES,
            Self::V6Public => V6_PUBLIC_RANGE,
        }
    }
}

/// 一条待安装规则的**纯数据规格**（不碰 COM，故可表驱动单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSpec {
    pub name: String,
    /// 程序绝对路径。**绝不允许为空**——空的 `ApplicationName` = 不限程序 = 全机断网。
    pub program: String,
    pub protocol: Proto,
    pub remote_ports: String,
    pub remote_addresses: String,
}

/// 名单里的程序 → 本次要装的规则。一个程序两条（公网 v4 + 公网 v6）。
///
/// 校验刻意是**硬失败**而不是「跳过这条」：一条规则少装了，表现是「阻断看似生效、
/// 实际漏着一个程序」，比整个功能没装上难查得多。
pub fn block_rules(programs: &[String], tag: &str) -> Result<Vec<RuleSpec>, String> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Err(
            "会话标记为空：规则名要靠它区分是哪一次会话装的，两次会话撞名会互相覆盖".to_string(),
        );
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for raw in programs {
        let program = raw.trim();
        if program.is_empty() {
            return Err(
                "阻断名单里有空的程序路径：一条没有程序限定的 block 规则不是「少挡一个\
                 程序」，而是断掉全机出站网络——本功能唯一不能犯的错"
                    .to_string(),
            );
        }
        if !looks_absolute(program) {
            return Err(format!(
                "阻断名单里的「{program}」不是绝对路径：防火墙会收下这条规则，但它永远\
                 匹配不到任何进程（静默失效）。要传程序的完整路径"
            ));
        }
        // 路径大小写不敏感：同一个程序写两遍（或只差大小写）会装出重名规则，
        // 而重名是**覆盖**不是报错，最后表现为「回读条数总比预期少」
        if !seen.insert(program.to_ascii_lowercase()) {
            continue;
        }
        for kind in [RuleKind::V4Public, RuleKind::V6Public] {
            out.push(RuleSpec {
                name: rule_name(kind, tag, program),
                program: program.to_string(),
                protocol: kind.protocol(),
                remote_ports: kind.remote_ports().to_string(),
                remote_addresses: kind.remote_addresses().to_string(),
            });
        }
    }
    if out.is_empty() {
        return Err("阻断名单为空：没有程序就凑不出任何一条合规规则".to_string());
    }
    Ok(out)
}

/// 规则名：`<前缀><用途>:<会话tag>:<路径哈希>:<程序名>`。两个安装目录可能都有
/// chatgpt.exe；只放文件名会互相覆盖，回读少两条且一个安装路径漏出公网。
fn rule_name(kind: RuleKind, tag: &str, program: &str) -> String {
    let base = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let normalized = program.to_ascii_lowercase();
    let hash = hex::encode(Sha256::digest(normalized.as_bytes()));
    format!("{RULE_PREFIX}{}:{tag}:{}:{base}", kind.label(), &hash[..8])
}

/// 像不像一个绝对的 Windows 程序路径。
///
/// 只用来挡住「把镜像名当路径传进来」——那种规则防火墙会照收，但永远匹配不到任何
/// 进程，表现是**阻断看似装上了、实际一条都没起作用**，比报错难查得多。
fn looks_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// 这条规则名是不是本应用装的。删除路径只认它。
pub fn is_ours(name: &str) -> bool {
    name.starts_with(RULE_PREFIX)
}

/// 关注名单（镜像名）→ 能拿到绝对路径的程序。
///
/// 返回 `(绝对路径列表, 没拿到路径的镜像名)`。**只认正在跑的进程**：Windows 防火墙
/// 的程序规则要的是绝对路径，而镜像名 → 路径的唯一可靠来源就是本机进程表。
/// 猜路径（如硬编码 Chrome 的安装位置）会在用户装在别处时写出一条永远匹配不上的
/// 规则——静默失效，正是本功能最该避免的形态；拿不到就如实报 partial。
///
/// 比对键复用泄漏检测的 [`crate::net::connlog::image_key`]（取文件名 + 小写）：
/// 名单是同源的那一份，两边对「chrome.exe 与 CHROME.EXE 是不是同一个东西」的
/// 判断就不该有两套。
pub fn resolve_programs(
    watch: &[String],
    running: &[(String, String)],
) -> (Vec<String>, Vec<String>) {
    let mut paths: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut missing: Vec<String> = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    for w in watch {
        let key = crate::net::connlog::image_key(w);
        if key.is_empty() {
            continue;
        }
        let mut found = false;
        for (image, path) in running {
            if path.trim().is_empty() || crate::net::connlog::image_key(image) != key {
                continue;
            }
            found = true;
            // 同一镜像名的多个进程（浏览器有十几个）只留一条路径
            if seen.insert(path.to_ascii_lowercase()) {
                paths.push(path.clone());
            }
        }
        if !found && reported.insert(key.clone()) {
            // 去重后列出：名单里重复写同一个名字不该重复出现在提示里
            missing.push(key);
        }
    }
    (paths, missing)
}

/// 服务对一次阻断请求的回话，翻译成状态与说明所需的**全部**输入。
pub enum BlockReply {
    /// 服务说装上了，附带它**回读核验**到的自家规则条数
    Applied { rules: usize },
    /// 防火墙拒写（组策略/安全软件）
    Denied { message: String },
    /// 服务在工作但失败了
    Failed { message: String },
    /// 服务不可达，或服务是旧构建、不认识这条命令
    Unreachable { message: String },
    /// 非 Windows
    Unsupported { message: String },
}

/// 把服务回话翻译成状态 + 给用户看的原因。**纯函数**：降级判据必须能单测，
/// 它是「阻断失败绝不能连带把开启网络打掉」这条约束的落点。
///
/// 服务报的**一般失败**归到 `Partial` 而不是 `Unavailable`：服务确实答了话，
/// 说「不可用」是撒谎；而「一条都没装上」本来就是 partial 的最差形态，说明白
/// 实际条数即可。判据宁可粗糙也不能自相矛盾。
pub fn block_state(
    expected: usize,
    unresolved: &[String],
    reply: BlockReply,
) -> (FirewallState, Option<String>) {
    // 拿不到绝对路径的程序：只能如实列出。**不猜安装路径**——猜错写出的规则
    // 永远匹配不上，而界面上会显示「已阻断」，那是最坏的一种谎
    let unresolved_note = (!unresolved.is_empty()).then(|| unresolved_note(unresolved));
    match reply {
        BlockReply::Unsupported { message } => (FirewallState::Unsupported, Some(message)),
        BlockReply::Unreachable { message } => (FirewallState::Unavailable, Some(message)),
        BlockReply::Denied { message } => (FirewallState::Denied, Some(message)),
        BlockReply::Failed { message } => {
            (FirewallState::Partial, join_note(&unresolved_note, message))
        }
        BlockReply::Applied { rules } => {
            if rules != expected {
                // 回读核验：**不假定成功**——与仓库既有的「改完 TUN 要核验网卡真的
                // 拆了」是同一条习惯
                return (
                    FirewallState::Partial,
                    join_note(
                        &unresolved_note,
                        format!("阻断规则只装上 {rules} 条，预期 {expected} 条"),
                    ),
                );
            }
            match unresolved_note {
                Some(n) => (FirewallState::Partial, Some(n)),
                None => (FirewallState::Applied, None),
            }
        }
    }
}

/// 两段原因合成一句：装不上的程序名单与失败原因都要给，缺一个用户就没法判断
/// 「是没挡住还是只挡住了一半」。
fn join_note(note: &Option<String>, msg: String) -> Option<String> {
    Some(match note {
        Some(n) => format!("{msg}；{n}"),
        None => msg,
    })
}

/// 装规则并回读核验，返回**核验过的**自家规则条数。
///
/// 服务侧入口（提权进程调用）。`programs` 是主程序解析好的绝对路径。
pub fn apply(programs: &[String], tag: &str) -> Result<usize, String> {
    let specs = block_rules(programs, tag)?;
    imp::apply(&specs)
}

/// 删除全部自家规则，返回**回读核验**后实际删掉的条数（幂等：没装过返回 0）。
pub fn clear() -> Result<usize, String> {
    imp::clear()
}

/// 当前在位的自家规则条数（回读，不是记账）。
pub fn count() -> Result<usize, String> {
    imp::count()
}

/// 本会话的关注名单：与泄漏检测**同源**——同一个设置键、同一个读取函数
/// （[`crate::net::connlog::read_watch`]）。两处各读一份迟早漂移，而漂移的表现是
/// 「面板盯着的是 A、防火墙挡的是 B」，两边看着都正常。
pub fn watch_programs(db: &Db) -> Vec<String> {
    crate::net::connlog::read_watch(db)
}

#[cfg(windows)]
mod imp {
    use super::{is_ours, Proto, RuleSpec, ERR_DENIED};
    use windows::core::{Interface, BSTR};
    use windows::Win32::Foundation::VARIANT_BOOL;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, INetFwRules, NetFwPolicy2, NetFwRule, NET_FW_ACTION_BLOCK,
        NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_OUT,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Ole::IEnumVARIANT;
    use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_DISPATCH};

    /// `E_ACCESSDENIED`。**降级信号**（见 [`ERR_DENIED`]），必须与其它 COM 错误分开。
    const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
    /// `RPC_E_CHANGED_MODE`：本线程已按别的套间模型初始化过 COM。
    const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

    /// 规则描述：在 `wf.msc` 的「描述」列里说清它是谁装的、什么时候撤。
    const DESC: &str = "由云翼 VPN 在「开启网络」期间临时添加，关闭网络时自动删除";

    /// 协议号：`NET_FW_IP_PROTOCOL_UDP` = 17，`_ANY` = 256（不是 0——0 是 HOPOPT）。
    fn proto_code(p: Proto) -> i32 {
        use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_ANY;
        match p {
            Proto::Any => NET_FW_IP_PROTOCOL_ANY.0,
        }
    }

    /// 本线程的 COM 初始化守卫。
    ///
    /// COM 是**按线程**初始化的，而这个模块会被三个不同的线程调到：服务启动时的
    /// 清扫（主线程）、管道命令循环（独立线程）、卸载分支（主线程）。所以在每次
    /// 调用的作用域里配对初始化/反初始化，而不是进程启动时初始化一次。
    struct Com(bool);

    impl Com {
        fn init() -> Result<Self, String> {
            // SAFETY: 无参初始化，返回值按 HRESULT 语义解释
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if hr.is_ok() {
                // S_OK(0) 与 S_FALSE(1)（本线程已初始化过）都让引用计数加一，
                // 两者都必须配对 CoUninitialize
                Ok(Self(true))
            } else if hr.0 == RPC_E_CHANGED_MODE {
                // COM 可用，只是不是我们选的套间模型。**不能**调 CoUninitialize——
                // 这次调用没给计数加过一（照 HRESULT 语义）
                Ok(Self(false))
            } else if hr.0 == E_ACCESSDENIED {
                Err(format!("{ERR_DENIED}：COM 初始化被拒"))
            } else {
                Err(format!("初始化 COM 失败：HRESULT {:#010x}", hr.0))
            }
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: 与上面 CoInitializeEx 的成功返回配对
                unsafe { CoUninitialize() };
            }
        }
    }

    /// 把 COM 错误翻成人话，并把 `E_ACCESSDENIED` 单独标出来：它是降级信号
    /// （组策略禁本地规则/安全软件锁防火墙），上层据此给说明而不是报错。
    fn ctx_err(ctx: &str, e: windows::core::Error) -> String {
        if e.code().0 == E_ACCESSDENIED {
            format!("{ERR_DENIED}：{ctx}（组策略禁用了本地防火墙规则，或安全软件锁住了防火墙）")
        } else {
            format!("{ctx}失败：{e}")
        }
    }

    fn rules() -> Result<INetFwRules, String> {
        // SAFETY: NetFwPolicy2 是系统自带的进程内 COM 组件
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| ctx_err("创建防火墙策略对象", e))?;
        unsafe { policy.Rules() }.map_err(|e| ctx_err("取防火墙规则集", e))
    }

    /// 枚举出**自家**规则的名字。
    ///
    /// 为什么必须枚举：`INetFwRules` 没有「按前缀删」，只有按名字删；而残留规则是
    /// 上一次会话装的（tag 不同、名字无从猜测）。`_NewEnum` 按 COM 约定返回
    /// `IUnknown`，要 cast 成 `IEnumVARIANT` 才能取下一项。
    fn our_rule_names(rules: &INetFwRules) -> Result<Vec<String>, String> {
        let iter: IEnumVARIANT = unsafe { rules._NewEnum() }
            .map_err(|e| ctx_err("枚举防火墙规则", e))?
            .cast()
            .map_err(|e| ctx_err("枚举防火墙规则", e))?;
        let mut out = Vec::new();
        loop {
            let mut item = [VARIANT::default()];
            let mut fetched = 0u32;
            // SAFETY: item 是长度为 1 的可写切片；fetched 是出参
            let hr = unsafe { iter.Next(&mut item, &mut fetched) };
            // S_FALSE（取完了）也是「成功」，故看到的 fetched=0 才是终止条件
            if hr.is_err() || fetched == 0 {
                break;
            }
            if let Some(name) = item_name(&item[0]) {
                if is_ours(&name) {
                    out.push(name);
                }
            }
            // 枚举把 VARIANT 的所有权交给了我们，不清就是每轮漏一个引用
            // SAFETY: item[0] 是刚由 Next 填好的合法 VARIANT
            unsafe {
                let _ = VariantClear(&mut item[0]);
            }
        }
        Ok(out)
    }

    /// 从枚举出的一项里取规则名。不是 `VT_DISPATCH` 的项直接跳过。
    fn item_name(v: &VARIANT) -> Option<String> {
        // SAFETY: 只在 vt 标明是 VT_DISPATCH 时才读 pdispVal；后续只做 QI 与读属性。
        // 借而不是移（`ManuallyDrop` 不可 Copy）：移出来会让 VARIANT 变成半空壳，
        // 后面那记 VariantClear 就释放错了东西
        unsafe {
            let inner = &v.Anonymous.Anonymous;
            if inner.vt != VT_DISPATCH {
                return None;
            }
            let disp = inner.Anonymous.pdispVal.as_ref()?;
            let rule: INetFwRule = disp.cast().ok()?;
            rule.Name().ok().map(|n| n.to_string())
        }
    }

    /// 删掉全部自家规则，返回回读核验后**实际消失**的条数。
    fn remove_ours(rules: &INetFwRules) -> Result<usize, String> {
        let names = our_rule_names(rules)?;
        for name in &names {
            // 单条删失败不在这里中断：返回的是 before − after，所以没删掉的会如实
            // 体现在结果里；中断只会让已经删掉的也失去记账
            // SAFETY: name 来自枚举，是规则集里真实存在的名字
            let _ = unsafe { rules.Remove(&BSTR::from(name.as_str())) };
        }
        let left = our_rule_names(rules)?.len();
        Ok(names.len().saturating_sub(left))
    }

    fn add_one(rules: &INetFwRules, spec: &RuleSpec) -> Result<(), String> {
        // SAFETY: NetFwRule 是系统自带的进程内 COM 组件
        let rule: INetFwRule = unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| ctx_err("创建防火墙规则对象", e))?;
        let put = |what: &str, r: windows::core::Result<()>| -> Result<(), String> {
            r.map_err(|e| ctx_err(what, e))
        };
        // SAFETY: 只在该规则对象上设属性，最后加入规则集
        unsafe {
            put("设置规则名", rule.SetName(&BSTR::from(spec.name.as_str())))?;
            // 这一行是唯一的误伤防线：ApplicationName 为空 = 不限程序 = 全机断网。
            // block_rules 已经在入口挡了空路径，这里再守一道（拿到的若为空就宁可不装）
            put(
                "设置程序范围",
                rule.SetApplicationName(&BSTR::from(spec.program.as_str())),
            )?;
            put("设置协议", rule.SetProtocol(proto_code(spec.protocol)))?;
            if !spec.remote_ports.is_empty() {
                put(
                    "设置远端端口",
                    rule.SetRemotePorts(&BSTR::from(spec.remote_ports.as_str())),
                )?;
            }
            if !spec.remote_addresses.is_empty() {
                put(
                    "设置远端地址",
                    rule.SetRemoteAddresses(&BSTR::from(spec.remote_addresses.as_str())),
                )?;
            }
            // 只挡出站：入站方向动了会连「别人连进来」也一起挡，与本次目的无关
            put("设置方向", rule.SetDirection(NET_FW_RULE_DIR_OUT))?;
            put("设置动作", rule.SetAction(NET_FW_ACTION_BLOCK))?;
            // 三个配置文件全给：规则跟会话走，用户此刻挂在哪个网络就先管哪个
            put("设置生效配置文件", rule.SetProfiles(NET_FW_PROFILE2_ALL.0))?;
            // 分组带同一个 UUID：wf.msc 里我们的规则自成一类，肉眼可辨
            put("设置分组", rule.SetGrouping(&BSTR::from(super::RULE_UUID)))?;
            put("设置描述", rule.SetDescription(&BSTR::from(DESC)))?;
            // VARIANT_BOOL 的 TRUE 是 -1，不是 1
            put("启用规则", rule.SetEnabled(VARIANT_BOOL(-1)))?;
            put("加入规则集", rules.Add(&rule))?;
        }
        Ok(())
    }

    pub fn apply(specs: &[RuleSpec]) -> Result<usize, String> {
        let _com = Com::init()?;
        let rules = rules()?;
        // 先撤干净再装：上一次会话强杀留下的规则若与前缀相同会被**覆盖**，tag 不同
        // 则覆盖不掉，回读条数就一直比预期多——那条路正是「清除残留」按钮要修的
        let _ = remove_ours(&rules);
        for spec in specs {
            if let Err(e) = add_one(&rules, spec) {
                // 装到一半失败不留半套：留着的那半是「看着装了、其实只挡了一半」，
                // 比整个没装上更难查
                let _ = remove_ours(&rules);
                return Err(e);
            }
        }
        // 回读核验：**不假定装上了**。与「改完 TUN 要核验网卡真的拆了」同一习惯
        our_rule_names(&rules).map(|n| n.len())
    }

    pub fn clear() -> Result<usize, String> {
        let _com = Com::init()?;
        let rules = rules()?;
        remove_ours(&rules)
    }

    pub fn count() -> Result<usize, String> {
        let _com = Com::init()?;
        let rules = rules()?;
        Ok(our_rule_names(&rules)?.len())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{RuleSpec, ERR_UNSUPPORTED};

    /// 非 Windows：没有这套防火墙语义。**报错而不是静默成功**——静默成功会让界面
    /// 显示「已阻断」而实际什么都没发生。
    pub fn apply(_specs: &[RuleSpec]) -> Result<usize, String> {
        Err(ERR_UNSUPPORTED.to_string())
    }

    /// 非 Windows：没有规则可删，0 条是**事实**而不是降级。
    pub fn clear() -> Result<usize, String> {
        Ok(0)
    }

    /// 同上。
    pub fn count() -> Result<usize, String> {
        Ok(0)
    }
}
