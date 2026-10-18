# vpn-ctl 多协议落地机 + 服务器页布局设计

日期:2026-08-30
状态:待评审

## 背景与目标

软件主打**便捷 / 高效 / 稳定**。现状与目标差距:

1. **部署面协议太少**:落地机部署向导(`src-tauri/src/servers/deploy.rs`、`src/components/DeployWizard.tsx`)只有 `xray`(VLESS+Reality)与 `ss`(SS-2022-blake3)两种 kind;对照参考 3x-ui 支持的协议面(VLESS/VMess/Trojan/SS/WireGuard/Hysteria2/TUIC/SOCKS/HTTP/Dokodemo),明显不足。
2. **节点面映射不全**:`src-tauri/src/config/mod.rs` 已支持 ss/vmess/vless/trojan + clash_raw 原样透传,但缺 hysteria2/tuic/wireguard 的 mihomo 映射与 hy2/tuic URI 导入。
3. **布局痛点**:点「部署落地机」要滑到页面底部找按钮;点「编辑」表单跳到页面顶部,长列表场景体验差。

目标:部署面"分级多协议"、节点面全协议可进可测可出、服务器页"就地展开"交互。

## 范围声明

- 本次涉及落地机(landing)部署与节点/订阅管理;**不改 relay/WG 链路**;
- **不引入 3x-ui 面板依赖**,不把部署托付外部面板;
- 节点协议导入/导出沿用现有 mihomo + sing-box 出口,不新增内核。

## 协议矩阵

### L1 必装组(部署一次全装,可逐个取消勾选)

| 协议 | 内核 | 部署产物 | 定位 |
|---|---|---|---|
| VLESS + Reality(tcp + Vision flow) | Xray(已有) | xray 单 config 多入站 | 主力,抗检测 |
| Trojan + Reality(tcp) | Xray(复用) | 同上 | 双保险 |
| VMess + WS(tcp/ws) | Xray(复用) | 同上 | 老客户端兼容(证书体系不在本次范围,WS 明文 `/ray`,前置 CDN/WAF 可换 TLS) |
| SS-2022-blake3(tcp) | shadowsocks-rust(已有) | 独立 systemd unit | 兜底 |

### L2 可选组(勾选才装;部署后可通过「协议管理」补装/卸载)

| 协议 | 内核 | 说明 |
|---|---|---|
| Hysteria2 | sing-box(新装) | UDP/QUIC,装前自动 UDP 丢包探测 |
| TUIC | sing-box(新装) | UDP,同上 |

L2 部署前自动执行 UDP 通路探测(约 30s 丢包率);差则**提示但不阻断**(用户知情可选)。

## 服务器端架构:分级部署流水线

- **Xray 单进程多入站**:vless-reality / trojan-reality / vmess-ws 三个入站写进**同一份 config.json**,复用现有 xray systemd unit,单点启动与验证。部署/升级/卸载都是"重写 config + 重启 xray",不新增系统服务。
- **SS-2022**:沿用现有 shadowsocks-rust unit,不改。
- **L2**:仅勾选时下载安装 sing-box + 独立 unit(hy2/tuic 两入站同一进程)。
- **unit 沿用现状**:现有部署用官方 `xray.service`(Xray-install)与 `ssserver.service`(shadowsocks-rust),本次不更名;仅 sing-box 新增 unit(命名 `singbox.service`,参照官方 deb 安装或自建 unit,计划阶段定)。
- **节点落库**:每个协议一行 `nodes`(复用现有 `protocol` 列,零迁移),name 形如 `112-vless`、`112-trojan`;同一台落地机的节点共享服务器来源信息。
- **端口规划**:每协议独立端口,**自动分配**(端口探测占用自动偏移,复用已修好的 `port_probe_cmd`);默认映射表:`vless=443`(被占则自动顺延,同现有部署语义)、`trojan=4443`、`vmess=8443`、`ss=8388`、`hy2=2097`、`tuic=2053`,全部在向导中显示"自动(可改)"。
- **参数自动化**:Reality 伪装域名默认 `www.microsoft.com`(向导可改)、SS-2022 密码自动生成、VMess UUID 自动生成、WS path 默认 `/ray`(可改)。
- **失败语义**:逐协议独立验证(is-active + 端口监听);某协议失败只标红该协议、其余已部署保留;重试幂等(配置重写即可)。卸载:停 unit + 删除 config(sing-box/xray 重写配置,xray 即从 config 中摘除对应入站)。

## 节点面(导入 / 测速 / 导出)

- `config/mod.rs` mihomo 映射补:`hysteria2`、`tuic`、`wireguard` 三种 proxy 映射(字段按 mihomo proxy schema);串 `clash_raw` 透传路径不变。
- URI 导入:补 `hy2://`、`tuic://`(及 wg 若成熟)解析成 nodes 行;现有订阅导入路径(混入 clash_raw 片段)保持。
- sing-box 出口子模块同步补三类协议映射。
- 测速引擎走 mihomo proxy 注入(config::node_proxy_json),新协议自动可测,不新增引擎。
- 导出订阅:部署产物 L1/L2 所有节点自动进入 build_yaml 流程(排序/分组已有)。

## 前端布局:服务器页就地展开

### ExpandableCard 模式

- 服务器列表页(现卡片式:`src/views/Servers.tsx`)内,同一时间最多一个卡片处于"展开"状态(部署向导或编辑表单);点击另一张卡片的操作按钮,前面的自动收起。
- 展开/收起自动 `scrollIntoView({ block: 'nearest' })`,视口对准展开区;展开区顶部有「收起」。
- 交互后(部署完成/保存/取消)恢复初始卡片形态。

### 部署向导(卡片内)

- 点击卡片「部署落地机」→ 向导就地展开在卡片下方,流程:+ 协议卡片网格(6 个,每个:协议名 + 一句特性标签,点选高亮,L1 默认全选)+ 参数区(全自动,可覆盖)+ [开始部署] 就地。
- job://progress 进度条**就地渲染在展开区底部**(不再全局一块)。
- 协议卡片样式参考 3x-ui 的列表密度:单行小卡,虚线边框选中态。

### 编辑表单(卡片内)

- 点击「编辑」→ 表单就地展开;保存/取消在展开区底部;提交或取消后自动收起、列表刷新。

### 卡片信息增强

- 服务器卡片增加"已部署协议"徽章行(如 `vless · trojan · ss`);
- 「更多」菜单增加「节点」项,跳转节点页并携带 `server_id` 过滤;「协议管理」项:查看已装清单 / 补装 L2 / 卸载;
- 现有功能(连通测试/测速/免密)按钮位置不变。

## 错误处理与重试

- 部署失败按协议隔离上报(进度事件+卡片红点),支持重试(幂等);
- L2 UDP 探测失败仅提示,不影响 L1;
- sing-box 下载失败:仅 L2 掉,不影响 L1;已有网络错误透传。

## 测试计划

| 面 | 用例 |
|---|---|
| config 映射 | 每新协议 2-3 组金样(mihomo YAML + sing-box);URI 导入 hy2/tuic 解析 |
| deploy | xray 多入站 config 生成断言;SS/L2 unit 内容;端口探测偏移;失败隔离(单协议失败其余保留) |
| 前端 | ExpandableCard(互斥展开、scrollIntoView、收起);协议卡默认态(L1 全选);部署进度就地渲染;节点页过滤 |
| 集成 | tauri loopback 模式沿用现有测试基建(p2/p3 的 ssh loopback server 模式) |

## 交付物

1. `src-tauri/src/servers/deploy.rs` 协议注册表化(ProtocolSpec 枚举 + per-protocol config 生成)
2. `src-tauri/src/config/mod.rs` 三点映射 + singbox 子模块
3. `src-tauri/src/servers/*` 协议管理命令(查看/补装/卸载)与 IPC
4. `src/components/ExpandableCard.tsx`(或等价)+ `Servers.tsx` 改造
5. `src/components/DeployWizard.tsx` 重做(协议卡组+自动参数+就地进度)
6. 测试按上表补齐

## 明确的非目标

- 不做服务器端多用户/流量限制(面板级功能,留给 3x-ui 等面板)
- 不做 relay 协议扩展
- 不迁移 nodes schema
