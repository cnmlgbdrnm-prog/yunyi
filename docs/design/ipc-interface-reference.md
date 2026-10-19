# IPC 接口全表（UI ↔ 后端）

本文列出应用注册的**全部 Tauri 命令**及其 UI 封装，供联调与排障时查表。

- **事实源**：前端 `src/api/index.ts`（94 个封装）↔ 后端
  `src-tauri/src/main.rs` 的 `generate_handler![...]`（94 条命令），当前 **1:1 完整对应**；
  Rust 壳在 `src-tauri/src/commands/ipc.rs`，线格式类型在 `src/api/types.ts` 与
  `src-tauri/src/domain.rs`。
- 页面**不得**直接 `invoke`，只能调用 `src/api/index.ts` 的封装。
- 参数从 JS 侧按 **camelCase** 传入，Tauri 自动转 snake_case；响应按 Rust 字段名
  序列化（snake_case）。
- 错误统一为可读字符串（`AppError` 的 `Display`），前端直接展示。
- 地址族相关接口的详细语义另见
  [`docs/design/server-address-family-interface.md`](server-address-family-interface.md)。

## 1. 订阅与节点

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_subscriptions_cmd` | `listSubscriptions()` | — | `Subscription[]` |
| `create_subscription_cmd` | `createSubscription` | `{ name, url, enabled, filterJson }` | `Subscription` |
| `update_subscription_cmd` | `updateSubscription` | `{ id, name, url, enabled, filterJson }` | `Subscription` |
| `delete_subscription_cmd` | `deleteSubscription(id)` | `{ id }` | `void` |
| `sync_subscription_cmd` | `syncSubscription(id)` | `{ id }` | `SyncOutcome { upserted, removed }` |
| `list_nodes_cmd` | `listNodes()` | — | `Node[]` |
| `set_node_enabled_cmd` | `setNodeEnabled(id, enabled)` | `{ id, enabled }` | `void` |
| `toggle_node_relay_cmd` | `toggleNodeRelay(id)` | `{ id }` | `void`；无可用规则时报错 |
| `change_reality_dest_cmd` | `changeRealityDest(serverId, dest)` | `{ serverId, dest }` | `void`；就地改写 + 重启 + 回写节点 sni |
| `delete_node_cmd` | `deleteNode(id)` | `{ id }` | `void` |
| `rename_node_cmd` | `renameNode(id, name)` | `{ id, name }` | `void`；订阅节点下次同步会回原名 |
| `speed_test_cmd` | `speedTest(nodeIds, concurrency)` | `{ nodeIds, concurrency }` | `job_id`（kind `speed`） |
| `get_merged_config_cmd` | `getMergedConfig()` | — | `string`（合并后的 YAML） |
| `export_yaml_cmd` | `exportYaml()` | — | `ExportOutcome \| null`（取消为 null） |
| `export_singbox_cmd` | `exportSingbox()` | — | `ExportOutcome \| null` |
| `list_exportable_nodes_cmd` | `listExportableNodes()` | — | `ExportableNode[]`（含跳过原因） |
| `export_yaml_selected_cmd` | `exportYamlSelected(nodeIds)` | `{ nodeIds }` | `ExportOutcome \| null`；空列表被拒 |
| `export_singbox_selected_cmd` | `exportSingboxSelected(nodeIds)` | `{ nodeIds }` | `ExportOutcome \| null` |

## 2. 服务器管理

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_servers_cmd` | `listServers()` | — | `Server[]` |
| `create_server_cmd` | `createServer` | `{ name, host, hostV6, port, user, authKind, kind, notes, credential }` | `Server`；出口族固定 v4 |
| `update_server_cmd` | `updateServer` | 同上 + `{ id, credential: string \| null, egressFamily: 'v4' \| 'v6' }` | `UpdateServerResult`（保存成功；自动重部署单独返回） |
| `set_server_addr_family_cmd` | `setServerAddrFamily(id, family)` | `{ id, family: 'v4' \| 'v6' }` | `Server`；必要时自动探测 IPv6 |
| `detect_server_ipv6_cmd` | `detectServerIpv6(id)` | `{ id }` | `string[]`（推荐在首位，不落库） |
| `delete_server_cmd` | `deleteServer(id)` | `{ id }` | `void`；有关联记录时被拒 |
| `test_server_connection_cmd` | `testServerConnection(id)` | `{ id }` | `ConnectionTest { ok, system, message }` |
| `setup_passwordless_cmd` | `setupPasswordless(id)` | `{ id }` | `void`；写入公钥并切换密钥认证 |

## 3. 部署与协议

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `start_deploy_cmd` | `startDeploy(input)` | `DeployInput { serverId, protocols[], realityDest?, enableBbr? }` | `job_id`（kind `deploy`） |
| `deployed_protocols_cmd` | `listDeployedProtocols(serverId)` | `{ serverId }` | `string[]`（节点行派生） |
| `deployed_protocol_details_cmd` | `listDeployedProtocolDetails(serverId)` | `{ serverId }` | `DeployedProtocolDetail[]`（变体 + 端口） |
| `install_supplement_cmd` | `installSupplement(serverId, protocols)` | `{ serverId, protocols }` | `job_id`；现有 ∪ 勾选，无新增时被拒 |
| `remove_protocol_cmd` | `removeServerProtocol(serverId, protocol)` | `{ serverId, protocol }` | `void`（骨架：P2 后续接停止 unit） |

协议白名单：`vless`、`trojan`、`vmess`、`ss`、`hysteria2`、`tuic`。

## 4. 通道（端口放行）

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_channels_cmd` | `listChannels(serverId)` | `{ serverId }` | `Channel[]` |
| `add_channel_cmd` | `addChannel(input)` | `{ serverId, name, port, protocol }` | `Channel` |
| `remove_channel_cmd` | `removeChannel(id)` | `{ id }` | `void` |
| `channel_status_cmd` | `channelStatus(id)` | `{ id }` | `ChannelStatus { running, detail }` |
| `channel_switch_cmd` | `channelSwitch(id, enabled)` | `{ id, enabled }` | `ChannelStatus` |
| `scan_server_ports_cmd` | `scanServerPorts(serverId)` | `{ serverId }` | `ServerPort[] { port, process }` |

## 5. 上游出口与隐私线

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_upstreams_cmd` | `listUpstreams(serverId)` | `{ serverId }` | `Upstream[]`（不含密码） |
| `add_upstream_cmd` | `addUpstream(input)` | `{ serverId, name, proto, host, port, username, password, listenPort, realityDest? }` | `Upstream` |
| `update_upstream_cmd` | `updateUpstream(id, input)` | `{ id, proto, host, port, username, password?, listenPort, realityDest }` | `Upstream`（实际生效值；端口冲突会自动改用空闲端口） |
| `remove_upstream_cmd` | `removeUpstream(id)` | `{ id }` | `void` |
| `start_upstream_job_cmd` | `startUpstreamJob(serverId)` | `{ serverId }` | `job_id`（kind `upstream`） |
| `probe_egress_ip_cmd` | `probeEgressIp(id)` | `{ upstreamId }` | `string`（最近探测的出口 IP） |
| `list_privacies_cmd` | `listPrivacies()` | — | `Privacy[]`（含未生效项与错误原因） |
| `enable_privacy_cmd` | `enablePrivacy(serverId, nodeId)` | `{ serverId, nodeId }` | `job_id`（kind `privacy`） |
| `disable_privacy_cmd` | `disablePrivacy(privacyId)` | `{ privacyId }` | `job_id \| null`（从未部署成功则为 null） |

`update_upstream` 的 `password` 省略/传空 = **不改密码**（凭据不回显）。

## 6. 分享与订阅服务

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_shares_cmd` | `listShares()` | — | `ShareList { shares, unreachable[] }`（状态实读自服务器） |
| `create_share_cmd` | `createShare(serverId, nodeIds)` | `{ serverId, nodeIds }` | `ShareInfo`；空列表被拒 |
| `delete_share_cmd` | `deleteShare(serverId, token)` | `{ serverId, token }` | `void`（停服务 + 删文件 + 撤规则，幂等） |
| `start_sub_server_cmd` | `startSubServer(port, token)` | `{ port, token }` | `number`（实际端口） |
| `stop_sub_server_cmd` | `stopSubServer()` | — | `void` |
| `get_sub_server_status_cmd` | `getSubServerStatus()` | — | `boolean` |

## 7. 本机上网（网络客户端与服务）

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `net_status_cmd` | `netStatus()` | — | `NetStatus`（含 TUN/防火墙/接管详情） |
| `net_repair_proxy_cmd` | `netRepairProxy()` | — | `void`（网络仍在跑、系统代理被关时恢复） |
| `net_start_cmd` | `netStart()` | `{ ignoreConflicts, conflictOverride }` | `NetStatus` |
| `net_stop_cmd` | `netStop()` | — | `NetStatus` |
| `net_set_tun_cmd` | `netSetTun(enable)` | `{ enable }` | `NetStatus`；失败**抛错**，不静默降级 |
| `net_service_state_cmd` | `netServiceState()` | — | `ServiceState { installed, running }` |
| `net_service_install_cmd` | `netServiceInstall()` | — | `void`；会弹一次 UAC，结果靠轮询 state |
| `net_service_uninstall_cmd` | `netServiceUninstall()` | — | `void`（同样过 UAC） |
| `net_select_cmd` | `netSelect(nodeName)` | `{ nodeName }` | `void`（切分组出口） |
| `net_current_cmd` | `netCurrent()` | — | `string`（当前手动选择的分组出口名） |
| `net_firewall_clear_cmd` | `netFirewallClear()` | — | `FirewallClearResult { removed }`；网络运行中会拒绝 |
| `net_conn_log_cmd` | `netConnLog(limit?, onlyLeaks?)` | `{ limit, onlyLeaks }` | `ConnLogSnapshot`（当前采样窗口，不触发新采样） |

## 8. 中转链路与设备

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_relay_rules_cmd` | `listRelayRules()` | — | `RelayRule[]` |
| `probe_link_cmd` | `probeLink(relayServerId, landingServerId)` | `{ relayServerId, landingServerId }` | `ProbeResult`（含 DNAT 候选与建议值） |
| `build_link_cmd` | `buildLink(input)` | `{ name, relayServerId, landingServerId, clientMode, wgPort, forwardPort, landingPort, converged, forwardMode?, action }` | `job_id`（kind `link`） |
| `adopt_link_cmd` | `adoptLink(input)` | `{ name, relayServerId, landingServerId, clientMode, wgPort, forwardPort, relayPubkey, landingPubkey, landingPort, subnet, converged }` | `RelayRule` |
| `teardown_link_cmd` | `teardownLink(ruleId, action)` | `{ ruleId, action: 'unregister' \| 'full' }` | `job_id`（kind `link`） |
| `readopt_link_cmd` | `readoptLink(ruleId)` | `{ ruleId }` | `RelayRule` |
| `delete_link_rule_cmd` | `deleteLinkRule(ruleId)` | `{ ruleId }` | `void` |
| `refresh_link_status_cmd` | `refreshLinkStatus(ruleId)` | `{ ruleId }` | `LinkStatus` |
| `list_device_peers_cmd` | `listDevicePeers(ruleId)` | `{ ruleId }` | `DevicePeer[]` |
| `add_device_peer_cmd` | `addDevicePeer(ruleId, name)` | `{ ruleId, name }` | `job_id`（kind `peer`） |
| `remove_device_peer_cmd` | `removeDevicePeer(peerId)` | `{ peerId }` | `job_id`（kind `peer_remove`） |
| `get_peer_config_cmd` | `getPeerConfig(peerId)` | `{ peerId }` | `string`（设备配置文本） |

`buildLink.forwardMode` 省略即 `single`；`same_port` = 按原端口转发落地机全部自建节点端口。
`buildLink.action`：`build`（新建）/ `rebuild`（重建）。

## 9. 测速与优化

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `server_speed_cmd` | `serverSpeed(serverIds, concurrency)` | `{ serverIds, concurrency }` | `job_id`（kind `server_speed`） |
| `server_throughput_cmd` | `serverThroughput(from, to, seconds)` | `{ fromServerId, toServerId, seconds }` | `job_id`（kind `server_throughput`） |
| `install_iperf3_cmd` | `installIperf3(serverId)` | `{ serverId }` | `string`（安装输出） |
| `list_throughput_cmd` | `listThroughput()` | — | `ThroughputTest[]` |
| `check_bbr_cmd` | `checkBbr(serverId)` | `{ serverId }` | `string`（当前拥塞控制算法） |
| `enable_bbr_cmd` | `enableBbr(serverId)` | `{ serverId }` | `string`（结果说明） |

节点测速（`speed_test_cmd`）与服务器测速是两套口径：前者是完整 HTTP 往返，后者是裸 TCP RTT。

## 10. 任务

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `list_jobs_cmd` | `listJobs()` | — | `Job[]`（最近 50 条，倒序） |
| `cancel_job_cmd` | `cancelJob(jobId)` | `{ jobId }` | `void` |

任务进度走事件流（见 §13），不是轮询命令。

## 11. 设置、系统代理与应用更新

| 命令 / API | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `get_settings_cmd` | `getSettings(keys)` | `{ keys }` | `Record<string, string>` |
| `set_setting_cmd` | `setSetting(key, value)` | `{ key, value }` | `void` |
| `get_system_proxy_cmd` | `getSystemProxy()` | — | `string \| null` |
| （Tauri 插件） | `getAppVersion()` | — | `string`（`tauri.conf.json` 的 version） |
| （Tauri 插件） | `checkForUpdate()` | — | `UpdateInfo \| null`；自动透传系统代理 |

`checkForUpdate` 不是 IPC 命令：它先取系统代理，再调用 `@tauri-apps/plugin-updater`。

## 12. 服务器健康监控

监控只在桌面端应用打开时按 `interval_secs` 采集；应用退出即停，不做常驻。

线格式类型：`MonitorSummary` / `MonitorSample` / `MonitorAlert` / `MonitorPoint` 定义在
`src-tauri/src/server_monitor/mod.rs`，`MonitorConfig` / `Thresholds` 在
`server_monitor/config.rs`；前端对应 `src/api/types.ts`。

| 命令 | UI 封装 | 参数 | 返回 |
| --- | --- | --- | --- |
| `server_monitor_summary_cmd` | `monitorSummary()` | — | `MonitorSummary` |
| `server_monitor_history_cmd` | `monitorHistory(serverId, metric, rangeSecs)` | `{ serverId, metric, rangeSecs }` | `MonitorPoint[]` |
| `server_monitor_alerts_cmd` | `monitorAlerts(includeResolved, limit)` | `{ includeResolved, limit }` | `MonitorAlert[]` |
| `server_monitor_collect_cmd` | `monitorCollect()` | — | `MonitorSummary` |
| `server_monitor_config_cmd` | `monitorConfig()` | — | `MonitorConfig` |
| `server_monitor_set_config_cmd` | `monitorSetConfig(config)` | `{ config }` | `MonitorConfig` |
| `server_monitor_ack_cmd` | `monitorAck(alertId)` | `{ alertId }` | `void` |

`metric` 取值：`cpu_pct`、`mem_pct`、`disk_pct`、`load1`、`connect_ms`；
`rangeSecs` 只接受 `3600`、`86400`、`604800`；`limit` 范围 1–500。

| 事件 | 载荷 | 说明 |
| --- | --- | --- |
| `monitor://tick` | `MonitorSummary` | 每轮采集完成后推送整份摘要 |
| `monitor://collecting` | `{ collecting: boolean }` | 采集开始/结束；前端据此禁用「立即采集」 |

## 13. 事件流

| 事件 | 载荷 | 说明 |
| --- | --- | --- |
| `job://progress` | `JobProgress { job_id, kind, step, message, progress, error }` | 后台任务分步进度与终态 |
| `net://conn_log` | 连接日志增量 | 仅本机上网运行期间发送；面板挂载时先用快照铺底 |

`JobProgress.kind` 取值：`deploy`、`upstream`、`privacy`、`link`、`peer`、`speed`、
`server_speed`、`server_throughput`、`verify`、`peer_remove`、`link_build`。

前端任务注册表约定（`src/jobs/registry.ts`）：

- 任务占位（负数 id）必须**先于** IPC 提交，否则提交窗口内的事件会被丢弃；
- 逐目标事件（`message` 等于某个 target）只写该目标结果，不改任务进度、不判终态；
- 终态判据：`progress >= 100` 或 `error != null`；
- 事件丢失由唯一的 5 秒对账兜底（`list_jobs_cmd`）。
