# vpn-ctl 设计文档

- 日期：2026-08-13
- 状态：已批准（头脑风暴完成，待实施计划）
- 工作名：vpn-ctl（最终命名待定，内部路径一律使用中性名，不绑定产品名）

## 1. 目标与范围

一个**本地桌面工具**（个人使用），统一管理：

1. **订阅节点**：机场订阅导入（base64 URI 列表 / Clash YAML）、自建落地节点、手动 URI 添加；合并、筛选、分组。
2. **中转服务器**：中转机 ↔ 落地机链路自动建立（含握手材料生成与分发），链路状态监控，节点入口自动改指中转机。
3. **落地机部署**：一键经 SSH 部署 Xray / SS 到新服务器。
4. **配置交付**：生成 Clash/Mihomo YAML，经本地订阅 URL 服务 + 导出 YAML 文件交付。
5. **测速**：节点延迟（TCP）+ 带宽（经代理 HTTP 下载），批量执行实时刷新。

不在本期范围：多用户/远程面板、移动端、客户端内核接管（只做配置生成与交付，不内置 Clash 客户端）。

## 2. 总体架构

**Tauri 2 桌面应用**（Windows 优先）：Rust 后端承担全部系统能力，React/TS 前端只做 UI。

```
vpn-ctl/
├── src/                    # React 前端（TS，Vite）
│   ├── views/              # 页面：仪表盘/节点/订阅/服务器/中转/设置
│   ├── components/         # 复用组件
│   ├── api/                # invoke 封装（TS 类型由 Rust 契约导出）
│   └── stores/             # 前端状态
├── src-tauri/
│   ├── src/
│   │   ├── main.rs         # 装配：DB、密钥、订阅服务、命令注册
│   │   ├── commands/       # Tauri 命令层（IPC 薄层：参数校验+转发）
│   │   ├── core/
│   │   │   ├── subs/       # 订阅拉取/解析/合并
│   │   │   ├── nodes/      # 节点领域逻辑
│   │   │   ├── servers/    # 服务器与凭据
│   │   │   ├── deploy/     # 落地机一键部署（Xray/SS）
│   │   │   ├── relay/      # 中转链路（WG / gost-TLS 两模式）
│   │   │   ├── config/     # Clash YAML 生成
│   │   │   ├── speed/      # mihomo sidecar 测速
│   │   │   └── serve/      # 本地订阅 HTTP 服务
│   │   ├── db/             # rusqlite 持久化 + 迁移
│   │   ├── crypto/         # DPAPI + AES-256-GCM 凭据库
│   │   └── ssh/            # SSH 适配器（连接/执行/上传/超时）
│   └── Cargo.toml
└── docs/superpowers/specs/
```

### 进程与信任边界

- 前端（WebView）无任何系统权限；一切能力经 Tauri 窄命令层调用，参数在 Rust 侧校验。
- Rust 侧独占：SSH、网络请求、SQLite、文件系统、sidecar 进程（mihomo）、本地订阅服务。
- sidecar：mihomo core（测速）；gost 只部署到远程服务器，不跑在本地。

### 关键选型

| 关注点 | 选择 | 理由 |
|---|---|---|
| 异步运行时 | tokio | Tauri 2 原生 async command |
| SSH | `russh`（纯 Rust，async 客户端） | 生产客户端/测试服务器/密钥生成单库全覆盖，Windows 零 C/OpenSSL 构建依赖；超时取消直接作用于 async 未来 |
| HTTP | reqwest | 订阅拉取、mihomo 下载 |
| 存储 | rusqlite（bundled SQLite，WAL） | 单文件、事务 |
| 加密 | aes-gcm + DPAPI（keyring → Windows 凭据管理器） | 随机数据密钥，免密解锁 |
| 订阅服务 | axum，仅监听 127.0.0.1，路径带随机 token | 成熟、轻量 |
| YAML | serde + serde_yaml | Clash 配置解析/生成 |
| 测速 core | mihomo alpha 二进制（自动下载或手动指定路径） | Clash Verge 同思路 |
| 中转链路 | 模式 A：WireGuard；模式 B：gost v3 relay+ TLS | 按服务器对自选，默认加密 |
| 落地节点程序 | Xray（默认）/ SS | Xray 用官方 install-release.sh；SS 用 shadowsocks-rust 预编译包 |

## 3. 中转链路设计（核心）

### 3.1 两种链路模式（按"服务器对"选择，UI 内切换）

**WG 模式**（适用于中转、落地均在海外机房）：

```
客户端 → 中转机:端口 (gost 收口)
              ↓ WG 隧道 (wg0, UDP, Noise 加密)
         落地机 wg0 内网 IP:Xray/SS 端口
```

**gost-TLS 模式**（适用于 UDP 不友好链路，如中转在国内、落地在海外）：

```
客户端 → 中转机:端口 (gost 客户端接续)
              ↓ relay+ TLS（自签证书，两机间加密握手）
         落地机 gost 服务端 → 本地 Xray/SS 端口
```

### 3.2 自动握手闭环

两机间握手全自动，部署后不需要再碰服务器：

| 模式 | 工具自动做的事 | 握手协议 |
|---|---|---|
| WG 模式 | 本地生成公私钥对 → SSH 下发两机 wg0.conf（互填对方公钥）→ systemd 拉起 | WG Noise 握手，隧道建立即自动完成 |
| gost-TLS 模式 | 本地生成自签证书 → SSH 分发证书给两机 → 配 gost 服务端/客户端 → systemd 拉起 | TLS 握手，客户端连服务端时自动完成 |

- 管理面握手（工具 ↔ 每台机器）：SSH。工具可生成密钥对并自动下发公钥到 `authorized_keys`，首次部署只需输一次密码。
- 隧道断线自动重连（WG 无连接状态、gost 自动重试）。
- **状态回传**：工具经 SSH 周期查询 `wg show` / gost 日志，把链路状态（上次握手时间、收发包数）显示在中转页。

## 4. 数据模型（SQLite，8 张表）

| 表 | 关键字段 | 说明 |
|---|---|---|
| servers | id, name, host, port, user, auth_kind, auth_ref, kind(relay/landing), system, notes | 服务器与凭据引用 |
| credentials | id, encrypted_blob | AES-256-GCM 密文（SSH 密码/私钥/订阅 token 统一走这里） |
| subscriptions | id, name, url, enabled, last_sync_at, sync_interval | 机场订阅 |
| nodes | id, name, source(import/self-built/manual), protocol, addr, port, params_json, group, relay_rule_id?, status, latency, bandwidth, last_checked_at | 节点（自建节点经 relay_rule_id 关联链路） |
| relay_rules | id, name, relay_server_id, landing_server_id, mode(wg/gost-tls), listen_port, landing_port, status | 中转规则（服务器对） |
| deploys | id, server_id, kind(xray/ss/wg/gost), config_json, deployed_at, status | 部署记录 |
| settings | key, value | 订阅服务端口/token、mihomo 路径、测速参数 |
| jobs | id, kind, status, progress, error, created_at | 后台任务（部署/建链/同步/测速） |

## 5. 核心流程（六条主线）

1. **订阅同步**：拉 URL → 自动识别格式（base64 URI 列表 / Clash YAML）→ 解析 → 按过滤规则 upsert 进 nodes → 失败保留旧数据并标"上次同步失败"。
2. **落地机部署**：选服务器 + 协议参数 → SSH 探测系统/架构 → 官方脚本装 Xray（install-release.sh）/ SS（shadowsocks-rust 预编译包）→ 生成服务配置 → systemd 启动 → 自动写回 node。
3. **中转链路建立**：选中转机 + 落地机 + 模式 → 自动握手闭环（见 3.2）→ 生成 relay_rule → 关联节点入口改指中转机端口。
4. **配置生成**：全部启用节点 → 按分组生成 Clash YAML（proxies + proxy-groups + 默认规则组）→ 维护内存态最新版本。
5. **订阅服务**：axum 监听 127.0.0.1，`GET /sub/<token>` 返回合并 YAML；客户端订阅 URL 填一次，节点变化自动生效。
6. **测速**：管理 mihomo sidecar 生命周期 → 临时配置逐节点起本地代理 → TCP 延迟 + 经代理 HTTP 下载测带宽 → 事件流回传 UI。

### 任务队列

所有 SSH 长操作（部署/建链/同步）走统一 job 队列：串行执行、可取消、进度事件回传 UI、失败可重试；**先 dry-run 探测再落配置，不残留半配置状态**。

## 6. UI 结构（左侧导航，暗色主题）

- **仪表盘**：启用节点数、链路状态总览、最近测速/同步结果。
- **节点页**：表格（协议/地址/分组/延迟/带宽）+ 筛选 + 批量测速 + 启用开关。
- **订阅页**：订阅列表 + 一键同步 + 过滤规则（名称正则/标签取舍）。
- **服务器页**：服务器列表 + 凭据管理 + 连通测试 + 一键部署落地机向导。
- **中转页**：服务器对列表 + 链路模式选择 + 一键建链 + 链路状态（握手时间/收发流量）。
- **设置**：订阅服务端口与 token、mihomo core 路径、测速参数、导出 YAML。

## 7. 安全边界

- 凭据（SSH 密码/私钥/订阅 token）**只存加密态**；内存解密仅在用时，用完即弃。
- 凭据解锁策略：**纯 DPAPI 免密**——随机 32 字节数据密钥，DPAPI 包装后存 Windows 凭据管理器；同 Windows 账户启动自动解锁，无需输入密码。
- 订阅 HTTP 服务只绑 127.0.0.1 + 随机 token 路径，外网不可达。
- SSH 命令白名单化：部署/建链只允许执行预期命令集，**不提供任意命令执行入口**。
- WG 私钥 / gost 证书在本地生成，经 SSH 加密通道分发，不落中间存储。
- 日志与 UI 错误信息脱敏：密码/token/私钥一律不出现在日志中。

## 8. 错误处理

- 错误分级（Rust `thiserror` 枚举，经 IPC 序列化为前端类型化错误）：认证失败、网络不可达、命令执行失败、超时。
- 部署类操作：先探测（系统/架构/已装组件）再执行；任一步失败即停止、状态标红、可重试。
- 测速：单节点超时自动跳过，不阻塞队列。
- 订阅拉取失败：保留旧数据、标记状态，UI 显示原因。

## 9. 测试策略

- **Rust 单测**（cargo test）：订阅解析（base64/URI/YAML 样本集）、YAML 生成金样、WG/gost 配置文件生成金样、加密往返、DB 迁移。
- **Rust 集成测**：用 russh（生产依赖，测试侧起本地 loopback SSH 服务器）真实验证部署/建链命令序列——本项目最关键的测试手段（生产侧 SSH 客户端同为 russh，2026-08-13 决策：ssh2 → russh 全套，Windows 构建链风险）。
- **前端**：Vitest + Testing Library，组件与交互。
- **IPC 契约**：Rust 导出命令 schema，TS 侧类型检查。

## 10. 分期实施（每期独立验收）

| 期 | 内容 | 验收 |
|---|---|---|
| P1 骨架 | Tauri 工程 + DB/加密 + 订阅导入 + 节点管理 + YAML 生成 + 本地订阅服务 + 导出 | 拉机场订阅 → 客户端用本地订阅 URL 完整工作 |
| P2 服务器 | 服务器/凭据管理 + SSH 适配器 + 落地机一键部署 Xray/SS | 新机经工具部署后节点可用 |
| P3 中转 | WG/gost-TLS 两模式建链 + 状态监控 | 服务器对一键建链成功，链路状态可见 |
| P4 测速 | mihomo sidecar + 延迟/带宽 + 队列 | 批量测速实时刷新 |

## 11. 已确认决策记录

- 形态：Tauri 2 + React 前端（Rust 写，老板指定）。
- 使用对象：自己本地用（非多用户产品）。
- 客户端生态：Clash/Mihomo 系。
- 节点来源：自建落地 + 机场订阅导入（可多选，手动 URI 亦可）。
- 测速：延迟 + 带宽都要。
- 配置交付：本地订阅 URL 服务 + 导出 YAML。
- 落地机：一键部署（不只记录）。
- 中转程度：自动配转发 + 生成配置；链路模式混合（WG / gost-TLS），用户按服务器对自选，**高安全优先**（默认加密链路）。
- 凭据保护：纯 DPAPI 免密。
