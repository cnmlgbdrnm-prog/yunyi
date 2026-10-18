# 导出选中节点 设计

**目标：** 导出配置时能挑具体哪几条线路写进文件，而不是只能整份导出。

**范围：** 「设置 → 配置导出」的两个按钮（YAML / sing-box）加一层节点选择。**不含**「把某条线路固化为默认出口」（用户已明确不要）。

## 现状（实现前已核对源码）

1. **导出就是整份缓存写盘**：`commands.rs:312-334` 的 `export_yaml` / `export_singbox` 直接读 `state.config` / `state.config_singbox`，唯一过滤是 ssr 节点被剔除并计数（`config/mod.rs:79`、`config/singbox.rs:44`，mihomo 与 sing-box 均已移除 SSR 支持）。用户唯一能施加的粒度是「节点」页的启用开关。
2. **生成逻辑本就是纯函数**：`config::build_yaml(nodes, smart, rules, servers) -> (String, usize)`（`config/mod.rs:67`）与 `config::singbox::build_singbox(nodes) -> Result<(String, usize), ConfigError>`（`singbox.rs:38`）都不依赖缓存。`rebuild_config`（`subs/service.rs:216`）只是「取全量 → 调它们 → 写两个缓存」。
3. **可见性判定藏在 `rebuild_config` 里**（`subs/service.rs:225-231`）：`sub_id` 为 `Some(sid)` 的节点要求 `sid` 在启用订阅集合中，`None` 一律可见。这是「哪些节点进配置」的唯一口径。
4. **导出的配置不含中转改址**：`node_relay_chain`（`config/mod.rs:32`）与 `effective_node`（`:16`）在 `node_mapping` 内部应用，proxy-groups 成员用的是**节点原名**，不是中转机地址。所以选择对话框显示节点名与 UI 一致，不需要额外解释。
5. **设置页导出块的现状**（`src/views/Settings.tsx:184-220`）：智能分流复选框 + `导出 YAML` / `导出 sing-box` 两个按钮 + 结果提示（路径、ssr 跳过计数）。两个按钮点击后直接进 `api.exportYaml()` / `api.exportSingbox()` → 原生保存对话框。

## 设计

### 核心：复用同一组纯函数，不碰缓存

导出子集不新写生成逻辑，用**与 `rebuild_config` 完全相同的输入组合**去调 `build_yaml` / `build_singbox`，只是节点列表换成用户勾选的那些。好处是导出结果与「开启网络」使用的配置天然一致，不会随两套路径漂移。

### 后端

**`subs/service.rs`：抽出可见性判定**

```rust
/// 「哪些节点进配置」的唯一口径：节点启用（由 build_* 内部过滤）且其订阅启用。
/// rebuild_config 与导出选中共用，避免两处漂移。
pub fn visible_nodes(&self) -> Result<Vec<Node>, SubError>
```

`rebuild_config` 改为调用它，行为不变。

**`commands.rs`：一次性产出配置（不写缓存）**

```rust
/// 按选中 id 生成指定格式的配置与跳过计数。输入组合与 rebuild_config 完全相同：
/// visible_nodes + smart 设置 + 中转规则 + 服务器表（后者仅 yaml 需要，singbox 只吃节点）。
/// node_ids 为空 = 全部可见节点（等价于旧行为）。
async fn build_config_for(
    state: &Arc<AppState>,
    node_ids: &[String],
    format: ExportFormat,   // enum { Yaml, Singbox }
) -> R<(String, usize)>
```

非空 `node_ids` 时按 id 过滤，**id 不在可见集合里的直接忽略**（不报错——前端给的清单来自同一个函数，出现差异说明中途有改动）。

**新增 IPC**

```rust
list_exportable_nodes_cmd() -> Vec<ExportableNode>            // { node: Node, exportable: bool, skip_reason: Option<String> }
export_yaml_selected_cmd(nodeIds: Vec<String>) -> Option<ExportOutcome>
export_singbox_selected_cmd(nodeIds: Vec<String>) -> Option<ExportOutcome>
```

- `ExportableNode.exportable=false` 目前只有一种情况：协议为 `ssr`（理由文案「mihomo 与 sing-box 已移除 SSR 支持」）。
- **空选集是错误**，不是「全部」：`Err("请至少选择一个节点")`。`nodeIds` 字段缺失或为空数组时按空选集处理并拒绝。
- 旧的 `export_yaml_cmd` / `export_singbox_cmd` **保留不动**（订阅服务下发走缓存那条路）。

### 前端

「设置 → 配置导出」点导出按钮 → 弹选择对话框（弹窗样式照 `components/PeerConfigDialog.tsx`）→ 确认后进原生保存对话框 → 结果提示不变。

```
导出 YAML
┌──────────────────────────────────┐
│ 选择要导出的节点（3 个可导出）      │
│ ☑ 大美             ss      自建    │
│ ☑ ashura-vless     vless   自建    │
│ ☐ 某机场-ssr       ssr     ✗ 不支持 │  ← 置灰不可勾
│                                  │
│ 智能分流：开（跟随上方复选框）      │
│         [取消]   [导出选中 2]      │
└──────────────────────────────────┘
```

- 弹窗打开时调 `list_exportable_nodes_cmd` 拉清单；**默认全勾可导出的**，ssr 行默认不勾且禁用。
- 失败态：清单拉取失败 → 弹窗内显示错误，不静默降级为「导出全部」。
- 按钮文案随勾选数变化（`导出选中 N`）；N=0 时禁用。
- 取消不发任何请求。

### 边界

| 情况 | 行为 |
|---|---|
| ssr 节点 | 清单里显示但置灰、注明「不支持」；前端不给勾。绕过前端硬传 → 后端报错（`build_*` 的未知协议/SSR 路径） |
| 一条都没勾 | 前端禁用按钮；后端拒绝空选集 |
| 清单与导出不一致 | 两者都走 `visible_nodes`，同一个函数；导出的 id 过滤对不可见 id 静默忽略 |
| 订阅/节点中途被改 | 导出按请求时刻的库状态重新计算，以导出结果为准 |
| sing-box 构建失败标记置位 | `export_singbox_selected_cmd` 沿用 `export_singbox` 的既有处理：报可读错误，不导出过期配置 |

## 测试

### 后端

- `visible_nodes` 与 `rebuild_config` 口径一致：禁用订阅下的节点不进清单；`sub_id=None` 的自建节点恒进。
- 选中 2 条导出 → YAML 的 `proxies` 与 `proxy-groups` 成员**只含**这 2 条，且不含被排除的节点名。
- **全选 = 与旧 `export_yaml_cmd` 产出逐字节相同**（防两套生成路径漂移，最关键的一条）。
- sing-box 同理（`outbounds` 只含选中项，`tag` 正确）。
- ssr 节点传进选中集 → 报错；`exportable` 标记为 false 且带理由。
- 空选集 → `Err("请至少选择一个节点")`；`nodeIds` 为 `[]` 同样拒绝。
- 清单为空（没有任何可见节点）→ 导出被既有 `ensure_exportable_has_nodes` 守卫拒绝。

### 前端

- 点「导出 YAML」弹窗出现，清单来自 `list_exportable_nodes_cmd`，默认全勾可导出项。
- ssr 行不可勾且显示「不支持」。
- 勾选数变化反映在按钮文案上；取消不调用导出命令。
- 确认后调用 `export_yaml_selected_cmd` 且**只带勾选的 id**；结果路径正常展示。
- 清单拉取失败 → 弹窗内显示错误。
- 既有 3 个导出用例（`Settings.test.tsx:78/89/99`）改为先过弹窗再断言。

### 门禁

`cargo test --manifest-path src-tauri/Cargo.toml`、`npx tsc --noEmit`、`npm test` 全绿。
