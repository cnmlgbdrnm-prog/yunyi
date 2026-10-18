//! 运行时连接日志 + 泄漏检测（用户需求第 7 项）。
//!
//! **为什么必须走 OS 层**（实测钉死，不是设计偏好）：mihomo 的 `/connections`
//! 只列**已经进了 mihomo 的**连接。没进代理的 socket 永远不会出现在那里——
//! 现场测到的正是这个形态：`chains` 全是 `('大美','手动选择')`，而来自
//! codex/ChatGPT 的连接数 = **0**，同一时刻 `POST auth.openai.com/oauth/token`
//! 直连出去拿到的是 CN 出口的 403 `unsupported_country_region_territory`。
//! 另外 mihomo 的 `metadata.process` 在本机 **16/16** 条连接上都是空的，
//! 所以 PROCESS 列也只能从 OS 侧拿，再按**本地源端口** join 回去。
//!
//! ## 判据（v2：补上了国家判定，因为 v1 在真机上满屏假警报）
//!
//! v1 特意不读 GeoIP，理由是「Rust 侧没有 mmdb reader」。实测证明这个取舍不成立：
//! 规则里写着 `GEOIP,CN,DIRECT` / `GEOSITE,cn,DIRECT`——**国内目标直连是我们自己定的
//! 行为**；浏览器又是「国内直连 + 国外走代理」的混合体，于是每开一个国内站点就多一条
//! 「泄漏」。用户报的 5 条里 4 条是国内 IP（上海移动 ×3、长沙电信 ×1，同一 /24 在
//! mihomo 里就是 `GeoIP(cn) → DIRECT`），只有 1 条 `msedge.exe → 135.234.174.40`（US）
//! 是真信号。假警报的代价不是噪音，是用户学会无视整个面板之后，真泄漏也一起被无视。
//!
//! 现在读的是 `{data_dir}/net/country.mmdb`——**mihomo 自己加载的那一份**（同源同文件，
//! 两边结论在构造上不可能打架）。
//!
//! v3 又补了第 0 条，起因是**探测器把自己的出站当成可疑流量**：2026-09-21 实测
//! 面板 36 条「疑点」里 26 条是 `mihomo.exe pid=54380 → 203.0.113.17:8444`——
//! 54380 就是监听 7890 的代理本体，那个地址是**用户自己的节点**（正是一堆 `proxied`
//! 行 `proxy_endpoint` 列上那个）。隧道自身的出站必然「未被认领」：mihomo 拨号出去的
//! 那条 socket 不在它**自己**的连接表里（表记的是入站客户端那一侧的源端口）。
//! 26 条噪音足以把真正要看的那几条挤下去——与 v1 的假警报是同一个病。
//!
//! 故四层判据：
//!
//! 0. 目标在**我们自己的节点**上（`endpoint_map` 里的地址，中继生效时是入口地址）
//!    → [`Verdict::ProxyEgress`]：隧道自身的出站，不经 mihomo 是必然；
//! 1. 目标在**国内**（`country_of(ip) == "CN"`）→ [`Verdict::DirectCn`]：设计意图，不是泄漏；
//! 2. 非 CN（含**国家未知**）→ 原来的两条：在关注名单里、或同程序另有连接走了代理 → Leak
//!    （「同程序」对**宿主型镜像**按 pid 认，见 [`is_host_image`]）；
//! 3. 都没有 → Suspect（拿不准，绝不叫泄漏）。
//!
//! 第 0 条排在**最前**：它是关于这条连接*是什么*的**事实**，比「目标国家在国内」或
//! 「程序名在你编辑的名单里」都硬。
//!
//! 国家未知时**不放宽**：宁可多报，不可把真泄漏说成「国内直连，属设计意图」。
//!
//! 直连行的域名尽力补上（`mihomo` 的 host 优先，否则查系统 DNS 缓存），拿不到就留空
//! ——见 [`domain_of`] 与 `net::dnscache` 的模块注释。**绝不做反向 DNS**。
//!
//! 检测器仍是**探测器不是保证**：拿不准就报 Suspect，绝不为了显得有用而报 LEAK。

use crate::crypto::CredentialService;
use crate::db::Db;
use crate::domain::{Node, RelayRule, Server};
use crate::net::winprobe::{Proto, SockRow};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 泄漏关注名单的设置表键（逗号分隔镜像名）。
pub const NET_LEAK_WATCH_KEY: &str = "net_leak_watch";
/// 默认关注名单：现场实测漏掉的是 codex（ChatGPT 客户端同理），浏览器是
/// 「有的进程吃系统代理、有的不吃」最常见的样本。
pub const DEFAULT_LEAK_WATCH: &str = "chrome.exe,msedge.exe,brave.exe,codex.exe,chatgpt.exe";
/// 采样间隔。连接比这短命是常态，故面板不开也要采——事件流才是日志的载体，
/// 前端轮询只能在「恰好活着」的瞬间看到东西。
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// 连续失败上限：mihomo 已经没了还每秒重试，只会把真实错误刷成噪音。到点自杀并
/// 留下 `last_error`，界面据此说「采样已停止 + 原因」。
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// 单次 mihomo 查询超时（本机回环，正常亚毫秒；卡住说明 mihomo 自己出问题了）。
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// 一条连接的裁决。`Suspect` 与 `Leak` **必须分开**：前者是「拿不准」，
/// 后者是「有证据」。混在一起，界面就只能靠猜。
///
/// `Direct` 与 `DirectCn` 也**必须分开**：前者是「进了 mihomo、被规则判直连」，
/// 后者是「压根没进 mihomo，但目标在国内——规则本来就让它直连」。两者的证据强度
/// 完全不同：前者有 mihomo 的规则行可查，后者只有一份国家库。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// 经 mihomo 转发出站（正常）
    Proxied,
    /// 经 mihomo 但被规则判成直连（GEOIP,CN → DIRECT）——设计如此，不是泄漏
    Direct,
    /// **没进 mihomo**、但目标在中国大陆。与内网直连同理：国内走直连正是我们规则里
    /// 写死的行为（`GEOIP,CN,DIRECT` / `GEOSITE,cn,DIRECT`），所以它是「结果与规则
    /// 本意一致」，**属设计意图，不是泄漏**。现场 5 条「泄漏」报告里 4 条是这种
    /// （上海移动 ×3、长沙电信 ×1）；单独一档而不是塞进 Leak：不分开就只剩「满屏
    /// 假警报」或「干脆不报」两条路——前者让用户学会无视面板，后者把真漏一起埋掉。
    #[serde(rename = "direct_cn")]
    DirectCn,
    /// 有证据的泄漏：在关注名单内，或同程序另有连接走了代理而这条没有
    Leak,
    /// 公网直连但**没有任何旁证**：可能是这个程序本就不吃系统代理，也可能正常
    Suspect,
    /// **隧道自身的出站**：本机代理拨号到我们自己的节点。不经 mihomo 是必然而非
    /// 疏漏，所以既不是泄漏也不是疑点。
    ///
    /// 单独一档而不并进 `Direct`：`Direct` 是「**进了 mihomo**、有一条规则判它直连」，
    /// 有规则行可查；这一档压根没进 mihomo，也没有规则——并进去会让界面上的
    /// 「规则直连」这个标签撒谎。同理不并进 `Proxied`：它是**没有**被代理的那一半。
    #[serde(rename = "proxy_egress")]
    ProxyEgress,
}

/// 连接日志的一行：前 8 列是用户点名要的，后面是判据与排障信息。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConnRow {
    /// PROCESS：镜像名（只留文件名）。OS 侧拿不到进程时为**空串**，界面渲染 `—`
    pub process: String,
    /// 排障用：join 就是靠 pid 查表查出来的
    pub pid: u32,
    /// DOMAIN：优先 mihomo 的 `metadata.host`（这条连接**真正请求**的名字），
    /// 没有才退到系统 DNS 缓存反查。**不做反向 DNS**：慢、会被污染、解出来的名字
    /// 还可能是别的站点的——宁可空着
    pub domain: String,
    /// `mihomo` / `dns-cache` / `none`。界面据此把「尽力而为」的名字和权威名字
    /// **显示得不一样**：DNS 缓存那条路对浏览器基本无效（自带解析器/DoH），
    /// 而且一个 IP 可能对应多个域名
    pub domain_source: String,
    /// 目标 IP 的国家/地区（ISO 码，来自 country.mmdb；查不到为空）。
    /// **拿不到域名时它就是用户唯一的线索**——「这条是国内的，本来就该直连」
    pub country: String,
    /// RESOLVED_IP：经 mihomo 时取 mihomo 的目标 IP；直连行取 OS 表的远端
    pub resolved_ip: String,
    pub dest_port: u16,
    /// "IPv4" / "IPv6"
    pub family: String,
    /// "TCP" / "UDP"
    pub proto: String,
    /// MATCHED_RULE：`rule` + `rulePayload` 合成的人话；直连行填 `—`
    /// （**`—` 本身就是漏的证据**：这条根本没经过 mihomo，谈不上匹配了什么规则）
    pub matched_rule: String,
    /// OUTBOUND：mihomo 的出口链（`大美 → 手动选择`）；直连行同样是 `—`
    pub outbound: String,
    /// PROXY_ENDPOINT：我们**拨号**的端点（如 `203.0.113.127:4443`），
    /// 与公网实际出口 IP 是两个东西，界面上要分开说
    pub proxy_endpoint: String,
    pub upload: u64,
    pub download: u64,
    pub start: Option<String>,
    pub verdict: Verdict,
    /// 为什么这么判——判据必须能自证，否则用户没法反驳也没法确认
    pub reason: String,
}

/// mihomo `/connections` 里我们真正用到的那几个字段。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MihomoConn {
    /// 连接唯一 id。泄漏判据用不上，但「切换出口后断开旧连接」必须靠它——
    /// `DELETE /connections/{id}` 是 mihomo 唯一能按条断开的入口
    /// （见 `runtime::conns_to_close`）。
    pub id: String,
    pub host: String,
    pub destination_ip: String,
    pub destination_port: u16,
    pub source_port: u16,
    pub process: String,
    pub network: String,
    pub rule: String,
    pub rule_payload: String,
    pub chains: Vec<String>,
    pub upload: u64,
    pub download: u64,
    pub start: Option<String>,
}

impl MihomoConn {
    fn proto(&self) -> Proto {
        if self.network.eq_ignore_ascii_case("udp") {
            Proto::Udp
        } else {
            Proto::Tcp
        }
    }
}

/// 「这个 IP 属于哪个国家」的唯一入口。抽成 trait 是为了让 `classify` 能完全
/// 脱离 7.8MB 的真实数据文件来测：判据逻辑与数据来源分开验。
pub trait CountryLookup: Send + Sync {
    /// 国家/地区 ISO 码。判据是 `== "CN"`（见 `unclaimed_row`），**同一个值也直接
    /// 给界面显示**——不另开一个 `is_cn` 的是非位：直连行往往拿不到域名（浏览器自带
    /// 解析器/DoH），那时国家就是用户判断「这条本来该不该直连」的唯一线索。
    ///
    /// 默认实现给 `None`（= 未知）：任何「查不到」的实现都不必重复这一句，
    /// 也不会因为漏实现而把未知说成某个国家。**未知不放宽**——照旧报泄漏/疑点。
    fn country_of(&self, _ip: std::net::IpAddr) -> Option<String> {
        None
    }
}

/// 生产实现：`net::mmdb` 读 `country.mmdb`——**mihomo 自己加载的那一份**，
/// 同源同文件，两边判「是不是国内」在构造上不可能打架。
///
/// 这里的调用必须写**全路径**：`self.country_of(..)` 会解析到本 trait 的同名方法，
/// 编译期一声不响，跑到第一次查询就无限递归（表现为栈溢出而不是编译错误）。
impl CountryLookup for crate::net::mmdb::CountryLookup {
    fn country_of(&self, ip: std::net::IpAddr) -> Option<String> {
        crate::net::mmdb::CountryLookup::country_of(self, ip)
    }
}

/// 什么都不知道的查询器——`Sample::default()` 用它，等于「没有 geo 数据」。
/// 它恒答 `None`（未知），于是**永远不会降级**：拿不准时宁可多报，这条与
/// 「mmdb 读不到就退回原行为」是同一个安全方向。
pub struct NoGeo;

impl CountryLookup for NoGeo {}

/// 一次分类的全部输入。**纯数据**，故 `classify` 是纯函数、可表驱动测。
#[derive(Clone)]
pub struct Sample {
    /// OS 侧 socket 行（还没 join，分不出谁进了代理）
    pub socks: Vec<SockRow>,
    /// mihomo 侧连接（前后两次读取的并集）
    pub conns: Vec<MihomoConn>,
    /// pid → 镜像名
    pub images: Vec<(u32, String)>,
    /// 泄漏关注名单（镜像名）
    pub watch: Vec<String>,
    /// 节点名 → `addr:port`（已按 `effective_node` 折算成中转机地址）
    pub endpoints: HashMap<String, String>,
    /// 国家查询器。默认 = `NoGeo`（什么都不知道 → 不降级），
    /// 于是所有既有调用点（`..Default::default()`）保持加这个功能之前的行为。
    pub geo: Arc<dyn CountryLookup>,
    /// 目标地址 → 域名（系统 DNS 客户端缓存反查）。**尽力而为**：浏览器自带解析器/
    /// DoH，它们的解析结果根本不进系统缓存，拿不到就留空（界面渲染 `—`）。
    /// 默认空表，故既有调用点行为不变。
    pub domains: HashMap<IpAddr, String>,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            socks: Vec::new(),
            conns: Vec::new(),
            images: Vec::new(),
            watch: Vec::new(),
            endpoints: HashMap::new(),
            geo: Arc::new(NoGeo),
            domains: HashMap::new(),
        }
    }
}

/// IPC 线格式：行 + 采样器状态。合成一个结构体的理由——面板要能区分
/// 「没有可疑连接」与「采样根本没在跑」，只给一个数组这两件事长得一模一样。
#[derive(Debug, Clone, Serialize)]
pub struct ConnLogSnapshot {
    pub rows: Vec<ConnRow>,
    pub sampling: bool,
    pub last_error: Option<String>,
    pub sampled_at: Option<String>,
    pub watch: Vec<String>,
    /// 本次会话（＝当前 mihomo 进程）经 mihomo 的累计上下行，单位字节。
    ///
    /// **只统计经过 mihomo 的流量**——绕过它的连接（表格里显示 `—` 的那些）它看不见，
    /// 界面文案必须写清楚，否则会被读成「整机流量」。
    pub upload_total: u64,
    pub download_total: u64,
}

// ---------------------------------------------------------------- 纯函数

/// 内网 / 保留 / 不可路由地址。**名单外的都当公网**，故这里漏一段的后果是假泄漏。
///
/// 两段是实测踩出来的，不能省：
/// - `198.18.0.0/15`：mihomo fake-ip 段。系统代理模式下正常流量根本不带这个地址，
///   但 DNS 劫持/TUN 残留时会出现，漏了就是满屏假泄漏；
/// - `::ffff:0:0/96`（IPv4-mapped）：Windows 的 v6 表里双栈 socket 就长这样，
///   必须**拆开按 v4 判**，否则 `::ffff:127.0.0.1` 会被当成公网 v6。
pub fn is_local_or_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_local_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local_v4(v4);
            }
            let seg = v6.segments();
            v6.is_unspecified()                       // ::
                || v6.is_loopback()                   // ::1
                || (seg[0] & 0xff00) == 0xff00        // ff00::/8 组播
                || (seg[0] & 0xfe00) == 0xfc00        // fc00::/7 ULA
                || (seg[0] & 0xffc0) == 0xfe80        // fe80::/10 链路本地
                || (seg[0] == 0x2001 && seg[1] == 0x0db8) // 2001:db8::/32 文档段
        }
    }
}

fn is_local_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()          // 127/8
        || v4.is_unspecified() // 0.0.0.0
        || v4.is_private()     // 10/8、172.16/12、192.168/16
        || v4.is_link_local()  // 169.254/16
        || v4.is_multicast()   // 224/4
        || v4.is_broadcast()   // 255.255.255.255
        || o[0] == 0                                   // 0/8 本网络
        || (o[0] == 100 && (o[1] & 0xc0) == 64)        // 100.64/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)     // 192.0.0/24
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)     // 192.0.2/24 TEST-NET-1
        || (o[0] == 198 && (o[1] & 0xfe) == 18)        // 198.18/15 基准测试段 = fake-ip
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)  // 198.51.100/24 TEST-NET-2
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)   // 203.0.113/24 TEST-NET-3
        || o[0] >= 240 // 240/4 保留
}

/// 规则名 + 参数合成人话：`GeoIP` + `CN` → `GeoIP(CN)`。
/// 有些 mihomo 版本直接把括号拼进了 `rule`（`GeoIP(cn)`），此时不再套一层。
pub fn rule_label(rule: &str, payload: &str) -> String {
    if rule.is_empty() {
        return "—".to_string();
    }
    if payload.is_empty() || rule.contains('(') {
        rule.to_string()
    } else {
        format!("{rule}({payload})")
    }
}

/// 解析关注名单：逗号分隔，去空白、转小写、丢空项。
///
/// 空白也当分隔符：设置表里那份是用户手打的，换行/空格分开照样得能用——
/// 整串当成一个镜像名的后果是名单静默失效（`codex.exe\nbrave.exe` 谁也匹配不上），
/// 而失效的名单只会让面板看起来「一切正常」。
pub fn parse_watch(s: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(image_key)
        .filter(|k| !k.is_empty() && seen.insert(k.clone()))
        .collect()
}

/// 进程名的比对键：只取文件名并小写。Windows 上 `chrome.exe` / `CHROME.EXE` 是同一个
/// 东西；mihomo 的 `process` 字段在别的版本里给过全路径，也得能命中名单。
pub fn image_key(s: &str) -> String {
    let trimmed = s.trim();
    let base = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed);
    base.to_ascii_lowercase()
}

/// 解析 mihomo `/connections`。字段缺失/形状不认识一律给空值——一条畸形响应
/// 不该把采样任务打死，探测器的输出缺一块也只是少一行。
pub fn parse_connections(v: &serde_json::Value) -> Vec<MihomoConn> {
    let Some(list) = v.get("connections").and_then(|c| c.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .map(|c| {
            let meta = c.get("metadata");
            let text = |key: &str| -> String {
                meta.and_then(|m| m.get(key))
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            MihomoConn {
                id: c
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                host: text("host"),
                destination_ip: text("destinationIP"),
                destination_port: port_field(meta, "destinationPort"),
                source_port: port_field(meta, "sourcePort"),
                process: text("process"),
                network: text("network"),
                rule: c
                    .get("rule")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                rule_payload: c
                    .get("rulePayload")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                chains: c
                    .get("chains")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
                upload: c.get("upload").and_then(|x| x.as_u64()).unwrap_or(0),
                download: c.get("download").and_then(|x| x.as_u64()).unwrap_or(0),
                start: c.get("start").and_then(|x| x.as_str()).map(String::from),
            }
        })
        .collect()
}

/// mihomo `/connections` 顶层的会话累计（字节）。
///
/// **语义是进程生命周期**——2026-10-02 实测：经混合端口下 3 MB 后读到
/// `uploadTotal=79 / downloadTotal=3000128`，杀掉进程用同一个 `-d` 目录重启后
/// 两个数都回到 0，且落盘目录只有 `cache.db`（流量不落盘）。
/// 而 mihomo 的进程恰好活在一次「开启网络 → 关闭网络」里（`runtime::stop` 会
/// `kill()`，`runtime::start` 每次重新 spawn），所以它**天然**就是界面要的
/// 「本次会话」口径，不需要任何累加器。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MihomoTotals {
    pub upload: u64,
    pub download: u64,
}

/// 解析 `/connections` 顶层的 `uploadTotal`/`downloadTotal`。
/// 字段缺失/形状不认识给 0——与 `parse_connections` 同款宽容。
pub fn parse_totals(v: &serde_json::Value) -> MihomoTotals {
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    MihomoTotals {
        upload: n("uploadTotal"),
        download: n("downloadTotal"),
    }
}

/// mihomo 的端口字段是**字符串**（实测 `"sourcePort":"54321"`），但历史版本给过数字，
/// 两种都收。解析不出按 0：0 端口不参与 join，等于放弃这条而不是错配到别人身上。
fn port_field(meta: Option<&serde_json::Value>, key: &str) -> u16 {
    match meta.and_then(|m| m.get(key)) {
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        Some(other) => other.as_u64().unwrap_or(0) as u16,
        None => 0,
    }
}

/// 合成前后两次读取。连接是活的，单次读取抓不到「刚建立」与「刚关闭」的那批，
/// 而这两批恰恰是假泄漏的来源，故采样时前后各读一次再合并。
pub fn merge_connections(mut a: Vec<MihomoConn>, b: Vec<MihomoConn>) -> Vec<MihomoConn> {
    let mut seen: HashSet<(String, u16, String, u16, String)> = a
        .iter()
        .map(|c| {
            (
                c.network.clone(),
                c.source_port,
                c.destination_ip.clone(),
                c.destination_port,
                c.host.clone(),
            )
        })
        .collect();
    for c in b {
        let key = (
            c.network.clone(),
            c.source_port,
            c.destination_ip.clone(),
            c.destination_port,
            c.host.clone(),
        );
        if seen.insert(key) {
            a.push(c);
        }
    }
    a
}

/// 节点名 → `addr:port`。**必须走 `effective_node`**：挂了中转的节点，客户端实际
/// 拨号的是中转机，`proxy_endpoint` 要显示那个才对得上排障现场。
pub fn endpoint_map(
    nodes: &[Node],
    rules: &[RelayRule],
    servers: &[Server],
) -> HashMap<String, String> {
    nodes
        .iter()
        .map(|n| {
            let e = crate::config::effective_node(n, rules, servers);
            (n.name.clone(), format!("{}:{}", e.addr, e.port))
        })
        .collect()
}

/// 端点点表（[`endpoint_map`] 的产物）→ **`(地址, 端口) → 节点名`** 反查表。
///
/// 两个调用方共用这一份实现：连接日志判「隧道自身的出站」（`classify` 里的 `own`），
/// 以及开启网络时判「这条直连是不是连到我们自己的节点」（`net::takeover`）——
/// 两处各写一份的话，改了一边漏另一边只是时间问题。
///
/// 解析不出地址的条目（节点地址写成域名）**整条丢掉**：调用方据此不匹配，
/// fail-open 与 [`parse_endpoint`] 同一条取舍。
pub fn own_endpoints(endpoints: &HashMap<String, String>) -> HashMap<(IpAddr, u16), String> {
    endpoints
        .iter()
        .filter_map(|(name, ep)| Some((parse_endpoint(ep)?, name.clone())))
        .collect()
}

/// 排序权重：LEAK 置顶。`limit` 截断时最要紧的行必须还在——按时间排的话
/// 「只看前 200 条」会把唯一那条泄漏挤掉。
///
/// `ProxyEgress` 垫底：隧道出站是面板上最没信息量的一档，而它**数量最多**
/// （实测单次采样 26 条）。给它靠前的权重，等于让它把真信号挤下去——
/// 那正是这次要修掉的形态。
fn rank(v: Verdict) -> u8 {
    match v {
        Verdict::Leak => 0,
        Verdict::Suspect => 1,
        Verdict::DirectCn => 2,
        Verdict::Direct => 3,
        Verdict::Proxied => 4,
        Verdict::ProxyEgress => 5,
    }
}

/// 把 OS 行与 mihomo 行按 `(proto, 本地源端口)` join，判出每条连接的裁决。
///
/// **join 键为什么是本地源端口**：mihomo 的 `metadata.process` 本机恒空（16/16 实测），
/// PROCESS 只能从 OS 侧取；而客户端 socket 的本地源端口就是 mihomo 报的 `sourcePort`，
/// 这是两边唯一能对上的东西。
///
/// 锚点选 OS 行：有 OS 行时行由 OS 行产出（拿得到 PROCESS），mihomo 那一侧补
/// 域名/规则/出口；反过来只有 mihomo 有的话，就只能给一行没有进程名的记录。
pub fn classify(s: &Sample) -> Vec<ConnRow> {
    let images: HashMap<u32, String> = s
        .images
        .iter()
        .map(|(pid, name)| (*pid, image_key(name)))
        .collect();
    let mut by_port: HashMap<(Proto, u16), &MihomoConn> = HashMap::new();
    for c in &s.conns {
        by_port.entry((c.proto(), c.source_port)).or_insert(c);
    }
    let mut os_by_port: HashMap<(Proto, u16), &SockRow> = HashMap::new();
    for r in &s.socks {
        os_by_port.entry((r.proto, r.local_port)).or_insert(r);
    }

    // 旁证集合：本窗口内哪些**程序**有连接走了 mihomo。聚合口径见 [`ProxiedEvidence`]。
    // 拿不到镜像名的（空串）一律不进集合：它既不能证明走过代理，也不该被当成旁证。
    let mut proxied = ProxiedEvidence::default();
    for key in by_port.keys() {
        if let Some(r) = os_by_port.get(key) {
            if let Some(name) = images.get(&r.pid) {
                if !name.is_empty() {
                    proxied.insert(name.clone(), r.pid);
                }
            }
        }
    }

    let watch: HashSet<String> = s
        .watch
        .iter()
        .map(|w| image_key(w))
        .filter(|w| !w.is_empty())
        .collect();

    // 我们自己的节点端点 → 节点名。用 `effective_node` 重写后的地址（中继生效时是
    // 入口地址），与 `proxy_endpoint` 列同源，两边不可能打架。
    let own = own_endpoints(&s.endpoints);

    let mut out: Vec<ConnRow> = Vec::new();
    // 一、逐条 OS 行：被 mihomo 认领的走 join，没被认领的才是泄漏候选
    for r in &s.socks {
        let proc = images.get(&r.pid).cloned().unwrap_or_default();
        match by_port.get(&(r.proto, r.local_port)) {
            Some(c) => out.push(joined_row(r, c, s, proc)),
            None => {
                // 内网/回环行不经代理是设计使然（GEOIP,LAN/PRIVATE 就是直连），
                // 列出来只会把真正的信号淹没；只在公网目标上才可能是漏
                if !is_local_or_private(r.remote_addr) {
                    out.push(unclaimed_row(
                        r,
                        proc,
                        &watch,
                        &proxied,
                        &own,
                        s.geo.as_ref(),
                        s,
                    ));
                }
            }
        }
    }
    // 二、只有 mihomo 侧的行：OS 快照里没对上（连接太短命，或 OS 枚举整个失败）。
    // 域名/规则/出口只有 mihomo 知道，丢掉等于把日志挖掉一半。
    for c in &s.conns {
        if !os_by_port.contains_key(&(c.proto(), c.source_port)) {
            out.push(mihomo_only_row(c, s));
        }
    }
    // 稳定排序：同裁决内保持原顺序
    out.sort_by_key(|r| rank(r.verdict));
    out
}

/// 解析 `endpoint_map` 里的 `"{addr}:{port}"`。
///
/// **按最后一个冒号切**：IPv6 地址自己就带冒号，`split(':')` 会把
/// `2001:db8::103:8443` 切碎（生产里 v6 端点就是这个无方括号的形状，
/// 因为它由 `format!("{}:{}", addr, port)` 拼出来）。
///
/// 解析不出来（节点地址写成域名、端口非法）返回 `None`：调用方据此**不匹配**。
/// 宁可留着一条疑点，也不误吞一条——fail-open 是这个检测器的一贯取舍。
fn parse_endpoint(s: &str) -> Option<(IpAddr, u16)> {
    let (addr, port) = s.rsplit_once(':')?;
    Some((addr.parse().ok()?, port.parse().ok()?))
}

/// 目标 IP → 国家/地区码。查不到给空串（界面渲染 `—`）——**不做任何猜测**，
/// 「未知」与「非 CN」是两回事，判据那边对两者都不放宽。
fn country_of(geo: &dyn CountryLookup, ip: &str) -> String {
    ip.parse::<IpAddr>()
        .ok()
        .and_then(|ip| geo.country_of(ip))
        .unwrap_or_default()
}

/// 国家码的补充说明，附在判决后面。**只写事实**——国家未知时也必须写出来，不然用户
/// 会以为这条已经排除过「国内直连」的可能。
fn country_note(country: &str) -> String {
    if country.is_empty() {
        "（国家未知）".to_string()
    } else {
        format!("（目标国家/地区 {country}）")
    }
}

/// 域名与它的来源：`mihomo` 的 `metadata.host` 优先——那是这条连接**真正请求**的
/// 名字；没有才退到系统 DNS 缓存反查（尽力而为）；都没有就留空。
///
/// **绝不做反向 DNS**：慢、会被污染、解出来的还可能是完全不相干的站点名。拿不到就
/// 渲染 `—`：空着是诚实的，猜一个是错的。
fn domain_of(host: &str, ip: &str, s: &Sample) -> (String, &'static str) {
    if !host.is_empty() {
        return (host.to_string(), "mihomo");
    }
    let cached = ip
        .parse::<IpAddr>()
        .ok()
        .and_then(|ip| s.domains.get(&ip))
        .cloned()
        .unwrap_or_default();
    if cached.is_empty() {
        (String::new(), "none")
    } else {
        (cached, "dns-cache")
    }
}

/// 经 mihomo 的行：PROCESS 来自 OS，其余三列来自 mihomo。
fn joined_row(r: &SockRow, c: &MihomoConn, s: &Sample, proc: String) -> ConnRow {
    // 目标以 mihomo 为准：系统代理模式下 OS 行的远端是 mixed-port 自己
    // （127.0.0.1:7890），拿它当目标等于每条都显示「连去了本地代理」
    let resolved_ip = if c.destination_ip.is_empty() {
        r.remote_addr.to_string()
    } else {
        c.destination_ip.clone()
    };
    let dest_port = if c.destination_port == 0 {
        r.remote_port
    } else {
        c.destination_port
    };
    let outbound = c.chains.join(" → ");
    let direct = c.chains.first().map(|c| c == "DIRECT").unwrap_or(false);
    let rule = rule_label(&c.rule, &c.rule_payload);
    let (domain, domain_source) = domain_of(&c.host, &resolved_ip, s);
    ConnRow {
        process: proc,
        pid: r.pid,
        domain,
        domain_source: domain_source.to_string(),
        country: country_of(s.geo.as_ref(), &resolved_ip),
        family: family_label(&resolved_ip, r.family.as_str()),
        resolved_ip,
        dest_port,
        proto: r.proto.as_str().to_string(),
        matched_rule: rule.clone(),
        outbound: if outbound.is_empty() {
            "—".to_string()
        } else {
            outbound
        },
        // 出口链的第一跳才是**我们拨号的那个节点**（分组名没有地址）
        proxy_endpoint: c
            .chains
            .first()
            .and_then(|n| s.endpoints.get(n))
            .cloned()
            .unwrap_or_default(),
        upload: c.upload,
        download: c.download,
        start: c.start.clone(),
        verdict: if direct {
            Verdict::Direct
        } else {
            Verdict::Proxied
        },
        reason: if direct {
            format!("经 mihomo，规则判定直连（{rule}）")
        } else {
            "经 mihomo 转发出站".to_string()
        },
    }
}

/// 「哪些**程序**有连接走了 mihomo」的旁证集合。
///
/// 默认按**镜像名**而不是 pid 聚合——浏览器的网络/渲染进程各有各的 pid，
/// 按 pid 判会把最典型的「同一个 Chrome 一半走代理一半直连」整条漏掉
/// （`process_split_between_proxied_and_direct_is_a_leak` 钉着这条）。
///
/// **宿主型镜像除外**（[`is_host_image`]）：它们按 pid 认。理由见那个函数的注释。
#[derive(Default)]
struct ProxiedEvidence {
    /// 按镜像名认的（绝大多数程序）
    images: HashSet<String>,
    /// 按 pid 认的（宿主型镜像）
    host_pids: HashSet<u32>,
}

impl ProxiedEvidence {
    fn insert(&mut self, image: String, pid: u32) {
        if is_host_image(&image) {
            self.host_pids.insert(pid);
        } else {
            self.images.insert(image);
        }
    }

    fn has(&self, image: &str, pid: u32) -> bool {
        if is_host_image(image) {
            self.host_pids.contains(&pid)
        } else {
            self.images.contains(image)
        }
    }
}

/// 宿主型镜像：**一个镜像名对应几十个互不相干的 OS 进程**，每个进程宿主不同的
/// 服务/组件，各有各的网络栈、各有各的代理认知（svchost.exe 尤其典型）。
///
/// 对它们，「同程序分流」的旁证只认**同一个 pid**。按镜像名认的后果是本机实测到的
/// （2026-09-22）：WpnService（Windows 推送服务）那条从没走过代理的直连，被同一个
/// 镜像里 DoSvc（传递优化）的一条代理连接牵连报成 Leak，而 DoSvc 那条一过期又变回
/// Suspect——**同一行在两个档位之间来回跳**，用户没法信这个面板。按 pid 认之后，
/// 真分流照样抓得到（DoSvc 自己既有走代理的也有直连的），却不会被别的实例连坐。
///
/// 名单刻意短：只收**结构上**就是多组件宿主的那几个。chrome.exe / msedge.exe 这种
/// 多进程镜像**不能**收——它们的进程共享同一份代理配置，正该按镜像名认。
pub fn is_host_image(name: &str) -> bool {
    matches!(
        name,
        "svchost.exe" | "dllhost.exe" | "rundll32.exe" | "wmiprvse.exe"
    )
}

/// 未被任何 mihomo 行认领的**公网** OS 行：泄漏候选。
///
/// 四条判据的强弱是刻意分开的——「目标是自己的节点」是**事实**，名单是用户的
/// **声明**，旁证是同程序的行为**证据**，三者都没有就只是「疑点」。把疑点也叫做
/// 泄漏，等于把面板变成噪音，用户学会无视它之后，真泄漏也一起被无视了。
#[allow(clippy::too_many_arguments)]
fn unclaimed_row(
    r: &SockRow,
    proc: String,
    watch: &HashSet<String>,
    proxied: &ProxiedEvidence,
    own: &HashMap<(IpAddr, u16), String>,
    geo: &dyn CountryLookup,
    s: &Sample,
) -> ConnRow {
    let country = country_of(geo, &r.remote_addr.to_string());
    let (verdict, reason) = if let Some(name) = own.get(&(r.remote_addr, r.remote_port)) {
        // 隧道自身的出站。**必然**「未被认领」：mihomo 拨号到节点的那条 socket
        // 不在它自己的连接表里（表里记的是入站客户端那一侧的源端口）。
        // 2026-09-21 实测：36 条疑点里 26 条是它，足以把真信号挤出面板。
        (
            Verdict::ProxyEgress,
            format!("隧道自身的出站：目标是本机代理自己的节点「{name}」"),
        )
    } else if country == "CN" {
        // 国内目标**先于**名单与旁证判定：走直连的国内流量结果与 `GEOIP,CN,DIRECT` /
        // `GEOSITE,cn,DIRECT` 完全一致，报成泄漏就是假警报（用户的现场反应是
        // 「这明明是国内的」，实测 5 条报告里 4 条如此）。
        // 仍然成行、仍然带原因——降级不等于藏起来。
        (
            Verdict::DirectCn,
            format!(
                "目标 {} 在中国大陆，GeoIP/GEOSITE 规则判直连",
                r.remote_addr
            ),
        )
    } else if !proc.is_empty() && watch.contains(&proc) {
        (
            Verdict::Leak,
            format!(
                "{proc} 在泄漏关注名单内，但这条连接没有经过 mihomo{}",
                country_note(&country)
            ),
        )
    } else if !proc.is_empty() && proxied.has(&proc, r.pid) {
        (
            Verdict::Leak,
            format!(
                "{proc} 在同一窗口内有连接走了代理，这条没有（同程序分流）{}",
                country_note(&country)
            ),
        )
    } else {
        (
            Verdict::Suspect,
            format!(
                "未经 mihomo；不在关注名单，同程序也无走代理记录，需人工确认{}",
                country_note(&country)
            ),
        )
    };
    let resolved_ip = r.remote_addr.to_string();
    let (domain, domain_source) = domain_of("", &resolved_ip, s);
    ConnRow {
        process: proc,
        pid: r.pid,
        // 直连行拿不到域名（除非系统 DNS 缓存里恰好留着这条的解析结果，
        // 那是尽力而为、界面会标出来）。**不做反向 DNS**——慢、会被污染、
        // 解出来的还可能是别的站点的名字
        domain,
        domain_source: domain_source.to_string(),
        country,
        family: family_label(&resolved_ip, r.family.as_str()),
        resolved_ip,
        dest_port: r.remote_port,
        proto: r.proto.as_str().to_string(),
        // 这三列的 `—` 不是「没显示」，而是结论本身：这条连接压根没经过 mihomo
        matched_rule: "—".to_string(),
        outbound: "—".to_string(),
        proxy_endpoint: String::new(),
        upload: 0,
        download: 0,
        start: None,
        verdict,
        reason,
    }
}

/// OS 快照里没对上的 mihomo 行：域名等照给，进程名只能空着（或退用 mihomo 自己的
/// `process` 字段——本机恒空，但别的环境未必）。
fn mihomo_only_row(c: &MihomoConn, s: &Sample) -> ConnRow {
    let outbound = c.chains.join(" → ");
    let direct = c.chains.first().map(|c| c == "DIRECT").unwrap_or(false);
    let rule = rule_label(&c.rule, &c.rule_payload);
    let (domain, domain_source) = domain_of(&c.host, &c.destination_ip, s);
    ConnRow {
        process: image_key(&c.process),
        pid: 0,
        domain,
        domain_source: domain_source.to_string(),
        country: country_of(s.geo.as_ref(), &c.destination_ip),
        family: family_label(&c.destination_ip, "—"),
        resolved_ip: c.destination_ip.clone(),
        dest_port: c.destination_port,
        proto: c.proto().as_str().to_string(),
        matched_rule: rule.clone(),
        outbound: if outbound.is_empty() {
            "—".to_string()
        } else {
            outbound
        },
        proxy_endpoint: c
            .chains
            .first()
            .and_then(|n| s.endpoints.get(n))
            .cloned()
            .unwrap_or_default(),
        upload: c.upload,
        download: c.download,
        start: c.start.clone(),
        verdict: if direct {
            Verdict::Direct
        } else {
            Verdict::Proxied
        },
        reason: if direct {
            format!("经 mihomo，规则判定直连（{rule}）")
        } else {
            "经 mihomo 转发出站".to_string()
        },
    }
}

/// IPv4-IPv6 列：按**实际显示的那个地址**判，免得 `::ffff:1.2.3.4` 显示成 v6 样式
/// 却标着 IPv4。
fn family_label(ip: &str, fallback: &str) -> String {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => "IPv4".to_string(),
        Ok(IpAddr::V6(_)) => "IPv6".to_string(),
        Err(_) => fallback.to_string(),
    }
}

// ---------------------------------------------------------------- 采样器

pub type ConnEmitFn = Arc<dyn Fn(ConnLogSnapshot) + Send + Sync>;

/// 采样器的全部可变状态（一把锁：读写的都是同一批字段，分锁只会多出不一致的窗口）。
#[derive(Default)]
struct Shared {
    /// Some = 正在采样。存的是**那一轮**的停止旗，退出时按 `Arc::ptr_eq` 认出自己
    stop: Option<Arc<AtomicBool>>,
    rows: Vec<ConnRow>,
    watch: Vec<String>,
    last_error: Option<String>,
    sampled_at: Option<String>,
    /// 会话累计。与 `rows` 同生命周期：查询失败保留、`stop` 清零。
    upload_total: u64,
    download_total: u64,
}

/// 连接日志采样器句柄。挂在 `AppState` 上，`runtime::start` 起、`runtime::stop` 停。
///
/// 走**后台任务 + 事件**而不是前端轮询：连接短命，面板不开的那些秒里的连接
/// 轮询永远看不到，而「漏掉的连接」正是这个功能要找的东西。
pub struct ConnSampler {
    emit: Arc<RwLock<ConnEmitFn>>,
    shared: Arc<Mutex<Shared>>,
}

impl Default for ConnSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnSampler {
    pub fn new() -> Self {
        Self {
            emit: Arc::new(RwLock::new(Arc::new(|_| {}))),
            shared: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// 绑定事件发射器（main setup 里接 AppHandle，与 jobs 的 `set_emit` 同范式）。
    pub fn set_emit(&self, emit: ConnEmitFn) {
        *crate::util::write_ignore_poison(&self.emit) = emit;
    }

    /// 起采样。**幂等**：已在采样直接返回，绝不并发起第二个（那会把行翻倍）。
    pub fn start(
        &self,
        ctrl_port: u16,
        token: String,
        db: Arc<Db>,
        creds: Arc<CredentialService>,
        geo_path: std::path::PathBuf,
    ) {
        let mut st = crate::util::lock_ignore_poison(&self.shared);
        if st.stop.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        st.stop = Some(stop.clone());
        st.last_error = None;
        drop(st);
        let emit = crate::util::read_ignore_poison(&self.emit).clone();
        let shared = self.shared.clone();
        let run = SamplerRun {
            ctrl_port,
            token,
            db,
            creds,
            emit,
            geo_path,
        };
        tokio::spawn(async move { drive(run, stop, shared).await });
    }

    /// 停采样。**不 join**：本函数在「关闭网络」的路径上，不该被一次 HTTP 超时拖住；
    /// 采样任务最迟一轮（1s）后自己退出。
    pub fn stop(&self) {
        let mut st = crate::util::lock_ignore_poison(&self.shared);
        if let Some(flag) = st.stop.take() {
            flag.store(true, Ordering::SeqCst);
        }
        st.rows.clear();
        // 网络关了就不存在「本次会话」：总量与行必须同步清掉，
        // 否则下次开启前会显示上一次会话的旧数字
        st.upload_total = 0;
        st.download_total = 0;
    }

    /// 当前窗口的行（面板刚打开时先拿这个，之后靠事件增量刷新）。
    pub fn snapshot(&self, limit: Option<usize>, only_leaks: bool) -> ConnLogSnapshot {
        let st = crate::util::lock_ignore_poison(&self.shared);
        let mut rows: Vec<ConnRow> = st
            .rows
            .iter()
            .filter(|r| !only_leaks || r.verdict == Verdict::Leak)
            .cloned()
            .collect();
        if let Some(n) = limit {
            rows.truncate(n);
        }
        ConnLogSnapshot {
            rows,
            sampling: st.stop.is_some(),
            last_error: st.last_error.clone(),
            sampled_at: st.sampled_at.clone(),
            watch: st.watch.clone(),
            upload_total: st.upload_total,
            download_total: st.download_total,
        }
    }
}

struct SamplerRun {
    ctrl_port: u16,
    token: String,
    db: Arc<Db>,
    creds: Arc<CredentialService>,
    emit: ConnEmitFn,
    /// `country.mmdb` 路径（mihomo 自己读的那份；与判 `GEOIP,CN` 用的是同一个文件）
    geo_path: std::path::PathBuf,
}

/// 把登记表里**自己那面旗**摘掉。stop→start 的竞态下新采样器可能已经登记，
/// 无条件清会把它的旗摘掉——界面从此显示「未采样」，而它其实还在跑。
fn unregister(shared: &Mutex<Shared>, me: &Arc<AtomicBool>) {
    let mut st = crate::util::lock_ignore_poison(shared);
    if st.stop.as_ref().is_some_and(|f| Arc::ptr_eq(f, me)) {
        st.stop = None;
    }
}

async fn drive(run: SamplerRun, stop: Arc<AtomicBool>, shared: Arc<Mutex<Shared>>) {
    // 复用连接：1Hz 采样每秒重建一次连接池纯属浪费
    let client = reqwest::Client::new();
    // 国家库只在任务开始时读一次（7.8MB，之后全是内存游走）。
    // 读不到就空库 = 不降级：与加这个功能之前逐字一致，宁可多报。
    // 不额外上报界面：geo 数据缺失本来就会让 `degraded_geo` 亮起来，用户已经看得到。
    // 国家库句柄**懒加载**（首次查询才读盘），故这里没有失败分支：文件不在/解不开
    // 都由 `mmdb::CountryLookup` 自己记着并退回「查不到」，语义与 `NoGeo` 一致
    // ——不降级、宁可多报。文件缺失时 `degraded_geo` 本来就会亮，用户已经看得到。
    let geo: Arc<dyn CountryLookup> =
        Arc::new(crate::net::mmdb::CountryLookup::new(run.geo_path.clone()));
    let mut fails = 0u32;
    while !stop.load(Ordering::SeqCst) {
        match sample_once(&client, &run, geo.clone()).await {
            Ok((rows, watch, totals)) => {
                fails = 0;
                let snap = {
                    let mut st = crate::util::lock_ignore_poison(&shared);
                    st.rows = rows;
                    st.watch = watch;
                    st.upload_total = totals.upload;
                    st.download_total = totals.download;
                    st.last_error = None;
                    st.sampled_at = Some(chrono::Local::now().to_rfc3339());
                    snapshot_of(&st)
                };
                (run.emit)(snap);
            }
            Err(e) => {
                fails += 1;
                let dead = fails >= MAX_CONSECUTIVE_FAILURES;
                // 先摘登记再发事件：反过来的话最后一帧会写着 sampling=true 配一句
                // 「采样已停止」，界面据此显示「每秒采样中」——自相矛盾的那类提示
                if dead {
                    unregister(&shared, &stop);
                }
                let snap = {
                    let mut st = crate::util::lock_ignore_poison(&shared);
                    // 失败时保留上一轮的行：连接日志的价值在历史，一次查询失败
                    // 不该把用户正在看的列表清空
                    st.last_error = Some(e);
                    snapshot_of(&st)
                };
                (run.emit)(snap);
                if dead {
                    return; // 最后一次事件已把 last_error 与「已停」一并带出去
                }
            }
        }
        tokio::time::sleep(SAMPLE_INTERVAL).await;
    }
    // 正常退出（关网络）：同样要推一帧「已停」。不发的话面板会一直停在最后一帧
    // 的「每秒采样中」，而网早就关了——界面在撒谎，而且是最难发现的那种。
    unregister(&shared, &stop);
    (run.emit)(snapshot_of(&crate::util::lock_ignore_poison(&shared)));
}

fn snapshot_of(st: &Shared) -> ConnLogSnapshot {
    ConnLogSnapshot {
        rows: st.rows.clone(),
        sampling: st.stop.is_some(),
        last_error: st.last_error.clone(),
        sampled_at: st.sampled_at.clone(),
        watch: st.watch.clone(),
        upload_total: st.upload_total,
        download_total: st.download_total,
    }
}

/// 一轮采样：mihomo → OS → mihomo，两次 mihomo 取并集。
///
/// **为什么前后各读一次**：两次读取之间新建立的连接会只出现在 OS 侧，join 不上，
/// 于是被判成「未经代理」——对名单里的进程就是一条**假泄漏**。前后各读一次把这段
/// 窗口夹住，代价只是一次 localhost 请求。顺序上也刻意让第一次 mihomo 读在 OS 之前：
/// 反过来的坏情况（OS 里在、mihomo 里没了）会退化成「多一行没有进程名的记录」，
/// 而不是「多一条假泄漏」。
async fn sample_once(
    client: &reqwest::Client,
    run: &SamplerRun,
    geo: Arc<dyn CountryLookup>,
) -> Result<(Vec<ConnRow>, Vec<String>, MihomoTotals), String> {
    // 前一次读只用来夹住「刚建立 / 刚关闭」那批连接。它的总量**故意丢弃**：
    // 会话总量取后一次——两次都是累计快照不是增量，相加会翻倍
    // （`sampler_reports_session_totals_without_double_counting` 钉着这条）
    let (before, _) = fetch_connections(client, run.ctrl_port, &run.token).await?;
    // 阻塞式 FFI（毫秒级，与 runtime.rs 里那几处同步 IO 同一取舍）
    let socks = crate::net::winprobe::socket_snapshot()?;
    let (after, totals) = fetch_connections(client, run.ctrl_port, &run.token).await?;
    let images = crate::net::winprobe::process_images()?;
    let conns = merge_connections(before, after);
    let watch = read_watch(&run.db);
    let endpoints = read_endpoints(&run.db, &run.creds);
    // DNS 客户端缓存反查：每次采样读一次。它是**尽力而为**（浏览器大多不走系统
    // 解析器，见 net::dnscache 的模块注释），代价实测 ~10-40ms（本机缓存几十到几百条），
    // 与四张 socket 表的枚举同一量级；库查不到就留空，界面渲染 `—`。
    let domains = crate::net::dnscache::read_cache();
    let sample = Sample {
        socks,
        conns,
        images,
        watch: watch.clone(),
        endpoints,
        geo,
        domains,
    };
    Ok((classify(&sample), watch, totals))
}

pub(crate) async fn fetch_connections(
    client: &reqwest::Client,
    ctrl_port: u16,
    token: &str,
) -> Result<(Vec<MihomoConn>, MihomoTotals), String> {
    let resp = client
        .get(format!("http://127.0.0.1:{ctrl_port}/connections"))
        .bearer_auth(token)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("查询 mihomo 连接表失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("查询 mihomo 连接表被拒: HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 mihomo 连接表失败: {e}"))?;
    // 同一次响应上顺手取出顶层总量，**不额外发请求**
    Ok((parse_connections(&v), parse_totals(&v)))
}

/// 读关注名单：设置表为空/缺省时回落到默认值（用户清空输入框不该等于「谁都不看」）。
///
/// `pub(crate)`：会话级防火墙阻断的名单必须与这里**同源**（`net::firewall::
/// watch_programs` 直接调它）。两处各读一份迟早漂移，而漂移的表现是「面板盯着
/// 的是 A、防火墙挡的是 B」，两边看着都正常。
pub(crate) fn read_watch(db: &Db) -> Vec<String> {
    let raw = crate::db::repos::settings::get(&db.lock(), NET_LEAK_WATCH_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    if raw.trim().is_empty() {
        parse_watch(DEFAULT_LEAK_WATCH)
    } else {
        parse_watch(&raw)
    }
}

fn read_endpoints(db: &Db, creds: &CredentialService) -> HashMap<String, String> {
    // 读失败一律当空表：端点只是展示列，缺了显示 `—`，不能让整轮采样失败
    let nodes = crate::db::repos::nodes::list(&db.lock(), creds).unwrap_or_default();
    let rules = crate::relay::list_relay_rules_inner(db).unwrap_or_default();
    let servers = crate::db::repos::servers::list(&db.lock()).unwrap_or_default();
    endpoint_map(&nodes, &rules, &servers)
}
