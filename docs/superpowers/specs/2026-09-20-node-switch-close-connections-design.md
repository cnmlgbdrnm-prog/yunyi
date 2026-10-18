# 切换出口节点后断开旧连接 设计

- 日期：2026-09-20
- 状态：**已实现**（设计经逐条确认后直接编码，规格为事后归档；代码见 `net/runtime.rs` 的
  `conns_to_close` / `close_stale_conns`）
- 依赖：无。改动落在 `runtime::select` 内部，不新增实体、不改配置格式、不动 UI

**目标：** 让「手动选择」真正等于「现在就在这个节点上」——切换生效后，仍钉在旧出口上的连接
被主动断开，客户端立刻重连到新出口。

**范围：** `runtime::select` 增加一步清理；`MihomoConn` 增加 `id` 字段。

**不做：**

- **不做成设置项**。社区（Clash Verge Rev「自动关闭连接」）把它放在设置里，是因为通用客户端有
  几十个策略组、订阅更新也会改组，掐连接的场景多。本应用只有「手动选择」这一个入站点，用户点
  它就是在明确表达意图——做成默认关闭的开关，等于这个功能默认是坏的
- **不取「全部断开」**（`DELETE /connections`）。理由见 §3.1
- **不在「自动选择」组自己换节点时触发**。url-test 每 300s 就可能重选一次，接上去等于持续打断
  用户
- **不重复处理 TUN 开关**。开关 TUN 本来就会拆网卡与路由、把既有连接当场全断
- 不做「按进程/按主机名」的更细粒度筛选（社区工具 `mihomo-rs` 有 `--host`/`--process`，本应用
  没有这个需求）

## 1. 需求前提

1. 用户点「手动选择」＝「现在就在这个节点上」，这是该控件的**语义承诺**
2. 切换的破坏性应当只落在**受这次选择影响**的连接上
3. 清理失败不能被静默吞掉

## 2. 现状（实现前已核对）

1. **`runtime::select` 只发一个请求**（`net/runtime.rs:834-857`）：
   `PUT /proxies/%E6%89%8B%E5%8A%A8%E9%80%89%E6%8B%A9 {"name": ...}`。**没有任何断开动作**。
2. **mihomo 换 selector 不重建既有连接**——已建立的连接继续走旧节点，直到自己自然关闭。因此
   用户看到的「切了」只是分组的当前值变了。
3. **实测现场（2026-09-20）**：同一时刻 Google 流量挂在三个出口上——
   `google.com` / `www.google.com` / `accounts.google.com` → 香港-vless；
   `accounts.google.com` → 大美；`gemini.google.com` → ashura-vmess。
   即**同一个域名同时挂在两条链上**。
4. **症状形态（决定了它极易被误诊）**：切节点后**当场**不可用 → 过一会儿**自己好了**；
   **开一次 TUN 也会好**。后者是假象：开关 TUN 会拆 `yunyi-tun0` 与路由，**把所有既有连接
   当场全断**，客户端被迫重连才真正走到新节点上——不是 TUN 修好了什么。
   同款误诊包括「必须用中转才能用」「之前能用现在不能用」（同一机制的方向变化）。
5. **mihomo 的 API 面**：只有 `DELETE /connections`（全断）与 `DELETE /connections/:id`
   （单条）。**没有「按节点/组断开」的接口**，筛选必须在客户端做。
6. **社区做法**：Clash Verge Rev 有设置项「自动关闭连接」（设置 → 杂项），对应其 Issue #1873
   （切模式后旧会话仍走旧模式，需重启内核）；Clash Party 只暴露 `mihomoCloseAllConnections()`
   手动按钮。两者都未做「按旧节点筛选」。
7. **`MihomoConn` 原先没有 `id`**（`net/connlog.rs:126-141`，只收 host/ip/port/process/
   network/rule/chains/upload/download/start）。该结构是泄漏检测的输入，`id` 对它无用——但
   `DELETE /connections/{id}` 非它不可，故本轮补上。

## 3. 设计

### 3.1 核心决定：判据「经手动选择、且不在新选中项上」

```
关闭满足以下两条的连接：
  ① chains 含「手动选择」        （走的是这个组）
  ② chains 不含新选中项          （还钉在旧节点上）
```

实现为纯函数 `conns_to_close(conns: &[MihomoConn], new_selection: &str) -> Vec<String>`：
只吃 `id` 与 `chains` 两个字段，因此好测、也好读。

**为什么不取「全部断开」**：规则集是

```
DOMAIN-SUFFIX,x.ai   → 手动选择
DOMAIN-SUFFIX,grok.com → 手动选择
GEOSITE,cn  → DIRECT     ← 与选哪个节点无关
GEOIP,CN    → DIRECT     ← 与选哪个节点无关
MATCH       → 手动选择
```

国内直连那批根本不经过节点，**掐掉它们换不来任何收益，只会打断正在进行的国内下载与 LAN
连接**。「全断」在通用客户端里是图省事（它们的规则集各不相同），在本规则集下是严格更差的。

三种形态都对得上：

| chains | 新选中项 | 判定 |
|---|---|---|
| `["香港-vless", "手动选择"]` | `ashura-vmess` | 关 ✓ |
| `["香港-vmess", "自动选择", "手动选择"]` | `ashura-vmess` | 关 ✓（看末端节点） |
| `["DIRECT"]` / `["REJECT"]` | 任意 | 不碰 ✓ |

**切到「自动选择」时的特殊处理**：此时「不含新选中项」这条判据失效——连接链里同样含「自动
选择」这个词，按名字比对会把全部连接都判成「已经在正确的节点上」而一条都不关。那时的意图是
「让 url-test 重新选」，故**放宽成「经手动选择的全部断开」**。

### 3.2 顺序：先改分组，再断连接

顺序不能反。反过来会有一瞬间「组还没改、连接已被踢」，客户端立刻用旧节点重连，等于没断。

### 3.3 失败处理：不上抛，但一律留痕

清理失败**不改变返回值**——切换本身确实生效了（组的确改了），报成「切换失败」是假报。但也
**不能静默**：静默失败的表现就是用户看到旧症状重现，且无从查起。三条失败路径
（读连接列表失败／解析失败／地址无法解析）与两种部分失败（单条 DELETE 未成功、触到上限）各自
打一行 `[vpn-ctl]` 日志。

### 3.4 上限

单次最多断开 200 条（`MAX_CLOSE_PER_SWITCH`），兜底防止连接数异常时把「切换节点」卡住。**触顶
不静默**：记一行并写明「其余仍走旧出口」。

### 3.5 实现要点

- 连接 id 由 mihomo 生成、形状不归我们管，故走 `Url::path_segments_mut().push(id)` 转义，
  **不手工拼串**——id 里出现 `/` 时手拼会打到别的路径上
- 单请求超时 2s（本机回环，正常亚毫秒）
- `select` 里原本为 PUT 建了一个 `reqwest::Client`，清理复用它（同一个连接池），不另建
- `providerChains` 不参与判定：本应用的配置来自 `client_yaml`，不含 provider

## 4. 验证

**单测**（`net/runtime_tests.rs`，判据是纯函数，夹具手造 `MihomoConn`；解析那一侧由
`connlog_tests` 独立覆盖，两边各自失败时能分清是谁错了）：

| 用例 | 钉住的行为 |
|---|---|
| `closes_connections_still_on_the_old_node` | 旧节点连接被关 |
| `leaves_direct_connections_alone` | `DIRECT` / `REJECT` 不误伤 |
| `closes_nested_auto_group_by_terminal_node` | 嵌套组看末端节点 |
| `selecting_auto_group_closes_everything_through_manual` | 切到「自动选择」时放宽成全断 |
| `skips_connections_without_id` | id 缺失时不打空 id |

`connlog_tests::parse_connections_reads_the_real_response_shape` 补了 `id` 断言。

**实测（未完成，留给真机）**：当前运行中的是旧二进制，需重编重启。且**重启本身就会把既有连接
全断**，所以重启后第一次切节点看到的可能是「好的」——**那是重启的功劳，不是修复的功劳**。
真正的验证是：重启（或停掉 TUN）之后，用浏览器切一次节点，**应当切完当场就在新节点上**，不再
需要等待或重启浏览器。

## 5. 已知代价

**切换的那一瞬间，经代理正在跑的下载／视频／长连接会断。** 这是「立刻就切」的固有含义，不是
缺陷。国内直连那批（§3.1）不受影响。

## 6. 参考

§2.6 的社区结论来自外部资料，非本项目实测，出处留档以便复核：

- <https://github.com/clash-verge-rev/clash-verge-rev/issues/1873> —— 切模式后旧会话仍走旧模式，
  指向「自动关闭连接」设置项
- <https://clashfaq.com/en/blog/articles/clash-verge-rev-add-subscription-use-2026.html> ——
  「切换只对新连接可靠生效；已有会话／WebSocket／下载会继续走原线路」
- <https://lib.rs/crates/mihomo-rs> —— `connection close [--id|--all|--host|--process]`，
  按 host/process 筛选在客户端做

mihomo 自身的 API 面（§2.5）出自官方文档
<https://github.com/metacubex/meta-docs/blob/main/docs/api/index.md>。

