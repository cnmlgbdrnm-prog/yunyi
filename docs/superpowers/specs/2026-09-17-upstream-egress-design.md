# 上游出口（静态住宅 IP 代理）设计

- 日期：2026-09-17
- 状态：设计已确认（老板逐节批准；指示写完规格直接进入实施，免规格复审）
- 依赖：[`2026-08-13-vpn-ctl-p2-design.md`](2026-08-13-vpn-ctl-p2-design.md)（服务器管理、Xray 部署）与 [`2026-08-13-vpn-ctl-p3-design.md`](2026-08-13-vpn-ctl-p3-design.md)（中转链路、节点改指）

**目标：** 让「客户端 → 中转机 → 买来的静态住宅 IP 代理 → 目标」这条链路能被工具一键建立与收回。客户端拿到的仍然是一条普通的 VLESS+REALITY 节点 —— 它不知道后面挂着住宅代理。

**范围：** 新增「上游出口」实体 + 中转机上的一个专用 Xray 实例。

**不做：**
- **不做客户端直连形态**（不把 socks5/http 加进节点导出面）——本轮明确排除
- 不做「出口 IP 是否属于住宅段」的判定（工具没有该数据源，不猜）
- 不用 WireGuard：住宅代理不可能成为 WG 对端
- 不做纯端口转发（见 §3.3，UDP 判死）

## 1. 需求前提（本轮已确认）

1. 上游是买来的静态住宅 IP 代理，**SOCKS5 与 HTTP(S) 两种形态都要支持**
2. **必须经中转**：客户端只认中转机的地址
3. **UDP 要**（QUIC／游戏／语音）
4. 一台中转机可能挂多个住宅 IP

## 2. 现状（实现前已核对源码）

1. **链路两端必须都是 SSH 主机**：`servers/deploy.rs` 的部署、BBR、通道 iptables，`relay/build.rs:208-210` 的 `ensure_wg_ready`（modprobe + 装 wg 工具链）、`build.rs:244-341` 的 `wg genkey` → 下发 wg0.conf → `wg-quick@wg0` → 握手校验，全部建立在「有 shell」之上。`Server` 结构体（`domain.rs:79-98`）只有 host/port/user/auth_kind/credential，就是对这件事的建模。住宅代理给不了 shell。
2. **节点协议面没有 socks5/http**：URI 订阅只认 vmess/vless/trojan/hysteria2/tuic/ss/ssr（`subs/parse.rs:5`）；未知协议在配置生成时报「未知协议且无 clash_raw」（`config/mod.rs:266-273`，sing-box 侧同构）。**本设计刻意不进这个面**（§5.4）。
3. **`servers/channels.rs:36` 的 `const PROTOCOLS = &["socks5","http"]` 是防火墙通道的协议标签**（只开关 iptables 端口规则），不是代理协议实现 —— 极易误读，此处显式排除。
4. **可复用的现成零件**：
   - Xray 二进制安装流程与 `XRAY_CONFIG_PATH` 常量（`servers/deploy.rs:26`）
   - `run_cmd` / `run_cmd_ok`（`deploy.rs:647/667`，带取消感知）、`write_remote_file`（base64 管道写远端文件）
   - **节点写回时 `addr` 直接取 `server.host`**（`deploy.rs:1171`）—— 这条是「部署到中转机就等于节点指向中转机」的省力点
   - `crypto::seal_param` / `open_param`（`crypto/mod.rs:158/173`）—— 与 device_peers 存私钥同一套
   - `PortInUse { port }` 错误形态（`relay/mod.rs:68`，判定在 `build.rs:238`）
   - 迁移机制：`db/migrations.rs` 的 `MIGRATIONS` 数组，**索引即版本号**，`PRAGMA user_version` 记录（追加一条即 v11）
   - IPC 注册点：`main.rs:136` 的 `generate_handler![...]`（现有 71 条）
   - Job 进度框架：`job://progress` 事件 + 队列串行执行
5. **现有 Xray 部署是整文件重渲染**：`read_existing_ports`（`deploy.rs:181`）复用已有端口、`merge_deploy_protocols`（`commands.rs:603`）合并协议集，整份 config.json 由工具重新生成 —— 这是本设计要**避开**的耦合面（§3.2）。
6. **UDP 事实核实**（决定了整个方案形状）：
   - VLESS 的 UDP/MUX 命令**不带 flow 值**，Vision 只管 TCP 命令 → **REALITY+Vision 入站的 UDP 是通的**
   - Xray **socks outbound 原生支持 UDP**（XUDP 未启用时走协议自身 UDP 通道）
   - Xray **http outbound 只能 TCP**（HTTP CONNECT 的协议限制，非实现取舍）
   - ⚠️ **Vision 会拦截发往 443 的 UDP（QUIC）**，逼客户端回落 HTTPS；`xtls-rprx-vision-udp443` 变体不拦截。这是**所有现有 REALITY 节点都有的既有行为**，本功能不新引入，但要在 UI 上说明

## 3. 设计

### 3.1 拓扑

```
客户端 ──VLESS+REALITY──▶ 中转机:listen_port ──┐
                                               │ 专用 Xray 实例
                                               └─ routing 按 inboundTag 配对
                                                       │
                                                       ▼
                                           socks5/http ──▶ 住宅代理 ──▶ 目标
```

客户端视角：一条普通 VLESS+REALITY 节点，`addr` = 中转机 host，`port` = listen_port。

### 3.2 核心决定一：中转机上跑**专用的第二个 Xray 实例**

不在 `deploy` 已经在管的那份配置里塞 outbound。理由是风险隔离：现有 deploy 是整文件重渲染（§2.5），上游出口一旦混进同一份 config，任何一次「重新部署协议／补装协议」都可能把用户的住宅线路静默搞断，且这种断法极难排查。

独立实例的形态：

| 项 | 值 |
|---|---|
| 配置 | `/usr/local/etc/xray-upstream/config.json` |
| systemd 单元 | `/etc/systemd/system/xray-upstream.service` |
| 二进制 | 复用现有 Xray 安装路径与流程（已装则不重装） |
| 路由 | 该实例独占，与 deploy 的 Xray、与 WireGuard 无交集 |

代价：多一个 unit。收益：拆除干净（停 unit + 删文件删 unit），与协议管理 IPC 零耦合，一次部署失误波及不到既有线路。

### 3.3 核心决定二：一条出口 = 一个 inbound + 一个 outbound + 一条 routing 配对

一台中转机上 N 条出口共存在同一个实例里：每个 inbound 带唯一 `tag`，对应一个 outbound，`routing.rules` 按 `inboundTag → outboundTag` 配对。这是 Xray 的标准能力。

**为什么不用纯端口转发**：socks5 的 UDP associate 会让客户端拿到的 relay 地址是**供应商自己的**，UDP 直接绕过中转机；iptables DNAT 改不了应用层握手内容。即「UDP 要」这一条判死了纯转发方案。

客户端入站协议**固定 VLESS+REALITY**：复用现有 REALITY 材料生成与节点 URI 形态（客户端解析面 `config/mod.rs` 已有 vless/reality 全套映射），伪装域名默认 `www.microsoft.com`（与 `DeployWizard.tsx:51`、`deploy.rs:865` 一致）。

配置形状（金样见测试）：

```json
{
  "inbounds": [{
    "listen": "0.0.0.0", "port": 443, "protocol": "vless", "tag": "up-1",
    "settings": { "clients": [{ "id": "<uuid>", "flow": "xtls-rprx-vision" }], "decryption": "none" },
    "streamSettings": {
      "network": "tcp", "security": "reality",
      "realitySettings": { "dest": "www.microsoft.com:443", "serverNames": ["www.microsoft.com"],
                           "privateKey": "<priv>", "shortIds": ["<sid>"] }
    },
    "sniffing": { "enabled": true, "destOverride": ["http", "tls", "quic"] }
  }],
  "outbounds": [
    { "protocol": "socks", "tag": "out-1",
      "settings": { "address": "1.2.3.4", "port": 1080, "user": "u", "pass": "p" } },
    { "protocol": "http", "tag": "out-2",
      "settings": { "address": "5.6.7.8", "port": 3128, "user": "u", "pass": "p" } }
  ],
  "routing": {
    "domainStrategy": "AsIs",
    "rules": [
      { "inboundTag": ["up-1"], "outboundTag": "out-1" }
      // 第二条出口同理：inbounds 里再加一条 tag="up-2" 的入站，此处省略
    ]
  }
}
```

两个要点：

- **outbound 的 `settings` 是扁平结构**（`address`/`port`/`user`/`pass`），**不是**入站那种 `servers[]` 数组 —— 与入站写法不同，容易写错（已核对官方文档）
- `domainStrategy: "AsIs"` 让域名保持未解析状态交给上游代理**远端解析**：既避开 GFW 的 DNS 污染，也避免 DNS 泄漏到本机

### 3.4 核心决定三：出口 IP 验证走本地 mihomo，不在中转机碰密码

**被否决的做法**：在中转机上 `curl --proxy socks5h://user:pass@host:port https://api.ipify.org`。它要求把住宅代理的密码塞进远端命令行，而中转机往往是别人托管的机器 —— `/proc/<pid>/cmdline`、shell 历史、可能的审计日志都会沾上。

**采用的做法**：经**整条链路**、由本地 mihomo 侧车请求 `https://api.ipify.org`，回显出口 IP。好处不止绕开密码问题 —— 它验证的是**完整链路**（客户端 → 中转机 → 住宅代理），比「中转机能连上代理」更接近用户真正关心的事。复用面也大：现有节点测速本来就在 mihomo 里注入节点做 HTTP 请求，加一个「取出口 IP」的探测动作即可。

## 4. 数据模型

迁移追加到 `db/migrations.rs` 的 `MIGRATIONS` 尾部（v11）：

```sql
CREATE TABLE upstreams (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id),  -- 中转机
  name TEXT NOT NULL,                                  -- 「住宅01」
  proto TEXT NOT NULL CHECK (proto IN ('socks5','http')),
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  username TEXT NOT NULL DEFAULT '',
  password TEXT NOT NULL,                              -- seal 密文
  listen_port INTEGER NOT NULL,                        -- 本实例的入站端口
  reality_dest TEXT NOT NULL,                          -- 伪装域名
  material_json TEXT NOT NULL,                         -- uuid/私钥/shortId（对齐 deploys 的存法）
  node_id TEXT,                                        -- 写回的节点 id
  egress_ip TEXT,                                      -- 最近一次探测到的出口 IP
  last_check_error TEXT,                               -- 最近一次失败原因（脱敏）
  created_at TEXT NOT NULL
);
```

- **账密**：密码经 `crypto::seal_param` 入库，解密即用即弃（同 `load_auth` 的纪律）
- **`material_json` 一旦生成就固定**：重建时复用而非重新生成 —— 否则客户端节点每次重建都要重新导入
- **删除服务器时**：`has_dependents` 检查要加上本表（现有实现只查 relay_rules），否则删出孤儿行
- 不建索引：单台中转机的出口行数量是「几个」，不值得

## 5. 流程

### 5.1 部署（新增／修改一条出口）

新增 job kind `upstream`，走现有队列：

1. **命令层校验**（同步、不进队列）：名称与 host 非空、端口范围、`proto` 白名单、`listen_port` 在该中转机上唯一且不与既有链路的 `wg_port`/`forward_port` 冲突（冲突按 `PortInUse` 形态报错）
2. 落库：首次生成 REALITY 材料（uuid/私钥/shortId），密码 seal
3. **job 执行**（SSH 到中转机）：
   - 探测系统与包管理器（复用现有探测步骤）
   - `command -v xray`；缺失则复用 deploy 的安装流程
   - **从 DB 读该服务器全部出口行，整份渲染**专用配置（多出口共存靠这一步）
   - 写 config 与 unit
   - 首次 `systemctl enable --now xray-upstream`；已存在则 `restart`
4. **写回节点**：`source='self-built'`、`protocol='vless'`、`addr` = 中转机 host、`port` = listen_port、`params_json` 与现有 REALITY 节点同构；节点名 `{中转机名}-{出口名}`；`node_id` 回写该行
5. `job://progress` 回传进度（复用现有面板）

**一个刻意的不对称**：relay 那边做的是「按 peer 公钥定位、段级替换」的精细活，这里**可以放心整文件重渲染**。因为整个实例是我们独占的、配置完全由 DB 派生 —— 不需要那套复杂逻辑，且天然幂等。

### 5.2 拆除

重渲染剩余出口 → 还有剩就 `restart`；一条不剩则 `systemctl disable --now` + 删 config + 删 unit。最后删节点、删行。

### 5.3 出口 IP 探测

独立动作（不绑死在部署流程里），也可在部署成功后自动跑一次：

- 经 mihomo 注入该节点，请求 `https://api.ipify.org` 取回显
- 成功 → 写 `egress_ip`，UI 展示
- 失败 → 写 `last_check_error`（脱敏），**不阻断部署**（配置已落地、节点已写回）

### 5.4 为什么不给节点导出面加 socks5/http

客户端侧永远说 VLESS+REALITY，socks5/http 只存在于中转机上、由工具管理。因此：**`config/mod.rs`、`config/singbox.rs`、订阅解析、DNS/分流规则全线不动** —— 这是本方案最大的省力点，也是「只做经中转」这个范围决定的直接结果。

## 6. UI

- **入口**：服务器页行内展开面板，新增 `UpstreamPanel.tsx`，位置与交互对齐现有 `ChannelPanel`（加/删/重建）。**不按 `kind` 过滤** —— 跟通道面板一致，少一条隐性规则（用户忘了标 kind 也能找到入口）
- **面板内容**：出口列表（名称、协议、上游 `host:port`、监听端口、出口 IP／错误红字）+「添加出口」
- **表单**：名称、协议（socks5/http）、上游地址、端口、用户名、密码、监听端口（默认 443）、伪装域名（默认 `www.microsoft.com`）
- **选 http 时显式提示**：「HTTP 上游不支持 UDP」（协议限制，见 §2.6）
- **进度**：复用 `JobProgressPanel`

## 7. 错误处理

| 场景 | 处理 |
|---|---|
| 端口冲突 | `PortInUse { port }` 形态（`relay/mod.rs:68` 现成） |
| Xray 缺失且装不上 | 复用现有安装失败错误形态（含包管理器探测结论） |
| 出口 IP 探测失败 | 不阻断；原因写 `last_check_error`，UI 红字 |
| 出口 IP 不是住宅段 | 只展示事实，不判定、不警告 —— 工具没有该数据源 |
| 密码泄漏面 | 命令文本不进事件（`run_cmd` 只把 step 标签用于进度与错误）；且本设计下密码根本不落远端命令行 |
| 配置渲染失败 | job 失败并保留远端原有配置（先写临时文件再原子替换） |

## 8. 测试

**Rust 单测：**
- 配置渲染金样：单出口；**多出口**（两个 inbound／两个 outbound／两条 routing 配对正确）
- outbound 扁平结构金样（防止写成入站那种 `servers[]` 数组）
- `domainStrategy: AsIs` 与 `sniffing` 存在性
- seal/open 往返（密码存取）
- 校验：端口范围、同一中转机 `listen_port` 重复被拒、`proto` 白名单
- 节点写回金样：与现有 REALITY 节点同构且 `addr` = 中转机 host
- 迁移：新表建立 + `user_version` 递增 + 既有数据不受影响
- 部署 job 分支（`FakeSshServer`）：首次 enable vs 已存在的 restart vs 全删时的 disable
- `has_dependents` 覆盖新表

**前端 vitest：**
- `UpstreamPanel` 增删改
- 选 http 时的 UDP 提示
- `egress_ip` / `last_check_error` 的展示

## 9. 验收

1. 在一台中转机上添加一条 socks5 住宅出口 → 部署成功 → 节点页出现一条 VLESS+REALITY 节点，`addr` = 中转机
2. 经该节点测速 → 延迟/带宽正常（链路通）
3. 出口 IP 探测 → 显示的是**住宅代理的 IP**，不是中转机的
4. 同一台中转机再加第二条出口（http 形态）→ 两条节点同时可用、互不影响（多出口共存）
5. 删掉第一条 → 第二条仍可用（拆除不误伤）
6. 在中转机上走一次现有 deploy（部署／补装协议）→ 上游出口线路不受影响（实例隔离）

## 10. 已知取舍

- **http 形态没有 UDP**：HTTP CONNECT 协议限制，不是实现取舍。UI 明示
- **QUIC 会被 Vision 拦截**：既有行为（所有 REALITY 节点都有），要 QUIC 得换 `xtls-rprx-vision-udp443`，本轮不做
- **多一个 systemd unit**：换取与 deploy 配置的彻底隔离，值
- **客户端侧多一跳**：客户端→中转机→住宅代理，比直连住宅代理多一段；这是「经中转」的固有代价，换来的是中转机线路质量与统一入口
