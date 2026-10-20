//! 配置生成模块：启用节点 → Clash/Mihomo YAML（proxies + proxy-groups + rules）
//! 与 sing-box config.json（[`singbox`] 子模块，第二出口）。
//! ssr 协议（mihomo 与 sing-box 均已移除 SSR 支持）在映射前跳过并计数，导出与测速面共用该策略；
//! clash_raw 来源节点在 YAML 出口原样输出以保证任意协议往返不失真；URI 来源按协议映射字段。

use crate::domain::{Node, Privacy, RelayRule, Server};
use serde_yaml::{Mapping, Value};
use thiserror::Error;

pub mod singbox;

/// 节点挂中转规则（relay_rule_id 命中「已建成」规则）时生效地址。**换向只在这里发生**：
/// switch 开关只在 relay_rule_id 上落位，`nodes` 表始终存直连地址（测速/上网客户端/
/// 导出订阅全部经此收敛）。
///
/// 两种客户端形态的入口不同——别把 peer 也塞进 forward 那套：
/// - `forward`：流量从中转机公网入，地址取中转机 host + 转发端口（DNAT 进隧道）。
/// - `peer`：本机自己就是隧道里的 peer，不经中转机 DNAT，直连落地机的**隧道地址**。
///   取中转机 host 会指向一个根本没开的公网端口（`forward_port` 在 peer 下恒为
///   None，端口还会掉进 `landing_port` 的兜底，看着像对的，其实连不上）。
///
/// 未挂 / 规则未建成 / 中转机缺失 → 直连原地址。
pub fn effective_node(n: &Node, rules: &[RelayRule], servers: &[Server]) -> Node {
    let Some((rule, relay, _)) = node_relay_chain(n, rules, servers) else {
        return n.clone();
    };
    let mut m = n.clone();
    if rule.client_mode == "forward" {
        m.addr = relay.host.clone();
        // 整机转发（same_port）：入口就是中转机的**原端口号**——该落地机每个端口各转各的，
        // 用规则的单个 forward_port 会把所有节点挤到同一个端口上（多协议必挂）
        m.port = if rule.forward_mode == "same_port" {
            n.port
        } else {
            rule.forward_port.unwrap_or(rule.landing_port)
        };
    } else {
        m.addr = rule.landing_wg_ip.clone();
        m.port = rule.landing_port;
    }
    m
}

/// 节点挂载的中转链路（规则 + 中转机 + 落地机）。命中条件与 `effective_node` 完全一致
/// （规则已建成、状态 imported/built、中转机存在），两处共用同一判定以免漂移；
/// 未挂 / 未命中 → None。
///
/// 落地机是 `Option`：**中转机在即算命中**（节点确实走中转，`effective_node` 只需要
/// 中转机）。落地机缺失只意味着分跳的后两段（本机→落地、中转→落地）测不了。
pub fn node_relay_chain<'a>(
    n: &Node,
    rules: &'a [RelayRule],
    servers: &'a [Server],
) -> Option<(&'a RelayRule, &'a Server, Option<&'a Server>)> {
    let rid = n.relay_rule_id?;
    let rule = rules
        .iter()
        .find(|r| r.id == rid && r.converged && (r.status == "imported" || r.status == "built"))?;
    let relay = servers.iter().find(|s| s.id == rule.relay_server_id)?;
    let landing = servers.iter().find(|s| s.id == rule.landing_server_id);
    Some((rule, relay, landing))
}

/// 带宽读数的保鲜期：超过它的读数不参与自动选择。
///
/// 本机国际出口共享且时变——2026-09-20 实测，同一批背靠背测出的读数 40 分钟后就整个
/// 翻转：香港-trojan 1.08 → 0.47 MB/s、大美 0.94 → 1.62 MB/s，赢家换了人。让小时级的
/// 旧快照跟刚出炉的读数比大小，比的是「什么时候测的」而不是「哪条链路快」，于是自动
/// 选择会稳定地选中上一轮的赢家并一直用下去。只让新鲜读数互相竞争。
pub const READING_TTL: chrono::Duration = chrono::Duration::minutes(15);

/// 读数是否在保鲜期内。**时间戳缺失或解析失败一律算不新鲜**：拿不准它有多旧，
/// 就不拿它决定出口。
fn is_fresh(n: &Node, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(ts) = n.last_checked_at.as_deref() else {
        return false;
    };
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    now.signed_duration_since(t.with_timezone(&chrono::Utc)) <= READING_TTL
}

/// 实测最快的节点：启用、非 ssr、**带宽读数还在保鲜期内**者里取带宽最大；并列取延迟小的。
///
/// 没有可用读数 → None。**绝不退而求其次挑个没测过的**——那等于把出口交给一个可能
/// 根本不通的节点，比留在 url-test 上糟得多。同理也不拿过期读数顶替。
pub fn fastest_measured(nodes: &[Node], now: chrono::DateTime<chrono::Utc>) -> Option<&Node> {
    nodes
        .iter()
        .filter(|n| n.enabled && n.protocol != "ssr" && is_fresh(n, now))
        .filter_map(|n| n.bandwidth.map(|b| (b, n)))
        .max_by_key(|(b, n)| (*b, std::cmp::Reverse(n.latency.unwrap_or(i64::MAX))))
        .map(|(_, n)| n)
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("配置生成失败: {0}")]
    BuildFailed(String),
}

/// 是否存在**可用**节点（启用且非 ssr）——决定客户端配置能否生成 proxy-groups。
///
/// 必须与 `client_yaml` 内部那个判据同源：`runtime::start` 也用它做上游闸门。
/// 两处各算各的一旦漂移，就会出现「闸门放行了、配置却降级成没有出口」的鬼状态，
/// 而那种状态的表现是**静默直连**——正是本函数要消灭的东西。
pub fn has_usable_nodes(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| n.enabled && n.protocol != "ssr")
}

/// 智能分流规则（顺序固定，不含 MATCH；导出与客户端共用）。
///
/// `GEOIP,CN,DIRECT` 这条**不能删**：本地代理拨号（`net::local_proxy`）正靠它让国内
/// 服务器直连——那条路径是**按 IP 拨号**（SSH/部署/测速的控制通道），本来就不经过
/// DNS，没有被污染的余地。
///
/// 试过给它加 `no-resolve`（想让带域名的连接不参与 GEOIP 判定，免得被解析到的国内
/// 地址带偏），**实测不生效**：加完之后日志里照样出现
/// `update.googleapis.com match GeoIP(cn) using DIRECT`。要么该选项对 GEOIP 不生效，
/// 要么 mihomo 在规则匹配前就已经把域名解析好了。故维持原样——别再来试这一手。
pub fn smart_rules() -> Vec<String> {
    vec![
        "GEOIP,LAN,DIRECT".into(),
        "GEOIP,PRIVATE,DIRECT".into(),
        "GEOSITE,cn,DIRECT".into(),
        "GEOIP,CN,DIRECT".into(),
    ]
}

/// 客户端专属的「钉死走代理」域名，**排在智能分流之前**（只在客户端配置里用，
/// 导出给别人用的 YAML 不加——那是别人的策略）。
///
/// Grok 的网页与 WebSocket 都应遵循用户选择的出口，不能被后面的国内直连规则抢走。
/// 规则匹配以识别到域名为前提：普通模式由 HTTP CONNECT 传递域名，TUN 模式可由
/// fake-ip 恢复域名。仅传真实 IP 的 SOCKS4 请求未必能命中这里。
///
/// `target` 是这些域名要走的组，恒为 [`MANUAL_GROUP`]（跟着用户的手动选择走）。
///
/// 曾用「实时直连」专用组，但它缺少 UI 选择器且不会自动切换，现已改为跟随手动选择。
/// 历史观测中的 400 不能证明中转出口必然被封：2026-09-19 同一路径未登录握手得到 401。
/// 实际会话是否可用需结合当次请求和响应判断，不能用节点类型代替诊断。
pub fn pinned_proxy_rules(target: &str) -> Vec<String> {
    vec![
        format!("DOMAIN-SUFFIX,x.ai,{target}"),
        format!("DOMAIN-SUFFIX,grok.com,{target}"),
    ]
}

/// 生成合并后的订阅 YAML；节点顺序按名称排序保证输出确定性。
/// ssr 节点（mihomo 已移除 SSR 支持）在映射前跳过并计数，不静默映射为 ss；
/// 返回 (yaml, 跳过的 ssr 节点数)。proxy-groups 与 proxies 同步剔除，避免悬空引用。
/// smart=true 时 rules 前置智能分流（国内直连），MATCH 兜底不变。
///
/// **仅测试可用**（`#[cfg(all(test, feature = "local-tests"))]`）。生产代码一律走
/// [`build_yaml_with_privacy`]：
/// 这个包装器把隐私传成空切片，用它就会产出「界面显示已开启、配置里却是 VPS 公网
/// 入口」的配置——2026-09-22 就是这么错了一整轮（订阅缓存、运行配置、导出三条路径
/// 全接在这个包装器上，隐私功能整体是死的）。留着它当公开 API 等于留着同一个坑，
/// 所以交给编译器看门。
#[cfg(all(test, feature = "local-tests"))]
pub fn build_yaml(
    nodes: &[Node],
    smart: bool,
    rules: &[RelayRule],
    servers: &[Server],
) -> Result<(String, usize), ConfigError> {
    build_yaml_with_privacy(nodes, smart, rules, servers, &[])
}

/// 同 [`build_yaml`]，但带上隐私开关（订阅导出与运行配置都该用这个）。
pub fn build_yaml_with_privacy(
    nodes: &[Node],
    smart: bool,
    rules: &[RelayRule],
    servers: &[Server],
    privacies: &[Privacy],
) -> Result<(String, usize), ConfigError> {
    let mut enabled: Vec<&Node> = nodes.iter().filter(|n| n.enabled).collect();
    enabled.sort_by(|a, b| a.name.cmp(&b.name));

    let mut proxies = vec![];
    let mut skipped_ssr = 0usize;
    for n in &enabled {
        // ssr：mihomo 与 sing-box 均已移除 SSR 支持，映射会丢失协议伪装层（静默失真），
        // 一律跳过并计数（含 clash_raw 片段，原样输出同样是不可用配置）
        if n.protocol == "ssr" {
            skipped_ssr += 1;
            continue;
        }
        proxies.push(node_mapping(n, None, rules, servers, privacies)?);
    }

    let group_names: Vec<String> = enabled
        .iter()
        .filter(|n| n.protocol != "ssr")
        .map(|n| client_name(n, privacies))
        .collect();
    // 开了隐私的线不进 url-test（理由见 net_groups_split），但仍在手动选择里可挑
    let auto_names: Vec<String> = enabled
        .iter()
        .filter(|n| n.protocol != "ssr" && !is_private(n, privacies))
        .map(|n| client_name(n, privacies))
        .collect();
    if privacies.iter().any(|p| !p.onion_host.is_empty()) {
        proxies.push(tor_dependency_mapping());
    }
    let groups = net_groups_split(&auto_names, &group_names);

    let mut rules: Vec<Value> = if smart {
        smart_rules().into_iter().map(Value::String).collect()
    } else {
        vec![]
    };
    // 兜底规则的目标要跟着分组实际形态走：全是隐私节点时没有「自动选择」组
    rules.push(Value::String(
        if auto_names.is_empty() {
            "MATCH,手动选择"
        } else {
            "MATCH,自动选择"
        }
        .into(),
    ));

    let mut root = Mapping::new();
    root.insert(s("proxies"), Value::Sequence(proxies));
    root.insert(s("proxy-groups"), Value::Sequence(groups));
    root.insert(s("rules"), Value::Sequence(rules));
    serde_yaml::to_string(&Value::Mapping(root))
        .map(|yaml| (yaml, skipped_ssr))
        .map_err(|e| ConfigError::BuildFailed(e.to_string()))
}

/// 自动选择组名（`url-test`，含全部节点）。
pub const AUTO_GROUP: &str = "自动选择";
/// 手动选择组名（`select`，首项是「自动选择」，可再挑单节点）。
///
/// 也是实时类域名（`x.ai`/`grok.com`）钉死的目标，见 [`pinned_proxy_rules`]。
pub const MANUAL_GROUP: &str = "手动选择";
/// Tor 专用的普通节点分组。HTTP 204 测速通过并不保证能建立 Tor TLS 握手；
/// 看门狗会独立切换它，绝不改用户的 `手动选择` 出口。
pub const TOR_UPSTREAM_GROUP: &str = "vpnctl-tor-route";

/// 客户端专用分组：自动选择(url-test) + 手动选择(select，首项=自动选择可再挑单节点)。
///
/// 两套成员列表：`url_test_names` 进自动选择，`manual_names` 进手动选择。
///
/// 为什么要分开——**隐私（Tor Onion）节点只进手动选择**：自动选择是 url-test，
/// mihomo 会对组内每个成员做周期性健康检查。把一个 `.onion` 节点放进去，等于让
/// 客户端主动、周期性地去拨它；那类探测的失败重试路径是否始终经 Tor 不由我们控制，
/// 而一旦回落到直连就是「软件自己的探测暴露了 VPS 公网 IP」——正是本功能要消灭的东西。
/// 用户手动点选没有这个问题：连接与失败都走同一条 dialer-proxy 链。
///
/// `url_test_names` 为空时**不产出 url-test 组**（空 proxies 的 url-test 组会被 mihomo
/// 拒绝加载），同时手动选择里也不列「自动选择」——否则指向一个不存在的组，同样拒绝加载。
pub fn net_groups_split(url_test_names: &[String], manual_names: &[String]) -> Vec<Value> {
    let mut manual = Mapping::new();
    manual.insert(s("name"), s(MANUAL_GROUP));
    manual.insert(s("type"), s("select"));
    let mut proxies = Vec::new();
    if !url_test_names.is_empty() {
        proxies.push(Value::String(AUTO_GROUP.into()));
    }
    proxies.extend(manual_names.iter().map(|n| Value::String(n.clone())));
    manual.insert(s("proxies"), Value::Sequence(proxies));

    let mut groups = Vec::new();
    if !url_test_names.is_empty() {
        groups.push(url_test_group(url_test_names));
    }
    groups.push(Value::Mapping(manual));
    groups
}

/// TUN 网卡的设备名（同时也是 `GetAdaptersAddresses` 里的友好名）。
///
/// **唯一真源**：`tun_block` 生成的 device 字段与 `conflict::TUN_PATTERNS` 的自家识别
/// 都引用它。曾经两处各写各的字面量，结果是网卡自己的名字不在接管特征表里——
/// 自家残留拦不住（见 `conflict` 模块的 tun_signals 测试）。
pub const TUN_DEVICE: &str = "yunyi-tun0";

/// TUN 块：只有 TUN 模式才有。改动这里等于改 TUN 的接管方式（stack/auto-route）。
///
/// 与 dns 块曾经绑在一起同进同退（理由是「TUN 不配 dns 时国外域名必挂」）。
/// 2026-09-18 查明常规（系统代理）模式**同样**需要 dns 块，两块就此解绑——
/// 原因见 `dns_block`。
pub fn tun_block(enabled: bool) -> Vec<(&'static str, Value)> {
    if !enabled {
        return vec![];
    }
    // 字面量内联：这几行是实测调出来的形态，拆成结构体反而看不出全貌。
    //
    // `dns-hijack` 必须显式写，且必须是 `any` 而不是 `0.0.0.0`：mihomo 的默认值是
    // `0.0.0.0:53`，**只盖 IPv4**。实测（2026-09-19，本机）：向 IPv4 DNS 223.5.5.5
    // 查一个不存在的域名返回 198.18.0.49（被劫持，mihomo 对任何名字都发 fake-ip），
    // 向路由器的 IPv6 DNS `fe80::1` 查同一个名字返回 NXDOMAIN（**没被劫持**）；
    // 而本机 WLAN 的 IPv6 DNS 恰恰就是 fe80::1。于是 IPv6 那条解析路径拿的是 GFW 的
    // 污染答案（实测 gemini.google.com → 98.159.108.71），且它连 TUN 都进不去——
    // 链路本地地址按接口直出 WLAN。`any` 覆盖两族。
    let tun: Value = serde_yaml::from_str(&format!(
        "enable: true\n\
         stack: mixed\n\
         device: {TUN_DEVICE}\n\
         auto-route: true\n\
         auto-detect-interface: true\n\
         dns-hijack:\n  - any:53\n"
    ))
    .expect("tun 块字面量恒合法");
    vec![("tun", tun)]
}

/// dns 块：**两种模式都要**（调用方按 `geo_present` 决定下不下）。
///
/// 为什么系统代理模式也要：mihomo 判 `GEOIP,CN,DIRECT` 这类规则时得先把域名解析成
/// IP，没有 dns 块就只能用**系统 DNS**，而被污染/被劫持的答案会把墙外域名判成国内
/// → DIRECT → 直连当场被墙。实测现场（2026-09-18）：`update.googleapis.com` 解析到
/// 120.253.253.225（中国移动）→ 命中 `GeoIP/cn` → DIRECT，同一刻 `www.google.com`
/// 走的是 `Match → 大美`——同一个客户端、同一条规则，全看系统 DNS 那一刻给什么答案。
/// 配上本块后，判定用的是这里 nameserver/fallback 的答案，不再取决于系统 DNS。
/// （TUN 模式下另有更强的理由：客户端自己解析 DNS，mihomo 只看得见 IP。）
///
/// `has_nodes` 决定 fallback 是否带 `#手动选择`——**这是整件事的关键**。只加
/// fallback + fallback-filter 没用：fallback 的查询自己也是**直连**发出的，在国内
/// 同样被污染，filter 拿不到可信答案。加 `#组名` 让查询经节点出去才有效
/// （google 从超时变 200/1.66s）。零节点降级时不能带组名——那时没有 proxy-groups，
/// 指向不存在的组会让 mihomo 拒绝加载。
///
/// 缺 geo 数据时不要下这块：`fallback-filter.geoip` 要 country.mmdb，启用 dns 而
/// 文件缺失会让 mihomo 启动期联网下载（卡住启动 5s+）。
pub fn dns_block(has_nodes: bool) -> (&'static str, Value) {
    let via = if has_nodes { "#手动选择" } else { "" };
    let dns: Value = serde_yaml::from_str(&format!(
        "enable: true\n\
         enhanced-mode: fake-ip\n\
         fake-ip-range: 198.18.0.1/16\n\
         nameserver:\n  - https://223.5.5.5/dns-query\n  - https://1.12.12.12/dns-query\n\
         fallback:\n  - https://1.1.1.1/dns-query{via}\n  - https://8.8.8.8/dns-query{via}\n\
         fallback-filter:\n  geoip: true\n  geoip-code: CN\n"
    ))
    .expect("dns 块字面量恒合法");
    ("dns", dns)
}

/// 本机上网客户端 config：顶层固定键（mixed-port/secret/仅本机 controller/rule 模式/双 DNS）
/// + 与 build_yaml 同源的 node_mapping 代理 + net_groups 分组 + 智能 rules。
///   geo_present=false 时降级：只留 LAN/PRIVATE 直连，GEOSITE/GEOIP CN 规则不下发
///   （缺失 geo 数据文件的 mihomo 启动即报错，降级优先于功能缺失）。
///
/// **仅测试可用**（`#[cfg(all(test, feature = "local-tests"))]`）。生产代码一律走
/// [`client_yaml_with_privacy`]，
/// 理由与 [`build_yaml`] 同一条：这个包装器把隐私传成空切片，用它就会产出
/// 「界面显示已开启、实际还走着公网入口」的运行配置。交给编译器看门。
#[cfg(all(test, feature = "local-tests"))]
#[allow(clippy::too_many_arguments)] // 客户端 config 编排函数：注入项即全部依赖，打包成结构体反增间接层
pub fn client_yaml(
    nodes: &[Node],
    mixed_port: u16,
    ctrl_port: u16,
    token: &str,
    geo_present: bool,
    geodata_dir: &str,
    rules: &[RelayRule],
    servers: &[Server],
    tun_enabled: bool,
) -> Result<String, ConfigError> {
    client_yaml_with_privacy(
        nodes,
        mixed_port,
        ctrl_port,
        token,
        geo_present,
        geodata_dir,
        rules,
        servers,
        tun_enabled,
        &[],
    )
}

/// 测试用的隐私配置包装器。运行时要用 [`client_yaml_with_tor_upstream`]，
/// 才能给 Tor 自己提供不会绕回 onion 线路的上游入口。
#[cfg(all(test, feature = "local-tests"))]
#[allow(clippy::too_many_arguments)]
pub fn client_yaml_with_privacy(
    nodes: &[Node],
    mixed_port: u16,
    ctrl_port: u16,
    token: &str,
    geo_present: bool,
    geodata_dir: &str,
    rules: &[RelayRule],
    servers: &[Server],
    tun_enabled: bool,
    privacies: &[Privacy],
) -> Result<String, ConfigError> {
    client_yaml_with_tor_upstream(
        nodes,
        mixed_port,
        ctrl_port,
        token,
        geo_present,
        geodata_dir,
        rules,
        servers,
        tun_enabled,
        privacies,
        None,
    )
}

/// 运行中的 mihomo 另开一个仅回环可达的 SOCKS 入口，指向只含普通节点的 Tor 专用组。
/// 看门狗可独立切换该组，不受用户在 `手动选择` 中切换到 onion 线路的影响。
#[allow(clippy::too_many_arguments)]
pub fn client_yaml_with_tor_upstream(
    nodes: &[Node],
    mixed_port: u16,
    ctrl_port: u16,
    token: &str,
    geo_present: bool,
    geodata_dir: &str,
    rules: &[RelayRule],
    servers: &[Server],
    tun_enabled: bool,
    privacies: &[Privacy],
    tor_upstream_port: Option<u16>,
) -> Result<String, ConfigError> {
    let mut enabled: Vec<&Node> = nodes.iter().filter(|n| n.enabled).collect();
    enabled.sort_by(|a, b| a.name.cmp(&b.name));
    let mut proxies = vec![];
    for n in &enabled {
        if n.protocol == "ssr" {
            continue;
        }
        // 运行配置里的代理名必须与分组引用**同源**：下面 `names` 用的是
        // `client_name`，这里若走 `node_mapping(n, None)`，clash_raw 分支会保留
        // 片段自带的旧 name，于是 proxies 与 proxy-groups 分叉——重名时 mihomo
        // 拒绝整份配置（`duplicate name`），不重名时分组指向不存在的成员。
        // 导出 YAML（build_yaml）保持“raw 自带 name 不改写”的既有语义，这里只
        // 收口运行配置；导入路径也已把消歧后的名字同步写进片段，双保险。
        let name = client_name(n, privacies);
        proxies.push(node_mapping(n, Some(&name), rules, servers, privacies)?);
    }
    let names: Vec<String> = enabled
        .iter()
        .filter(|n| n.protocol != "ssr")
        .map(|n| client_name(n, privacies))
        .collect();
    // 有开着的隐私线才注入内部 Tor 依赖；没有它时多一条无用代理只是噪音
    if privacies.iter().any(|p| !p.onion_host.is_empty()) {
        proxies.push(tor_dependency_mapping());
    }
    // 开了隐私的线只进手动选择（理由见 net_groups_split）
    let auto_names = tor_upstream_candidates(nodes, privacies);
    let mut root = Mapping::new();
    root.insert(s("mixed-port"), serde_yaml::Value::from(mixed_port));
    root.insert(s("mode"), s("rule"));
    root.insert(s("secret"), s(token));
    root.insert(
        s("external-controller"),
        s(&format!("127.0.0.1:{ctrl_port}")),
    );
    root.insert(s("allow-lan"), serde_yaml::Value::Bool(false));
    // geo-auto-update 关 + geodata-dir 指向 net 目录：启动期不探测网络。
    root.insert(s("geo-auto-update"), serde_yaml::Value::Bool(false));
    root.insert(s("geodata-dir"), s(geodata_dir));
    root.insert(s("proxies"), Value::Sequence(proxies));
    if let Some(port) = tor_upstream_port {
        let mut listener = Mapping::new();
        listener.insert(s("name"), s("vpnctl-tor-upstream"));
        listener.insert(s("type"), s("socks"));
        listener.insert(s("listen"), s("127.0.0.1"));
        listener.insert(s("port"), serde_yaml::Value::from(port));
        // 没有非隐私节点时明确拒绝 Tor 上游连接，绝不回落到手动组或 DIRECT。
        listener.insert(
            s("proxy"),
            s(if auto_names.is_empty() {
                "REJECT"
            } else {
                TOR_UPSTREAM_GROUP
            }),
        );
        listener.insert(s("udp"), Value::Bool(false));
        root.insert(
            s("listeners"),
            Value::Sequence(vec![Value::Mapping(listener)]),
        );
    }
    // 零节点降级：url-test 组空 proxies 会被 mihomo 拒绝（`use` or `proxies` missing），
    // 无节点时省略分组，兜底改 REJECT（**不是** DIRECT，见下方 push 处说明）
    let has_nodes = has_usable_nodes(nodes);
    if has_nodes {
        let mut groups = net_groups_split(&auto_names, &names);
        if tor_upstream_port.is_some() && !auto_names.is_empty() {
            let mut tor_group = Mapping::new();
            tor_group.insert(s("name"), s(TOR_UPSTREAM_GROUP));
            tor_group.insert(s("type"), s("select"));
            tor_group.insert(
                s("proxies"),
                Value::Sequence(auto_names.iter().map(|name| s(name)).collect()),
            );
            groups.push(Value::Mapping(tor_group));
        }
        root.insert(s("proxy-groups"), Value::Sequence(groups));
    }
    // dns 与 tun 两块都放在分组之后：dns 的 fallback 是否带组名取决于 has_nodes
    if geo_present {
        let (key, value) = dns_block(has_nodes);
        root.insert(s(key), value);
    }
    for (key, value) in tun_block(tun_enabled) {
        root.insert(s(key), value);
    }
    let mut rules: Vec<Value> = Vec::new();
    // 钉死走代理的域名排最前，免得被下面的直连规则抢走。零节点降级时不加：那时没有
    // proxy-groups，指向不存在的组会让 mihomo 拒绝加载整份配置
    if has_nodes {
        rules.extend(
            pinned_proxy_rules(MANUAL_GROUP)
                .into_iter()
                .map(Value::String),
        );
    }
    let base: Vec<String> = if geo_present {
        smart_rules()
    } else {
        smart_rules()[..2].to_vec()
    };
    rules.extend(base.into_iter().map(Value::String));
    // 零节点兜底是 REJECT 而不是 DIRECT：一份没有代理的配置如果兜底成 DIRECT，
    // 效果是「本应用什么都没干，却让所有墙外流量悄悄走直连」——用户看到的只是
    // 「某个网站上不去」，完全查不到是没节点。（实测形态：没有可用节点时
    // OpenAI 的 region 封锁，见 CHANGELOG。）
    // 换 REJECT 是让失败**可见**：REJECT 是 mihomo 内建出站，不需要在
    // proxy-groups 里定义，零节点降级配置照样能加载；且它立刻回 RST，
    // 客户端当场报错而不是挂到超时（REJECT-DROP 才是静默丢包）。
    // 国内直连不受影响：四条 GEOIP/GEOSITE 规则仍排在它前面。
    rules.push(Value::String(if has_nodes {
        "MATCH,手动选择".into()
    } else {
        "MATCH,REJECT".into()
    }));
    root.insert(s("rules"), Value::Sequence(rules));
    serde_yaml::to_string(&Value::Mapping(root))
        .map_err(|e| ConfigError::BuildFailed(e.to_string()))
}

/// 单节点 → Clash 代理映射。name_override 为 Some 时强制覆盖 name（测速注入必须用
/// node.id）；为 None 时保留原语义：clash_raw 片段自带 name 则原样保留（手工节点可能
/// 与节点名不一致，YAML 导出不得改写），无 name 的 raw 片段不加；URI 映射用 n.name。
/// 未知协议且无 clash_raw 报错。经中转的节点先 apply effective_node（addr/port 换指
/// 中转机）——schema 不区分，以最终 server/port 字段生效。
fn node_mapping(
    n: &Node,
    name_override: Option<&str>,
    rules: &[RelayRule],
    servers: &[Server],
    privacies: &[Privacy],
) -> Result<Value, ConfigError> {
    let n = &effective_node(n, rules, servers);
    let params: serde_json::Value = serde_json::from_str(&n.params_json)
        .map_err(|e| ConfigError::BuildFailed(e.to_string()))?;
    // Clash 来源：解析 raw 片段原样放入（全部字段保留）；仅测速注入时覆盖 name
    if let Some(raw_str) = params.get("clash_raw").and_then(|v| v.as_str()) {
        let fragment: Value =
            serde_yaml::from_str(raw_str).map_err(|e| ConfigError::BuildFailed(e.to_string()))?;
        let mut map = fragment
            .as_mapping()
            .ok_or_else(|| ConfigError::BuildFailed("clash_raw 不是映射".into()))?
            .clone();
        if let Some(name) = name_override {
            map.insert(s("name"), s(name));
        }
        if let Some(p) = crate::servers::privacy::effective_privacy(&n.id, privacies) {
            apply_privacy_overlay(&mut map, n, p);
        }
        return Ok(Value::Mapping(map));
    }
    let name = name_override.unwrap_or(&n.name);
    let mut map = match n.protocol.as_str() {
        "ss" => ss_mapping(n, name, &params),
        "vmess" => vmess_mapping(n, name, &params),
        "vless" => vless_mapping(n, name, &params),
        "trojan" => trojan_mapping(n, name, &params),
        "hysteria2" => hysteria2_mapping(n, name, &params),
        "tuic" => tuic_mapping(n, name, &params),
        "wireguard" => wireguard_mapping(n, name, &params),
        other => {
            return Err(ConfigError::BuildFailed(format!(
                "未知协议且无 clash_raw: {other}"
            )))
        }
    };
    // 隐私叠加层：这条线开着隐私就换入口。**在协议映射全部做完之后**才叠加，
    // 所以任何一种协议都自动适用，不需要各写一份（这就是「开关」的实现方式）。
    if let Some(p) = crate::servers::privacy::effective_privacy(&n.id, privacies) {
        apply_privacy_overlay(&mut map, n, p);
    }
    Ok(Value::Mapping(map))
}

/// 单节点代理 JSON（测速注入用）：name 强制覆盖为 proxy_name，与导出 YAML 形态一致。
/// ssr 协议直接返回专有错误（测速侧据此跳过该节点，不触碰引擎）。
pub fn node_proxy_json(
    node: &Node,
    proxy_name: &str,
    rules: &[RelayRule],
    servers: &[Server],
) -> Result<serde_json::Value, ConfigError> {
    node_proxy_json_with_privacy(node, proxy_name, rules, servers, &[])
}

/// 同 [`node_proxy_json`]，但带上隐私开关。**测速路径必须走这个**：
/// 用不带隐私的那个，开着隐私的线会被直连测速，探测打到 VPS 公网 IP 上。
pub fn node_proxy_json_with_privacy(
    node: &Node,
    proxy_name: &str,
    rules: &[RelayRule],
    servers: &[Server],
    privacies: &[Privacy],
) -> Result<serde_json::Value, ConfigError> {
    if node.protocol == "ssr" {
        return Err(ConfigError::BuildFailed(
            "mihomo 不支持 SSR 协议，测速跳过".into(),
        ));
    }
    // §11（隐私模式的硬要求）：**开了隐私的线不许直连测速**。
    // 测速侧车拨的是节点的 (addr, port)，也就是 VPS 公网 IP——那正是隐私模式
    // 要藏起来的东西。让「软件自己的健康检查」把 IP 暴露给本地网络，是这套功能
    // 最讽刺的失败方式。v1 直接跳过：界面显示「未测试」，
    // **不为了显示延迟去破坏隐私模型**。
    if crate::servers::privacy::effective_privacy(&node.id, privacies).is_some() {
        return Err(ConfigError::BuildFailed(
            "隐私线路不参与直连测速（探测会暴露 VPS 公网 IP），已跳过".into(),
        ));
    }
    serde_json::to_value(node_mapping(
        node,
        Some(proxy_name),
        rules,
        servers,
        privacies,
    )?)
    .map_err(|e| ConfigError::BuildFailed(e.to_string()))
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn str_param(params: &serde_json::Value, key: &str) -> String {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 这条线在客户端配置里的名字：开了隐私就带 `[隐私]` 后缀。
///
/// **必须以真源为准**——`proxies` 的条目名与 `proxy-groups` 的成员名都走这里。
/// 只改一边的后果是分组指向一个不存在的成员，整份配置静默失效（本文件里
/// `rename_node` 那条注释记着同一个坑）。
/// 这条线开着隐私吗（且 onion 已就绪）。开关的判据只有这一个来源。
fn is_private(n: &Node, privacies: &[Privacy]) -> bool {
    crate::servers::privacy::effective_privacy(&n.id, privacies).is_some()
}

/// 自动测速组与“按带宽选最快”共用的候选条件；隐私线只供手动选择。
pub fn auto_eligible(n: &Node, privacies: &[Privacy]) -> bool {
    n.enabled && n.protocol != "ssr" && !is_private(n, privacies)
}

/// Tor 专用组与看门狗共用这一份候选列表；顺序与配置中的成员顺序一致。
pub fn tor_upstream_candidates(nodes: &[Node], privacies: &[Privacy]) -> Vec<String> {
    let mut eligible: Vec<&Node> = nodes
        .iter()
        .filter(|n| auto_eligible(n, privacies))
        .collect();
    eligible.sort_by(|a, b| a.name.cmp(&b.name));
    eligible
        .into_iter()
        .map(|n| client_name(n, privacies))
        .collect()
}

/// 这份（节点 × 隐私）组合会不会真的产出**经 Tor 拨号**的代理。
///
/// 三个条件缺一不可，且**必须**与 [`build_yaml_with_privacy`] 里注入
/// `dialer-proxy` 的那条路径同源：启用 + 非 ssr + [`is_private`]。
/// 少一个就会出现两种错误，方向相反但都难查：
/// - 判松了（只看「库里有没有 onion 非空的记录」）→ 给一条已被停用的线白起一次 Tor；
/// - 判严了 → 配置里有 `__privacy_tor__` 依赖、却没人去起 Tor，**那条线连不上**。
///
/// 调用方是 `net::runtime`，用它决定要不要懒启动本机 Tor
/// （用户拍板：真要用才起，不是开网络就起）。
///
/// `clash_raw` 同样会经过 [`node_mapping`] 的隐私叠加层，判据与产物一致。
pub fn privacy_in_use(nodes: &[Node], privacies: &[Privacy]) -> bool {
    nodes
        .iter()
        .any(|n| n.enabled && n.protocol != "ssr" && is_private(n, privacies))
}

pub fn client_name(n: &Node, privacies: &[Privacy]) -> String {
    match crate::servers::privacy::effective_privacy(&n.id, privacies) {
        Some(p) => crate::servers::privacy::display_name(&p.name),
        None => n.name.clone(),
    }
}

/// 隐私叠加层：把这条线的**入口**换成 `.onion`，挂上内部 Tor 依赖。
///
/// 这是开关版的全部机制——照中转 [`effective_node`] 的同一形状：中转换 `addr/port`
/// 指到中转机，隐私换 `server` 指到洋葱地址。TCP 协议参数（uuid / flow /
/// reality-opts / 网络类型）原样保留；Tor 不支持 UDP，隐私线必须关掉 UDP 能力。
///
/// 三条硬要求（都有测试钉住）：
/// 1. `server` **只**取隐私记录的 `onion_host`，绝不回落到 `n.addr`——那是 VPS 公网 IP，
///    回落等于把「经 Tor 拨号」静默换成「直连 VPS」，隐私模型失效且不报任何错；
/// 2. 必须挂 `dialer-proxy`，否则 mihomo 会直接去连那个 `.onion`（必然失败，
///    但更糟的是若某天它被解析成了 IP，就是一次裸奔）；
/// 3. **端口不动**：onion 侧与回环侧同号，客户端配置里 `port` 保持原值。
///
/// 刻意**不**给它派生任何健康检查字段，且这条线不进 url-test 组（见 [`net_groups_split`]）：
/// mihomo 的 URLTest 会周期性探测成员，那是「软件自己直连 VPS 公网 IP」最可能的路径。
fn apply_privacy_overlay(m: &mut Mapping, n: &Node, p: &Privacy) {
    m.insert(s("server"), s(&p.onion_host));
    m.insert(
        s("dialer-proxy"),
        s(crate::servers::privacy::INTERNAL_TOR_PROXY),
    );
    m.insert(s("udp"), Value::Bool(false));
    // 名字也要换，且与 client_name 同源——分组成员名取的是同一个函数
    m.insert(
        s("name"),
        s(&crate::servers::privacy::display_name(&p.name)),
    );
    let _ = n;
}

/// 内部 Tor 依赖：**不是用户节点**，不进任何分组，只作为隐私节点的 `dialer-proxy`。
/// `udp: false`——Tor 的 SOCKS 端口不支持 UDP associate。
fn tor_dependency_mapping() -> Value {
    let mut m = Mapping::new();
    m.insert(s("name"), s(crate::servers::privacy::INTERNAL_TOR_PROXY));
    m.insert(s("type"), s("socks5"));
    m.insert(s("server"), s(crate::servers::privacy::TOR_SOCKS_HOST));
    m.insert(
        s("port"),
        Value::Number(crate::servers::privacy::TOR_SOCKS_PORT.into()),
    );
    m.insert(s("udp"), Value::Bool(false));
    Value::Mapping(m)
}

fn ss_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("ss"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("cipher"), s(&str_param(params, "cipher")));
    m.insert(s("password"), s(&str_param(params, "password")));
    m.insert(s("udp"), Value::Bool(true));
    m
}

fn vmess_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("vmess"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("uuid"), s(&str_param(params, "uuid")));
    let alter_id: i64 = str_param(params, "alterId").parse().unwrap_or(0);
    m.insert(s("alterId"), Value::Number(alter_id.into()));
    m.insert(s("cipher"), s(&str_param(params, "cipher")));
    m.insert(s("udp"), Value::Bool(true));
    let network = str_param(params, "network");
    if !network.is_empty() {
        m.insert(s("network"), s(&network));
    }
    let tls = str_param(params, "tls");
    if !tls.is_empty() {
        m.insert(s("tls"), Value::Bool(tls == "tls"));
    }
    // VMess 的 SNI 字段名是 servername（与 vless/trojan 的 sni 不同）；
    // sing-box 订阅导入后会把 TLS server_name 落到 sni，这里必须转一手，
    // 否则经本程序导出的 VMess-WS 节点再导入时丢失 SNI。
    let sni = str_param(params, "sni");
    if !sni.is_empty() {
        m.insert(s("servername"), s(&sni));
    }
    if skip_cert_verify_true(params) {
        m.insert(s("skip-cert-verify"), Value::Bool(true));
    }
    if network == "ws" {
        let mut ws = Mapping::new();
        let path = str_param(params, "path");
        let host = str_param(params, "host");
        if !path.is_empty() {
            ws.insert(s("path"), s(&path));
        }
        if !host.is_empty() {
            ws.insert(s("headers"), {
                let mut h = Mapping::new();
                h.insert(s("Host"), s(&host));
                Value::Mapping(h)
            });
        }
        m.insert(s("ws-opts"), Value::Mapping(ws));
    }
    m
}

fn vless_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("vless"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("uuid"), s(&str_param(params, "uuid")));
    m.insert(s("udp"), Value::Bool(true));
    // URI 原生键：type → network；security=tls/reality → tls
    let network = str_param(params, "type");
    if !network.is_empty() {
        m.insert(s("network"), s(&network));
    }
    if let Some(tls) = tls_from_security(params) {
        m.insert(s("tls"), tls);
    }
    if network == "ws" {
        if let Some(ws) = ws_opts(params) {
            m.insert(s("ws-opts"), Value::Mapping(ws));
        }
    }
    // URI 原生键：flow/reality-opts/client-fingerprint —— reality-opts 仅当 security=reality
    // 且 public-key 非空才输出。参数键沿袭 URI 的 fingerprint（vmess://…fp=），但 mihomo
    // 1.19+ 里 fingerprint 是证书 PINNING 字段、浏览器指纹必须用 client-fingerprint
    // （实测 v1.19.10 用 fingerprint: chrome 直接拒绝建连）。
    let flow = str_param(params, "flow");
    if !flow.is_empty() {
        m.insert(s("flow"), s(&flow));
    }
    let security = str_param(params, "security");
    if security == "reality" {
        let public_key = str_param(params, "public-key");
        if !public_key.is_empty() {
            let mut reality_opts = Mapping::new();
            reality_opts.insert(s("public-key"), s(&public_key));
            let short_id = str_param(params, "short-id");
            if !short_id.is_empty() {
                reality_opts.insert(s("short-id"), s(&short_id));
            }
            m.insert(s("reality-opts"), Value::Mapping(reality_opts));
        }
    }
    let fingerprint = str_param(params, "fingerprint");
    if !fingerprint.is_empty() {
        m.insert(s("client-fingerprint"), s(&fingerprint));
    }
    if let Some(sni) = params.get("sni").and_then(|v| v.as_str()) {
        if !sni.is_empty() {
            m.insert(s("servername"), s(sni));
        }
    }
    m
}

fn trojan_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("trojan"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("password"), s(&str_param(params, "password")));
    m.insert(s("udp"), Value::Bool(true));
    // URI 原生键：type → network；security=tls → tls
    let network = str_param(params, "type");
    if !network.is_empty() {
        m.insert(s("network"), s(&network));
    }
    if let Some(tls) = tls_from_security(params) {
        m.insert(s("tls"), tls);
    }
    if network == "ws" {
        if let Some(ws) = ws_opts(params) {
            m.insert(s("ws-opts"), Value::Mapping(ws));
        }
    }
    let sni = str_param(params, "sni");
    if !sni.is_empty() {
        m.insert(s("sni"), s(&sni));
    }
    // skip-cert-verify：skip-cert-verify 或 allowInsecure 任一为 true/1 即真
    if skip_cert_verify_true(params) {
        m.insert(s("skip-cert-verify"), Value::Bool(true));
    }
    // URI 原生键：security=reality → reality-opts(与 vless_mapping 同构)
    let security = str_param(params, "security");
    if security == "reality" {
        let public_key = str_param(params, "public-key");
        if !public_key.is_empty() {
            let mut reality_opts = Mapping::new();
            reality_opts.insert(s("public-key"), s(&public_key));
            let short_id = str_param(params, "short-id");
            if !short_id.is_empty() {
                reality_opts.insert(s("short-id"), s(&short_id));
            }
            m.insert(s("reality-opts"), Value::Mapping(reality_opts));
        }
        let fingerprint = str_param(params, "fingerprint");
        if !fingerprint.is_empty() {
            m.insert(s("client-fingerprint"), s(&fingerprint));
        }
    }
    m
}

/// Hysteria2(mihomo meta 字段):password/sni/up/down/skip-cert-verify。
fn hysteria2_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("hysteria2"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("password"), s(&str_param(params, "password")));
    let sni = str_param(params, "sni");
    if !sni.is_empty() {
        m.insert(s("sni"), s(&sni));
    }
    for key in ["up", "down"] {
        let v = str_param(params, key);
        if !v.is_empty() {
            m.insert(s(key), s(&v));
        }
    }
    if skip_cert_verify_true(params) {
        m.insert(s("skip-cert-verify"), Value::Bool(true));
    }
    m.insert(s("udp"), Value::Bool(true));
    m
}

/// TUIC(mihomo meta 字段):uuid/password(或 token)/sni。
fn tuic_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("tuic"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    m.insert(s("uuid"), s(&str_param(params, "uuid")));
    let password = {
        let v = str_param(params, "password");
        if v.is_empty() {
            str_param(params, "token")
        } else {
            v
        }
    };
    if !password.is_empty() {
        m.insert(s("password"), s(&password));
    }
    let sni = str_param(params, "sni");
    if !sni.is_empty() {
        m.insert(s("sni"), s(&sni));
    }
    if skip_cert_verify_true(params) {
        m.insert(s("skip-cert-verify"), Value::Bool(true));
    }
    m.insert(s("udp"), Value::Bool(true));
    m
}

/// WireGuard(mihomo meta 字段):private-key/public-key/preshared-key/ip/dns/mtu 直传。
fn wireguard_mapping(n: &Node, name: &str, params: &serde_json::Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(s("name"), s(name));
    m.insert(s("type"), s("wireguard"));
    m.insert(s("server"), s(&n.addr));
    m.insert(s("port"), Value::Number(n.port.into()));
    for key in [
        "ip",
        "private-key",
        "public-key",
        "preshared-key",
        "dns",
        "mtu",
    ] {
        let v = str_param(params, key);
        if !v.is_empty() {
            m.insert(s(key), s(&v));
        }
    }
    m
}

/// security 键（URI 原生）：tls/reality → tls: true；其余取值或缺失不输出 tls。
fn tls_from_security(params: &serde_json::Value) -> Option<Value> {
    match str_param(params, "security").as_str() {
        "tls" | "reality" => Some(Value::Bool(true)),
        _ => None,
    }
}

/// ws-opts 映射（URI 原生键）：path → path，host → headers.Host；两者皆缺时省略整段。
fn ws_opts(params: &serde_json::Value) -> Option<Mapping> {
    let path = str_param(params, "path");
    let host = str_param(params, "host");
    if path.is_empty() && host.is_empty() {
        return None;
    }
    let mut ws = Mapping::new();
    if !path.is_empty() {
        ws.insert(s("path"), s(&path));
    }
    if !host.is_empty() {
        ws.insert(s("headers"), {
            let mut h = Mapping::new();
            h.insert(s("Host"), s(&host));
            Value::Mapping(h)
        });
    }
    Some(ws)
}

/// skip-cert-verify 判定：URI 原生键 skip-cert-verify 或 allowInsecure，值 "true"/"1" 为真。
fn skip_cert_verify_true(params: &serde_json::Value) -> bool {
    ["skip-cert-verify", "allowInsecure"]
        .iter()
        .any(|k| matches!(str_param(params, k).as_str(), "true" | "1"))
}

fn url_test_group(names: &[String]) -> Value {
    let mut m = Mapping::new();
    m.insert(s("name"), s("自动选择"));
    m.insert(s("type"), s("url-test"));
    let proxies: Vec<Value> = names.iter().map(|n| s(n)).collect();
    m.insert(s("proxies"), Value::Sequence(proxies));
    m.insert(s("url"), s("http://www.gstatic.com/generate_204"));
    m.insert(s("interval"), Value::Number(300i64.into()));
    Value::Mapping(m)
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/config/mods.rs"
));
