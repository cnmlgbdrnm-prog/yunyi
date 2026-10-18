# 节点改名 设计

**目标：** 在「节点管理」页给任意一条节点改名。部署出来的节点名是 `{服务器名}-{协议}` 的默认值（`servers/deploy.rs:1093` 硬编码），此前没有任何入口能改，名字不好认就只能忍着。

**范围：** 单条节点重命名。**不含**「订阅同步后保留新名」（已评估，见「被否决的方案」）、**不含**服务器改名联动部署节点。

## 现状（实现前已核对源码）

1. **「节点管理」页没有任何改名入口**。`src/views/Nodes.tsx` 的每行只有 勾选 / 测速 / 删除 / 启用 / 中转 五个操作；`nodes` 表里 `name` 只是显示字段，改它不动协议参数。
2. **节点名是多个消费方的键**，不只是显示：
   - `config/mod.rs:90`、`:148`：proxy-groups 成员列表取 `n.name`。
   - `config/singbox.rs:171/185/212/231/247/277/301`：sing-box outbound 的 `tag` 就是 `n.name`。
   - `config/mod.rs:73`、`:137`、`config/singbox.rs:40`：按 `name` 排序，保证输出确定性。
   - `net/runtime.rs:310` `select(proxy_name)`：手动切换出口节点按名字 PUT `/proxies/手动选择`。
   - `subs/service.rs:303-304`：订阅过滤器的 include/exclude 是对 `n.name` 跑正则。
3. **订阅导入节点的主键由名字派生**：`subs/service.rs:310` `stable_node_id = sha256(sub_id|协议|地址|端口|名字)[..8]`。名字一变主键就变，下次同步时该行被 `delete_for_sub_except` 删除、按订阅原名重建。
4. **Clash 来源（`clash_raw`）节点的名字走另一条路**：`config/mod.rs:215` 对 raw 片段是「片段自带 `name` 则原样保留、忽略 `n.name`」（`:188-193` 注释说明了这条约定），而 proxy-groups 成员仍取 `n.name`；sing-box 侧 `raw_outbound(raw, &n.name)`（`config/singbox.rs:90`，函数在 `:112`）又拿 `n.name` 当 tag。

## 关键正确性约束

**只改 `nodes.name` 会把配置改坏，且是静默的。** 对 `clash_raw` 节点：proxies 里输出的是片段自带的旧名，proxy-groups 里写的是新的 `n.name` → 分组指向一个不存在的成员，整份 mihomo 配置失效。sing-box 侧同样是 tag 与 `nodes.name` 分叉。

因此改名必须在**同一处**做两件事：改 `nodes.name`，并同步改写 raw 片段里的 `name`。收进 `repos::nodes::rename` 一个函数，杜绝「只做一半」。

**节点名必须唯一且非空。** 名字直接当 sing-box 的 tag 和 Clash 的 proxy 名，两行同名会让 tag 冲突、分组引用落到别人身上；空名同理。所以重名/空名在写库前拒绝，而不是让它静默生成坏配置。

## 设计

### 后端数据流

```
前端 renameNode(id, name)
  → IPC rename_node_cmd(id, name)
    → commands::rename_node                              src-tauri/src/commands.rs
        1. 规整：servers::clean_text(&name)              复用现成（剔零宽字符 + trim）
        2. 空名 → Err("节点名不能为空")
        3. 重名（其它行已有同名）→ Err("已存在同名节点：<名字>")
        4. repos::nodes::rename(&conn, id, name, &creds)
        5. state.subs.rebuild_config().await             沿用 set_node_enabled（commands.rs:139-144）的既有模式
    → Ok(())
```

**`repos::nodes::rename(conn, id, name, creds) -> rusqlite::Result<bool>`**（`src-tauri/src/db/repos.rs` 的 `nodes` 模块）

1. `UPDATE nodes SET name=?2 WHERE id=?1`。
2. 若 `params_json`（`crypto::open_param` 解密后）含 `clash_raw` 字符串：`serde_yaml` 解析片段 → 改写 `name` 键 → 序列化回字符串塞回 `params_json.name` → `crypto::seal_param` 回写。读写同加密口径，照 `set_route`（`:202`）与 `update_reality_sni`（`:241`）的现成写法。

返回 `bool` 表示名字是否真的变了，供上层决定要不要重建配置缓存。

**`commands::rename_node`**：节点不存在 → `Err("节点不存在")`（照 `toggle_node_relay` `commands.rs:156-162` 的写法）。名字未变 → 直接返回，不做 UPDATE、不重建缓存。

### 前端

`src/views/Nodes.tsx` 名称单元格改成就地编辑：

- 未编辑：显示 `n.name`；操作列在「测速」旁加一个 `改名` 按钮。
- 编辑中：名称格换成 `<input>`，操作列换成 `保存` / `取消`；`Enter` 保存、`Esc` 取消；保存中两个按钮禁用（防连点）。
- 保存 → `renameNode(n.id, draft)` → `refresh()` → 退出编辑。
- 后端错误（空名 / 撞名 / 节点不存在）落在页面顶部既有的 `role="alert"` 错误条（`Nodes.tsx:244`），与其它操作一致。
- 编辑态按 `node.id` 记，同一时间只允许一行处于编辑态。

`src/api/index.ts` 增 `renameNode(id: string, name: string): Promise<void>`；`src/api/types.ts` 无需改动（沿用既有 `Node.name`）。

### 边界（已知且接受，不静默）

| 情况 | 行为 |
|---|---|
| 自建节点（部署产生） | 改了就稳，直到下次对同一服务器重新部署（会重建节点行、回到 `{服务器名}-{协议}`） |
| 订阅导入节点 | 本次生效；下次订阅同步按订阅里的原名重建节点行，改动消失 |
| Clash 来源（`clash_raw`） | 片段里的 `name` 同步改写，proxies/groups/tag 三者保持一致 |
| 网络客户端正在运行 | 重建的是**配置缓存**；运行中的 mihomo 实例仍持旧名，切到新名会失败，重开网络后生效 |

### 被否决的方案

**墓碑表**（新增 `node_renames` 表记 `sub_id|协议|地址|端口 → 新名`，同步时套用）能让订阅节点改名永久生效，但代价是多一张表、同步逻辑多一条分支、多一类清理与测试；而订阅节点名本来就由订阅方决定，改它说服力不强。已与用户确认取简单方案。

## 测试

### 后端

- `repos::nodes::rename`（`src-tauri/src/db/tests.rs`）：
  - 改名落库、`Node.name` 读到新值。
  - `clash_raw` 片段的 `name` 同步改写，`params_json` 仍可往返（加密口径）。
  - 名字未变返回 `false`。
  - 不存在的 id 返回 `false` 且不报错。
- `commands::rename_node`（`src-tauri/src/commands_tests.rs`，沿用 `test_state()` 夹具）：
  - 成功改名。
  - 空名 / 纯空白名被拒。
  - 首尾空白与零宽字符被 `clean_text` 剔除后再入库。
  - 与其它行撞名被拒。
  - 成功后 `get_merged_config_cmd` 输出里出现新名、不再出现旧名（证明缓存重建真的发生——`rebuild_config` 被调用，不只是改了库）。

### 前端

`src/views/Nodes.test.tsx`（沿用既有 `invoke` mock 风格）：

- 点「改名」出现输入框，预填当前名。
- 保存调用 `rename_node_cmd` 且带 `{id, name}`，随后刷新列表。
- `Esc` / 点「取消」不发请求、恢复显示。
- 输入为空或纯空白时不提交。
- 后端报错（撞名）上屏到 `role="alert"`。

### 门禁

`cargo test --manifest-path src-tauri/Cargo.toml`（bin crate，`--lib` 不可用）与 `npx tsc --noEmit` + `npm test` 全绿。
