// Tauri 能力封装：IPC 命令与插件 API 的类型化入口，视图层只依赖本模块
import { getVersion } from '@tauri-apps/api/app'
import { invoke } from '@tauri-apps/api/core'
import { check } from '@tauri-apps/plugin-updater'
import type {
  Channel,
  ChannelStatus,
  ConnectionTest,
  ConnLogSnapshot,
  DeployedProtocolDetail,
  DeployInput,
  DeployProtocol,
  DevicePeer,
  ExportableNode,
  ExportOutcome,
  FirewallClearResult,
  Job,
  LinkStatus,
  MonitorAlert,
  MonitorConfig,
  MonitorMetric,
  MonitorPoint,
  MonitorSummary,
  NetStatus,
  Node,
  Privacy,
  ProbeResult,
  ServiceState,
  RelayRule,
  Server,
  ServerPort,
  SettingsMap,
  ShareInfo,
  ShareList,
  Subscription,
  SyncOutcome,
  ThroughputTest,
  UpdateInfo,
  UpdateServerResult,
  Upstream,
} from './types'

export function listSubscriptions(): Promise<Subscription[]> {
  return invoke('list_subscriptions_cmd')
}

export function createSubscription(input: {
  name: string
  url: string
  enabled: boolean
  filterJson: string
}): Promise<Subscription> {
  return invoke('create_subscription_cmd', { ...input })
}

export function updateSubscription(input: {
  id: string
  name: string
  url: string
  enabled: boolean
  filterJson: string
}): Promise<Subscription> {
  return invoke('update_subscription_cmd', { ...input })
}

export function deleteSubscription(id: string): Promise<void> {
  return invoke('delete_subscription_cmd', { id })
}

export function syncSubscription(id: string): Promise<SyncOutcome> {
  return invoke('sync_subscription_cmd', { id })
}

export function listNodes(): Promise<Node[]> {
  return invoke('list_nodes_cmd')
}

export function setNodeEnabled(id: string, enabled: boolean): Promise<void> {
  return invoke('set_node_enabled_cmd', { id, enabled })
}

/** 节点「直连/中转」开关（后端按已建成规则自动挂/摘；无可用规则时报错） */
export function toggleNodeRelay(id: string): Promise<void> {
  return invoke('toggle_node_relay_cmd', { id })
}

/** 仅更换 REALITY 伪装域名（秒级，不重部署）：服务器 xray 就地改写+重启+回写节点 sni */
export function changeRealityDest(serverId: number, dest: string): Promise<void> {
  return invoke('change_reality_dest_cmd', { serverId, dest })
}

export function deleteNode(id: string): Promise<void> {
  return invoke('delete_node_cmd', { id })
}

/** 单条节点改名。clash_raw 节点的片段 name 由后端同步改写；订阅节点下次同步回原名 */
export function renameNode(id: string, name: string): Promise<void> {
  return invoke('rename_node_cmd', { id, name })
}

export function speedTest(nodeIds: string[], concurrency: number): Promise<number> {
  return invoke('speed_test_cmd', { nodeIds, concurrency })
}

export function getMergedConfig(): Promise<string> {
  return invoke('get_merged_config_cmd')
}

/** 导出合并 YAML：返回保存路径与 ssr 跳过计数；用户在对话框取消时为 null */
export function exportYaml(): Promise<ExportOutcome | null> {
  return invoke('export_yaml_cmd')
}

/** 导出 sing-box config.json：返回保存路径与 ssr 跳过计数；取消时为 null */
export function exportSingbox(): Promise<ExportOutcome | null> {
  return invoke('export_singbox_cmd')
}

/** 导出候选清单：与导出共用后端 visible_nodes，所见即所得 */
export function listExportableNodes(): Promise<ExportableNode[]> {
  return invoke('list_exportable_nodes_cmd')
}

/** 导出选中节点。nodeIds 为空会被后端拒绝，前端不会走到 */
export function exportYamlSelected(nodeIds: string[]): Promise<ExportOutcome | null> {
  return invoke('export_yaml_selected_cmd', { nodeIds })
}

export function exportSingboxSelected(nodeIds: string[]): Promise<ExportOutcome | null> {
  return invoke('export_singbox_selected_cmd', { nodeIds })
}

/** 列出所有服务器上的分享。状态实读自服务器，本地不存分享记录 */
export function listShares(): Promise<ShareList> {
  return invoke('list_shares_cmd')
}

/** 在指定服务器上安装一份分享。nodeIds 为空会被后端拒绝，前端不会走到 */
export function createShare(serverId: number, nodeIds: string[]): Promise<ShareInfo> {
  return invoke('create_share_cmd', { serverId, nodeIds })
}

/** 作废一份分享：停服务 + 删文件 + 撤防火墙规则，各步幂等 */
export function deleteShare(serverId: number, token: string): Promise<void> {
  return invoke('delete_share_cmd', { serverId, token })
}

export function startSubServer(port: number, token: string): Promise<number> {
  return invoke('start_sub_server_cmd', { port, token })
}

export function stopSubServer(): Promise<void> {
  return invoke('stop_sub_server_cmd')
}

export function getSubServerStatus(): Promise<boolean> {
  return invoke('get_sub_server_status_cmd')
}

// ---------- 本机上网 ----------

export function netStatus(): Promise<NetStatus> {
  return invoke('net_status_cmd')
}

/** 当前网络仍在运行但系统代理被关掉时，恢复到原端口；不重启代理进程。 */
export function netRepairProxy(): Promise<void> {
  return invoke('net_repair_proxy_cmd')
}

export function netStart(): Promise<NetStatus> {
  // 冲突扫描默认开（提醒不抢占）：ignoreConflicts 为 tauri 命令必填参数
  return invoke('net_start_cmd', { ignoreConflicts: false, conflictOverride: null })
}

export function netStop(): Promise<NetStatus> {
  return invoke('net_stop_cmd')
}

/**
 * 开关 TUN 全局模式。失败时**抛错而不是静默返回**——TUN 没有降级路径，
 * 建网卡要管理员令牌；静默成功会让界面显示已开启而实际什么都没接管。
 */
export function netSetTun(enable: boolean): Promise<NetStatus> {
  return invoke('net_set_tun_cmd', { enable })
}

/** 提权服务的安装与运行状态（设置页挂载时拉取）。 */
export function netServiceState(): Promise<ServiceState> {
  return invoke('net_service_state_cmd')
}

/**
 * 安装网络服务。**会弹一次 UAC**，且这是全程唯一一次。
 *
 * 返回只代表「提权助手已拉起」，不代表装好了——UAC 窗口是模态的，后端不等它。
 * 调用方要靠轮询 netServiceState 判断结果。
 */
export function netServiceInstall(): Promise<void> {
  return invoke('net_service_install_cmd')
}

/** 卸载网络服务（同样过一次 UAC）。 */
export function netServiceUninstall(): Promise<void> {
  return invoke('net_service_uninstall_cmd')
}

/** 切换出口节点（手动选择组 → 指定节点名；「自动选择」改回测速最快） */
export function netSelect(nodeName: string): Promise<void> {
  return invoke('net_select_cmd', { nodeName })
}

/**
 * 清除残留的会话阻断规则（设置页逃生门）。
 *
 * 残留的 BLOCK 规则是**真会挡住用户网络**的，所以必须有一条不依赖任何自动路径的
 * 手工出口。正常路径上不该有残留（关网络会无条件撤、开网络会自愈上次强杀留下的），
 * 网络运行中会拒绝——那时规则正在生效，清掉等于界面撒谎。
 */
export function netFirewallClear(): Promise<FirewallClearResult> {
  return invoke('net_firewall_clear_cmd')
}

/** 当前手动选择的分组出口名（下拉回显） */
export function netCurrent(): Promise<string> {
  return invoke('net_current_cmd')
}

/**
 * 运行时连接日志 + 泄漏检测快照。
 *
 * 返回的是采样器**当前窗口**，不触发新一轮采样：面板挂载时先拿它铺满列表，
 * 之后靠 `net://conn_log` 事件增量刷新——连接短命，轮询只在「恰好活着」的
 * 瞬间看得到东西，而漏掉的那些连接正是这个功能要找的。
 *
 * @param limit 最多几行（后端已把泄漏排在最前，截断不会切掉它们）
 * @param onlyLeaks 只看有证据的泄漏；疑点（suspect）是另一回事，不混进来
 */
export function netConnLog(limit?: number, onlyLeaks = false): Promise<ConnLogSnapshot> {
  return invoke('net_conn_log_cmd', { limit: limit ?? null, onlyLeaks })
}

export function getSettings(keys: string[]): Promise<SettingsMap> {
  return invoke('get_settings_cmd', { keys })
}

export function setSetting(key: string, value: string): Promise<void> {
  return invoke('set_setting_cmd', { key, value })
}

export function listServers(): Promise<Server[]> {
  return invoke('list_servers_cmd')
}

export function createServer(input: {
  name: string
  host: string
  hostV6: string
  port: number
  user: string
  authKind: 'password' | 'key'
  kind: 'relay' | 'landing'
  notes: string
  credential: string
}): Promise<Server> {
  return invoke('create_server_cmd', { ...input })
}

export function updateServer(input: {
  id: number
  name: string
  host: string
  hostV6: string
  port: number
  user: string
  authKind: 'password' | 'key'
  kind: 'relay' | 'landing'
  notes: string
  credential: string | null
  egressFamily: 'v4' | 'v6'
}): Promise<UpdateServerResult> {
  return invoke('update_server_cmd', { ...input })
}

/** 切换整台服务器（SSH/测速/自建直连节点）的地址族。
 *  首次切到 v6 且未登记地址时，后端会 SSH 到服务器自动探测并登记。 */
export function setServerAddrFamily(id: number, family: 'v4' | 'v6'): Promise<Server> {
  return invoke('set_server_addr_family_cmd', { id, family })
}

/** 只探测不切换：从服务器自身读取全局 IPv6，返回候选地址（推荐值在首位）。 */
export function detectServerIpv6(id: number): Promise<string[]> {
  return invoke('detect_server_ipv6_cmd', { id })
}

export function deleteServer(id: number): Promise<void> {
  return invoke('delete_server_cmd', { id })
}

export function testServerConnection(id: number): Promise<ConnectionTest> {
  return invoke('test_server_connection_cmd', { id })
}

export function setupPasswordless(id: number): Promise<void> {
  return invoke('setup_passwordless_cmd', { id })
}

// ---------- 通道管理 ----------

export function listChannels(serverId: number): Promise<Channel[]> {
  return invoke('list_channels_cmd', { serverId })
}

export function addChannel(input: {
  serverId: number
  name: string
  port: number
  protocol: string
}): Promise<Channel> {
  return invoke('add_channel_cmd', { ...input })
}

export function removeChannel(id: number): Promise<void> {
  return invoke('remove_channel_cmd', { id })
}

export function channelStatus(id: number): Promise<ChannelStatus> {
  return invoke('channel_status_cmd', { id })
}

export function channelSwitch(id: number, enabled: boolean): Promise<ChannelStatus> {
  return invoke('channel_switch_cmd', { id, enabled })
}

export function listUpstreams(serverId: number): Promise<Upstream[]> {
  return invoke('list_upstreams_cmd', { serverId })
}

export function addUpstream(input: {
  serverId: number
  name: string
  proto: string
  host: string
  port: number
  username: string
  password: string
  listenPort: number
  realityDest?: string
}): Promise<Upstream> {
  return invoke('add_upstream_cmd', { ...input })
}

/** 编辑出口（协议/地址/端口/凭据/监听端口/伪装域名）。
 *
 *  返回**实际生效**的记录：监听端口撞车时后端会自动改用空闲端口，所以返回值可能与
 *  传进去的不同，调用方要按返回值回显与提示。
 *  只写库——远端要等 `startUpstreamJob`（重新部署）才切过去。
 *  `password` 省略/传空 = **不改密码**：凭据不回显，表单拿不到原值。 */
export function updateUpstream(
  id: number,
  input: {
    proto: string
    host: string
    port: number
    username: string
    password?: string
    listenPort: number
    realityDest: string
  },
): Promise<Upstream> {
  return invoke('update_upstream_cmd', {
    id,
    ...input,
    // 空串一律当「不改」：后端把 Option::None 与空串都按「不动密码」处理，
    // 但显式传 undefined 更不容易被后来者当成「有意清空」
    password: input.password?.trim() ? input.password : undefined,
  })
}

export function removeUpstream(id: number): Promise<void> {
  return invoke('remove_upstream_cmd', { id })
}

export function startUpstreamJob(serverId: number): Promise<number> {
  return invoke('start_upstream_job_cmd', { serverId })
}

export function probeEgressIp(id: number): Promise<string> {
  return invoke('probe_egress_ip_cmd', { upstreamId: id })
}

/** 全部隐私开关（含**尚未生效**的：`onion_host` 为空 + `last_check_error` 就是界面上的红字）。 */
export function listPrivacies(): Promise<Privacy[]> {
  return invoke('list_privacies_cmd')
}

/** 给某条线开隐私保护（Tor 隐藏服务入口）。返回部署任务的 job id。
 *
 *  记录会先落库、onion 地址等作业回读到才生效——所以「点了但部署失败」的终态是
 *  **仍然直连** + 一行红字，绝不会是「以为在走 Tor 其实直连 VPS」。 */
export function enablePrivacy(serverId: number, nodeId: string): Promise<number> {
  return invoke('enable_privacy_cmd', { serverId, nodeId })
}

/** 关掉隐私保护。返回 `null` = 没提交作业（从未部署成功，记录已直接删掉）。 */
export function disablePrivacy(privacyId: number): Promise<number | null> {
  return invoke('disable_privacy_cmd', { privacyId })
}

export function scanServerPorts(serverId: number): Promise<ServerPort[]> {
  return invoke('scan_server_ports_cmd', { serverId })
}

export function startDeploy(input: DeployInput): Promise<number> {
  return invoke('start_deploy_cmd', { ...input })
}

/** 服务器已部署协议（以本地自建节点为准）。协议管理 P2 里程碑的前置只读接口 */
export function listDeployedProtocols(serverId: number): Promise<string[]> {
  return invoke('deployed_protocols_cmd', { serverId })
}

/** 服务器已部署协议详情：完整变体 + 端口（UI 用这个，不再显示裸协议名） */
export function listDeployedProtocolDetails(
  serverId: number,
): Promise<DeployedProtocolDetail[]> {
  return invoke('deployed_protocol_details_cmd', { serverId })
}

/** 补装协议：现有 ∪ 勾选 提交一次部署任务，返回 jobId */
export function installSupplement(serverId: number, protocols: DeployProtocol[]): Promise<number> {
  return invoke('install_supplement_cmd', { serverId, protocols })
}

/** 卸载协议（骨架：P2 里程碑接入停止 unit + 摘除入站） */
export function removeServerProtocol(serverId: number, protocol: DeployProtocol): Promise<void> {
  return invoke('remove_protocol_cmd', { serverId, protocol })
}

// ---------- 服务器测速 ----------

export function serverSpeed(serverIds: number[], concurrency: number): Promise<number> {
  return invoke('server_speed_cmd', { serverIds, concurrency })
}

export function serverThroughput(fromServerId: number, toServerId: number, seconds: number): Promise<number> {
  return invoke('server_throughput_cmd', { fromServerId, toServerId, seconds })
}

export function installIperf3(serverId: number): Promise<string> {
  return invoke('install_iperf3_cmd', { serverId })
}

export function listThroughput(): Promise<ThroughputTest[]> {
  return invoke('list_throughput_cmd')
}

export function checkBbr(serverId: number): Promise<string> {
  return invoke('check_bbr_cmd', { serverId })
}

export function enableBbr(serverId: number): Promise<string> {
  return invoke('enable_bbr_cmd', { serverId })
}

export function listJobs(): Promise<Job[]> {
  return invoke('list_jobs_cmd')
}

export function cancelJob(jobId: number): Promise<void> {
  return invoke('cancel_job_cmd', { jobId })
}

// ---------- 中转链路 ----------

export function listRelayRules(): Promise<RelayRule[]> {
  return invoke('list_relay_rules_cmd')
}

export function probeLink(relayServerId: number, landingServerId: number): Promise<ProbeResult> {
  return invoke('probe_link_cmd', { relayServerId, landingServerId })
}

export function buildLink(input: {
  name: string
  relayServerId: number
  landingServerId: number
  clientMode: 'forward' | 'peer'
  wgPort: number
  forwardPort: number | null
  landingPort: number
  converged: boolean
  /** 省略即 'single'（后端 default）；'same_port' = 按原端口转发落地机全部自建节点端口 */
  forwardMode?: 'single' | 'same_port'
  action: 'build' | 'rebuild'
}): Promise<number> {
  return invoke('build_link_cmd', { ...input })
}

export function adoptLink(input: {
  name: string
  relayServerId: number
  landingServerId: number
  clientMode: 'forward' | 'peer'
  wgPort: number
  forwardPort: number | null
  relayPubkey: string | null
  landingPubkey: string | null
  landingPort: number
  subnet: string
  converged: boolean
}): Promise<RelayRule> {
  return invoke('adopt_link_cmd', { ...input })
}

export function teardownLink(ruleId: number, action: 'unregister' | 'full'): Promise<number> {
  return invoke('teardown_link_cmd', { ruleId, action })
}

export function readoptLink(ruleId: number): Promise<RelayRule> {
  return invoke('readopt_link_cmd', { ruleId })
}

export function deleteLinkRule(ruleId: number): Promise<void> {
  return invoke('delete_link_rule_cmd', { ruleId })
}

export function refreshLinkStatus(ruleId: number): Promise<LinkStatus> {
  return invoke('refresh_link_status_cmd', { ruleId })
}

export function listDevicePeers(ruleId: number): Promise<DevicePeer[]> {
  return invoke('list_device_peers_cmd', { ruleId })
}

export function addDevicePeer(ruleId: number, name: string): Promise<number> {
  return invoke('add_device_peer_cmd', { ruleId, name })
}

export function removeDevicePeer(peerId: number): Promise<number> {
  return invoke('remove_device_peer_cmd', { peerId })
}

export function getPeerConfig(peerId: number): Promise<string> {
  return invoke('get_peer_config_cmd', { peerId })
}

// ---------- 应用更新 ----------

/** 当前应用版本号：读取 tauri.conf.json 的 version 字段（打包后的真实版本） */
export function getAppVersion(): Promise<string> {
  return getVersion()
}

/** 系统代理探测（更新检查/下载自动跟随系统代理用）：无代理返回 null */
export function getSystemProxy(): Promise<string | null> {
  return invoke('get_system_proxy_cmd')
}

/** 检查更新：无可用更新返回 null；有更新返回版本信息与下载安装入口。
 * 系统存在代理时透传给 updater（其 HTTP 不读系统代理，国内直连 GitHub 超时）。 */
export async function checkForUpdate(): Promise<UpdateInfo | null> {
  const proxy = await getSystemProxy()
  const update = await check(proxy ? { proxy } : undefined)
  if (!update) return null
  return {
    version: update.version,
    body: update.body,
    downloadAndInstall: () => update.downloadAndInstall(),
  }
}

// ---------- 服务器健康监控 ----------

export function monitorSummary(): Promise<MonitorSummary> {
  return invoke('server_monitor_summary_cmd')
}

export function monitorHistory(
  serverId: number,
  metric: MonitorMetric,
  rangeSecs: number,
): Promise<MonitorPoint[]> {
  return invoke('server_monitor_history_cmd', { serverId, metric, rangeSecs })
}

export function monitorAlerts(
  includeResolved: boolean,
  limit = 100,
): Promise<MonitorAlert[]> {
  return invoke('server_monitor_alerts_cmd', { includeResolved, limit })
}

export function monitorCollect(): Promise<MonitorSummary> {
  return invoke('server_monitor_collect_cmd')
}

export function monitorConfig(): Promise<MonitorConfig> {
  return invoke('server_monitor_config_cmd')
}

export function monitorSetConfig(config: MonitorConfig): Promise<MonitorConfig> {
  return invoke('server_monitor_set_config_cmd', { config })
}

export function monitorAck(alertId: number): Promise<void> {
  return invoke('server_monitor_ack_cmd', { alertId })
}
