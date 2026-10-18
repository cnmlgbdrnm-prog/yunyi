# vpn-ctl P3 设计文档（中转链路：WireGuard 建链 + 接管管理 + 链路可视化）

- 日期：2026-08-13
- 状态：已批准（头脑风暴完成；老板指示直接进入实施，免规格复审）
- 依赖：主规格 [`2026-08-13-vpn-ctl-design.md`](2026-08-13-vpn-ctl-design.md)（§10 P3 行）；P2 已落地（服务器管理、SSH 适配器、部署 job 队列）；P2 规格 §12 预留设计为本轮基础

## 1. 范围与验收

P3 交付：

1. **WG 建链**：中转机 ↔ 落地机 WireGuard 隧道一键建立（工具全托管：探测 → 安装 → 密钥 → 配置 → 启动 → 握手验证 → 写回），经统一 job 队列执行。
2. **两种客户端形态**：模式 A（forward）clash 零改动（中转机公网端口 DNAT 进隧道，节点入口改指中转机）；模式 B（peer）设备 WG 客户端（每设备专属配置，默认分网）。
3. **落地机监听收敛**：建链向导默认勾选「建链后收敛落地机监听到 WG 内网」（可取消），拆链自动恢复。
4. **既有链路接管**：探测两机已握手（公钥互认 + 握手非零）→ 主按钮「接管管理」（纯 DB 登记 + 节点改指，不动服务器配置）；未握手 → 「重新建链」（覆盖前自动备份原配置）。
5. **链路可视化**：独立「中转」页（建链向导 + 链路列表 + 状态监控 + 设备 peer 管理）+ 服务器页行内链路摘要（哪台中转哪台、谁被谁中转）。

**验收**（对应主规格 §10 P3）：任选两台已纳管服务器（一中转一落地），经工具一键建链成功，链路状态可见；落地机公网监听收敛后节点经中转机可用。

不在 P3：测速（P4）、多落地机批量建链（单条执行，多条可依次建）、WG 之外的中转协议（gost-TLS 已由本轮决策淘汰）。

## 2. 已确认决策（本轮头脑风暴）

- 隧道层：原生 WireGuard（工具全托管），不引入 Tailscale（第三方控制面、账号凭据出机器、不可离线测试）与 gost-TLS（要求客户端走 gost 链，机场场景无效）。
- WG 握手是协议内置行为（配置互填公钥后首次通信自动握手），工具建链无需人工干预；「探测握手」仅用于识别既有链路以接管。
- 客户端两种形态都支持：A）clash 节点零改动（中转 DNAT 进隧道）；B）设备 WG peer（每设备专属配置，默认分网）。
- 分网职责：WG 只能按 IP 网段路由，网站（域名）级分流由 clash 规则负责；模式 B 默认 `AllowedIPs = 隧道网段`（分网），不提供全局模式。
- 落地机监听收敛：向导默认勾选；收敛只改落地机入站监听（服务端口），不影响其出站上网；拆链自动恢复。
- 设备配置分发：二维码（手机官方 app 扫码导入）+ 复制文本 + 下载 .conf 三种都要。

## 3. 架构与模块

沿用 P1/P2 扁平模块布局，新增：

```
src-tauri/src/
├── relay/
│   ├── mod.rs          # 链路服务：CRUD、探测（probe）、接管（adopt）、状态查询、设备 peer 管理
│   ├── build.rs        # 建链/拆链执行器：WG 安装、配置生成与段级管理、握手等待、收敛/恢复、DNAT
│   └── config.rs       # wg0.conf / 设备 .conf 渲染与解析金样
├── jobs/mod.rs         # 扩展：JobRequest 枚举（Deploy / LinkBuild / LinkTeardown / PeerAdd / PeerRemove）
└── commands.rs         # 新增 IPC 命令（扁平参数形态，P2 线格式教训）
```

**数据流**：前端向导提交 → `build_link_cmd` 写 jobs 行 → 队列 worker 串行执行（单 job 持双 SSH 会话，P2 预留）→ `job://progress` 回传 → 成功写回 relay_rules + 节点改指；拆链/设备增删同样经队列。**状态查询不走队列**（P2 §12 决定）：独立轻量 SSH 查询通道。

**多链路**：一台中转机可服务多台落地机（relay_rules 多行，共享中转机 wg0 接口）。建链/拆链/设备增删一律**peer 段级管理**：读回 wg0.conf → 按 peer 公钥定位段 → 替换/追加/删除 → 写回 + `wg syncconf` 热加载，绝不整文件盲写覆盖其他链路。

## 4. 数据模型

### 4.1 relay_rules 重建（迁移）

现表由 P1 建立、**无任何写入路径**（空表确认后 DROP + CREATE）：

```sql
CREATE TABLE relay_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  relay_server_id INTEGER NOT NULL REFERENCES servers(id),
  landing_server_id INTEGER NOT NULL REFERENCES servers(id),
  client_mode TEXT NOT NULL CHECK (client_mode IN ('forward','peer')),
  wg_port INTEGER NOT NULL DEFAULT 51820,   -- 中转机 WG 监听端口
  forward_port INTEGER,                     -- 模式 A：中转公网转发端口；模式 B 为 NULL
  landing_port INTEGER NOT NULL,            -- 落地机服务端口（转发/访问目标）
  subnet TEXT NOT NULL,                     -- 隧道网段（10.9.x.0/24，自动分配未占用段）
  relay_wg_ip TEXT NOT NULL,                -- 10.9.x.1
  landing_wg_ip TEXT NOT NULL,              -- 10.9.x.2
  relay_pubkey TEXT,                        -- v3：中转本端公钥（公开材料）
  landing_pubkey TEXT,                      -- v3：落地本端公钥（公开材料）
  converged INTEGER NOT NULL DEFAULT 0,     -- 落地机监听是否已收敛（拆链按此决定恢复）
  status TEXT NOT NULL DEFAULT 'not-built'  -- not-built / building / built / imported / failed
);
```

- 网段分配：从 `10.9.0.0/24` 起取第一个未被 relay_rules 占用的段；删除链路释放网段（仅本表占用判定）。
- status 语义：`imported` = 接管登记（服务器配置非本工具所建）；拆链后回 `not-built`（保留行，可重建）。
- **v3 迁移（真机验收追加）**：`ALTER TABLE relay_rules ADD COLUMN relay_pubkey / landing_pubkey`。
  手动建好的链路接口名不一定是 wg0（实测为 wg1），状态查询/拆链必须按**公钥**定位 peer，
  不依赖接口名；公钥为公开材料，非敏感。`failed` 半成品行（建链中途取消/失败残留）允许被
  adopt 直接替换接管——否则用户永远卡在「已存在链路」且拆链会去动服务器。

### 4.2 nodes 加列

```sql
ALTER TABLE nodes ADD COLUMN relay_rule_id INTEGER REFERENCES relay_rules(id);
```

建链/接管成功后：落地机对应节点（`source='self-built'` 且指向该落地机）addr/port 改指——模式 A：addr=中转机公网 host、port=forward_port；模式 B：addr=landing_wg_ip、port=landing_port。拆链改回落地机原公网 host:port。`relay_rule_id` 记录关联，删除服务器时 has_dependents 已含 relay_rules 检查（既有），节点侧经此列定位。

### 4.3 device_peers 新表（模式 B）

```sql
CREATE TABLE device_peers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  rule_id INTEGER NOT NULL REFERENCES relay_rules(id),
  name TEXT NOT NULL,                 -- 设备名（如 iPhone）
  public_key TEXT NOT NULL,           -- 明文：需下发中转机 wg0（公钥非敏感）
  private_key TEXT NOT NULL,          -- seal 密文（base64）
  psk TEXT NOT NULL,                  -- seal 密文（base64）
  wg_ip TEXT NOT NULL,                -- subnet 内 .3 起最大+1 分配，删除不复用
  created_at TEXT NOT NULL
);
```

- 设备私钥/PSK 本地生成 → seal 入库；`get_peer_config_cmd` 解密即用即弃渲染 .conf，不进日志。
- 设备归属一条链路：加入中转机 wg0（peer 段 AllowedIPs = 该链路 subnet）。
- 拓扑：设备 ↔ 中转机（WG）↔ 落地机（WG）。中转机 `ip_forward=1` 路由转发；落地机 peer 段 AllowedIPs 覆盖设备网段（回程）。

## 5. 核心流程

### 5.1 建链（build / rebuild，job）

| 步骤 | 中转机 | 落地机 | 超时 |
|---|---|---|---|
| 1 探测 | `modprobe wireguard` + `command -v wg` + `ip route get 8.8.8.8`（取公网出口网卡名，DNAT 用） | 同上（无需网卡名） | 30s |
| 2 安装 | `apt-get update && apt-get install -y wireguard-tools`（已装跳过；modprobe 失败 → `WgNotSupported`） | 同左 | 300s |
| 3 密钥 | `wg genkey` + `wg pubkey`：**私钥留服务器不出机**，工具只回读公钥（同 Reality 策略） | 同左 | 30s |
| 4 配置 | 渲染 wg0.conf（Interface + Peer(落地) AllowedIPs=landing/32；模式 A 加 DNAT PostUp/PostDown；模式 B Peer(落地) AllowedIPs=subnet/24 供设备回程）；rebuild 时先备份原 conf（.bak），再**按 peer 公钥段级替换**（保留其他链路段） | 渲染 wg0.conf（Peer(中转) Endpoint=中转公网IP:wg_port） | 60s |
| 5 启动 | `systemctl enable --now wg-quick@wg0` + `sysctl -w net.ipv4.ip_forward=1`（持久化 /etc/sysctl.d/） | `systemctl enable --now wg-quick@wg0` | 60s |
| 6 握手 | `ping -c1 -W2 10.9.x.2` 触发 → `wg show wg0 latest-handshakes` 两端非零；30s 内未握手 → `HandshakeTimeout` → failed（可重试） | 被动等待 | 30s |
| 7 收敛（勾选时） | 无 | `cat` 读回 Xray/SS 配置 → serde 解析 listen/server 字段改为 landing_wg_ip → base64 管道写回 → `systemctl restart` → `ss -tln` 验证 10.9.x.2:port 监听 | 60s |
| 8 写回 | relay_rules.status=built + converged 标记 + 节点改指 + deploys 记录（无凭据） | 无 | — |

- 幂等：重复建链 = 段级覆盖 + 重启，不卸载。
- 模式 A DNAT 规则（挂在 wg0.conf PostUp/PostDown，随接口生命周期不残留）：
  ```
  iptables -t nat -A PREROUTING -i <出口网卡> -p tcp --dport forward_port -j DNAT --to-destination landing_wg_ip:landing_port
  iptables -A FORWARD -p tcp -d landing_wg_ip --dport landing_port -j ACCEPT
  ```

### 5.2 探测与接管（probe / adopt）

- `probe_link_cmd`：两端 SSH 只读回读。**接口名不假设 wg0**（真机验收发现手动链路用 wg1）：
  `wg show interfaces` 枚举 → 逐接口 `public-key` + `latest-handshakes`（有握手 epoch>0 的接口优先，
  其中 wg0 优先）→ 命中接口 `allowed-ips`（**不用 `dump`**——dump 首行含本端私钥，不得过日志/缓冲区）
  → 中转侧另回读 `listen-port` 与 iptables DNAT；`ip -br link` 检测 Tailscale（仅提示，不参与判断）。
  → `ProbeResult { relay_reachable, landing_reachable, relay_wg, landing_wg, handshake,
  relay_pubkey, landing_pubkey, wg_iface, tailscale, wg_port, subnet, forward_port }`
  （回读参数作为接管表单默认值）。探测是只读 dry-run，不落任何配置。
- 已握手（两端公钥互认 + 握手非零）→ UI 主按钮「接管管理」：`adopt_link_cmd` 纯 DB 写回
  （status=imported + **两端公钥入库** + 节点改指 + converged 按开关），**不走 job、不改任何服务器配置**；
  次按钮「重新建链」走 5.1 rebuild 路径。落地机已有 status=failed 的建链半成品行时，adopt 先级联
  删除该行再接管（恢复路径，避免卡在「已存在链路」）。
- 两端无 WG 或公钥不互认 → 未握手 → 直接「建链」。

### 5.3 拆链（teardown，job）

`teardown_link_cmd(rule_id, action)`，action 二选一：

- **`unregister`（解除登记，仅 imported 行）**：**纯登记解除，不发任何 SSH**——接口非本工具所建；
  仅清设备行 + 节点回指直连 + status=not-built（与 adopt 的纯 DB 语义对称）。built 行拒绝。
- **`full`（真实拆除）**：
  - **built / failed**：`wg-quick down wg0`（两端）→ 删接口配置 → converged=1 时恢复落地机监听
    （0.0.0.0）→ 中转 iptables 规则随 PostDown 清理 → 节点改回落地机原地址 → status=not-built。
    幂等：接口已不存在时跳过 down。
  - **imported**：**按公钥定位实际接口（不假设 wg0）**——`wg show interfaces` 枚举 →
    逐接口 `latest-handshakes` 找对端公钥所在接口 → 删该 peer 段（无剩余 peer 则
    `wg-quick down <iface>` + 删 conf，否则写回 + `wg syncconf <iface> <conf>`）→ 模式 A
    删中转 DNAT → 收敛恢复（仅有关联节点时）→ DB 复位。接口名来自远端输出，拼接进
    文件路径/命令前白名单校验（`[a-zA-Z0-9_-]{1,15}`）；缺公钥的历史行拒绝（提示走解除登记）。

**适用状态**：built / imported / failed；not-built 拒绝（`LinkNotBuilt`）。设备 peer 随拆链级联删除（见 §5.4）。拆链对共享中转机 wg0 接口执行 `wg-quick down/up` 会造成**其他链路数秒级短暂中断**（设计取舍：conf 段级保留，接口整存整取）。

### 5.4 设备 peer（模式 B）

- 添加：本地生成密钥对 + PSK → seal 入库 → job：中转机 wg0.conf **追加 peer 段**（公钥 + AllowedIPs=设备自身 wg_ip/32）→ `wg syncconf wg0` 热加载。
- 中转机侧 AllowedIPs 恒为 /32 唯一前缀（落地段 landing_wg_ip/32、设备段各自 wg_ip/32）：WG cryptokey 路由同一接口上同前缀 AllowedIPs 会互相覆盖（后添加者抹除先者），若落地段/设备段写 subnet/24 将导致添加设备后落地机不可达；落地机回程由落地侧 conf AllowedIPs=subnet/24 承担（§6.2），设备 .conf 单 peer 侧保持 subnet/24（§6.3）。
- 删除：job：按公钥移除 peer 段 → syncconf → 删行。
- 设备密钥体系为 **X25519**（与 WireGuard 同构：32 字节私钥 base64 与 `wg genkey` 输出一致）；私钥原始 32 字节 seal 入库，导出时解码渲染、即用即弃。
- 拆链时级联删除该链路全部设备行（实施修正：原「peer 行保留」在 FK 引用与 syncconf 生命周期下会造成设备行不可删的死锁，改为拆链即级联删除；重建后设备需重新添加）。
- 配置分发：`get_peer_config_cmd(peer_id)` 解密渲染标准 .conf（Interface：设备私钥/地址/DNS；Peer：中转公网 IP:wg_port、中转公钥、PresharedKey、AllowedIPs=subnet/24、PersistentKeepalive=25）→ 前端二维码（qrcode 依赖）/复制/下载。

### 5.5 状态监控（轻量查询，不走队列）

`refresh_link_status_cmd(rule_id)`：两端 `wg show interfaces` 枚举 → **按对端公钥**（规则行 relay_pubkey/landing_pubkey）定位所在接口与 peer 行 → 该 peer 的 `latest-handshakes` + `transfer`（历史行公钥为 NULL 时回退 wg0 聚合）→ `LinkStatus { ok, relay_handshake_secs_ago, landing_handshake_secs_ago, relay_rx_bytes, relay_tx_bytes, landing_rx_bytes, landing_tx_bytes, checked_at, error? }`。链路列表「刷新状态」+ 打开中转页自动查一轮；并发限制（同 P1 同步并发模式）+ 15s 超时；失败显示「状态未知」不阻塞页面。

## 6. 配置金样

### 6.1 中转机 wg0.conf（模式 A，含收敛开关）

```ini
[Interface]
PrivateKey = <中转私钥，服务器生成>
ListenPort = 51820
Address = 10.9.0.1/24
PostUp = iptables -t nat -A PREROUTING -i eth0 -p tcp --dport 443 -j DNAT --to-destination 10.9.0.2:443
PostUp = iptables -A FORWARD -p tcp -d 10.9.0.2 --dport 443 -j ACCEPT
PostDown = iptables -t nat -D PREROUTING -i eth0 -p tcp --dport 443 -j DNAT --to-destination 10.9.0.2:443
PostDown = iptables -A FORWARD -p tcp -d 10.9.0.2 --dport 443 -j ACCEPT

[Peer]
PublicKey = <落地机公钥>
AllowedIPs = 10.9.0.2/32
PersistentKeepalive = 25
```

（模式 B：无 DNAT 段；Peer(落地) AllowedIPs = 10.9.0.2/32；设备 peer 段 AllowedIPs = 该设备 wg_ip/32（如 10.9.0.3/32），追加在末尾。落地段与设备段均为唯一 /32 前缀——WG cryptokey 路由同一接口上同前缀 AllowedIPs 会互相覆盖，subnet/24 覆盖将导致添加设备后落地机不可达；落地机回程由落地侧 conf（§6.2）AllowedIPs=subnet/24 承担。）

### 6.2 落地机 wg0.conf

```ini
[Interface]
PrivateKey = <落地私钥，服务器生成>
Address = 10.9.0.2/24

[Peer]
PublicKey = <中转公钥>
Endpoint = <中转公网IP>:51820
AllowedIPs = 10.9.0.0/24
PersistentKeepalive = 25
```

### 6.3 设备 .conf（模式 B）

```ini
[Interface]
PrivateKey = <设备私钥>
Address = 10.9.0.3/32
DNS = 1.1.1.1

[Peer]
PublicKey = <中转公钥>
PresharedKey = <PSK>
Endpoint = <中转公网IP>:51820
AllowedIPs = 10.9.0.0/24
PersistentKeepalive = 25
```

### 6.4 收敛改动（落地机 Xray/SS 配置）

- Xray config.json：inbound 无 listen 或 `"listen":"0.0.0.0"` → `"listen":"10.9.x.2"`；其余不动。
- SS config.json：`"server":"0.0.0.0"` → `"server":"10.9.x.2"`。
- 读回（cat）→ serde 解析 → 改字段 → base64 管道写回 → restart → `ss -tln` 验证。恢复（拆链）= 反向写回 0.0.0.0。

## 7. 安全

- WG 私钥两端服务器上 `wg genkey` 生成，永不出服务器（工具只回读公钥，同 Reality 私钥策略）；设备私钥/PSK 本地生成 seal 入库，导出即用即弃，不进日志、不进 error 文本。
- 覆盖建链前备份原 wg0.conf 为 `.bak`；rebuild 按 peer 公钥段级替换，不动其他链路段。
- iptables 规则只经 wg0.conf PostUp/PostDown 管理，随接口生命周期清理，不残留系统规则。
- 收敛只改服务监听地址，不改出站；拆链对称恢复。
- 命令白名单延续 P2：固定命令序列 + 参数校验（端口 u16、网段/IP 格式、base64 字符集、路径白名单 `/etc/wireguard` `/etc/sysctl.d` `/usr/local/etc/xray` `/etc/shadowsocks-rust`）。
- 凭据材料（服务器密码/私钥、设备私钥/PSK）不进 job 事件、日志与错误文本。

## 8. 错误处理与超时

沿用 SshError/ServersError 模式（字符串序列化回前端），新增：

- `WgNotSupported { os_id }`（modprobe 失败/内核不支持）
- `WgCommandFailed { step, stderr_tail }`（wg/iptables/sysctl 非零退出，stderr 末尾 500 字符脱敏）
- `LinkNotBuilt` / `LinkBuilt`（状态不匹配的操作）
- `PortInUse { port }`（forward_port/wg_port 被占）
- `HandshakeTimeout`（建链 30s 未握手 → failed 可重试）
- `PeerIpExhausted`（设备 IP 分配满）
- `ProbeFailed { side }`（探测 SSH 失败侧）

超时基线：探测 30s、安装 600s（沿用 P2 既有 INSTALL_TIMEOUT 常量）、配置 60s、握手等待 30s、状态查询 15s。失败不回滚已落配置（沿用 P2），重试探测步骤识别现状继续。

## 9. UI（左侧导航新增「中转」页，5 页变 6 页）

- **建链向导**（内联表单，沿用 DeployWizard 模式）：选中转机（下拉：kind=relay 的服务器；为空提示先去服务器页标注类型）→ 选落地机（kind=landing）→ 模式 A/B（radio）→ 端口（A：默认 443 可改；B：wg 默认 51820 可改）→ 开关「建链后收敛落地机监听到 WG 内网」（默认勾选）→ 提交探测 → 已握手：「接管管理」（主）/「重新建链」（次）/取消；未握手：直接「建链」。分步进度 + 日志 + 取消（复用 job://progress）。
- **链路列表**：名称、中转 → 落地（含 host）、模式、端口、状态徽标（not-built/built/imported/failed）、最近握手（相对时间）、传输字节、操作（刷新状态 / 拆链 / 删除）。built 状态删除被拒（提示先拆链）。
- **设备面板**（模式 B 链路展开）：设备列表（名称、wg_ip）、添加设备（名称输入）、配置弹窗（二维码 + 复制文本 + 下载 .conf）、删除设备。
- **服务器页行内摘要**：每台机显示链路关系（「中转 → ASHURA」/「被 xxx 中转」），无链路不显示。
- UI 一律不使用 emoji（图标用文字/CSS）；新增前端依赖 `qrcode`。

## 10. IPC 命令清单（新增，扁平参数形态）

| 命令 | 签名（Rust 内部函数） | 说明 |
|---|---|---|
| `list_relay_rules_cmd` | `list_relay_rules(state) -> Vec<RelayRule>` | 服务器页摘要共用 |
| `probe_link_cmd` | `probe_link(state, relay_server_id, landing_server_id) -> ProbeResult` | 只读 dry-run |
| `build_link_cmd` | `build_link(state, name, relay_server_id, landing_server_id, client_mode, wg_port, forward_port: Option, landing_port, converged, action: 'build'\|'rebuild') -> i64` | 返回 job_id |
| `adopt_link_cmd` | `adopt_link(state, name, relay_server_id, landing_server_id, client_mode, wg_port, forward_port: Option, landing_port, subnet, converged) -> RelayRule` | 纯 DB 同步命令，不走 job |
| `teardown_link_cmd` | `teardown_link(state, rule_id) -> i64` | job_id |
| `delete_link_rule_cmd` | `delete_link_rule(state, rule_id) -> ()` | 仅 not-built；built/imported 需先拆链 |
| `refresh_link_status_cmd` | `refresh_link_status(state, rule_id) -> LinkStatus` | 轻量查询不走队列 |
| `list_device_peers_cmd` | `list_device_peers(state, rule_id) -> Vec<DevicePeer>` | |
| `add_device_peer_cmd` | `add_device_peer(state, rule_id, name) -> i64` | job_id（SSH 下发中转机） |
| `remove_device_peer_cmd` | `remove_device_peer(state, peer_id) -> i64` | job_id |
| `get_peer_config_cmd` | `get_peer_config(state, peer_id) -> String` | 解密渲染 .conf |

事件：沿用 `job://progress`（payload 扩展 job_kind 区分 deploy/link/peer）。前端 TS 沿用 P1 契约教训：响应 snake_case、invoke 参数 camelCase、命令扁平参数（P2 手工验收修复记录）。

## 11. 测试策略（对应主规格 §9）

- **russh loopback 集成测**（FakeSshServer 扩展脚本化响应：`modprobe`/`wg genkey`/`wg pubkey`/`wg show`/`wg syncconf`/`iptables`/`sysctl`/`ip route`/`ping`）：建链全序列（含收敛）、rebuild 段级替换（含 .bak 备份与多链路保留）、已握手接管、拆链恢复（converged 对称）、握手超时失败、端口占用、peer 增删热加载。
- **Rust 单测**：两端 wg0.conf 金样（A/B）、设备 .conf 金样、DNAT PostUp/PostDown 金样、wg0.conf 段级解析（追加/替换/删除）、网段自动分配、设备 IP 分配不回收、收敛改动 Xray/SS 金样、迁移（relay_rules 重建 + nodes 加列 + 数据保留）、状态查询解析、错误脱敏。
- **前端 Vitest**：中转页渲染、建链向导三态（未握手→建链、已握手→接管/重新建链）、进度与取消、设备面板（添加/删除/配置弹窗三入口）、服务器页链路摘要、mockIPC 全命令覆盖（扁平参数形态断言）。
- IPC 线格式教训：mock 必须断言扁平参数键名，防结构体参数 input 键回归。

## 12. 与 P1/P2 的联动

- 删除服务器保护已含 relay_rules 检查（P1 has_dependents）；节点侧 relay_rule_id 定位改指/回改。
- P2 部署节点（source=self-built）建链后改指；订阅导入节点不参与改指。
- 配置生成器（clash YAML）零改动：改指是节点数据层 addr/port 变化。
- JobQueue 扩展 JobRequest 枚举（Deploy 保留），worker 串行语义、取消、启动恢复、事件全沿用。
