# 服务器地址族接口（入口 / 出口）

本文是「服务器地址族」相关 IPC 的线格式与语义说明。设计背景见
[`docs/superpowers/specs/2026-10-18-server-egress-family-design.md`](../superpowers/specs/2026-10-18-server-egress-family-design.md)。

**事实源：** 前端封装在 `src/api/index.ts`，Rust 壳在 `src-tauri/src/commands/ipc.rs`，
领域类型在 `src-tauri/src/domain.rs`。本文与之不一致时以代码为准，并应同步修正本文。
全部 IPC 命令见 [`docs/design/ipc-interface-reference.md`](ipc-interface-reference.md)。

## 1. 两个地址族

| 字段（`Server`） | 语义 | 写入方式 | 生效时机 |
| --- | --- | --- | --- |
| `addr_family: 'v4' \| 'v6'` | **入口**：客户端/管理面连 VPS 用哪一族。决定 SSH/测速管理地址与自建直连节点的 `addr` | `set_server_addr_family_cmd` | 立即（同一事务） |
| `egress_family: 'v4' \| 'v6'` | **出口**：VPS 再访问目标域名时优先/限定哪一族 | `update_server_cmd` 的 `egressFamily` | 已部署协议时自动重部署后 |

两者**完全独立**：任一命令都不会改另一个字段。`host` 始终保存 IPv4（或域名），
`host_v6` 是可选的 IPv6 字面量（不带方括号）；实际管理地址由 `addr_family` 选择。

## 2. 命令一览

所有命令参数从 JS 侧按 **camelCase** 传入，Rust 侧接收 snake_case；响应按 Rust
字段名序列化（snake_case）。

### 2.1 `list_servers_cmd`

- 参数：无
- 返回：`Server[]`
- 说明：排序按 `name`。`Server` 含 `host`、`host_v6`、`addr_family`、
  `egress_family`、`port`、测速字段等；**不含凭据引用**。

### 2.2 `create_server_cmd`

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 必填，去首尾空白后不得为空 |
| `host` | string | IPv4 或域名 |
| `hostV6` | string | 可空；IPv6 字面量，不带方括号 |
| `port` | number | SSH 端口 |
| `user` | string | 登录用户 |
| `authKind` | `'password' \| 'key'` | 认证方式 |
| `kind` | `'relay' \| 'landing'` | 中转 / 落地 |
| `notes` | string | 备注 |
| `credential` | string | 密码或私钥 PEM |

- 返回：`Server`
- **不接受 `egressFamily`**：新服务器出口族固定为 `v4`（迁移 v16 默认值）。
- 凭据加密入库，响应不含凭据。

### 2.3 `update_server_cmd`

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `id` | number | 服务器 id |
| `name` / `host` / `hostV6` / `port` / `user` / `authKind` / `kind` / `notes` | 同上 | 全量更新 |
| `credential` | `string \| null` | `null` 或空串 = 保留原凭据；切换认证方式时必须提供新凭据 |
| `egressFamily` | `'v4' \| 'v6' \| null` | `null`/省略 = 保留原值；其它值必须是 `'v4'` 或 `'v6'` |

- 返回：`UpdateServerResult`（见 §3）
- **保存本身总是成功**；出口族变化且该机已部署协议时，命令在内部排一次重部署，
  提交失败只写进 `deploy_error`，**不回滚保存**。
- 「已部署协议」以本地自建节点行为准（`deployed_protocols`），不以部署摘要为准。
- 未部署任何协议时不排任务：新出口值会在首次部署时自然写进配置。
- 非法 `egressFamily` → 校验错误（`出站地址族必须是 v4 或 v6`），不落库。

### 2.4 `set_server_addr_family_cmd`（入口）

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `id` | number | 服务器 id |
| `family` | `'v4' \| 'v6'` | 目标入口族 |

- 返回：更新后的 `Server`
- 切到 `v6` 且 `host_v6` 为空时，会先 SSH 到原始 `host` 自动探测全局 IPv6 并登记；
  探测失败返回可读错误（提示可在编辑里手填），服务器不变。
- 同一事务内更新该机全部**自建直连节点**的 `addr`；中转节点（入口地址）与
  其它服务器的节点不动。出口族不受影响。

### 2.5 `detect_server_ipv6_cmd`

- 参数：`id`
- 返回：`string[]`，推荐地址在首位；空数组表示没有发现全局 IPv6。
- 只探测不切换、不落库，供编辑表单预填候选。

### 2.6 `start_deploy_cmd`

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `serverId` | number | 目标服务器 |
| `protocols` | `string[]` | 非空，且在部署白名单内 |
| `realityDest` | `string?` | 省略 = 由部署侧沿用既有节点 `sni`，没有才用默认值 |
| `enableBbr` | `boolean?` | 省略按 `false` |

- 返回：`job_id`（number，> 0）
- 出口族**不是**部署参数：部署时从服务器行读取 `egress_family`。

## 3. 线格式类型

```ts
interface Server {
  id: number
  name: string
  host: string
  host_v6: string        // 空串 = 未登记
  addr_family: 'v4' | 'v6'
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

interface UpdateServerResult {
  server: Server
  /** 已排队的重部署任务 id；未触发（出口未变 / 未部署协议）为 null */
  deploy_job_id: number | null
  /** 排重部署失败的原因；成功或未触发为 null */
  deploy_error: string | null
}
```

## 4. 任务事件契约

自动重部署与手动部署走同一条 `job://progress` 事件流：

```ts
interface JobProgress {
  job_id: number
  kind: 'deploy' | ...
  step: string
  message: string
  progress: number
  error: string | null
}
```

- 部署任务的目标列表首位 = 服务器 id（`targets[0]`），前端据此把进度挂到对应行。
- 逐目标事件（`message` 恰好等于某个 target）只写该目标的结果，**不改变任务进度、
  不判终态**；任务级进度才更新 `step/message/progress`。
- 终态判据：`progress >= 100` 或 `error != null`（前端注册表 `src/jobs/registry`）。
- 任务提交前必须先占位（`reserveJob`）再发 IPC，否则提交窗口内的事件会被丢弃。

## 5. 错误与校验

- 错误统一序列化为可读字符串（`AppError` 的 `Display`），前端直接展示。
- `update_server_cmd` 只在参数非法/服务器不存在时失败；重部署失败不算命令失败。
- `set_server_addr_family_cmd` 的 `family` 只接受 `v4`/`v6`；探测失败会返回原因。
- `start_deploy_cmd` 拒绝空协议列表、白名单外协议与不存在的服务器。

## 6. 数据库兼容性

| 迁移 | 变更 | 兼容性 |
| --- | --- | --- |
| v15 | `servers.host_v6`、`servers.addr_family` | 旧行默认空串 / `'v4'` |
| v16 | `servers.egress_family`（`NOT NULL DEFAULT 'v4'`） | 旧行与旧前端默认 v4 |

- 旧前端不传 `egressFamily`：后端按「保留原值」处理。
- 旧后端不认识 `egressFamily`：Tauri 会拒绝多余参数——升级前端前先升级后端，
  或让前端保持不传该字段（当前 `createServer` 即如此）。
- 用比数据库旧的二进制启动会触发降级护栏（`数据目录比本程序新`），见
  `src-tauri/src/db/migrations.rs`。

## 7. 与远端配置的对应

| 协议 | 远端文件 | 出口字段 |
| --- | --- | --- |
| Xray | `/usr/local/etc/xray/config.json` | `outbounds[0].settings.domainStrategy` = `UseIPv4` / `UseIPv6` |
| shadowsocks-rust | `/etc/shadowsocks-rust/config.json` | 顶层 `ipv6_first`（尽力而为） |
| sing-box ≥1.12 | `/etc/sing-box/config.json` | 出站 `domain_resolver` + `dns.servers` local 解析器 |
| sing-box <1.12 | 同上 | 出站 `domain_strategy` |
| sing-box 版本未知 | 同上 | 不写出口字段（保证服务可启动），部署进度提示本次未生效 |

配置改写后由部署任务执行 `systemctl restart xray|ssserver|singbox`，并以
`systemctl is-active` 的**状态**（不是 restart 退出码）判定成败。
