//! 领域类型：跨模块共享的数据结构（数据库行 ↔ IPC 序列化）。

use serde::{Deserialize, Serialize};

/// 机场订阅：来源 URL 与同步状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub last_sync_at: Option<String>,
    pub sync_interval: i64,
    pub last_sync_error: Option<String>,
    /// JSON：{"include": ["正则"], "exclude": ["正则"]}，空数组表示不过滤
    pub filter_json: String,
}

/// 代理节点：导入/自建/手动三类来源，测速字段由 P4 填充。
/// 注意：DB 行 ↔ Node 映射（repos::nodes::row_to_node）与 4 条 SELECT 必须与字段
/// 同步补列；新增字段时用 rg "Node \{" 找全字面量构造点。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub source: String,
    pub protocol: String,
    pub addr: String,
    pub port: i64,
    pub params_json: String,
    pub group_name: String,
    pub sub_id: Option<String>,
    pub relay_rule_id: Option<i64>,
    pub enabled: bool,
    pub latency: Option<i64>,
    pub bandwidth: Option<i64>,
    pub last_checked_at: Option<String>,
    /// 最近一次测速失败原因（成功置 NULL；IPC 序列化后前端红字展示）。
    pub last_check_error: Option<String>,
    /// 「直连形态」延迟（ms）：仅挂了中转规则的节点会额外测一次，与 `latency`（经中转）
    /// **同口径**（都是 mihomo URL 测试）故可直接相减，前端据此显示中转收益；
    /// 未测过或直连失败为 NULL（前端不显示对比行，不把「没测过」谎报成「失败」）。
    pub direct_latency: Option<i64>,
    /// 分跳裸 TCP RTT（ms，各 1 个往返），仅挂中转的节点会测：
    /// `hop_relay_ms` 本机→中转机、`hop_landing_ms` 本机→落地机、
    /// `hop_tunnel_ms` 中转机→落地机（经 WireGuard 隧道）。
    /// 与 `latency`（经代理的完整 HTTP 往返，每段 ≥2 个往返且含代理开销）**口径不同**，
    /// 三者相加 ≠ latency——它们只回答「哪一跳烂」，不是总延迟的加法分解。
    pub hop_relay_ms: Option<i64>,
    pub hop_landing_ms: Option<i64>,
    pub hop_tunnel_ms: Option<i64>,
}

/// 节点的**直连地址**：挂上中转的节点 `addr` 已被改指成中转入口，原始直连地址存在
/// `params_json.direct.addr`（`repos::nodes::set_route` 写入、拆链时据此还原）。
/// 「这个节点属于哪台落地机」一律按本函数判，不能看 `addr`。
pub fn direct_addr(n: &Node) -> String {
    serde_json::from_str::<serde_json::Value>(&n.params_json)
        .ok()
        .and_then(|p| {
            p.get("direct")?
                .get("addr")?
                .as_str()
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| n.addr.clone())
}

/// 一次节点测速得到的全部指标（一次写库单元，整行覆盖）。
/// 收口成结构体是因为字段已达 7 个，位置参数会失控；同时让「失败时一起清空」这件事
/// 只有一个落点（见 `repos::nodes::update_speed`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeSpeed {
    /// 经代理（挂中转时即经中转）的延迟 ms。
    pub latency: Option<i64>,
    /// 「直连形态」对比值 ms，仅挂中转的节点会测（同口径，可直接与 latency 相减）。
    pub direct_latency: Option<i64>,
    /// 分跳裸 TCP RTT（ms）：本机→中转机。
    pub hop_relay_ms: Option<i64>,
    /// 分跳裸 TCP RTT（ms）：本机→落地机。
    pub hop_landing_ms: Option<i64>,
    /// 分跳裸 TCP RTT（ms）：中转机→落地机（经隧道）。
    pub hop_tunnel_ms: Option<i64>,
    /// 带宽 bytes/s。
    pub bandwidth: Option<i64>,
    /// 失败原因（成功为 None）。
    pub error: Option<String>,
}

/// 服务器：SSH 主机与认证元数据；凭据引用与凭据内容不出 IPC。
/// 测速四字段由 P5 填充（语义同节点测速）；注意 DB 行 ↔ Server 映射
/// （repos::servers::row_to_server 与 SELECT）必须与字段同步补列。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Server {
    pub id: i64,
    pub name: String,
    /// IPv4 或域名（主地址，始终保留）。
    pub host: String,
    /// 可选 IPv6 字面量（不带方括号）；空串表示该服务器没有登记 IPv6。
    pub host_v6: String,
    /// 当前选用的地址族：`v4` 用 `host`，`v6` 用 `host_v6`。
    /// 由 `set_server_addr_family_cmd` 与节点地址在同一事务内切换。
    /// **只影响入口**（SSH/测速/自建节点 addr），与 `egress_family` 无关。
    pub addr_family: String,
    /// 服务器出站地址族：VPS → 目标域名时优先/限定使用的地址族（`v4`/`v6`）。
    /// 部署时写入 Xray/shadowsocks/sing-box 配置；切换后需重部署生效。
    /// 不影响入口地址族，也不影响字面 IP 目标与客户端侧分流。
    pub egress_family: String,
    pub port: i64,
    pub user: String,
    pub auth_kind: String,
    pub kind: String,
    pub system: String,
    pub notes: String,
    pub created_at: String,
    /// 本机→服务器单次 SSH 端响应耗时（ms，包含建连；旧记录需重新测速）。
    pub latency: Option<i64>,
    /// 服务器下载带宽（bytes/s，SSH 内 curl 实测）。
    pub bandwidth: Option<i64>,
    /// 最近一次测速时间（RFC3339）。
    pub last_checked_at: Option<String>,
    /// 最近一次测速失败原因（成功置 NULL；IPC 序列化后前端红字展示）。
    pub last_check_error: Option<String>,
    /// 分享域名（解析到该机）；空串 = 未配置。新分享强制 HTTPS。
    pub share_domain: String,
}

/// 服务器当前用于 SSH、测速与节点写回的地址：只有登记了 IPv6 且选了 v6 才用它，
/// 否则一律回落到 `host`（IPv4/域名）。
pub fn effective_host(s: &Server) -> &str {
    if s.addr_family == "v6" && !s.host_v6.is_empty() {
        &s.host_v6
    } else {
        &s.host
    }
}

/// 节点地址是否属于该服务器：IPv4/域名与 IPv6 两个地址都算。
/// 服务器页「已部署协议」徽章、部署写回与节点归属判断都用它，不能只看 `host`
/// ——切到 IPv6 后节点 `addr` 是 host_v6，只比 `host` 会全部对不上。
pub fn server_owns_addr(s: &Server, addr: &str) -> bool {
    addr == s.host || (!s.host_v6.is_empty() && addr == s.host_v6)
}

/// 通道：服务器上的端口（防火墙放行/封锁开关），本地 UI 直接管理开关。
/// 开关状态不入库——SSH iptables 规则存在性是事实源，经 channel_status_cmd 实时查询。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    pub port: i64,
    pub protocol: String,
    pub created_at: String,
}

/// 通道开关状态（SSH iptables 规则存在性的实时结果）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelStatus {
    /// true=端口已放行（INPUT ACCEPT 规则存在），false=已封锁或查询失败。
    pub running: bool,
    /// 状态明细：已放行/已封锁/错误原因（脱敏，不含凭据）。
    pub detail: String,
}

/// 阶段1 服务器测速请求（批量/单发统一入口，并发 1-5 默认 3）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerSpeedReq {
    pub job_id: i64,
    pub server_ids: Vec<i64>,
    pub concurrency: u8,
}

/// 阶段2 服务器间吞吐对测请求（seconds 5-30 默认 15）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThroughputReq {
    pub job_id: i64,
    pub from_server_id: i64,
    pub to_server_id: i64,
    pub seconds: u8,
}

/// 一次对测结果行（成功 bits_per_second + checked_at；失败 error 含时间）。
/// full_output：失败时完整客户端 stdout/stderr（P5.7，8KB 截断），供真机诊断；
/// 成功为 None。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThroughputTest {
    pub id: i64,
    pub from_server_id: i64,
    pub to_server_id: i64,
    pub bits_per_second: Option<i64>,
    pub checked_at: String,
    pub error: Option<String>,
    pub full_output: Option<String>,
}

/// 连通测试结果：ok=false 时 message 为可展示原因（脱敏）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionTest {
    pub ok: bool,
    pub system: String,
    pub message: String,
}

/// 后台任务行（deploy 专用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub progress: i64,
    pub error: Option<String>,
    pub created_at: String,
}

/// 创建服务器输入（credential 为密码或 PEM 私钥明文，仅创建时过 IPC 一次）。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateServerInput {
    pub name: String,
    pub host: String,
    /// 可选 IPv6 字面量；空串表示不登记。
    pub host_v6: String,
    pub port: i64,
    pub user: String,
    pub auth_kind: String,
    pub kind: String,
    pub notes: String,
    pub credential: String,
    /// 分享域名（裸域名；空串 = 未配置）。
    pub share_domain: String,
}

/// 更新服务器输入（credential 为 None=保留原凭据）。
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateServerInput {
    pub id: i64,
    pub name: String,
    pub host: String,
    /// 可选 IPv6 字面量；空串表示清除 IPv6 登记。
    pub host_v6: String,
    pub port: i64,
    pub user: String,
    pub auth_kind: String,
    pub kind: String,
    pub notes: String,
    pub credential: Option<String>,
    /// 出站地址族；None = 保留原值（兼容只改其它字段的调用方）。
    pub egress_family: Option<String>,
    /// 分享域名；None = 保留原值，Some("") = 清除。
    pub share_domain: Option<String>,
}

/// 更新服务器的结果：保存一定成功才返回；出口变化且该服务器已有部署时，
/// 后端会自动排一次重部署，其结果通过 `deploy_job_id` / `deploy_error` 反馈。
/// 部署提交失败**不回滚保存**——保存是事实，部署只是让新配置生效的后续动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateServerResult {
    pub server: Server,
    pub deploy_job_id: Option<i64>,
    pub deploy_error: Option<String>,
}

/// 部署任务输入（身份材料由工具生成，无密码项）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeployInput {
    pub server_id: i64,
    pub protocols: Vec<String>,
    pub reality_dest: Option<String>,
    /// 缺省 false。Tauri 的 Option 参数在 key 缺失时走 visit_none()，不报错
    /// （tauri-2.11.5/src/ipc/command.rs:134-145 已核实）
    pub enable_bbr: Option<bool>,
}

/// 上网启动选项：ignore_conflicts 跳过冲突扫描（注意仍会扫描端口占用）；
/// conflict_override 驱动冲突列表（生产 None = 真实进程扫描，前端仅测试用）。
#[derive(Debug, Clone)]
pub struct NetStartInput {
    pub ignore_conflicts: bool,
    pub conflict_override: Option<Vec<String>>,
}

/// job 进度事件负载（event `job://progress`），kind 区分 deploy/link/peer。
#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {
    pub job_id: i64,
    pub kind: String,
    pub step: String,
    pub message: String,
    pub progress: i64,
    pub error: Option<String>,
}

/// 建链任务请求（jobs 行已由命令层建立并落 kind='link'）。
#[derive(Debug, Clone)]
pub struct LinkBuildReq {
    pub job_id: i64,
    pub name: String,
    pub relay_server_id: i64,
    pub landing_server_id: i64,
    pub client_mode: String,
    pub wg_port: u16,
    pub forward_port: Option<u16>,
    pub landing_port: u16,
    pub converged: bool,
    pub action: String,
    /// 转发粒度：Some('same_port') = 整机转发——该落地机每个自建节点端口按原端口号
    /// 转发出去，一条链路承载整台落地机的多协议；Some('single') = 显式要单端口。
    /// **None = 未指定**：已有规则沿用原粒度、新建回落 'single'——重试/老调用方
    /// 不带这个字段时不能把已有 same_port 链路悄悄降级（实测踩过）。
    pub forward_mode: Option<String>,
}

/// 测速任务请求（jobs 行已由命令层建立并落 kind='speed'）。
#[derive(Debug, Clone)]
pub struct SpeedTestReq {
    pub job_id: i64,
    pub node_ids: Vec<String>,
    /// 并发窗口 1..=5（命令层已校验，执行器按块并发）
    pub concurrency: u8,
}

/// 拆链任务请求（jobs 行已由命令层建立并落 kind='link'）。
#[derive(Debug, Clone)]
pub struct LinkTeardownReq {
    pub job_id: i64,
    pub rule_id: i64,
    /// 'unregister' = 解除登记（纯 DB，仅 imported 行）；
    /// 'full' = 真实拆除（built/failed 走 wg0 全序列；imported 按公钥定位实际接口拆除）
    pub action: String,
}

/// 设备 peer 增删任务请求（jobs 行已由命令层建立并落 kind='peer'）。
#[derive(Debug, Clone)]
pub struct PeerAddReq {
    pub job_id: i64,
    pub peer_id: i64,
}

/// 设备 peer 删除请求：rule_id 供定位中转机，public_key 供段级删除。
#[derive(Debug, Clone)]
pub struct PeerRemoveReq {
    pub job_id: i64,
    pub rule_id: i64,
    pub public_key: String,
}

/// 中转链路规则：一对中转/落地服务器的 WG 建链配置与状态。
#[derive(Debug, Clone, Serialize)]
pub struct RelayRule {
    pub id: i64,
    pub name: String,
    pub relay_server_id: i64,
    pub landing_server_id: i64,
    /// 'forward'（forward 模式）或 'peer'（peer 模式）
    pub client_mode: String,
    pub wg_port: i64,
    pub forward_port: Option<i64>,
    pub landing_port: i64,
    pub subnet: String,
    pub relay_wg_ip: String,
    pub landing_wg_ip: String,
    /// 两端 WG 公钥（公开材料）：状态查询/拆链按公钥定位 peer 段，不依赖接口名；历史行为 NULL
    pub relay_pubkey: Option<String>,
    pub landing_pubkey: Option<String>,
    pub converged: bool,
    /// 'not-built' / 'building' / 'built'
    pub status: String,
    /// 'single'：一个落地端口一条链路（可重映射入口端口，既有语义）；
    /// 'same_port'：整机转发——该落地机的**每个自建节点端口按原端口号**转发出去，
    /// 一条链路承载整台落地机的多协议（v12 迁移新增，存量行默认 single）
    pub forward_mode: String,
}

/// 设备 peer（出 IPC 的瘦类型）：不含 private_key/psk 密封字段。
#[derive(Debug, Clone, Serialize)]
pub struct DevicePeer {
    pub id: i64,
    pub rule_id: i64,
    pub name: String,
    pub public_key: String,
    pub wg_ip: String,
    pub created_at: String,
}

/// 设备 peer 完整行（含 sealed 字段），仅供仓库层内部与测试使用。
#[derive(Debug, Clone, Serialize)]
pub struct DevicePeerRow {
    pub id: i64,
    pub rule_id: i64,
    pub name: String,
    pub public_key: String,
    pub private_key_b64: String,
    pub psk_b64: String,
    pub wg_ip: String,
    pub created_at: String,
}

/// 一条 DNAT 转发规则（回读自中转机 `iptables -t nat -S PREROUTING`）：
/// 入口端口 → 隧道内的目标地址与端口。
///
/// 接管表单的「落地端口」就取自这里的 `target_port`——DNAT 行里本来就写着，
/// 回读半截（只读 `--dport`）会逼用户手填，而填错的表现是接管成功但
/// 「挂中转」永远匹配不上节点（匹配按落地端口对端口做）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DnatPair {
    pub forward_port: i64,
    pub target_ip: String,
    pub target_port: i64,
}

/// 建链前探测结果：两端可达性与 WG 公钥材料，message 为可展示原因（脱敏）。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub relay_reachable: bool,
    pub landing_reachable: bool,
    pub relay_wg: bool,
    pub landing_wg: bool,
    pub handshake: bool,
    pub relay_pubkey: String,
    pub landing_pubkey: String,
    /// 命中接口的真实 IPv4 地址（`ip -o addr show` 解析；P5.1：adopt/readopt
    /// 落库用，不再硬凑 {prefix}.1/.2 合成值）
    pub relay_wg_ip: Option<String>,
    pub landing_wg_ip: Option<String>,
    /// 探测命中的 WG 接口名（任意名称，如 wg0/wg1），供展示与接管默认值
    pub wg_iface: Option<String>,
    /// 检测到 Tailscale 接口（不在本工具管理范围，仅提示）
    pub tailscale: bool,
    pub wg_port: Option<i64>,
    pub subnet: Option<String>,
    pub forward_port: Option<i64>,
    /// 中转机 DNAT 表（原样回读，不预筛）：接管表单的转发端口/落地端口
    /// 都从这张表选，改一个另一个跟着走
    pub dnat_pairs: Vec<DnatPair>,
    pub message: String,
}

/// 链路状态：两端握手与流量，checked_at 为探测时刻，error 为探测失败原因。
#[derive(Debug, Clone, Serialize)]
pub struct LinkStatus {
    pub ok: bool,
    pub relay_handshake_secs_ago: Option<i64>,
    pub landing_handshake_secs_ago: Option<i64>,
    pub relay_rx_bytes: i64,
    pub relay_tx_bytes: i64,
    pub landing_rx_bytes: i64,
    pub landing_tx_bytes: i64,
    pub checked_at: String,
    pub error: Option<String>,
}

/// 建链输入：action ∈ build/rebuild，前端参数经 camelCase→snake_case 转换后经 IPC 传入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildLinkInput {
    pub name: String,
    pub relay_server_id: i64,
    pub landing_server_id: i64,
    pub client_mode: String,
    pub wg_port: u16,
    pub forward_port: Option<u16>,
    pub landing_port: u16,
    pub converged: bool,
    pub action: String,
    /// 见 LinkBuildReq::forward_mode：None = 未指定（沿用已有规则 / 回落 single）
    #[serde(default)]
    pub forward_mode: Option<String>,
}

/// 接管（adopt）输入：对已存在的中转链路补充规则，subnet 由探测结果提供。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptLinkInput {
    pub name: String,
    pub relay_server_id: i64,
    pub landing_server_id: i64,
    pub client_mode: String,
    pub wg_port: u16,
    pub forward_port: Option<u16>,
    /// 两端公钥（探测回读，公开材料）：入库后状态查询/拆链按公钥定位
    pub relay_pubkey: Option<String>,
    pub landing_pubkey: Option<String>,
    pub landing_port: u16,
    pub subnet: String,
    pub converged: bool,
}

/// 导出候选节点：`exportable=false` 时 `skip_reason` 说明为什么导不出去。
/// 目前唯一不可导出的情况是 ssr——mihomo 与 sing-box 均已移除 SSR 支持，
/// 静默剔除会让用户以为「这条线上传不了」却找不到原因。
#[derive(Debug, Clone, Serialize)]
pub struct ExportableNode {
    pub node: Node,
    pub exportable: bool,
    pub skip_reason: Option<String>,
}

/// 上游出口：中转机上专用 Xray 实例的一个「入站 + 出站」配对，出站指向买来的
/// 静态住宅 IP 代理。**刻意不含 password 字段**——该结构体经 IPC 序列化给前端，
/// 凭据只能经 `repos::upstreams::secrets` 在服务端取出即用即弃。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Upstream {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    /// "socks5" | "http"
    pub proto: String,
    pub host: String,
    pub port: i64,
    pub username: String,
    /// 旧专用实例的入站端口（客户端节点连的就是它）。复用主入口时恒为 0；
    /// 复用入口的真实端口见 `entry_port`。
    pub listen_port: i64,
    /// REALITY 伪装域名（仅旧专用实例使用；复用主入口时为空）。
    pub reality_dest: String,
    /// 复用入口的主节点 id（`nodes.id`）。`None` = 旧专用实例或尚未部署。
    /// 仅用于展示/失效检测；渲染按 `entry_proto` 匹配主配置里的入站。
    pub entry_node_id: Option<String>,
    /// 复用入口的协议（vless/trojan/vmess/hysteria2/tuic）。空串 = 旧专用实例。
    pub entry_proto: String,
    /// 复用入口当前的端口。0 = 该协议尚未部署，下一次部署会补上并回写这里。
    pub entry_port: i64,
    /// 写回的节点 id（拆除时按它删，不能用 protocol+addr 定位——同一中转机上
    /// 多条出口的节点协议与地址都相同，会互相误删）。
    pub node_id: Option<String>,
    /// 最近一次探测到的出口 IP。
    pub egress_ip: Option<String>,
    /// 最近一次失败原因（脱敏）。
    pub last_check_error: Option<String>,
    pub created_at: String,
}

/// 上游出口「入口协议」下拉里的一项：服务器上已经部署、可直接复用的主入站。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpstreamEntryOption {
    /// 主节点 id（`nodes.id`），选中后作为 `upstreams.entry_node_id`
    pub node_id: String,
    pub protocol: String,
    pub port: u16,
    /// 与「已部署协议」一致的人类可读变体名（VLESS + REALITY + Vision 等）
    pub label: String,
}

/// 入口协议选项：`deployed` 是可直接复用的主入站；`supported` 是支持复用、
/// 但当前尚未部署的协议（选中 = 先部署再复用）。SS 不在 `supported` 里：
/// 它由独立的 ssserver 承载，没有按用户分流的能力，无法把出口接到住宅代理。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpstreamEntryOptions {
    pub deployed: Vec<UpstreamEntryOption>,
    pub supported: Vec<String>,
}

/// 隐私保护：**某条既有线路的 Tor 入口**（开关，不是新节点）。
///
/// 链路：客户端 → 本机 Tor(9050) → Tor 网络 → `<onion>:<port>` →
/// `{fwd_host}:{fwd_port}`（**这条线原本的入口送不到的那一站**）→ 原样走它本来的出站。
///
/// 「原本的入口」由 [`crate::config::effective_node`] 决定，所以隐藏服务的落点也由它决定：
/// - 直连线：入口就是那台机器自己，`127.0.0.1:{node.port}` 正在听；
/// - 中转 forward：入口是**中转机**的转发端口，DNAT 把它送进隧道到
///   `{landing_wg_ip}:{landing_port}`——所以隐藏服务开在中转机上、转发到隧道目标。
///   （中转机的 `127.0.0.1:{入口端口}` 没有在听：DNAT 挂在 PREROUTING 上，只对从
///   eth0 进来的包生效，本机产生、发往回环的流量走的是 OUTPUT。）
///
/// 所以：Xray 配置不动、协议栈不动、出口不用选——「开哪条线就是哪条线」。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Privacy {
    pub id: i64,
    /// **隐藏服务部署在哪台机器上**（直连线＝落地机自己，forward 中转线＝中转机）。
    pub server_id: i64,
    /// 被保护的那条线的节点 id。开关按它定位，**不按 protocol+addr**
    /// （同机多条线的协议与地址相同，会互相误删）。
    pub node_id: String,
    pub name: String,
    /// onion 侧监听的端口 = 客户端配置里那个 `port`（`effective_node` 算出来的入口端口）。
    pub port: i64,
    /// 隐藏服务转发目标。直连线是 `127.0.0.1:{port}`；中转线是隧道目标
    /// `{landing_wg_ip}:{landing_port}`——收敛会把落地机的入站监听地址从 0.0.0.0
    /// 改成隧道地址，那台机器上的 `127.0.0.1:{port}` 此时没有在听。
    pub fwd_host: String,
    pub fwd_port: i64,
    /// 本条独享的 Tor HiddenServiceDir；删除时按它精确清理，不碰别的 onion 身份。
    pub tor_dir: String,
    /// `.onion` 地址（v3，56 字符）。部署中断未回读到时为空串。
    pub onion_host: String,
    /// 最近一次失败原因（脱敏）。
    pub last_check_error: Option<String>,
    pub created_at: String,
}
