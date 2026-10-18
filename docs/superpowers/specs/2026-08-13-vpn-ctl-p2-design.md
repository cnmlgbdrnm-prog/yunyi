# vpn-ctl P2 设计文档（服务器/凭据管理 + SSH 适配器 + 落地机一键部署）

- 日期：2026-08-13
- 状态：已批准（头脑风暴完成；老板指示直接进入实施，免规格复审）
- 依赖：主规格 [`2026-08-13-vpn-ctl-design.md`](2026-08-13-vpn-ctl-design.md)（§10 P2 行）；P1 已落地（8 表、CredentialService、IPC 命令层、5 页 UI）

## 1. 范围与验收

P2 交付：

1. **服务器/凭据管理**：servers 表 CRUD；SSH 密码/私钥凭据加密存储；连通测试；一键免密（生成 ed25519 密钥对并下发公钥）。
2. **SSH 适配器**：russh async 连接/执行/超时（tokio::time::timeout 真实中断），远端写文件走 base64 管道。
3. **落地机一键部署**：Xray（VLESS+Reality）与 SS（2022-blake3-aes-256-gcm）两条部署线，探测→安装→配置→启动→写回，经统一 job 队列执行。
4. **job 队列基础设施**：jobs 表 + 串行执行器 + 进度事件 + 取消 + 失败重试入口。

**验收**（对应主规格 §10 P2）：一台全新 Ubuntu 落地机，经工具部署后节点出现在节点页并可用。

不在 P2：中转链路（P3）、测速（P4）、订阅同步迁入队列（P1 in-flight guard 已足够）。

## 2. 架构与模块

沿用 P1 扁平模块布局（不引入主规格架构图的 `core/` 子目录，避免无谓迁移）：

```
src-tauri/src/
├── ssh/mod.rs          # SSH 适配器：连接/执行/超时（russh async 客户端，tokio::time::timeout
│                       # 真实超时与取消）；远端文件写入走「echo base64 | base64 -d > 路径」管道
├── servers/
│   ├── mod.rs          # 服务器 CRUD、连通测试、自动免密
│   └── deploy.rs       # 部署命令序列（探测/安装/配置/启动/写回），Xray 与 SS 两条线
├── jobs/mod.rs         # job 队列：串行 worker、取消、进度事件、启动时中断恢复
├── config/mod.rs       # P1 生成器扩展：vless flow/reality-opts/fingerprint
└── commands.rs         # 新增 IPC 命令（_cmd 壳 + 内部函数，沿用 P1 模式）
```

新增 Rust 依赖：`russh`（纯 Rust SSH：生产客户端 + 测试 loopback 服务器 + ed25519 密钥生成，单库全覆盖，Windows 零 C/OpenSSL 构建依赖）；dev：`async-trait`。其余复用 P1 依赖。

russh 依赖声明为 `default-features = false` + `features = ["ring","flate2"]`：默认的 aws-lc-rs 后端在 Windows 需 NASM 汇编工具链，违背零 C/OpenSSL 构建链约束；ring 后端全程纯 Rust 构建（已落地于 `src-tauri/Cargo.toml`）。

**数据流**：前端向导提交 → `start_deploy_cmd` 写 jobs 行（queued）→ 队列 worker 串行执行 → 每步 `app.emit("job://progress", …)` 回传 UI → 成功写回 nodes（source=self-built）+ deploys 记录；失败 jobs.error 落库、UI 标红可重试。

**job 队列语义**（细化主规格 §5）：

- 单 worker 串行：同一时刻最多一个 job 在改服务器状态，配合"先探测再落配置"保证不出现半配置撕裂。
- 单个 job 内部可同时持多个 SSH 会话（为 P3 建链预留：一个 job 包圆中转+落地两机）。
- 取消是协作式：命令序列每步之间检查取消标志；russh async 下单条命令也可经 `tokio::select!` 在取消/超时到达时即时中断。
- 状态监控（P3 的 wg show / gost 日志周期查询）不走队列，是独立轻量查询通道。
- 应用重启时把遗留 running 状态的 jobs 标为 failed（错误：应用重启中断）。

## 3. 数据模型（复用 P1 八表，明确 P2 语义）

| 表 | P2 用法 |
|---|---|
| servers | CRUD 目标。`auth_ref` 指向 credentials.id（seal 密文的密码或私钥 PEM）；**auth_ref 与凭据内容永不出 IPC** |
| credentials | `seal(密码)` 或 `seal(私钥PEM)`，id 为 uuid |
| deploys | 每次成功部署追加一行：server_id、kind(`xray`/`ss`)、config_json（本次部署参数摘要，不含私钥）、deployed_at、status(`ok`) |
| jobs | kind=`deploy`；status：`queued`/`running`/`done`/`failed`/`cancelled`；progress 0–100 |
| nodes | 部署成功写回：source=`self-built`、protocol=`vless`/`ss`、addr=服务器 host、port、params_json（见 §5）、group_name=`自建`、enabled=1 |
| subscriptions / relay_rules / settings | P2 不动；relay_rules 为 P3 预留 |

**删除保护**：删除服务器前检查 deploys（与 P3 的 relay_rules）关联，存在则报"存在关联记录"并拒绝。

**Server 出 IPC 结构**：`{ id, name, host, port, user, auth_kind, kind, system, notes, created_at }`——不含 auth_ref 与任何凭据材料。

## 4. 核心流程

### 4.1 服务器 CRUD 与连通测试

- 创建：校验 host/port/user 非空、auth_kind ∈ {password,key}、凭据非空 → seal 入库 → 返回 Server。
- 更新：凭据字段为可选（None=保留原凭据）。
- 连通测试：SSH 连接 + 认证 + `uname -s` 与 `cat /etc/os-release` → 返回 `{ ok, system, message }`；成功时把探测到的系统（如 `Ubuntu 22.04`）回填 servers.system。连接超时 15s。
- 删除：FK 关联检查后删除。

### 4.2 一键免密

1. 本地 `russh::keys` 生成 ed25519 密钥对（公钥注释 `vpn-ctl-admin`）。
2. 用当前凭据登录 → `mkdir -p ~/.ssh && chmod 700 ~/.ssh` → 幂等追加（`grep -qF "<pub>" ~/.ssh/authorized_keys || echo "<pub>" >> ~/.ssh/authorized_keys`）→ `chmod 600 ~/.ssh/authorized_keys`。
3. 成功：私钥 PEM seal 入 credentials → servers 行 `auth_kind='key'`、auth_ref 指向新凭据 → **删除旧密码凭据（高安全优先，用完即弃）**。
4. key 失效时用户重新填写密码即可重新免密。私钥不提供导出（避免泄露面；工具本地库是唯一存放处）。

### 4.3 一键部署（Xray / SS 共用主线）

每步有独立超时（探测 30s、安装 600s、配置/启动 60s、验证 30s）；任一步失败→停止后续→jobs.error 落库→UI 标红可重试。**探测为 dry-run，不落任何配置。**

| 步骤 | Xray（VLESS+Reality） | SS（2022-blake3-aes-256-gcm） |
|---|---|---|
| 1 探测 | `uname -m`（x86_64/aarch64）；`cat /etc/os-release`（ID 含 ubuntu/debian，否则报"暂不支持的系统"）；`command -v xray`、`systemctl is-active xray`（已装检测）；`ss -tln "sport = :PORT"`（端口占用） | 同上；已装检测 `command -v ssserver` |
| 2 安装 | 官方脚本：`bash -c "$(curl -L https://github.com/XTLS/Xray-install/raw/main/install-release.sh)" @ install`（已装则跳过） | Rust 侧 reqwest 查 GitHub API 拿最新版本与资产名 → 远程 `curl -L <asset>` 下载 `shadowsocks-v{ver}.{target}.tar.xz` → `tar -xJ -C /usr/local/bin`（已装且可执行则跳过下载） |
| 3 配置 | 服务器上 `xray x25519` 生成密钥对（**私钥留在服务器**，解析回读公钥）；本地生成 uuid(v4) + shortId(8 位 hex)；本地渲染 config.json → `mkdir -p` + `echo '<b64>' \| base64 -d >` 写 `/usr/local/etc/xray/config.json`（路径白名单，内容 base64 无注入面） | 本地生成 32 字节随机密钥 → base64；本地渲染 config.json 写 `/etc/shadowsocks-rust/config.json`（同上管道）；工具自写 systemd unit `/etc/systemd/system/ssserver.service`（不依赖发布包内容） |
| 4 启动 | `systemctl enable --now xray && systemctl restart xray`；验证 `systemctl is-active` + `ss -tln` 端口监听 | `systemctl daemon-reload && systemctl enable --now ssserver`；同样验证 |
| 5 写回 | nodes 行（params_json 见 §5）+ deploys 行 | 同左 |

**幂等**：重复部署 = 覆盖配置 + 重启服务（不卸载、不留副本）；已装状态在向导探测结果中显示。

**向导默认值**：端口 Xray 默认 443、SS 默认随机 10000–60000（可改）；Xray Reality fallback 目标默认 `www.microsoft.com:443`（可改）。**身份材料全由工具生成，向导无密码输入项**：Xray=uuid(v4)（本地生成）、SS=32 字节随机密钥 base64（本地生成）——比用户设密码更安全。

## 5. 配置生成与 P1 生成器对接

### 5.1 Xray Reality config.json（金样字段，完整内容见计划）

inbound：`protocol=vless`、`settings.clients[0]={id, flow:"xtls-rprx-vision"}`、`decryption:"none"`；`streamSettings={network:"tcp", security:"reality", realitySettings:{dest, serverNames, privateKey, shortIds}}`；`sniffing.enabled=true`。outbound：freedom。

### 5.2 SS config.json（/etc/shadowsocks-rust/config.json）

```json
{"servers":[{"server":"0.0.0.0","server_port":PORT,"method":"2022-blake3-aes-256-gcm","password":"<base64(32B)>","mode":"tcp_and_udp"}],"locals":[]}
```

### 5.3 节点 params_json（写回 nodes 表）

- vless：`{"uuid":U,"security":"reality","type":"tcp","flow":"xtls-rprx-vision","sni":"www.microsoft.com","public-key":PUB,"short-id":SID,"fingerprint":"chrome"}`
- ss：`{"cipher":"2022-blake3-aes-256-gcm","password":"<base64 key>"}`

### 5.4 P1 生成器扩展（config/mod.rs `vless_mapping`）

- `flow` 非空 → 输出 `flow`；
- `security=="reality"` 且 `public-key` 存在 → 输出 `reality-opts: {public-key, short-id?}`；
- `fingerprint` 非空 → 输出 `fingerprint`。
- `sni→servername`、`security=reality→tls:true` 映射 P1 已有，不动。
- SS 的 cipher/password 是通用透传，2022-blake3 无需改生成器。

## 6. 安全（延续主规格 §7，不弱化）

- 密码/私钥只存 seal 密文；连通测试与部署解密即用即弃，不进日志、不进 events、不进 error 文本。
- SSH 命令白名单化：部署只执行固定命令序列，可变参数（端口/密码/UUID/公钥/路径）经校验（端口 u16、UUID 格式、base64 字符集、路径白名单 `/usr/local/etc/xray` `/etc/shadowsocks-rust` `/etc/systemd/system/ssserver.service` `~/.ssh`）后拼接，不提供任意命令入口。
- Reality 私钥在服务器生成，永不出服务器；工具只回读公钥。
- ed25519 私钥本地生成，seal 入库；公钥经 SSH 加密通道下发。
- 免密成功后删除密码凭据。

## 7. 错误处理与超时

`thiserror` 枚举（序列化为字符串回前端，沿用 P1 AppError 模式）：

- `AuthFailed`（认证失败，远端 stderr 脱敏后摘要）
- `NetworkUnreachable`（TCP/SSH 不可达）
- `CommandFailed { step, stderr_tail }`（命令非零退出，带远端 stderr 末尾 500 字符，脱敏）
- `Timeout { step }`（单命令超时）
- `UnsupportedSystem { os_id }`
- `PortInUse { port }`
- `KeyGenFailed`
- `JobCancelled`（取消即失败语义）

超时基线：SSH 连接 15s、探测命令 30s、安装 600s、配置/启动 60s、验证 30s。失败不回滚已落配置（安全起见不自动删），重试时探测步骤识别已装状态继续。

## 8. UI（左侧导航新增「服务器」页，4 页变 5 页；中转页 P3 再加）

- **服务器列表**：名称、host:port、用户、认证方式（密码/密钥）、类型（中转/落地）、系统、备注。
- **添加/编辑对话框**：名称、host、port、user、认证方式（密码输入框或私钥 PEM 多行框）、类型、备注。
- **行操作**：「连通测试」（几秒出结果 + 回填系统）、「一键免密」、「删除」（有关联时提示拒绝）。
- **一键部署向导**（沿用 P1 内联表单面板模式，P1 无 Modal 组件）：选服务器 → 选 Xray/SS → 端口（默认自动生成可改，Xray 显示 fallback 域名可改）→ 部署。分步进度（探测→安装→配置→启动→写回）+ 日志 + 取消按钮；失败标红显示原因、可重试。
- 进度来源：Tauri event `job://progress`（payload：job_id、step、message、progress、error?）。
- **UI 一律不使用 emoji**（沿用老板全局规则，图标用文字/CSS）。

## 9. 测试策略（对应主规格 §9）

- **russh loopback 集成测（P2 关键手段）**：russh（生产依赖，测试侧起本地 loopback SSH 服务器）脚本化 shell（命令表假响应 + 记录写入文件），真实验证：密码/密钥认证、连通测试、免密公钥下发（幂等）、Xray/SS 部署全序列、失败中断、取消、命令序列与金样一致性。
- **Rust 单测**：Xray Reality config.json 金样、SS config.json 金样、systemd unit 金样、params_json 金样、2022-blake3 密钥 base64 格式、job 队列（串行/取消/事件/启动恢复）、servers CRUD + FK 删除保护、凭据 seal/open 往返（扩展 key 类型）、生成器 reality-opts 输出金样。
- **前端 Vitest**：服务器页 CRUD 交互、连通测试/免密按钮联动、部署向导分步进度与取消、错误展示、既有 mockIPC 模式扩展。

## 10. IPC 命令清单（新增，沿用 P1 `_cmd` 命名与内部函数模式）

| 命令 | 签名（Rust 内部函数） | 说明 |
|---|---|---|
| `list_servers_cmd` | `list_servers(state) -> Vec<Server>` | |
| `create_server_cmd` | `create_server(state, name, host, port, user, auth_kind, kind, notes, credential) -> Server` | credential 明文仅此一次过 IPC，即 seal |
| `update_server_cmd` | `update_server(state, id, name, host, port, user, auth_kind, kind, notes, credential: Option) -> Server` | credential 为 None=保留原凭据 |
| `delete_server_cmd` | `delete_server(state, id) -> ()` | FK 保护 |
| `test_server_connection_cmd` | `test_server_connection(state, id) -> ConnectionTest` | `{ ok, system, message }` |
| `setup_passwordless_cmd` | `setup_passwordless(state, id) -> ()` | 生成+下发+切换+删密码 |
| `start_deploy_cmd` | `start_deploy(state, server_id, kind, port, reality_dest: Option) -> i64` | 返回 job_id |
| `list_jobs_cmd` | `list_jobs(state) -> Vec<Job>` | 最近 50 条倒序 |
| `cancel_job_cmd` | `cancel_job(state, job_id) -> ()` | 协作式取消 |

事件：`job://progress`（见 §8）。前端 TS 类型沿用 P1 契约教训：**响应字段 snake_case，invoke 参数 camelCase（Tauri 自动转换只作用于参数）**。命令线格式为**扁平参数**：Tauri 按参数名匹配顶层键并做 camelCase→snake_case 转换，结构体参数会产生 `input` 键（前端 `{ input: {...} }`）且嵌套字段不做大小写转换——实测 create/update/start_deploy 三条命令已改为扁平参数形态（本教训见 2026-08-13 手工验收修复）。

## 11. P2 决策记录（本轮头脑风暴新增）

- SSH 认证：密码+密钥都支持，附「一键免密」（生成 ed25519 下发公钥，成功切 key 并删密码凭据）。
- 部署协议固定最强档：Xray=VLESS+Reality、SS=2022-blake3-aes-256-gcm；向导只填端口/密码（默认自动生成）。
- 系统支持范围：Ubuntu/Debian 家族（老板落地机为 Ubuntu）。
- 部署机制：ssh2 命令序列（非上传单脚本）。
- job 队列：串行单 worker；订阅同步不迁入；P3 状态监控不走队列；单 job 内可持多个 SSH 会话。
- 部署幂等：覆盖配置+重启，不卸载。

## 12. P3 预留设计（本轮确认，实现留待 P3）

- **既有链路 = 正常管理对象**：建链向导探测到两机已握手（WG 公钥互认 / gost 配置指向落地机）时，主按钮「接管管理」——写入 relay_rule（status=`imported`），不改服务器任何配置，关联节点入口自动改指中转机；次按钮「重新建链」覆盖重建；gost-TLS 探测结果标"疑似"需确认。relay_rules.status 无 CHECK 约束，`imported` 无需迁移。
