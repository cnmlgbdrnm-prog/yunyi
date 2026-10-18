# 本机上网客户端（智能分流 + 系统代理）设计

> 2026-08-30。背景：多协议落地机部署、中转链路、订阅导出/测速链已全部可用，但"上网"仍需旁挂 Clash Verge。本设计让 vpn-ctl **自己就是上网客户端**：本地 mihomo + 一键系统代理 + 智能分流（国内直连/国外走落地机）。

## 目标与非目标

**目标（P1）：**
- 仪表盘"网络"卡片一键开关：开启 = 本地起 mihomo（mixed-port 系统代理）+ 智能分流规则 + 测速兜底分组，全部自动
- 智能分流：国内流量直连（不出海），国外流量走落地机节点；规则客户端侧执行
- 订阅/节点导出也带智能分流规则（设置页开关，默认开）
- 冲突提醒不抢占：检测到 Clash Verge / 其它代理进程 → 提示用户，绝不接管

**非目标（P2 Reserve，本期不做）：**
- TUN 全局模式（wintun + 管理员权限 + 网卡恢复）
- 分流出站自定义、规则可视化编辑器
- 多引擎（sing-box 本地客户端）

## 架构

```
仪表盘「网络」卡 ── 一键开关 ──→ net_start/net_stop (IPC)
                                   │
                       ┌───────────┴────────────┐
                       │ net::ClientRuntime      │
                       │  spawn mihomo(捆绑二进制)│
                       │  config: 节点库+智能规则 │
                       └───────────┬────────────┘
                   开前快照 ───────────┴────────── 关时还原
              WinINET 系统代理 (sysproxy_set)
```

- 引擎：**已捆绑 mihomo.exe**（复用测速引擎的 controller REST + mixed-port 模式，速度引擎不改）
- 规则数据：`geoip.dat`/`geosite.dat` 首次启动自动下载到应用数据目录 `data_dir/net/`（候选数据源 MetaCubeX/meta-rules-dat，走现有"跟随系统代理"的下载路径；**下载失败降级**为 `GEOIP,LAN/PRIVATE → MATCH` 规则并提示，不让 App 上网功能整体挂掉）
- 智能分流不依赖下载：LAN/PRIVATE/CN 兜底规则里 `GEOSITE,cn`/`GEOIP,CN` 需数据文件；缺失时仅去掉这两条，MATCH 仍兜底全走代理

## 智能分流规则（顺序即优先级）

```
GEOIP,LAN,DIRECT
GEOIP,PRIVATE,DIRECT
GEOSITE,cn,DIRECT
GEOIP,CN,DIRECT
MATCH,自动选择
```

- 客户端运行与订阅导出共用同一规则函数（`rules_yaml()`），保证两处一致
- 分组沿用现有 `url_test_group` + `select_group`（自动选择=URL 测速优先可用节点）

## 组件与文件

| 文件 | 职责 |
|---|---|
| `src-tauri/src/net/mod.rs`（新） | `ClientRuntime`：config 生成（`client_yaml(enabled_nodes, port, secret)`）、启动/停止/状态、端口顺延（7890→7891…）、geosite 数据确保 |
| `src-tauri/src/net/sysproxy_set.rs`（新） | WinINET 写：快照 → 设 `ProxyEnable=1, ProxyServer=127.0.0.1:{port}` → 停止还原快照；纯函数 `registry_updates(port)` 承载值生成 |
| `src-tauri/src/net/conflict.rs`（新） | 常见代理进程扫描（`clash-verge/mihomo/sing-box/v2rayN/wingy` 等名称列表，Windows tasklist，非 Windows 空）→ `Vec<String>` |
| `src-tauri/src/sysproxy.rs` | 现有读取逻辑不动（更新下载仍用） |
| `src-tauri/src/config/mod.rs` | `build_yaml(nodes, smart_rules: bool)` 加参数；`rules_yaml(enabled: bool)->Vec<String>` 智能规则 |
| `src-tauri/src/commands.rs` | `net_status_cmd`（running/port/conflicts）、`net_start_cmd`（一键：检查冲突→确认→起→设代理）、`net_stop_cmd`（杀进程→还原代理） |
| `src-tauri/src/state.rs` | AppState 挂 `net: ClientRuntime`（spawn 注入同测速引擎模式） |
| `src/api/types.ts` + `index.ts` | `NetStatus { running, port, conflicts }`；`netStatus/netCheck/netStart/netStop` |
| `src/views/Dashboard.tsx` | 「网络」卡：状态徽章（未开启/运行中·端口/系统代理已接入）+ 一键开关 + 冲突/错误横幅 |
| `src/views/Settings.tsx` | 「订阅导出带智能分流」开关（默认开） |

## 数据流与关键行为

- **开启**：`net_start` → ① 扫描冲突，有则返回 `conflicts`（UI 横幅列出，用户自行关闭后重试）；② 磁盘端口探测 7890 起顺延；③ 写 `data_dir/net/config.yaml`（mixed-port + 随机 controller secret + 智能规则）；④ spawn mihomo → 控制器就绪轮询（10s）；⑤ 快照当前 WinINET → 写入系统代理 → 状态=running
- **关闭**：杀 mihomo 进程 → 还原快照 → 状态=idle
- **状态**：`net_status` 只读内存/进程，不触网；仪表盘挂载时拉取、开关动作后刷新（与现有 StatCard 的挂载刷新模式一致），不做后台轮询
- **重启恢复**：不自动恢复（`running` 不持久化）；App 重启后仪表盘显示"未开启"，用户再点
- **导出**：`build_yaml` 按设置 `smart_split` 生成智能规则 or 旧 `MATCH,自动选择`

## 错误处理

| 情形 | 行为 |
|---|---|
| mihomo 二进制缺失/被杀软瞬态锁 | 返回"引擎不可用"（复用现有二进制加载与锁重试语义） |
| 7890-7910 全被占 | 明确报错"端口全被占用" |
| 已有 Clash Verge 等进程 | `net_start` 前置检测返回冲突列表，不注入、不杀、不接管 |
| geosite 下载失败 | 降级仅 `LAN/PRIVATE→MATCH`，横幅一次提示"分流数据缺失，全部流量走代理" |
| 设置代理后 mihomo 崩溃 | `net_status` 检测进程不在 → 状态回 idle 并还原系统代理 |

## 测试计划

| 面 | 用例 |
|---|---|
| config | `rules_yaml` 顺序金样（LAN→PRIVATE→GEOSITE→GEOIP→MATCH）；`build_yaml` smart 开/关两组金样；`client_yaml` 端口/secret/分组 金样 |
| sysproxy_set | `registry_updates(port)` 值金样；快照/还原走 fakes（注册表不真写） |
| runtime | 端口顺延纯函数；冲突检测列表签名（进程名 mock）；start 控制器就绪超时/成功两种（假 TCP listener）；stop 后状态回 idle |
| 前端 | 仪表盘网络卡：初始态、开启成功（状态徽章）、冲突横幅（不动进程）、关闭（还原）；Settings 智能分流开关联动 build_yaml 参数（mock IPC） |
| 冒烟 | tauri dev 手动：一键开 → 浏览器访问 Google 走落地机、访问百度直连（真体验证，用户有 Verge 对照） |

## 交付物

1. `src-tauri/src/net/` 模块（runtime + sysproxy_set + conflict）与 IPC 三命令
2. `config::rules_yaml` + `build_yaml` 智能参数化（客户端/导出共用）
3. Dashboard 网络卡 + Settings 开关
4. 测试按上表补齐（Rust 白盒 + 前端 mock IPC）

## 明确不做（P2 Reserve）

- TUN 全局模式：wintun 依赖、管理员权限、网卡恢复逻辑，独立一期
- 客户端引擎切换 sing-box；分流出站自定义；规则编辑器；局域网共享代理（其它设备用本机做网关）
