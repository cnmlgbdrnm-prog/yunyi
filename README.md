# 云驿（YunYi）

Windows 桌面端 VPN 订阅与节点管理工具：导入机场订阅、管理节点、生成 Clash / sing-box
配置，并提供服务器管理、一键部署、WireGuard 中转链、测速与自动更新等能力。

- 技术栈：Tauri 2 + Rust + React 19 + TypeScript
- 平台：Windows 10/11（WebView2）
- 命名：安装包 productName 为 `YunYi`；应用标识保持中性（`com.vpnctl.app`）

## 功能

### 订阅与节点

- 订阅导入：base64 URI 列表与 Clash YAML；支持 `ss`、`ssr`、`vmess`、`vless`、
  `trojan`、`hysteria2` 等协议
- 订阅过滤与启停；同步失败时保留旧数据并记录可读原因
- 节点管理：列表、筛选、启用开关、就地改名

### 配置交付

- 本地订阅服务：`127.0.0.1` + token，供 Clash / mihomo 客户端订阅
- Clash YAML 导出：可选节点、智能分流规则与隐私配置
- sing-box 导出：完整 `config.json`（mixed 入口与自动选择 / 自动测速组）
- 自建代理与订阅节点合并导出；SSR 节点在两种配置中跳过并给出提示

### 服务器与部署

- 服务器与凭据管理：凭据以数据密钥加密存储（AES-256-GCM，密钥由 Windows
  凭据管理器保管）
- SSH 连通测试、系统信息回填、一键免密（ed25519）
- 一键部署 Xray（VLESS + Reality）与 Shadowsocks（2022-blake3-aes-256-gcm），
  部署进度实时推送，支持协作式取消与重试

### 中转与网络

- WireGuard 一键建链：forward / peer 两种模式，支持既有握手链路接管、状态监控与
  设备配置下发（二维码 / 复制 / 下载 `.conf`）
- 落地机监听收敛：建链后自动收敛到 WG 内网，拆链时对称恢复
- TUN 全局模式：通过一次性提权安装的网络服务，把不读系统代理的程序也纳入
- 上游出口：客户端 → 中转机 → 静态住宅 IP
- 分享订阅链接、会话级防火墙阻断、运行时连接日志与泄漏检测
- 隐私线（Tor Onion 入口）逐线开关

### 测速与诊断

- 节点测速：延迟 + 真实下载带宽，批量测速与并发数可调，结果持久化
- 服务器测速：本机 TCP 延迟 + SSH curl 下载带宽
- iperf3 吞吐对测（已建链路时经 WG 内网地址）
- BBR 检测与一键开启：幂等脚本，内核不支持时给出可读原因

### 自动更新

- 检查、下载并安装 GitHub Releases 更新，完成后应用自动重启
- 更新包以 minisign 签名校验，防止发布资产被替换
- 检查与下载自动跟随 Windows 系统代理（WinINET），无代理时直连

## 技术栈

| 层 | 选型 |
|---|---|
| 桌面框架 | Tauri 2 |
| 后端 | Rust（tokio、rusqlite、russh、axum、windows-rs） |
| 前端 | React 19 + TypeScript + Vite |
| 代理内核 | mihomo（打包为 sidecar） |
| 部署目标 | Xray / shadowsocks-rust / WireGuard |
| 存储 | SQLite（应用数据目录）+ Windows 凭据管理器 |

## 开发

### 环境要求

- Windows 10/11
- Node.js 22+
- Rust stable（MSVC 工具链）

### 启动

```powershell
npm install
npm run tauri dev
```

### 构建

```powershell
npm run build        # 获取打包资源并构建前端
npm run tauri build  # 生成安装包
```

`npm run build` 会先执行 `fetch:sidecar` 与 `fetch:tor`，下载并校验 mihomo 与 Tor
资源；二进制不提交仓库（`src-tauri/binaries/` 已被忽略）。

## 测试

测试源码集中放在仓库根 `tests/`（`tests/frontend` 前端、`tests/rust` Rust），
已加入 `.gitignore`——**不随源码上传**，只保留在本机。新克隆的仓库没有 `tests/`，
CI 因此不跑测试；本地跑法：

```powershell
# Rust（必须带 local-tests 特性，否则测试模块不会被 include! 编进 crate）
cd src-tauri; cargo test --features local-tests; cd ..
# 前端
npm test
# 测试源码的类型检查（主 tsconfig.json 不含 tests/frontend）
npm run test:typecheck
# 类型与构建
npx tsc --noEmit; npm run build
```

## 持续集成

push 与 PR 双触发 `.github/workflows/ci.yml`，任一 job 红不可合并：

- **rust**（windows-latest）：`fetch:sidecar` → `fetch:tor` → `check:package` →
  `cargo fmt --check` → `cargo clippy --all-targets -- -D warnings`
- **frontend**（ubuntu-latest）：`npm ci` → `npx tsc --noEmit` →
  `check:updater-pubkey`

测试只在本地执行：CI 检出中没有 `tests/`，且不作测试门禁。

## 发布与自动更新

- **更新通道**：更新分发在独立公开仓库 `vpn-ctl-releases`，端点为
  `releases/latest/download/latest.json`
- **签名**：更新包以 minisign 私钥签名，客户端用内置公钥验签；公钥位于
  `src-tauri/tauri.conf.json` → `plugins.updater.pubkey`，必须是**公钥盒全文的
  base64**（`npm run check:updater-pubkey` 与 CI 会校验），私钥只存放在 GitHub
  Secrets `TAURI_SIGNING_PRIVATE_KEY`
- **发布流程**：手动触发 `.github/workflows/release.yml`（仅 master）。发布前先在
  `CHANGELOG.md` 新增对应版本节（`## [0.x.y] - 日期`）——该节同时作为 GitHub Release
  正文与 `latest.json` 的 notes；随后完成构建、签名、创建 Release 并上传
  `latest.json`
- **验收**：发布后必须真实安装一次，验证更新链路

## 项目结构

```
src/          React 前端
src-tauri/    Rust 后端（命令层、网络、部署、服务、测速）
scripts/      打包资源获取与仓库检查脚本
tests/        本地测试源码（.gitignore，不随仓库分发）
```

## 许可证

本项目以 [GNU General Public License v3.0](LICENSE) 发布。

