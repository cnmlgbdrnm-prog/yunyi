# vpn-ctl P7 设计规格：交付面扩展（ssr 解析修复 + sing-box 全协议导出）

> 状态：已老板批准并实施（2026-08-14）。战略见 `docs/strategy.md`——「配置工厂」护城河的核心交付面。

## 1. 背景与缺口（已探明）

| 缺口 | 现状 | 后果 |
|---|---|---|
| ssr 解析 | `parse_uri` **无 ssr:// 分支**（`looks_like_uri_list` 认前缀但解析不认） | 订阅里一行 ssr 节点 → `parse_uri_list` 的 `?` 直接**整单订阅解析失败** |
| hysteria2 URI | `parse_uri` 无 `hysteria2://` 分支（Clash YAML 来源靠 clash_raw 透传支持） | URI 列表订阅含 hysteria2 整单失败 |
| Clash 导出 ssr | config 无 ssr 映射 | 修好解析后导出仍会撞「未知协议且无 clash_raw」错误 |
| sing-box 导出 | 完全不存在 | 交付面只有 Clash/Mihomo 单边 |

## 2. 设计

### 2.1 ssr:// 解析（parse.rs 新分支）

SSR URI：`ssr://base64(host:port:protocol:method:obfs:base64(password)/?obfsparam=…&protoparam=…&remarks=…&group=…)`

- 外层 URL-safe base64 解码（SSR 惯例**无填充**，先标准解码失败回退无填充，复用 `decode_ss_userinfo` 的引擎策略）
- 字段拆分：`host:port:protocol:method:obfs:base64pw`（`?` 前 6 段，冒号分隔、`rsplit_once` 防 IPv6 端口段误切）
- 查询参数全部 base64 内层编码：obfsparam / protoparam / remarks（节点名）/ group
- 统一入 ParsedNode：`protocol: "ssr"`，params 携带 protocol（origin/auth_sha1_v4/…）、method、obfs、password、obfsparam、protoparam；remarks 空回落「未命名节点」
- 解码失败 → 可读错误（沿用现有 err() 截断 32 字模式）

### 2.2 hysteria2:// 解析（parse.rs 新分支）

`hysteria2://password@host:port/?insecure=1&sni=x&obfs=salamander&obfs-password=y#name`

- 结构同 trojan 分支（url::Url 解析、percent_decode fragment、query_pairs 全量入 params），`protocol: "hysteria2"`

### 2.3 ssr 导出策略：双生态均已移除 SSR（官方文档核实 2026-08-14）

- **mihomo**：2023 起移除 SSR；**sing-box**：官方 deprecated 文档——ShadowsocksR 自 1.6.0 起废弃并删除
- ssr 节点在两个导出（Clash YAML / sing-box）中均**跳过** + 导出结果附提示「N 个 ssr 节点未包含（mihomo 与 sing-box 已移除 SSR 支持）」
- **禁止**把 ssr 映射为 ss（丢协议伪装层，静默失真）
- ssr 解析修复的独立价值：订阅含 ssr 行不再整单失败；节点入库可管理；未来若生态恢复支持可导出
- `build_yaml` 返回 `(yaml, skipped_ssr_count)`（签名变更同步全部调用方）；sing-box 导出同样返回跳过计数

### 2.4 sing-box 导出（新模块 config/singbox.rs）

- 输入：Node 列表（与 build_yaml 同源同排序）
- 输出：**完整 sing-box config.json**（serde_json 序列化，确定性排序）：
  - `log`：level info
  - `dns`：基础 servers（1.1.1.1 + tls://dns.google）
  - `inbounds`：mixed `127.0.0.1:7890`（常量，后续可配置化——P7 不做 UI 配置）
  - `outbounds`：每启用节点一个，**全协议映射（type 名以官方文档为准，2026-08-14 经 context7 核实）**：
    - ss → `"type": "shadowsocks"`（server/server_port/method/password）
    - vmess → `"type": "vmess"`（server/server_port/uuid/security/alter_id）
    - vless → `"type": "vless"`（server/server_port/uuid/flow）
    - trojan → `"type": "trojan"`（server/server_port/password）
    - hysteria2 → `"type": "hysteria2"`（server/server_port/password/up_mbps/down_mbps/obfs{type,password}/tls{enabled,server_name,insecure}）
    - ssr → **跳过**（同 §2.3）
    - clash_raw 来源：解析 raw YAML 片段按 clash 字段映射同协议（clash_raw 不能透传——sing-box 字段体系不同；type 名映射表：ss→shadowsocks，其余同名）
  - 选择组：`selector`（自动选择）+ `urltest`——与 Clash 导出「自动选择/手动选择」同构，tag 命名一致
  - `route`：final = 自动选择
- 未知协议（clash_raw 里 sing-box 也不认识）→ 可读错误带协议名（同 Clash 导出现状）

### 2.5 UI

- 导出区新增「导出 sing-box」按钮（下载 config.json），与现有「导出 YAML」并列
- Clash 导出的 ssr 跳过提示展示在导出结果消息里
- 无 emoji（铁律）

## 3. 变更面清单

- `src-tauri/src/subs/parse.rs`：+ssr 分支、+hysteria2 分支、`looks_like_uri_list` 已含前缀（无需改）
- `src-tauri/src/config/mod.rs`：ssr 跳过逻辑 + build_yaml 签名（返回 skipped 计数）
- `src-tauri/src/config/singbox.rs`：新建
- `src-tauri/src/config/tests.rs`：扩展
- `src-tauri/src/subs/parse_tests.rs`：扩展
- 命令层：`export_*_cmd` 现有命令检查 + 新 `export_singbox_cmd`
- 前端：导出按钮 + 调用 + 消息展示（Servers/Nodes 视图或导出组件——实施时按现状定位）
- 文档：README P7 功能节、spec 同步

## 4. 测试与验收

1. parse 金样：ssr URI（含 remarks/group/obfs 参数、无填充 base64）、hysteria2 URI；坏输入可读错误
2. sing-box 映射金样：六协议各一（真实字段）；输出 JSON 可被 `jq` 解析（结构断言）；urltest/selector 组存在
3. Clash 导出：含 ssr 的节点列表 → yaml 无 ssr 项 + skipped 计数正确
4. 端到端：订阅导入（含 ssr 行）成功落库（此前整单失败的回归验证）；导出 sing-box 命令返回合法 JSON
5. 前端交互：导出 sing-box 按钮调用命令并下载；Clash 导出提示展示
6. 门禁：cargo test + clippy + fmt 全绿（CI 已立，push 自动跑）

## 5. 风险

- **sing-box 字段版本漂移**：按 sing-box 1.8+ 现行 schema 实现；字段名以官方文档/本地验证为准（铁律：不凭记忆定 API）
- **clash_raw 协议面**：Clash YAML 的 ssr 节点在 sing-box 导出处同样**跳过+计数**（sing-box 1.6.0 起已移除 shadowsocksr 出站——§2.3 已修订，风险节初稿的「映射为 shadowsocksr」作废）
- **ssr 外层 base64 变体**：无填充/有填充/URL-safe 混合——解码回退链覆盖
