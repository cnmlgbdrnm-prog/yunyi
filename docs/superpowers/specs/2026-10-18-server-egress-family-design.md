# 服务器出站地址族（VPS 出口 v4/v6）设计

- 日期：2026-10-18
- 状态：设计已确认并实施
- 依赖：[`2026-08-13-vpn-ctl-p2-design.md`](2026-08-13-vpn-ctl-p2-design.md)（服务器管理、Xray 部署）、
  [`2026-09-17-upstream-egress-design.md`](2026-09-17-upstream-egress-design.md)（出口概念的另一条链路）

**目标：** 允许用户按服务器手动选择 **VPS → 目标** 的出口地址族（IPv4 / IPv6），
且切换必须真正改写 VPS 上已部署的配置，而不是只改库里的一个数字。

**范围：** `servers` 新增 `egress_family`；编辑表单新增下拉；三种部署产物
（Xray、shadowsocks-rust、sing-box）按选择生成不同写法；出口变化时自动重部署。

**不做：**
- 不改入口（客户端 → VPS）开关的行为，不做「一键同时切入口 + 出口」
- 不做按协议分别设置出口
- 不升级/固定远端 sing-box 版本
- 不接管客户端侧 DNS 与分流

## 1. 两个地址族必须分开

| 字段 | 管什么 | 改哪里 | 生效时机 |
| --- | --- | --- | --- |
| `addr_family`（v15） | 入口：SSH/测速管理地址、自建直连节点的 `addr` | 行内 IPv4/IPv6 开关 | 立即（同一事务；必要时自动探测） |
| `egress_family`（v16） | 出口：VPS 再访问目标域名时优先/限定哪一族 | 编辑表单「出站地址族」 | 保存后自动重部署 |

两者**完全独立**：任何一个开关都不改另一个。`servers::update_server` 里只读取
`input.egress_family` 写回该列，`addr_family` 与节点 `addr` 不参与。

## 2. 语义边界（UI 与 CHANGELOG 同步写明）

- 出口族只影响 **VPS → 目标域名**的解析/建连。
- 客户端已经发来的**字面 IP 目标**不受影响（没有可解析的域名）。
- 客户端侧 DNS 与分流规则不受影响——它们在本机发生。
- 严格 v6 要求 VPS 自身有可用 IPv6 出口；没有时域名目标会直接失败，
  这是预期行为（表单里提示）。

## 3. 三协议的映射

集中在 `servers/deploy_spec.rs` 的 `EgressFamily`：

```rust
pub enum EgressFamily { V4, V6 }
```

| 协议 | v4 | v6 | 强度 |
| --- | --- | --- | --- |
| Xray（vless/trojan/vmess） | `settings.domainStrategy = "UseIPv4"` | `"UseIPv6"` | 对域名目标**严格** |
| shadowsocks-rust | `ipv6_first = false` | `true` | **尽力而为**：只把 AAAA 排在 A 前面，DNS 没有 AAAA 时仍回落 v4；ssserver 没有「只允许某族」的开关 |
| sing-box ≥ 1.12 | 出站 `domain_resolver = { server: "egress", strategy: "ipv4_only" }` + `dns.servers = [{ type: "local", tag: "egress" }]` | `strategy = "ipv6_only"` | 对域名目标严格 |
| sing-box < 1.12 | 出站 `domain_strategy = "ipv4_only"` | `"ipv6_only"` | 同上 |
| 版本探测失败 | **不写任何出口字段**（出站只有 `type: direct`） | 同左 | 本次切换不生效；服务保持可启动 |

关键约束：

1. **Xray 的 `domainStrategy` 必须写在 freedom 出站的 `settings` 内**。写在出站顶层
   会被 Xray 静默忽略——配置能加载、行为不变，是最难发现的一类失效。测试同时钉住
   「值正确」与「顶层没有同名字段」。
2. **sing-box 必须做版本探测**：`/usr/local/bin/sing-box version` → `parse_singbox_version`。
   `domain_strategy` 自 1.12 起弃用；新版字段 `domain_resolver` 在旧版上是未知键，
   会导致 sing-box **FATAL**。
   探测失败时**两种写法都不写**——2026-10-18 用真实的 1.13.20 跑
   `sing-box check` 实测发现，新版对 `domain_strategy` 不是警告而是 **FATAL**
   （须设 `ENABLE_DEPRECATED_LEGACY_DOMAIN_STRATEGY_OPTIONS` 才放行），
   原先「探测失败按旧版处理」会把正在运行的服务打挂。省略出口字段则新旧版都能加载，
   代价是本次出口切换不生效；部署进度里会明确告知，重试部署即可。
3. **SS 的能力边界必须如实展示**，UI 文案为「优先 v6，可能回落 v4」，不得宣称强制。

## 4. 部署侧改动

### 4.1 读取与生成

`run_deploy` 从 `Server` 行读取 `egress_family`（**不扩展 `DeployRequest`**——
这是服务器属性，不是某次部署的参数），传给三个生成函数：

- `xray_config_json_multi(entries, egress)`
- `ss_config_json(port, password_b64, egress)`
- `singbox_config_json(hy2, tuic, egress, version)`

### 4.2 配置改写后必须重启

原来 SS 与 sing-box 只做 `systemctl enable --now`：服务已是 active 时该命令是
no-op，新配置停在磁盘上不生效，表现就是「切了出口但行为没变」。现在与 Xray 一致：

- SS：写配置后 `systemctl restart ssserver`
- sing-box：写配置后 `systemctl restart singbox`

**restart 的退出码不作判据**，判据仍是随后的 `wait_service_active`（与 Xray 现有
做法一致：`is-active` 未激活时本身返回非零，按状态轮询才能给出可读原因）。

### 4.3 REALITY 伪装域名不被重置

自动重部署以 `reality_dest: None` 提交。原实现会在 `None` 时回落
`DEFAULT_REALITY_DEST`，把用户自定义的伪装域名静默改回默认值（服务器与节点两侧
一致，却不再是用户选的那个域名）。

现在 `None` 时按以下顺序取值：

1. 本次部署协议对应的既有节点 `params_json.sni`；
2. 否则依次尝试 `vless` / `trojan` 节点的 `sni`；
3. 都没有才用 `DEFAULT_REALITY_DEST`。

读 sni 的 helper 在 `servers/reality_dest.rs`（只读，不改节点）。

## 5. 保存与重部署编排

`commands::update_server` 返回 `UpdateServerResult { server, deploy_job_id, deploy_error }`：

1. 先读旧 `egress_family`（保存后库里已是新值，再读就判断不出变化）；
2. 保存服务器资料；
3. 出口族变化 → `deployed_protocols`（以自建节点行为准）：
   - 非空 → `start_deploy(DeployInput { reality_dest: None, .. })`；
   - 空 → 不排任务（新值会在首次部署时自然写进配置）；
4. 部署提交失败**不回滚保存**，原因放进 `deploy_error`。

不用 `install_supplement`：它要求合并后产生新增协议，纯出口变更会被
`merged == existing` 拒绝。

### 5.1 前端

- 表单：`ServerFormPanel` 仅在编辑模式渲染「出站地址族」下拉；
  `createServer` **不传**该字段（v16 默认 v4），避免把未声明的参数透给 command。
- 保存编排收在 `views/servers/useServerSave.ts`（`Servers.tsx` 已顶到 500 行组件上限）：
  1. 变化时先 `reserveJob('deploy', [String(id)])`——**必须先于 IPC**，
     否则 `__ingest` 会把未知 job id 的进度事件全部丢掉；
  2. `updateServer` 返回后：有 `deploy_job_id` → `bindJob(ticket, jobId)`；
     否则 `release(ticket)`；
  3. `deploy_error` 上屏，但保存仍算成功、照常关闭表单。
- 进度展示：编辑保存后表单即关闭，进度落在**行级**。`ServerRow` 新增不随展开区
  收起的 `footer` 槽位，`ServerList` 在其中渲染现有 `JobProgressPanel`
  （`progress: ProgressView | null`）；部署向导已展开时不重复渲染。
- `useJobTerminal('deploy')` 的终态回调把失败原因上屏（自动重部署没有向导兜底）；
  `DeployWizard` 的终态回调增加**目标判据**，避免把别处发起的 deploy 任务
  （例如保存出口触发的重部署）误当成向导自己的部署。

## 6. 测试

- 迁移 v16：最新版本为 16，默认 `'v4'`，旧库升级后既有数据可读；
- 仓储：`egress_family` 经 get/update 往返，且 update 不动 `addr_family`；
- `deploy_spec` 金样：Xray v4/v6（含「必须在 settings 内」）、SS `ipv6_first`
  两项、sing-box 1.11 / 1.12 / 1.14 / 探测失败四条分支、`parse_singbox_version`；
- 部署全序列：SS 与 sing-box 写出的配置包含出口字段，且发出对应的
  `systemctl restart`；`reality_dest=None` 的重部署保留既有节点 `sni`；
- 命令层：出口变化 + 已部署 → 入队；未变化/未部署 → 不入队；
- 前端：下拉出现且随 IPC 提交、保存出口不调用 `set_server_addr_family_cmd`、
  返回 job id 时行内出现进度、返回 null 时无占位、`deploy_error` 上屏。

## 7. 已知遗留

- **SS 无法强制单族**：`ipv6_first` 只是排序偏好，DNS 无 AAAA 时仍走 v4。
- **sing-box 弃用时间线**：`domain_strategy` 1.12 起弃用；真实 1.13.20 的报错文本
  称「将在 1.14.0 移除」，当前已默认 FATAL（需环境变量才放行）。
  新版 `domain_resolver.strategy` 本身也在 1.14 标记弃用（移除时间未定）。
  本实现按版本择一，远端升级到移除该字段的版本后需要再跟进。
- **严格 v6 依赖 VPS 的 v6 出口**；没有可用 v6 时域名目标直接失败，属预期。
- 出口族只覆盖三份部署配置；上游住宅出口（upstreams）与隐私线（Tor）的出口
  语义不受这个开关控制。
