# 分享流量管控与 sing-box 订阅 设计

**目标：** 让分享链接真正可管控——每条分享有独立凭据与独立入口，支持流量配额、上下行限速、到期和**作废即失效**；同时提供 sing-box 订阅格式，订阅传输走 HTTPS。

**范围：** 重做分享模块在服务器上的形态；新增分享专属代理实例、入口网关、TLS 与 ACME；新增配额/限速/到期/用量状态与 UI。**不改**本地订阅服务（`serve/`）、**不改**主服务的协议配置与部署流程、**不建**本地分享表。

> **2026-10-20 范围变更（用户确认）：** 用户要求「让程序自动检测，上游节点访问应该可以分享」。据此：①分享域名在 `share_domain` 为空时自动从服务器分享证书的 DNS SAN 读取，已有有效证书不再要求 ACME 邮箱；②上游出口节点解除排除，创建时从专用 `xray-upstream` 配置克隆对应入站/出站/路由。下文涉及「排除上游」的旧描述以本节为准。

**前置设计：** `2026-09-13-share-link-design.md`（本文在其基础上演进，保留「真实状态在服务器上」与「快照」两条原则）。

## 现状与根因

当前分享是「把配置文件的副本借一个 URL 发出去」：

1. `share/mod.rs:312` 生成快照时写死 `ExportFormat::Yaml`；sing-box 生成器（`config/singbox.rs`、`commands/mod.rs:399`）早已存在，只是分享没接。
2. 订阅服务（`share/mod.rs:112` 的静态文件服务）只负责返回文件，**不参与数据转发**。
3. 快照里的节点凭据就是 owner 自己的单用户凭据（`deploy_spec.rs:636`、`:772`、`:704`）。因此：
   - 没有可计量的数据面，配额/限速无从落地；
   - 作废只删订阅文件，**对方已下载的配置继续有效**；
   - 无法区分接收方，无法审计；
   - 快照内容与主服务共用凭据，轮换凭据会同时打断 owner 自己的节点。
4. 订阅是纯 HTTP，无到期、无访问审计。
5. 旧分享的 iptables 放行规则只在创建时写入，服务器重启后丢失，链接静默失效。

**结论：** 流量管控不是加一个统计接口就能做到的，必须让分享流量经过一个能计数、能切断的咽喉，并让分享使用与主服务隔离的身份。

## 已确认的决策

| 决策 | 选择 |
|---|---|
| 技术路线 | 每条分享在选定服务器上跑**独立代理实例**，专属端口 + 专属凭据；入口网关做计量与限速 |
| 管控能力 | 配额 + 上下行限速 + 到期 + 可吊销（全套） |
| 传输安全 | 域名 + Let's Encrypt 自动证书，新分享强制 HTTPS |
| 分享范围 | 一条分享只属于一台服务器；只允许该机上本应用部署的自建节点 |
| 排除节点 | ssr、隐私线、挂中转链路的节点（上游出口自 2026-10-20 起允许分享，见范围变更） |
| 旧分享 | 不自动迁移；标记「旧版·无管控」，可查看/作废 |
| 限速位置 | 网关内的令牌桶，不碰 `tc`/网卡 qdisc |
| 更新语义 | 分享是创建那一刻的快照；改主服务不更新已有分享，想更新就作废重建 |

## 架构

```
接收方客户端
 ├─ HTTPS :18080-18180  /sub/<token>              → vpnshare-<id>.service（静态 TLS 服务）
 │                      /singbox/<token>          → 同一服务，返回 sing-box JSON
 └─ 代理协议 :18200-18499（公网）                  → vpnshare-<id>-gateway.service
                                                        │ 计数 / 配额 / 限速 / 到期
                                                        ▼
                                                  127.0.0.1:18500-18799
                                                  vpnshare-<id>-xray / -singbox / -ss
                                                  （分享专属凭据，只监听回环）
                                                        │ 克隆主服务同款出站
                                                        ▼
                                                     互联网
```

### 组件职责

| 组件 | 职责 |
|---|---|
| `share-http` | 只读静态订阅服务：YAML / sing-box JSON；TLS；禁列举；访问日志；失败请求限速 |
| `share-gateway` | 每条分享一个：监听公网数据端口，TCP/UDP 转发到回环上的分享实例；字节计数、令牌桶限速、配额与到期判断、状态落盘 |
| 分享实例 | 按协议族按需：Xray（vless/trojan/vmess）、sing-box（hysteria2/tuic）、shadowsocks-rust；入站只绑 `127.0.0.1`，凭据每条分享独立 |
| 防火墙助手 | 安装一次：按注释幂等增删 iptables 规则；服务单元的 `ExecStartPost`/`ExecStopPost` 调用它，重启后自动恢复 |
| `vpnshare` 系统用户 | 所有订阅服务/网关/分享实例以它运行；无权读取主服务配置 |

### 服务器上的文件与单元

`<id>` 是独立的 16 位十六进制内部 id，**不等于** URL 里的 token。

| 路径/单元 | 内容 |
|---|---|
| `/srv/vpnshare/<id>/share.json` | 清单：token、节点、端口映射、凭据、配额/速率/到期；0600 |
| `/srv/vpnshare/<id>/gateway.json` | 公网端口 → 回环目标、proto、配额、速率、到期 |
| `/srv/vpnshare/<id>/usage.json` | 已用字节、是否停用、停用原因、更新时间；原子写 |
| `/srv/vpnshare/<id>/sub/<token>` | Clash/mihomo YAML |
| `/srv/vpnshare/<id>/sub/<token>.json` | sing-box config.json |
| `/srv/vpnshare/<id>/xray.json` / `singbox.json` / `ss.json` | 分享实例配置，按需；0600；入站强制回环 |
| `/srv/vpnshare/<id>/singbox.crt` / `singbox.key` | 从主服务自签证书复制，供分享实例读取 |
| `/srv/vpnshare/<id>/access.log` / `last_access.json` | 订阅访问日志（大小封顶）与最近访问（原子写） |
| `/srv/vpnshare/share_server.py` | 订阅 TLS 服务脚本，所有分享共用 |
| `/srv/vpnshare/share_gateway.py` | 网关脚本，所有分享共用 |
| `/srv/vpnshare/vpnshare-firewall.sh` | 防火墙助手，所有分享共用 |
| `/srv/vpnshare/tls/fullchain.pem` / `privkey.pem` | 证书副本，续期钩子更新；`vpnshare` 可读 |
| `/etc/systemd/system/vpnshare-<id>.service` | 订阅 HTTPS 服务 |
| `/etc/systemd/system/vpnshare-<id>-gateway.service` | 网关，`Restart=always` |
| `/etc/systemd/system/vpnshare-<id>-xray/-singbox/-ss.service` | 分享实例，按需 |

**端口段：**

| 用途 | 范围 | 绑定 |
|---|---|---|
| 订阅 HTTP/HTTPS | 18080-18180 | 公网 |
| 数据公网入口（网关） | 18200-18499 | 公网 |
| 分享实例回环端口 | 18500-18799 | `127.0.0.1` |

每个端口分配前探测：TCP 用 `ss -tln`，UDP 用 `ss -uln`。SS 的 `tcp_and_udp` 要求同一端口 TCP 与 UDP 都空闲。范围内全占即报错，不覆盖。

## 数据流

### 创建

1. 后端校验选择：非空；每个节点满足准入条件（见下）；服务器存在且可连。
2. 校验分享域名与证书就绪；未就绪直接拒绝，提示先执行 `prepare_share_tls_cmd`。
3. SSH 连接服务器，探测 `python3`、主服务配置（按所选协议）、所需二进制（xray/sing-box/ssserver）。
4. 读主服务配置，按节点克隆入站（见「配置克隆」），生成分享专属凭据。
5. 分配端口：HTTP、每个节点的公网入口、每个节点的回环端口。
6. 写清单、网关配置、订阅 YAML/JSON、分享实例配置、systemd 单元。
7. `enable --now` 所有单元；单元的 `ExecStartPost` 放行公网端口（HTTP + 数据口）。
8. 验证：所有单元 active；公网数据口在监听；回环端口只在 `127.0.0.1` 监听；从服务器本地 `curl --resolve <域名>:<端口>:127.0.0.1` 按证书验证 HTTPS 取到 200；`usage.json` 已生成。
9. 返回 `ShareInfo`。任一步失败 → 反向拆除本次创建的单元/文件/端口，并核验端口已关。

### 使用

- 接收方用 HTTPS 链接取 YAML 或 sing-box JSON，导入客户端。
- 客户端连 `host:182xx`（公网），网关计数并限速后转发到 `127.0.0.1:185xx`，分享实例再按主服务同款出站访问互联网。
- 网关每 5 秒把 `used_bytes` 原子写入 `usage.json`；连接关闭时再写一次。

### 停用与恢复

- `used_bytes >= quota_bytes` 或服务器时间 ≥ `expire_at`：网关关闭公网监听、断开现有连接，写 `blocked` 原因；进程保持运行。
- 订阅 HTTP 服务不停：对方仍可取到配置，但数据端口连不上。
- 「编辑」（配额/速率/到期）：停网关 → 原子写 `share.json` 与 `gateway.json`，并清空 `usage.json` 的 `blocked`（**保留已用字节**）→ 起网关。编辑期间连接会断一下。
- 「重置用量」：停网关 → 原子写 `usage.json`（`used_bytes=0`、`blocked=null`）→ 起网关。

### 作废

1. 读 `share.json`。
2. 停用并禁用全部相关单元（HTTP、网关、分享实例）。
3. 删除单元文件与分享目录；证书属于服务器、不删除。
4. 单元的 `ExecStopPost` 撤销防火墙规则；再按注释兜底删除并核验端口已关。
5. 各步幂等；分享不存在也算成功。

### 列举

- 仍不建本地表。`list_shares` 逐服务器 SSH，**按服务器批量读取**（目录清单、所有 `share.json`、所有 `usage.json`、`systemctl` 状态），不按分享逐条往返。
- 新分享：读清单与用量，推导状态。
- 旧分享（无 `share.json`）：按现有目录/单元/快照解析，标记 `legacy`；顺带写入防火墙持久化单元（不改变凭据、端口、内容）。
- 连不上的服务器仍进 `unreachable`，并带原因。

## 配置克隆

创建时从主服务配置克隆，**不重新发明协议字段**。

### Xray（vless / trojan / vmess）

- 解析 `/usr/local/etc/xray/config.json`，按节点的协议与端口找到对应入站。
- 克隆入站 → `listen` 改为 `127.0.0.1`、`port` 改为回环端口。
- `settings.clients` 换成单个新客户端：
  - vless：新 uuid，保留 `flow`；
  - trojan：新 password；
  - vmess：新 uuid，保留 `alterId`。
- `streamSettings`（reality / ws）原样保留。
- 新配置 = 克隆入站 + 主配置的 `outbounds` / `routing`（若存在）；删除 `api` / `stats` / `policy` 等控制面。
- 节点参数：克隆原 `params_json`，只替换 `uuid` / `password`；`sni` / `public-key` / `short-id` / `fingerprint` 等原样。

### sing-box（hysteria2 / tuic）

- 解析 `/etc/sing-box/config.json`，按 type 与端口找到对应入站。
- 克隆入站 → `listen` 改为 `127.0.0.1`、`listen_port` 改为回环端口。
- `users` 换成单个新用户：hysteria2 新 password；tuic 新 uuid + password。
- `tls` 保留，但证书路径改指向分享目录内的副本（`singbox.crt` / `singbox.key`），创建时从 `/etc/sing-box/self.crt|key` 复制。
- 新配置 = 克隆入站 + 主配置的 `outbounds` / `route` / `dns`；删除 `experimental`。
- 节点参数：克隆原 `params_json`，替换 `password` / `uuid`；`skip-cert-verify` 等原样。

### shadowsocks

- 从节点参数取 `cipher`，生成新的 32 字节随机密码（base64）。
- 配置：`servers = [{ server: "127.0.0.1", server_port: 回环端口, method: cipher, password: 新密码, mode: "tcp_and_udp" }]`；`ipv6_first` 从主配置复制。
- 节点参数：替换 `password`。

### 准入条件（后端与前端共用同一判断）

节点必须同时满足：

1. `source == "self-built"`；
2. `direct_addr(node)` 属于选定服务器（`server_owns_addr`）；
3. `enabled == true`；
4. `protocol != "ssr"`；
5. 不是隐私线（`effective_privacy` 为 None）；
6. 没有挂中转链路（`relay_rule_id.is_none()`）。

> 上游出口节点自 2026-10-20 起**允许**分享（原第 6 条已删除）。它们仍是
> `self-built`、启用、未挂中转的 VLESS 节点；克隆时按 `upstreams.node_id`
> 识别来源，从 `/usr/local/etc/xray-upstream/config.json` 读取对应入站，并且
> 只复制被选中出口的出站与路由。

不满足的节点在向导里置灰并显示原因；后端在创建时重新校验，不能只信前端。

### 快照语义

分享实例与订阅内容在创建时固定。之后：

- 主服务重新部署、轮换凭据、改伪装域名、改出口，**不会**更新已有分享；
- 分享有自己独立的端口与凭据，主服务轮换不会打断分享；
- 想更新分享内容 → 作废重建。

## 管控语义

### 计量

- 网关是唯一数据咽喉。`up` = 客户端→服务器，`down` = 服务器→客户端；配额取两者之和。
- 计入的是网关实际转发的字节（含 TLS/QUIC/协议开销），不是 VPS 账单口径；UI 注明。
- `usage.json` 每 5 秒原子落盘（临时文件 + rename），连接关闭时再写。进程崩溃最多丢 5 秒用量；服务器重启后从文件续计。
- 订阅 HTTP 下载不计入配额。

### 限速

- 每条分享聚合，上下行分开，单位 Mbps，0 = 不限。默认上下行各 100 Mbps。
- 令牌桶容量 = 1 秒额度。
- TCP：额度不足时延迟转发（先 `await` 令牌再写目标），靠 TCP 窗口向上游施加背压，不丢数据。
- UDP：额度不足时丢弃数据报，让 QUIC 自身拥塞控制降速。
- 只作用于分享流量，不使用 `tc`/qdisc，不影响主服务与系统其他流量。

### 到期与配额

- 到期时间由创建时的**服务器时钟**计算（`date +%s` + 天数），存 UTC；网关按服务器时间判断。
- 触发后关闭监听并断开连接；`usage.json` 写 `blocked: "quota" | "expired"`。
- 网关进程保持运行（不退出，避免 systemd 反复拉起），每秒检查到期。

### 状态

| 状态 | 判据 | UI 文案 |
|---|---|---|
| `active` | 网关与所需实例均 active，未超额/未到期 | 运行中 |
| `blocked_quota` | `used_bytes >= quota_bytes` | 已停用（超额） |
| `blocked_expired` | 服务器时间 ≥ `expires_at` | 已停用（到期） |
| `degraded` | 任一所需单元 inactive，或 `usage.json` 缺失/损坏 | 异常（原因） |
| `legacy` | 无 `share.json` | 旧版·无管控 |

### 默认值

配额 100 GB（1 GB = 1024³ 字节）、上下行各 100 Mbps、有效期 30 天。向导可改；`quotaGb` 必须 > 0，速率 ≥ 0，天数 1-3650。

## 传输安全（HTTPS）

### 配置

- `servers.share_domain`：每台服务器一个分享域名，必须解析到该机；可为空。
- 全局设置 `share_acme_email`：首次签发用的 ACME 邮箱。
- 域名自动检测：`share_domain` 为空时，读取服务器 `/srv/vpnshare/tls/fullchain.pem`
  的全部 DNS SAN，取第一个合法域名；配置值与证书 SAN 不一致时拒绝放行。
- 已有有效证书时，不要求 ACME 邮箱（邮箱只用于签发新证书）；没有可用证书且缺
  域名/邮箱时拒绝创建，并指向配置入口。**绝不静默退回 HTTP**。

### 签发（`prepare_share_tls_cmd`）

1. 从服务器上用 `python3` 解析域名与 SSH 地址（`host`/`host_v6`），两边地址无交集即报错并显示解析结果。
2. 探测 80 端口空闲；被占即报错（HTTP-01 standalone 需要）。
3. 安装 certbot：按包管理器（apt-get/dnf/yum/pacman/zypper/apk）；没有可用包管理器即报错。不走来路不明的安装脚本。
4. 临时放行 80 端口（`vpnshare-firewall.sh`），执行 `certbot certonly --standalone -d <域名> --non-interactive --agree-tos -m <邮箱> --keep-until-expiring`，完成后撤回。
5. 校验证书存在且未过期；复制 `fullchain.pem` / `privkey.pem` 到 `/srv/vpnshare/tls/`，属主 `vpnshare:vpnshare`，0600。
6. 安装续期 deploy hook：certbot 续期后复制新证书并重启所有 `vpnshare-*` 服务。
7. 返回证书到期时间。已有该域名的证书时仍执行 certbot，由 `--keep-until-expiring` 决定是否续期；证书缺失或域名变更才全新签发。

### 使用

- 订阅服务用 `ssl.SSLContext(PROTOCOL_TLS_SERVER)` 加载证书；不支持 TLS 1.2 以下的协议。
- 链接形如 `https://<域名>:<端口>/sub/<token>`（YAML）与 `https://<域名>:<端口>/singbox/<token>`（sing-box JSON）。
- 80 被占、DNS 不匹配、签发失败：全部显式失败，不降级。

## 安全设计

### 凭证与链接

- 新 token：32 位小写十六进制（128 bit）；旧 16 位 token 仍可读。
- 内部 id 与 token 分离：目录名、单元名、`ps` 里只出现 id，不出现 token。
- 订阅内容只含分享专属凭据；作废后已下载的配置立即失效，即使对方扫到该机其他端口也没有可用凭据。
- 分享实例入站强制 `127.0.0.1`；公网只暴露网关端口。测试必须断言这一点。

### 进程与文件权限

- 系统用户 `vpnshare`（无 shell、无 home），首次创建分享时幂等创建。
- `/srv/vpnshare` 为 root:root 0755；共用脚本（`share_server.py`、`share_gateway.py`、`vpnshare-firewall.sh`）root:root 0755，`vpnshare` 只读/可执行、不可修改。防火墙助手以 root 执行，绝不能让被托管的用户改写它。
- 新分享目录 0700 `vpnshare:vpnshare`；清单、实例配置、证书副本 0600。
- 新分享的订阅服务、网关、分享实例均以 `vpnshare` 运行；创建阶段 SSH 仍为 root。旧分享维持原有的 root 运行与权限，不强行改造。
- systemd 硬化：`NoNewPrivileges=yes`、`ProtectSystem=strict`、`ProtectHome=yes`、`PrivateTmp=yes`、`ReadWritePaths=/srv/vpnshare/<id>`、`RestrictAddressFamilies=AF_INET AF_INET6`。
- 防火墙助手通过单元里的 `ExecStartPost=+...` / `ExecStopPost=+...` 以 root 执行，规则带 `vpnshare-<id>-<port>` 注释，幂等。

### 订阅服务

- 脚本按 `<port> <dir> [--cert ... --key ...]` 启动；期望的 token 从 `<dir>/share.json` 读取（无清单时 = 目录名，兼容旧分享）。
- 路由只有两条：`/sub/<token>` 返回 YAML，`/singbox/<token>` 返回 JSON；查询参数 `?format=singbox` 作为兼容别名。
- 只允许 GET/HEAD；其余方法 405；路径不匹配 404。
- 禁目录列举、禁点文件；单 IP 失败请求限速（30 次/分）；并发连接上限（如 64）；连接读写超时，防慢速攻击。
- 响应头：`Cache-Control: no-store`、`X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`。
- 访问日志记时间、来源 IP、格式、状态码，**不记原始 token**；日志超过 1 MB 截断。`last_access.json` 记录最近一次访问时间与 IP，供 UI 展示。

### 失败与清理

- 创建任一步失败 → 反向拆除本次创建的单元/文件/端口，再核验端口已关。
- 回滚本身失败时，把「原始错误 + 残留清单」一起返回，不静默成功。
- 作废幂等；删除后核验端口确实关闭。

## 接口

### 类型

```rust
pub struct ShareInfo {
    pub id: String,            // 内部 id；旧分享 = token
    pub server_id: i64,
    pub server_name: String,
    pub host: String,
    pub token: String,         // 仅展示/兼容；变更操作一律用 id
    pub port: u16,             // 订阅 HTTPS 端口
    pub active: bool,          // 订阅服务/所需单元是否在运行（旧分享=HTTP 单元）
    pub url: String,           // YAML 链接
    pub url_singbox: String,   // /singbox/<token>
    pub node_names: Vec<String>,
    pub legacy: bool,
    pub status: ShareStatus,
    pub status_detail: Option<String>,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub rate_up_mbps: u32,     // 0 = 不限
    pub rate_down_mbps: u32,
    pub expires_at: Option<String>,      // RFC3339
    pub last_access_at: Option<String>,
    pub last_access_ip: Option<String>,
}

#[serde(rename_all = "snake_case")]
pub enum ShareStatus {
    Active,
    BlockedQuota,
    BlockedExpired,
    Degraded,
    Legacy,
}

pub struct ShareCandidate {
    pub node: Node,
    pub shareable: bool,
    pub reason: Option<String>,
}

pub struct ShareTlsStatus {
    pub domain: String,
    pub ready: bool,
    pub cert_expires_at: Option<String>,
    pub detail: String,
}
```

### IPC

| 命令 | 参数 | 返回/说明 |
|---|---|---|
| `list_shares_cmd` | — | `ShareList { shares, unreachable }` |
| `list_share_candidates_cmd` | `serverId` | `Vec<ShareCandidate>`；后端与前端共用准入判断 |
| `create_share_cmd` | `serverId, nodeIds, quotaGb, rateUpMbps, rateDownMbps, expiresInDays` | `ShareInfo`；先校验候选与 TLS 就绪 |
| `update_share_cmd` | `serverId, id, quotaGb, rateUpMbps, rateDownMbps, expiresInDays` | `ShareInfo`；停-写-起网关 |
| `reset_share_usage_cmd` | `serverId, id` | `ShareInfo` |
| `delete_share_cmd` | `serverId, id` | `void`；幂等 |
| `prepare_share_tls_cmd` | `serverId` | `ShareTlsStatus`；DNS/80/certbot/签发/钩子 |
| `share_tls_status_cmd` | `serverId` | `ShareTlsStatus` |

### 设置与迁移

- 迁移 v18：`servers` 加 `share_domain TEXT NOT NULL DEFAULT ''`；同步 `row_to_server` 与全部 SELECT（`domain.rs` 注释点名的同步点）。
- 全局设置 `share_acme_email`。
- 服务器编辑表单加「分享域名」；设置页加「ACME 邮箱」。

### 链接路径

- YAML：`/sub/<token>`（旧链接不变）。
- sing-box JSON：`/singbox/<token>`（规范形式）；同时接受 `/sub/<token>?format=singbox`，方便只改查询参数的客户端。
- 旧分享没有 JSON 快照，`url_singbox` 为空字符串，UI 禁用复制。

## 界面

- 分享页列表列：链接、节点、服务器、已用/配额（进度条）、速率、到期、状态、操作。
- 链接列提供两个复制按钮：YAML / sing-box；复制失败分别提示。
- 状态：运行中 / 已停用（超额）/ 已停用（到期）/ 异常（原因）/ 旧版·无管控。
- 操作：编辑、重置用量、作废（沿用 `ConfirmButton`）。重置用量只清用量；到期停用需在编辑里同时延长到期。
- 新建向导：选服务器 → 只列 `list_share_candidates_cmd` 的候选（不可分享的置灰并显示原因）→ 配额/速率/到期（默认 100 GB / 100 / 100 Mbps / 30 天）→ 生成。
- 向导顶部显示 TLS 状态：未配置域名/邮箱时给出配置入口并禁用生成；证书过期/不可用显示原因。
- 页面提示改为四条：HTTPS、分享专属凭据、配额/到期、快照不随主服务更新；删除「明文 HTTP」与「仅 YAML」。
- 旧分享行只保留复制 YAML 与作废；编辑/重置禁用。

## 错误处理

| 情况 | 行为 |
|---|---|
| 未配域名/邮箱 | 拒绝创建，指向配置入口 |
| 域名解析与服务器地址不匹配 | 报错并显示两边解析结果 |
| 80 端口被占 | 报错；HTTP-01 无法完成，不降级 |
| certbot 不可安装/签发失败 | 报错并附 certbot 输出 |
| 节点不满足准入 | 创建前逐条报原因 |
| 主配置缺失/解析失败 | 报错并指明协议与路径 |
| 缺少 xray/sing-box/ssserver | 报错并指明缺哪个 |
| 端口段耗尽（TCP/UDP） | 报错，不覆盖 |
| 写文件/启动单元失败 | 回滚本次创建内容，附残留清单 |
| 验证失败（单元 inactive/监听不符/HTTPS 非 200） | 回滚并报具体判据 |
| 服务器连不上 | 进 `unreachable`，列表其余照常 |
| 旧分享仍在服务器上 | 沿用现有升级逻辑；补防火墙持久化 |

## 测试

### 后端（Rust，FakeSshServer 驱动）

- 准入规则逐条：非自建、不属于该机、挂中转、隐私、ssr、未启用；上游出口必须可分享。
- 克隆金样：Xray / sing-box / SS 主配置 → 分享配置；上游专用配置 → 只复制被选出口的出站与路由，断言入站 `127.0.0.1`、新端口/新凭据、出站与 TLS/stream 字段原样保留、无 `api`/`experimental`、无 `0.0.0.0`、无同机其它出口凭据。
- TLS 自动检测：空 `share_domain` + 有效证书 → 返回 SAN 域名；多 SAN 命中配置值；配置值与 SAN 不一致拒绝；有效证书不要求邮箱；过期证书续签使用 SAN 域名。
- token/id 校验：16/32 位十六进制、大小写、注入字符；变更操作只接受 id。
- URL：HTTPS、域名、IPv6 字面量（旧分享）加方括号。
- 端口分配：TCP 占用、UDP 占用、SS 双栈占用、范围耗尽。
- 清单/用量/旧版识别与状态推导；配额与到期换算；1 GB = 1024³。
- TLS：DNS 不匹配、80 占用、certbot 缺失、签发失败、成功路径的命令序列。
- 创建/更新/重置/作废的命令序列；各步失败注入并断言回滚；作废幂等。

### 网关（Python，stdlib `unittest`）

- 令牌桶速率、TCP 背压、UDP 丢包降速。
- 配额精确停、到期停、恢复；状态原子落盘与重启续计。
- TCP 转发正确性；UDP 会话映射与空闲回收。
- 坏配置、端口占用、目标不可达的失败路径。

### 前端（Vitest）

- 列表新字段、进度条、五种状态、旧版行。
- 向导准入置灰与原因、TLS 未就绪禁用、参数边界。
- 双格式复制与失败提示。
- 编辑/重置/作废确认与刷新。

### 真机验证（实施计划单列）

- 真实域名签发证书；HTTPS 取 YAML/JSON 且证书校验通过。
- mihomo 与 sing-box 各导入一次并实际连通。
- 跑流量核对 `usage.json`；测限速接近设定值。
- 超额、到期、作废后旧配置立即失效。
- 服务器重启、应用关闭后分享仍可用；单元与防火墙规则自动恢复。

### 门禁

- `cargo test --manifest-path src-tauri/Cargo.toml`（带既有 `--skip net::runtime_tests --skip net::runtime_service_tests`）
- `npx tsc --noEmit`
- `npm test`
- `python -m unittest discover -s tests/python`

## 兼容与迁移

- **不建本地分享表**；服务器仍是唯一事实源。
- 旧分享目录名 = token，无 `share.json`：按旧逻辑列举，标记 `legacy`，不提供编辑/配额；作废接口用 id（= token）仍可用。
- 旧 token 16 位、新 token 32 位，校验收两种；内部 id 固定 16 位。
- 旧 YAML 链接 `/sub/<token>` 不变；新分享同时提供 `/singbox/<token>`。
- 旧分享在列举时补写防火墙持久化单元（`ExecStartPost`/`ExecStopPost`，维持原 root 运行与目录权限，不引入 TLS、不改凭据/端口/内容），修复重启失联。
- `delete_share_cmd` 参数由 token 改为 id；前端同步更新。
- 迁移 v18 只加列，不改既有行。

## 非目标与已知限制

- 不支持跨服务器分享；需要就发多条。
- 不支持分享挂中转/隐私节点。上游出口自 2026-10-20 起可分享。
- 一条分享的配额与速率是所有使用该链接的人**共享**的；不按接收方分别计量。
- Python 网关的吞吐有上限；若真机验证发现成为瓶颈，可在不改变配置/状态接口的前提下替换为静态编译的 Rust 助手。
- HTTP-01 要求 80 端口在签发与续期时可用；不支持 DNS-01。
- 用量按网关转发字节计，含协议开销；进程崩溃最多丢 5 秒用量。
- 分享是快照：主服务后续改动不更新已有分享，需作废重建。
- 旧分享仍无流量管控，且其快照含 owner 旧凭据；只能作废，不能追回已下载的配置。
