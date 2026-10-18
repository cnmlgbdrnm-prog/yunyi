# 全仓缺陷修复 设计

- 日期：2026-09-27
- 状态：**待实施**（设计经逐条确认后成文）
- 来源：2026-09-27 全仓评审（含一次针对服务器侧模块的专项巡检），缺陷逐条回读源码复核
- 依赖：无新三方依赖。阶段③引入 `useSyncExternalStore`（React 18+ 内置，已在用 React 19）

**目标：** 修掉评审确认的 27 条缺陷，重点是三类**静默破坏**——读失败被当成"没有"而覆盖用户
数据、凭据在不该轮换时轮换导致节点变砖、页面切换后仍在跑的远端任务失去跟踪与取消入口。

**范围：** `src-tauri/src/{servers,relay,share,crypto,db,jobs,net}` 与 `src/{App.tsx,views,
components,api}`。不新增实体、不改配置格式。

**不做：**

- **不重写架构**。评审的结论是"问题不是无知导致的，而是知道这个陷阱、但在另一条路径上忘了
  应用"（`privacy.rs` 的 `|| true` vs 同仓 `deploy.rs:290` 判 `exit_code`；`need_singbox` 的
  轮换 vs 33 行之前 `need_ss` 的复用；`valid_iface_name` 写了却只调一处）。因此修法是**把已经
  存在的正确做法铺到它没铺到的地方**，不是引入新范式。
- **不做全链路结构化错误契约**。见 §3.4，当前没有任何 UI 需要按错误类别分支。
- **不修 `serve/mod.rs:66` 的 token 非常量时间比较**。token 是 UUIDv4（122 bit）且仅回环绑定，
  不构成实际攻击面。记录在案即可。
- **不把"取消"做成抢占式**。SSH 命令一旦发出，中途中断由命令粒度决定；本设计只承诺在**步骤
  边界**检查取消位，不承诺打断执行中的命令（见 §3.5）。

## 1. 缺陷清单

编号沿用评审报告。**🔴 五条已逐条回读源码确认**，其余来自专项巡检。

### 🔴 严重

| # | 缺陷 | 位置 |
|---|---|---|
| 1 | 分享 token 可被目录列举拿到 | `share/mod.rs:91-104,121-123` |
| 2 | torrc 读失败 → 整份文件被覆盖 | `servers/privacy.rs:630-638` / `850-858` |
| 3 | wg0.conf 读失败 → 轮换接口私钥 + 整文件覆盖 | `relay/build.rs:932-946` |
| 4 | sing-box 凭据每次部署都轮换 → 静默变砖 | `servers/deploy.rs:1193-1196` |
| 5 | 切页丢失任务跟踪 → 重复远端部署 | `App.tsx:56-63` + `Servers.tsx:419` |

### 🟠 主要

| # | 缺陷 | 位置 |
|---|---|---|
| 6 | busy 守卫在 `await` 之后才提交 | `Nodes.tsx:145-146` / `Servers.tsx:225-234` |
| 7 | `setLiveResults({})` 抹掉另一个 job 的结果 | `Servers.tsx:230,383` / `Nodes.tsx:292,330` |
| 8 | 互斥锁毒化处理不一致（一半抗毒化、一半连锁 panic） | `jobs/mod.rs`(9) `net/connlog.rs`(9) `net/mmdb.rs`(2) `db/mod.rs:32` |
| 9 | 错误类型擦成字符串；终态判定前端存在两套约定 | `commands.rs:25-41`；`Servers.tsx:377` vs `LinkWizard.tsx:76` |
| 10 | 远端输出回流成 root 命令 | `relay/build.rs:1290-1313` / `256-266` |
| 11 | SSH 主机密钥不校验（TOFU 在 backlog） | `ssh/mod.rs:56-62` |
| 12 | 远端写文件非原子（truncate-then-stream） | `servers/deploy.rs:783` |
| 13 | 凭据加密静默降级明文，无可见性 | `crypto/mod.rs:158-168` |
| 14 | 取消在 BBR 段与回写段是空操作却仍报成功 | `deploy.rs:1362-1376,1380-1446` |
| 27 | **`wg0.conf`（含私钥）落盘权限从未设置**（设计阶段新增） | `deploy.rs:770-786`，relay 全模块无 chmod |

### 🟡 次要

| # | 缺陷 | 位置 |
|---|---|---|
| 15 | 7 处重复事件订阅脚手架；2 处无对账兜底（丢一个终态事件即永久锁死面板） | `DevicePeersPanel.tsx:48-72` / `LinkWizard.tsx:66-85` |
| 16 | 巨型组件 | `Servers.tsx` 845 行 / `Nodes.tsx` 750 / `Dashboard.tsx` 540 |
| 17 | 派生状态与后端真相重复；`listDeployedProtocols` 死代码 | `Servers.tsx:72` vs `api/index.ts:398` |
| 18 | 定时器依赖 `[rules,…]` → 每次刷新被拆了重建 | `Relay.tsx:86-98` |
| 19 | clipboard 无 catch → 静默 unhandled rejection | `Shares.tsx:217` |
| 20 | UAC 轮询无卸载保护（全仓唯一一处） | `Settings.tsx:116-138` |
| 21 | 模块级缓存删除时不清 | `views/relayStatusCache.ts` + `Relay.tsx:161,262` |
| 22 | 索引拼 key → 每秒整表重挂载 | `ConnLogPanel.tsx:197` |
| 23 | 迁移无降级护栏（`user_version` 超前时静默跳过） | `db/migrations.rs:263-269` |
| 24 | 测试文件命名不符 `*_tests.rs` 约定，被全局 grep 漏掉 | `server_speed/mod_test.rs` |
| 25 | 供应链：Xray 装可变 `main` 脚本、ss/sing-box 无校验和（tor 已钉 sha256） | `deploy.rs:1011,1020-1038` |
| 26 | token 非常量时间比较 — **记录不修** | `serve/mod.rs:66` |

## 2. 需求前提

1. **静默失败是这批缺陷的共同根因**。用户看到的不是错误，而是"节点坏了""我的 torrc 没了"。
   修法的第一原则是让失败**响亮**。
2. **本仓"既有测试已覆盖"的纸面断言不可信**——已有两次实测被推翻的记录。RED 必须以实测为准。
3. **前端无一处裸 `invoke`**（已核实 100% 走 `api/index.ts`），因此错误形状变更可单点归一化。
4. **终态事件与逐目标事件必须分开判**（本节于 2026-09-27 实施阶段更正，原判断有误）。
   后端补发的**终态**事件（`jobs/mod.rs:253-275`）`message` 恒为空串，失败带 `error=Some`、
   成功带 `progress=100`；而 `speed` / `server_speed` 的**逐目标**事件恰恰用 `error` 字段承载
   单台失败原因（`jobs/mod.rs:219-227`，原文注释「前端按 message=node_id 定位」）。
   因此 `progress >= 100 || error != null` 对后两类 kind 会把「一台机器失败」误判成「整个任务
   终结」，后续目标再也不上屏。正确判据：**`message` 命中本任务已登记的目标 id = 逐目标事件
   （只记结果、不判终态）；`message` 为空且（`progress >= 100` 或 `error != null`）= 终态**。
   后端不必改，但 `jobs/mod.rs:223-227` 的 error 重载属**契约**，不得视为可顺手清理的死代码。
5. `privacy.rs:255` 的 `never_cancel`（回滚用恒假 flag）是**正确设计**——回滚必须跑完——但回滚
   之后必须重新检查取消位，不能继续走成功路径。

## 3. 设计

### 3.1 总体原则

1. **区分"不存在"与"读失败"**。新增远端读取原语，把这两个此前不可区分的结果分开，读失败一律
   `Err` 中止，`Absent` 才走首次路径。
2. **先逼红**。每处修复先构造能复现缺陷的用例并**实际看到它红**，再改实现。
3. **契约零破坏**。线上 IPC 形状不变；前端视图层的 `catch` 一行不动。

### 3.2 阶段①（P1）远端破坏性路径 — #2 #3 #4 #12 #27

**A. 统一远端读取语义（#2 #3 的共同根因）**

新增（建议落 `servers/deploy.rs`，与 `write_remote_file` 同处，`pub(crate)` 供 relay 复用）：

```rust
/// 远端文件读取：区分「文件不存在」（正常）与「读取失败」（异常）。
pub(crate) enum RemoteFile { Absent, Content(String) }

/// 单次往返：存在则 cat 并 exit 0；不存在则 exit 42；其它非零 = 读失败。
/// 注意 [ -e ] 为真但 cat 失败（权限/IO）→ cat 非零 → 落 Err，正是要的语义。
pub(crate) async fn read_remote_file(
    sess: &mut SshSession, cancel: &AtomicBool,
    step: &str, path: &str, timeout: Duration,
) -> Result<RemoteFile, String> {
    let script = format!("if [ -e '{path}' ]; then cat '{path}'; else exit 42; fi");
    let out = run_cmd(sess, cancel, step, &script, timeout).await?;
    match out.exit_code {
        0 => Ok(RemoteFile::Content(out.stdout)),
        42 => Ok(RemoteFile::Absent),
        _ => Err(format!("读取 {path} 失败：{}", stderr_tail(&out.stderr))),
    }
}
```

（`stderr_tail` 现只在 `relay/build.rs:1317` 有一份私有实现；本阶段提到共用处，与
`shell_quote` 的提取合并为同一次动作——见 #10。）

改造点：

- `relay/build.rs:932-946` `read_conf` 返回 `RemoteFile`。**`Absent` 才走"首次建链"**（生成新私钥、
  整文件渲染）；`Content` 走合并；**读失败直接 `Err` 中止建链**，不轮换私钥、不覆盖。
- `servers/privacy.rs:630-638` 与 `850-858`：去掉 `2>/dev/null || true`，改用该原语。读失败一律
  中止。`run_privacy_remove_job` 另需补备份+回滚（当前只有 `run_privacy_job` 有）。

**B. 远端写文件原子化 + 显式权限（#12 #27）**

```rust
/// mode: wg0.conf=600；其余配置 644。dir_mode: wg0 的父目录=700，其余 None。
let mut cmd = format!(
    "mkdir -p '{parent}' && \
     echo '{b64}' | base64 -d > '{path}.new' && \
     chmod {mode} '{path}.new' && \
     mv -f '{path}.new' '{path}'"
);
// 父目录显式 chmod（幂等）：`mkdir -p -m` 只对**新建**目录生效，
// 已存在但权限是 755 的 /etc/wireguard 它修不了
if let Some(dm) = dir_mode {
    cmd = format!("chmod {dm} '{parent}' && {cmd}");
}
```

`mv -f` 在同一文件系统内是原子的。**必须带 `chmod`**：现状 `> path` 会保留既有文件的模式，改成
`> tmp && mv` 会把 tmp 的 umask 默认模式（644）装上去——**反而把已经是 600 的 wg0.conf 降级**。
这正是 #27 必须在同一阶段修掉的原因，否则"修复"会制造新漏洞。

父目录用**显式 `chmod` 而非 `mkdir -p -m`**：后者只对新建目录生效，已存在却是 755 的
`/etc/wireguard` 修不了，而恰恰是这种情况最需要修。

**C. sing-box 凭据复用（#4）**

照 `parse_singbox_ports`（`deploy.rs:318-332`）增加 `parse_singbox_creds`，从既有
`/etc/sing-box/config.json` 读回 hy2 password / tuic uuid+password 并复用；读不回才新生成。与
`need_ss`（`deploy.rs:1163-1168`）同构——那里已经把理由写清楚了："轮换 = 两侧短暂不一致，中途
失败就把节点变砖"。

**RED（必须先看到红）**

1. FakeSshServer 对 `cat {path}` **显式**回 `exit 1` + stderr → 断言 job 落 failed，**且断言
   「没有发出任何写盘命令」**。关键是不止断言"报错了"，要断言**没有覆盖**——否则把 `|| true`
   换成"报错但照写"也能骗过测试。
2. 对 `cat {path}` 回 `exit 42` → 断言仍走首次建链/首次写入（正常路径不回归）。
3. 断言 `write_remote_file` 发出的命令含 `chmod 600`（wg0 用例）与 `chmod 644`（xray 用例）。
4. sing-box：部署两次，断言第二次的 config.json 里凭据与第一次相同。

> **夹具陷阱（本仓已知）**：`ssh/tests.rs:229-231` 的 FakeSshServer 对未命中命令回落
> `exit 0 + 空 stdout`，因此 `(0,"","")` 夹具条目是**装饰性的**。本阶段所有新夹具必须用**显式
> exit code**，并先逼红确认断言真的能抓住回归。

### 3.3 阶段②（P2）分享 token — #1 #25

**A. 换掉 `python3 -m http.server`（#1）**

`http.server` 在无 `index.html` 时**必然列目录**；而文档根就是 `share_dir(token)`。改名成固定
文件名**修不好**——目录名仍是 token，列举照样暴露。**必须让服务不再列举**。

落一个极小脚本（走 `write_remote_file`，base64 传输天然免疫转义问题），`ExecStart` 指向它：

```python
import http.server, socketserver, os, sys
os.chdir(sys.argv[2])                      # 不用 directory= 参数，避免依赖 python 3.7+
class H(http.server.SimpleHTTPRequestHandler):
    def list_directory(self, path):
        self.send_error(404)               # 服务端不再暴露目录结构
        return None
    def log_message(self, *a):
        pass
socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("0.0.0.0", int(sys.argv[1])), H) as s:
    s.serve_forever()
```

**B. 已存在的分享自动升级**

`create_share` / `list_shares` 检测 unit 的 `ExecStart` 与当前模板不一致 → 重写 unit +
`daemon-reload` + `restart`（幂等）。老分享无需用户手工重建。

**C. 供应链一致性（#25）**

ss / sing-box 二进制补 sha256 校验和（tor 已钉，属同一套模式应用不均）；Xray 安装脚本从可变的
`main` 分支改为固定版本 URL。

**RED**：断言生成的 unit 内容指向脚本而非 `-m http.server`；断言脚本内容含 `list_directory`
覆盖；升级路径：给定一个旧形态 unit，断言 `list_shares` 后它被重写。

### 3.4 阶段③（P3）前端任务跟踪 — #5 #6 #7 #15 #17 #9(终态部分)

新建 `src/jobs/registry.ts`：模块级 `Map<jobId, TrackedJob>`，经 **`useSyncExternalStore`** 接进
React（React 官方方案，避免自定义订阅在并发渲染下的撕裂）。

- **`listen` 只注册一次**（模块级，App 启动时），不再每视图各注册 → 一次消灭 7 处重复脚手架，
  `DevicePeersPanel` / `LinkWizard` 缺对账兜底的问题随之消失（#15）。
- **终态判定收进注册表一处**（判据见 §2.4，后端不必改）：`message` 命中已登记目标 = 逐目标事件，
  只记结果；`message` 为空且 `progress >= 100 || error != null` = 终态。两套
  `'完成'/'结束'` vs `progress>=100` 的约定自然合并（#9 的前端部分）。
- **busy 守卫提前**：IPC 调用**前**先 `reserve(nodeId)` 乐观占位，失败或终态时释放 → 关掉
  `await` 窗口（#6）。
- **结果按 jobId 分桶**，终态只清该桶 → #7 消失。
- `deployedProtos` 改从 `listDeployedProtocols` 读（或删死代码），消除派生状态重复（#17）。

**RED**：`切到下一视图再切回，进行中的 job 仍显示忙碌且可取消`（当前**必然红**）、`双击只发一次
IPC`、`job A 完成不清空 job B 的行内结果`。

### 3.5 阶段④（P4）Rust 一致性 — #8 #14 #23 + #9

**A. 锁毒化统一（#8）**

抽 `lock_ignore_poison()` helper（`unwrap_or_else(|e| e.into_inner())`），把 `jobs/mod.rs`(9)、
`net/connlog.rs`(9)、`net/mmdb.rs`(2)、`db/mod.rs:32` 统一过去——仓内已有 19 处这个写法，抽出来
正好消除重复并让意图显式。`Db::lock()` 是全局最热路径，收益最大：DB 锁一旦毒化，当前是**整个
应用连锁 panic**。

**B. 错误类型（#9）**

`AppError` 扩展为多 variant（内部可 `match` 分类），**`Serialize` 仍输出字符串** → 线上契约不
变，前端视图层不动。将来若 UI 需要按类分支，在 `api/index.ts` 单点归一化即可。这是刻意的最小
化：当前没有任何一处 UI 真的需要按类别分支，做全链路 `{code,message}` 是无需求驱动的破坏性变更。

**C. 取消语义（#14）**

在 BBR 段（`deploy.rs:1362-1376`）与回写段（`1380-1446`）**之前**加取消检查点：已取消则跳过
BBR、回写段前已取消则落 `cancelled` 而非继续。不承诺打断执行中的命令（SSH 粒度决定）。
`privacy.rs` 回滚后重新检查取消位，避免回滚完继续走成功路径（`set_onion` / `sync_local_private`）。

**D. 迁移降级护栏（#23）**

`migrations.rs:263-269`：`user_version > MIGRATIONS.len()` 时报可读错误而非静默跳过。这个形态
本仓已踩过一次（跨分支共享数据目录）。

### 3.6 阶段⑤（P5）清理 — #10 #11 #13 #16 #18–#22 #24 #26

- **#10 注入面**：`shell_quote`（已存在于 `server_speed/iperf.rs:36`）提到共用处；`egress_if`
  （`relay/build.rs:256-266`）与 `read_conf` 同址，按 `valid_iface_name`（同文件 744 行，已写却
  只在一处调用）的字符集白名单在边界校验、使用点单引号。**顺带修** `line.replace("-A",…)`
  会把注释里的 `-A` 一并替换的正确性 bug。
- **#11 主机密钥**：TOFU 落地（首次连接记录指纹，之后不匹配即拒绝），替代恒 `Ok(true)`。
- **#13 凭据降级可见**：`seal_param` 失败降级明文时落一个标记位/告警，让用户看得见。
- **#16 组件拆分**：`Servers.tsx` / `Nodes.tsx` 按 279 个前端测试兜底拆分。
- **#18–#22 #24**：逐条小修；`server_speed/mod_test.rs` 改名为 `_tests.rs` 后缀以符合约定。
- **发布**：CHANGELOG「未发布」已积 787 行、自 0.2.4（09-01）未发版，本批完成后切一版。

## 4. 验证策略

每阶段独立提交、独立验证，命令与 CI 门禁同源：

```
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
npm test && npx tsc --noEmit
```

- 基线（2026-09-27 实测）：clippy ✅ · cargo test **811 passed / 0 failed / 3 ignored** · vitest
  **279 passed**。任何阶段结束时不得低于此基线。
- 每处修复的 RED **必须先实际看到红**，不接受"纸面上这个断言应该会失败"。
- 阶段①②另需真机验证（本机可免密 SSH 到 203.0.113.127 / 203.0.113.17）：分享链接改完后
  **用端口扫描器视角确认 `GET /` 与 `GET /sub/` 不再列目录**、且原链接仍可取到配置。
