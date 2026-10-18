// API 类型定义：与 Rust 端 domain/commands 的序列化结构一一对应。
// 字段名对齐 Rust serde 线格式（snake_case）：Rust 结构体未声明
// #[serde(rename_all)]，命令响应按声明名序列化；Tauri 的 camelCase→
// snake_case 转换只作用于 invoke 参数，不作用于响应。

/** 本机上网客户端状态（net_status_cmd 返回） */
/**
 * 提权网络服务的安装与运行状态。
 *
 * 两个独立布尔而非一个枚举：设置页要按四象限给不同入口——没装给「安装」，
 * 装了没起给「启动/卸载」，装了在跑给「卸载」。
 */
export interface ServiceState {
  installed: boolean
  running: boolean
}

export interface NetStatus {
  running: boolean
  /** 运行中的 mixed 端口（系统代理指向），未运行 null */
  port: number | null
  /** 检测到的冲突代理进程名（开启前扫描结果，运行中为空） */
  conflicts: string[]
  /** 分流数据（geoip/geosite）未就绪 → 客户端降级为仅国内直连兜底 */
  degraded_geo: boolean
  /**
   * mihomo 是否由提权服务托管。false = 主程序直接 spawn 的回退路径（服务未装/
   * 未启动）。TUN 开关据此判断可用性——没有服务就没有提权通道。
   */
  via_service: boolean
  /**
   * TUN 全局模式是否开启。答案来自服务（它是唯一知道 mihomo 实际配置的一方），
   * 主程序不自己记账。
   */
  tun: boolean
  /**
   * 会话级阻断（防火墙）的状态。
   *
   * **这是增益功能的状态，不是网络能不能用的状态**：任何非 `applied` 的取值都不
   * 表示「开启网络」失败了，它只说明「非 TUN 下关注程序的公网直连没被挡住」。
   */
  firewall: FirewallState
  /** 为什么不是 applied——给用户看的原话，说明直接展示 */
  firewall_detail: string | null
  /**
   * 这次「开启网络」接管了几条**开启前就已建立**的直连（系统代理只管新连接，
   * 那些既有的 socket 得掐断才会改用代理重连）。
   *
   * `null` = 这次没做（原因见 `takeover_detail`）；`0` = 做了但没有要接管的（常态）。
   * 可选：后端比前端旧时这两个字段整个不存在，界面据 `undefined` 当作没这回事。
   */
  takeover_killed?: number | null
  /** 为什么没接管/没接管完——给用户看的原话 */
  takeover_detail?: string | null
  /** 本机 Tor 客户端状态。旧版后端可能不返回。 */
  tor?: 'off' | 'starting' | 'ready' | 'external' | 'failed'
  tor_detail?: string | null
}

/**
 * 会话级防火墙阻断的状态（后端 firewall::FirewallState，serde lowercase 直出）。
 *
 * - `off` 没开网络，或本次会话没有要挡的程序
 * - `applied` 按名单装齐了
 * - `partial` 只装上一部分（列在 firewall_detail 里：多半是程序没在跑，拿不到
 *   绝对路径——**绝不猜安装路径**，猜错写出的规则永远匹配不上，界面却会显示已阻断）
 * - `denied` 防火墙拒写（组策略禁本地规则/安全软件锁了防火墙），网络照常可用
 * - `unavailable` 服务不可达，或服务是旧构建不认识这条命令（需重装服务）
 * - `unsupported` 非 Windows
 */
export type FirewallState =
  | 'off'
  | 'applied'
  | 'partial'
  | 'denied'
  | 'unavailable'
  | 'unsupported'

/** 「清除残留阻断规则」的结果（net_firewall_clear_cmd 返回） */
export interface FirewallClearResult {
  /** 回读核验后实际消失的规则条数（0 = 本来就没有残留） */
  removed: number
}

/**
 * 一条连接的裁决（后端 connlog::Verdict，serde snake_case 直出）。
 *
 * `suspect` 与 `leak` 必须分开显示：前者是「拿不准」，后者是「有证据」。
 * 把疑点也标成泄漏，用户很快就会学会无视整个面板。
 *
 * `direct_cn` 与 `leak` 更要分开：前者是「目标在国内、规则本就让它直连」
 * （GEOIP,CN / GEOSITE,cn），**不是泄漏**。现场的 5 条「泄漏」里 4 条是这种。
 *
 * `proxy_egress` 是**本机代理拨号到自己节点**的那条连接（隧道自身的出站）。
 * 它也不能并进 `direct`：`direct` 是「进了 mihomo、有规则判它直连」，有规则可查；
 * 这一档压根没进 mihomo。现场单次采样里它有 26 条，是面板上最没信息量的一档。
 */
export type ConnVerdict =
  | 'proxied'
  | 'direct'
  | 'direct_cn'
  | 'proxy_egress'
  | 'leak'
  | 'suspect'

/** 运行时连接日志的一行（net_conn_log_cmd / net://conn_log） */
export interface ConnRow {
  /** 镜像名（只有文件名）。OS 侧拿不到进程时为空串，界面渲染 — */
  process: string
  pid: number
  /**
   * 域名：经 mihomo 的行是它报的 host（权威）；直连行尽力查系统 DNS 缓存得到，
   * 拿不到就空。**不做反向 DNS**——慢、会被污染、解出来的还可能是别的站点的名字。
   */
  domain: string
  /** 'mihomo'（权威）/ 'dns-cache'（尽力而为）/ 'none'（没有，渲染 —） */
  domain_source: string
  /** 目标 IP 的 GeoIP 标签（可能是 ISO 码，也可能是 GOOGLE 等组织标签；查不到为空） */
  country: string
  resolved_ip: string
  dest_port: number
  /** 'IPv4' / 'IPv6' */
  family: string
  /** 'TCP' / 'UDP' */
  proto: string
  /** 匹配到的规则；直连行是 —（没经过 mihomo，谈不上匹配了什么规则） */
  matched_rule: string
  /** 出口链（如 `大美 → 手动选择`）；直连行是 — */
  outbound: string
  /** 我们**拨号**的端点（如 203.0.113.127:4443），不是公网实际出口 IP */
  proxy_endpoint: string
  upload: number
  download: number
  start: string | null
  verdict: ConnVerdict
  /** 为什么这么判——判据要能自证，否则用户没法确认也没法反驳 */
  reason: string
}

/**
 * 连接日志快照。行与采样状态必须一起给：只给一个数组的话，面板分不清
 * 「没有可疑连接」和「采样根本没在跑」，而这两件事的处置完全相反。
 */
export interface ConnLogSnapshot {
  rows: ConnRow[]
  sampling: boolean
  /** 采样自杀的原因（连续查询失败）；null = 正常 */
  last_error: string | null
  sampled_at: string | null
  /** 当前的泄漏关注名单（镜像名），面板展示给用户看「在盯谁」 */
  watch: string[]
  /**
   * 本次会话（＝当前 mihomo 进程）经 mihomo 的累计上下行，单位字节。
   * **只统计经过 mihomo 的流量**——绕过它的泄漏流量它看不见，文案必须写清楚。
   */
  upload_total: number
  download_total: number
}

export interface Subscription {
  id: string
  name: string
  url: string
  enabled: boolean
  last_sync_at: string | null
  sync_interval: number
  last_sync_error: string | null
  filter_json: string
}

export interface Node {
  id: string
  name: string
  source: 'import' | 'self-built' | 'manual'
  protocol: string
  addr: string
  port: number
  params_json: string
  group_name: string
  sub_id: string | null
  relay_rule_id: number | null
  enabled: boolean
  latency: number | null
  bandwidth: number | null
  last_checked_at: string | null
  last_check_error: string | null
  /** 「直连形态」延迟（ms）：仅挂了中转的节点会额外测，与 latency（经中转）同口径可直接相减；未测为 null */
  direct_latency: number | null
  /** 分跳裸 TCP RTT（ms，各 1 个往返，仅挂中转的节点会测）：本机→中转机。
   *  与 latency 口径不同（后者是经代理的完整 HTTP 往返），三者相加 ≠ latency */
  hop_relay_ms: number | null
  /** 分跳：本机→落地机 */
  hop_landing_ms: number | null
  /** 分跳：中转机→落地机（经 WireGuard 隧道） */
  hop_tunnel_ms: number | null
}

export interface SyncOutcome {
  upserted: number
  removed: number
}

/** 配置导出结果：sing-box 的 skipped_ssr 也计入跳过的 Tor 隐私线路。 */
export interface ExportOutcome {
  path: string
  skipped_ssr: number
}

/** 导出候选节点：exportable=false 时 skip_reason 说明原因（目前仅 ssr） */
export interface ExportableNode {
  node: Node
  exportable: boolean
  skip_reason: string | null
}

/** 一份已安装到服务器上的分享链接（状态实读自服务器，本地不存记录） */
export interface ShareInfo {
  server_id: number
  server_name: string
  host: string
  token: string
  port: number
  /** http://<host>:<port>/sub/<token> */
  url: string
  /** 快照里的节点名（解析不出时为空数组） */
  node_names: string[]
  active: boolean
}

/**
 * 分享列表 + 读取失败的服务器名。
 * 分开返回是刻意的：某台连不上时无法知道它上面有哪些分享，列表里就少几条——
 * 不点名的话，用户会以为发出去的链接凭空消失了。
 */
export interface ShareList {
  shares: ShareInfo[]
  unreachable: string[]
}

/** 应用更新信息：check 到的新版本与下载安装入口；downloadAndInstall 成功后由 updater 自动重启应用 */
export interface UpdateInfo {
  version: string
  body?: string
  downloadAndInstall(): Promise<void>
}

export type SettingsMap = Record<string, string>

export interface Server {
  id: number
  name: string
  host: string
  /** 可选 IPv6 字面量（不带方括号）；空串表示未登记。 */
  host_v6: string
  /** 当前选用的地址族：v4 用 host，v6 用 host_v6。 */
  addr_family: 'v4' | 'v6'
  /** 服务器出口地址族：VPS → 目标域名时优先/限定哪一族。
   *  与 `addr_family` 完全独立；改动后需重部署才在 VPS 上生效。 */
  egress_family: 'v4' | 'v6'
  port: number
  user: string
  auth_kind: 'password' | 'key'
  kind: 'relay' | 'landing'
  system: string
  notes: string
  created_at: string
  latency: number | null
  bandwidth: number | null
  last_checked_at: string | null
  last_check_error: string | null
}

/** 更新服务器结果：保存本身总是成功；出口族变化时后端自动排的重部署
 *  作为「后续动作」单独返回，提交失败只进 `deploy_error`，不回滚保存。 */
export interface UpdateServerResult {
  server: Server
  /** 已排队的重部署任务 id；未触发（出口未变/未部署任何协议）为 null */
  deploy_job_id: number | null
  /** 排重部署失败的原因；成功或未触发为 null */
  deploy_error: string | null
}

export interface ConnectionTest {
  ok: boolean
  system: string
  message: string
}

export interface Job {
  id: number
  kind: string
  status: 'queued' | 'running' | 'done' | 'failed' | 'cancelled'
  progress: number
  error: string | null
  created_at: string
}

export interface JobProgress {
  job_id: number
  /** 后端 jobs 表的 kind 列（jobs/mod.rs:102-110）。`upstream` 一度漏在这张表里，
   *  而事件确实带它——夹具写成 object 时类型没查出来，漏的是类型不是实现。 */
  kind: 'deploy' | 'upstream' | 'link' | 'peer' | 'speed' | 'server_speed' | 'privacy'
  step: string
  message: string
  progress: number
  error: string | null
}

// ---------- 服务器测速 ----------

export interface ThroughputTest {
  id: number
  from_server_id: number
  to_server_id: number
  bits_per_second: number | null
  checked_at: string
  error: string | null
  /** P5.7：失败时完整客户端 stdout/stderr（8KB 截断），成功为 null；UI 不展示，仅落库供诊断 */
  full_output: string | null
}

/** 部署协议白名单(与后端 servers::deploy::PROTOCOLS 一致) */
export type DeployProtocol = 'vless' | 'trojan' | 'vmess' | 'ss' | 'hysteria2' | 'tuic'

/** 已部署协议详情：label 是完整变体名（如 VLESS + REALITY + Vision），不是裸协议名 */
export interface DeployedProtocolDetail {
  protocol: DeployProtocol
  port: number
  label: string
  transport: string
  security: string
  flow: string | null
}

export interface DeployInput {
  serverId: number
  protocols: DeployProtocol[]
  realityDest?: string
  /** 部署成功后顺手开启 BBR（失败不影响部署结论） */
  enableBbr?: boolean
}

// ---------- 中转链路 ----------

export interface RelayRule {
  id: number
  name: string
  relay_server_id: number
  landing_server_id: number
  client_mode: 'forward' | 'peer'
  wg_port: number
  forward_port: number | null
  landing_port: number
  subnet: string
  relay_wg_ip: string
  landing_wg_ip: string
  relay_pubkey: string | null
  landing_pubkey: string | null
  converged: boolean
  status: 'not-built' | 'building' | 'built' | 'imported' | 'failed'
  /**
   * 转发粒度。'single'（默认）= 一条链路转发一个端口，可把入口端口重映射到别的落地端口；
   * 'same_port' = 整机转发，按**原端口号**覆盖这台落地机的全部自建节点端口
   * （后端在建链时读一次 nodes 派生端口表，不落库）。
   */
  forward_mode: 'single' | 'same_port'
}

export interface DevicePeer {
  id: number
  rule_id: number
  name: string
  public_key: string
  wg_ip: string
  created_at: string
}

/** 一条 DNAT 转发规则（回读自中转机）：入口端口 → 隧道内的目标地址与端口。 */
export interface DnatPair {
  forward_port: number
  target_ip: string
  target_port: number
}

export interface ProbeResult {
  relay_reachable: boolean
  landing_reachable: boolean
  relay_wg: boolean
  landing_wg: boolean
  handshake: boolean
  relay_pubkey: string
  landing_pubkey: string
  /** 命中接口的真实 IPv4 地址（接管落库用；DNAT 表的归属按它认） */
  relay_wg_ip: string | null
  landing_wg_ip: string | null
  wg_iface: string | null
  tailscale: boolean
  wg_port: number | null
  subnet: string | null
  forward_port: number | null
  /** 中转机 DNAT 表（原样回读）：转发端口/落地端口的自动预填都从这里选 */
  dnat_pairs: DnatPair[]
  message: string
}

export interface LinkStatus {
  ok: boolean
  relay_handshake_secs_ago: number | null
  landing_handshake_secs_ago: number | null
  relay_rx_bytes: number
  relay_tx_bytes: number
  landing_rx_bytes: number
  landing_tx_bytes: number
  checked_at: string
  error: string | null
}

export interface Channel {
  id: number
  server_id: number
  name: string
  port: number
  protocol: string
  created_at: string
}

export interface ChannelStatus {
  running: boolean
  detail: string
}

export interface ServerPort {
  port: number
  process: string
}

/** 隐私保护：**某条既有线路的 Tor 入口**（开关，不是新节点）。
 *
 *  链路：客户端 → 本机 Tor → Tor 网络 → `<onion>:<port>` → `{fwd_host}:{fwd_port}`
 *  → 那条线本来的出口。客户端配置里只是把 `server` 换成 onion，协议栈一个字不动。
 *
 *  隐藏服务的落点跟着**这条线的入口**走：直连线开在落地机自己身上，挂了中转就开在
 *  中转机上（客户端连的入口是它）。 */
export interface Privacy {
  id: number
  /** **隐藏服务部署在哪台机器上**（直连线＝落地机自己，forward 中转线＝中转机）。 */
  server_id: number
  node_id: string
  name: string
  /** onion 侧监听的端口 = 客户端配置里的 `port`。 */
  port: number
  fwd_host: string
  fwd_port: number
  tor_dir: string
  /** `.onion` 地址（v3，56 字符）。**空串 = 部署中断、尚未生效**，那条线照原样走。 */
  onion_host: string
  last_check_error: string | null
  created_at: string
}

/** 上游出口：中转机上指向静态住宅 IP 代理的一个出口。
 *  注意**不含密码**——凭据只在后端，经 IPC 拿不到。 */
export interface Upstream {
  id: number
  server_id: number
  name: string
  /** 'socks5' | 'http' */
  proto: string
  host: string
  port: number
  username: string
  /** 中转机上本实例的入站端口（客户端节点连的就是它） */
  listen_port: number
  reality_dest: string
  node_id: string | null
  /** 最近一次探测到的出口 IP */
  egress_ip: string | null
  last_check_error: string | null
  created_at: string
}
