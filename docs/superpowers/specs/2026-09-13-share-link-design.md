# 分享订阅链接 设计

**目标：** 把选中的几条线路做成一条**公网可达、可单独收回**的订阅链接发给别人，而不是把配置文件整个发出去。

**范围：** 新增「分享」页与后端 share 模块。**不含**内容动态更新（快照制）、**不含** sing-box 格式输出、**不含** HTTPS。

## 现状（实现前已核对源码）

1. **本地订阅服务只绑回环**：`serve/mod.rs:44` `SocketAddr::from(([127,0,0,1], port))`，且只有一条全局 token（`sub_token` 设置）。发给别人的链接对方打不开自己的 `127.0.0.1`，换 token 又会连自己的订阅一起废掉。
2. **服务端不管发了哪些节点**：`serve/mod.rs:65-78` 命中 token 就返回**全部**合并配置缓存。
3. **可复用的现成零件**：
   - `servers/deploy.rs:606` `write_remote_file(sess, cancel, step, path, content, timeout)`——base64 管道写远端文件
   - `servers/deploy.rs:646` `run_cmd_ok(sess, cancel, step, cmd, timeout)`——带取消感知的命令执行
   - `servers/deploy.rs:37` `port_probe_cmd(port)`——`ss -tln 'sport = :N' | grep -q '^LISTEN'`
   - `relay/build.rs:188` 的 SSH 会话建立范式：`load_auth(state, id)` → `SshSession::connect(host, port, user, auth, CONNECT_TIMEOUT)`
   - `servers/channels.rs` 的 iptables 放行/封锁范式（含**拒绝封锁 SSH 端口**的防自锁保护）
4. **远端有 python3**：`servers/reality_dest.rs:92` 已在用 `python3 -c` 改 JSON。本设计依赖它起静态服务，**但生成前仍要探测**（不同镜像可能没装）。
5. **导出子集的生成逻辑已存在**：`commands::build_config_for(state, node_ids, ExportFormat::Yaml)` 返回 `(yaml, skipped_ssr)`，与 `rebuild_config` 同口径（E 系列刚做的）。

## 设计

### 核心决定：分享的**真实状态在服务器上**

app 只是「装」与「拆」的执行者。服务器上装着的东西自成一体（文件 + systemd 单元 + 防火墙规则），app 被卸载、重装、本地库丢失，分享照样在跑，也照样能被下次 `list` 列出来。因此**本地不建表**——`list` 每次都 SSH 去实读。

### 服务器上的形态（每个分享一套）

| 项 | 值 |
|---|---|
| 目录 | `/srv/vpnshare/<token>/` |
| 内容 | `config.yaml`（固定快照，写入后不再变） |
| systemd 单元 | `/etc/systemd/system/vpnshare-<token>.service` |
| 服务 | `python3 -m http.server <port> --directory /srv/vpnshare/<token> --bind 0.0.0.0` |
| 防火墙 | `iptables -I INPUT 1 -p tcp --dport <port> -m string --string "GET /sub/<token>" --algo bm -j ACCEPT` |
| 链接 | `http://<服务器host>:<端口>/sub/<token>` |

**token**：16 位十六进制（UUID v4 去横线取前 16 位）。**只含 `[0-9a-f]`** 是刻意的——它要拼进 systemd 单元名与 iptables 命令行，限制字符集可省掉一整类转义问题，也让 `run_cmd` 的路径白名单校验（`deploy.rs:600` 的 `is_safe_path_part`）直接可用。

**防火墙规则刻意限定路径**而不是裸开端口：`-m string --string "GET /sub/<token>" -j ACCEPT` 只放行**带正确 token 的请求**，其余请求被链路丢弃（不是 404，而是 TCP 层就没有响应，等于端口隐身）。代价是依赖 xt_string 模块（`iptables -m string`），绝大多数发行版默认可用；探测阶段验证一次，不可用就报错而不是裸开端口降级。

### 数据流

```
前端 分享页 [新建分享]
  → 选服务器（list_servers）→ 勾节点（复用 export 选择弹窗）
  → create_share_cmd(serverId, nodeIds)
      1. build_config_for(state, nodeIds, Yaml)        ← 与导出同口径
      2. 空配置守卫（与导出共用 ensure_exportable_has_nodes）
      3. SSH 连服务器
      4. 探测：python3 存在？iptables -m string 可用？端口占用？（port_probe_cmd）
      5. 端口从 18080 起找第一个空闲的
      6. write_remote_file(config.yaml) + 写 systemd 单元
      7. systemctl daemon-reload && enable --now vpnshare-<token>
      8. iptables -I INPUT 1 ... -j ACCEPT
      9. 回读 systemctl is-active 确认真的起来
  → 返回 ShareInfo（token/port/url/node_names）

前端 [作废]
  → delete_share_cmd(serverId, token)
      systemctl disable --now → rm 单元 → daemon-reload → rm -rf 目录 → iptables -D
      （每步幂等：不存在不算错）
```

### 三个必须在 UI 上讲清的点

1. **链接就是钥匙**：拿到链接的人即可使用这些节点，且**服务是纯 HTTP、没有加密**——链接在传输途中被截获即等于泄漏。生成时在页面上明示。
2. **快照不跟更新**：你之后改节点、改名、删节点、重新部署，链接里仍是生成那一刻的内容。要更新就重新生成一条。
3. **仅 YAML**：mihomo / Clash 系客户端可直接用；sing-box 吃 JSON，本次不做。

### 边界

| 情况 | 行为 |
|---|---|
| 服务器没有 python3 | 探测即报错「服务器上未找到 python3，无法提供分享服务」，不静默失败 |
| iptables 不支持 `-m string` | 探测即报错（规则是本设计的安全前提，不能降级成裸开端口） |
| 端口全被占用 | 探测范围内（18080-18180）无空闲 → 报错，不覆盖别人的服务 |
| 端口与已登记的通道重叠 | 检查本地 `channels` 表 + `port_probe_cmd`，撞上就换下一个 |
| 节点全被跳过（如全是 ssr） | 空配置守卫拒绝，与导出同一错误文案 |
| ssh 端口 | 不在本次范围（不触碰 22 端口的规则） |
| 服务器上手动删了分享 | `list` 以实际探测为准，列表里自然消失 |
| 作废时服务已不存在 | 各步幂等，返回成功 |

## 接口

```rust
// src-tauri/src/share/mod.rs
pub struct ShareInfo {
    pub server_id: i64,
    pub server_name: String,
    pub host: String,
    pub token: String,
    pub port: u16,
    pub url: String,          // http://<host>:<port>/sub/<token>
    pub node_names: Vec<String>,  // 从快照 YAML 里解析出的 proxies 名
    pub active: bool,         // systemctl is-active
}

/// 列表 + 读取失败的服务器。
pub struct ShareList {
    pub shares: Vec<ShareInfo>,
    pub unreachable: Vec<String>,
}

pub async fn list_shares(state: &Arc<AppState>) -> Result<ShareList, String>;
pub async fn create_share(state: &Arc<AppState>, server_id: i64, node_ids: &[String])
    -> Result<ShareInfo, String>;
pub async fn delete_share(state: &Arc<AppState>, server_id: i64, token: &str) -> Result<(), String>;
```

`list_shares` 遍历所有服务器：读 `/srv/vpnshare/` 的目录清单 + `systemctl is-active` + 从单元文件解析端口 + 从快照解析节点名，逐台 SSH。

某台连不上时**不整体失败**：好那台的分享照常列出，坏那台的名字进 `unreachable`。只能标到**服务器**粒度、标不到分享粒度——本地不存分享记录，连不上就无从得知它上面有哪些分享。UI 必须把 `unreachable` 显示出来，否则用户会以为发出去的链接凭空消失了。

## 测试

### 后端（`FakeSshServer` 驱动，照 `relay/` 与 `servers/` 的既有范式）

- 探测失败分支：无 python3 → 明确报错；`iptables -m string` 不可用 → 明确报错。
- 端口分配：18080 被占 → 落到 18081；18080-18180 全占 → 报错。
- 创建：写入的 `config.yaml` 内容 = `build_config_for` 的同口径产物；单元内容正确；命令序列含 daemon-reload/enable/iptables。
- 作废：命令序列正确；服务不存在的幂等路径不报错。
- token 白名单：非 `[0-9a-f]` 的 token 进 `delete_share` 被拒（防命令注入）。
- `list_shares`：一台服务器连不上不影响列出另一台的。

### 前端

- 分享页列出分享（token/端口/链接/节点名/状态）。
- 新建向导：选服务器 → 勾节点 → 生成后链接上屏；空选择禁用按钮。
- 作废：二次确认（复用 `ConfirmButton`）→ 调 `delete_share_cmd` → 刷新。
- 复制链接按钮。
- 三个边界提示可见（无加密 / 快照 / 仅 YAML）。

### 门禁

`cargo test --manifest-path src-tauri/Cargo.toml`、`npx tsc --noEmit`、`npm test` 全绿。
