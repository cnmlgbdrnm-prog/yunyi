# 部署端口随机化设计

- 日期：2026-11-01
- 状态：设计已确认，待评审后实施
- 涉及：[`src-tauri/src/servers/deploy.rs`](../../../src-tauri/src/servers/deploy.rs)、
  [`deploy_spec.rs`](../../../src-tauri/src/servers/deploy_spec.rs)、
  [`src-tauri/src/share/install.rs`](../../../src-tauri/src/share/install.rs)、
  [`naming.rs`](../../../src-tauri/src/share/naming.rs)

**目标：** 新部署的服务器协议端口与分享端口不再使用固定默认值，改为在限定池内随机
起点分配；已部署的协议/分享在重部署时继续复用远端现有端口，不迁移。

**范围：** 主协议部署（vless / trojan / vmess / ss / hysteria2 / tuic）与分享
（订阅口、数据口、回环内部口、CDN 自动口）的**自动分配**逻辑。

**不做：**

- 不迁移存量服务器与存量分享；重部署继续复用 `read_existing_ports` /
  `reserved_share_ports` 读到的端口。
- 不改上游出口专用实例（`upstream.rs::pick_listen_port`）与中转扩展端口
  （`relay/extend.rs::allocate_forward_port`）。
- 不改协议参数、节点写回格式与客户端侧任何逻辑；节点行记录实际端口即可。
- 不做端口选择 UI、不做「重新随机」按钮。

## 1. 现状

- 主协议：`default_port(proto)` 返回固定端口（443/4443/8443/8388/2097/2053），
  被占时 `allocate_port` 从该值起逐 +1 探测，最多 21 个候选。
- 分享：三段固定区间顺序扫描，`find_free_port` 从段首开始，跳过远端保留端口与
  正在监听的端口。
- 重部署：主协议从远端 `config.json` 读回端口；分享从 `share.json` 读回，均复用。
- 分享 CDN 自动模式：按 `HTTPS_PORTS` 固定顺序取第一个空闲端口。

问题：首次部署端口可预测，容易被按默认端口名单扫描。

## 2. 主协议随机分配

### 2.1 端口池

```
池：10000–65535，排除分享段 18080–18799
段 1：10000–18079（8080 个）
段 2：18800–65535（46736 个）
总计：54816 个
```

排除分享段是为了保持现有性质：主协议与分享端口永不重叠。分享分配只依赖
`ss` 探测与 manifest 保留，若主协议随机落入该段，两边在「服务已登记但暂时停机」
的窗口里会互相抢占。分享段边界直接引用 `share/naming.rs` 的
`PORT_RANGE_START` / `INTERNAL_PORT_END`，不复制数字。

### 2.2 起点与候选序列

- 起点：`rng.gen_range(0 .. 54816)`，按段长映射为实际端口：
  - `index < 8080` → `10000 + index`
  - 否则 → `18800 + (index - 8080)`
- 候选：从起点沿逻辑索引 `+1` 前进，**回绕**，最多 `MAX_PROBES = 64` 个。
- 分布：起点在池内均匀；顺序扫描只在遇到占用时前进，不改变均匀性。

新增模块 `src-tauri/src/servers/port_pool.rs`，只放纯函数与常量：

- `MAIN_PORT_MIN` / `MAIN_PORT_MAX` / `MAX_PROBES`
- `random_pool_start(rng) -> u16`
- `pool_candidates(start, limit) -> Vec<u16>`（回绕、跳过排除段、不重复）

### 2.3 分配流程（`deploy.rs`）

1. `read_existing_ports` 不变，先读远端已有端口与凭据。
2. 维护 `taken: HashSet<u16>`：
   - 初始化时放入 `existing.ports` 的**全部**端口（不只本次请求的协议）。
     这样即使某个已部署协议本次未请求、服务恰好停机，新协议也不会抢它的端口。
   - 每分配一个端口立即插入。
3. 每个请求协议：
   - 远端已有该协议 → 复用，不探测、不随机。
   - 否则 → `allocate_port(sess, cancel, protocol, &mut taken)`：
     生成随机起点 → 依次检查候选：`taken` 中的跳过；其余用
     `listen_probe_cmd(protocol, port)` 探测；第一个空闲即返回并插入 `taken`。
4. 64 个候选全被占用 → 报错：
   `端口池 10000–65535 中连续 64 个候选均被占用`。

`default_port()` 删除；`jobs/mod.rs` 中「按 default_port + 占用偏移」的注释同步
改为随机池描述。

## 3. 分享随机分配

### 3.1 区间内随机

三个区间各自随机起点、回绕顺序扫描：

```
订阅 HTTP：18080–18180
公网数据：18200–18499
回环内部：18500–18799
```

新增 `src-tauri/src/share/ports.rs`（纯函数）：

- `random_start_in(range, rng) -> u16`
- `range_candidates(range, start) -> impl Iterator<Item = u16>`（回绕、不重复）

`install.rs::find_free_port` 改为接收区间与排除集合，内部使用
`range_candidates`；探测逻辑与错误语义不变。

### 3.2 同次分配去重

`allocate_share_ports` 维护 `taken`：

- 初值 = `reserved_share_ports` 的返回值（远端已有分享保留的端口）。
- 每分配一个订阅口/公网口/内部口就插入 `taken`。

现在靠游标单调前进保证不重复；随机起点后必须显式维护，否则同一次创建里多个节点
可能抽到同一个端口。

`allocate_share_ports` 拆成：

- `random_share_starts(transports) -> SharePortStarts`：同步生成三段随机起点
  （`SharePortStarts { http, pairs }`）。RNG 不跨 `await` 传递（`ThreadRng` 非
  `Send`），起点算完即丢弃；测试可直接构造 `SharePortStarts`；
- `allocate_share_ports_with_starts(sess, cancel, transports, &starts)`：按给定起点
  分配，供测试使用；
- `allocate_share_ports(...)`：生产入口，内部调用 `random_share_starts` 生成起点后
  转调 `allocate_share_ports_with_starts`。

编辑分享时新增节点走同一函数，既有节点仍沿用原端口（调用方传入的 `pairs` 不经过
分配）。

### 3.3 CDN 自动口

- 用户显式指定端口：保持不变，只探测该端口。
- 自动模式：从 `HTTPS_PORTS`（`[2083, 2053, 2087, 2096, 8443, 443]`）中随机
  起点、回绕探测，跳过保留与已占用端口；整表都被占用才报错。
- 端口仍只能落在这 6 个 Cloudflare 支持的端口里，不引入任意端口。

## 4. 数据流与兼容

```
部署请求
  → read_existing_ports（远端 config.json / share.json）
  → 协议已存在？ 是 → 复用旧端口
                  否 → 随机池起点 → 跳过 taken → ss 探测 → 分配
  → 写配置 / 防火墙 / 节点参数（记录实际端口）
```

- 存量部署不迁移：重部署后的节点参数与客户端配置保持原端口。
- 新协议/新分享才消费随机端口。
- 节点写回、订阅导出、链路/中转引用均读取实际端口，无格式变化。
- 前端无需改动；`DeployedProtocolList` 仍显示后端返回的实际端口。

## 5. 错误处理

- 主协议池耗尽：`端口池 10000–65535 中连续 64 个候选均被占用`。
- 分享某段耗尽：`端口段 <start>–<end> 已耗尽`（回绕扫完整段才判定耗尽）。
- SSH 探测失败：沿用 `run_cmd` 的错误向上传递，不降级为「端口空闲」。
- 随机数生成不可用属于进程级故障，不单独处理。

## 6. 测试

`tests/rust` 为本地测试源码（`.gitignore` 中 `/tests/`），不随仓库上传，但本地
`--features local-tests` 下必须通过。

纯函数：

- `random_pool_start`：大量样本全部落在两段内，且不落入 18080–18799。
- `pool_candidates`：起点 65500 时回绕到 10000；候选不重复、不越界、长度不超过 64。
- `range_candidates`：分享三段各自的回绕与不重复。

分享分配（FakeSshServer + 直接构造 `SharePortStarts` 起点）：

- 随机起点空闲时，探测命令落在该起点上；
- 起点被占时顺延；段尾回绕；
- 跳过 `reserved` 与本次已分配端口；
- 整段占用时报「已耗尽」。

CDN：

- 自动模式的候选序列由随机起点轮转生成，单测用固定起点验证轮转；
- 指定端口被占时仍报错，不换端口。

回归：

- 重部署复用端口的既有用例（`parse_*`、`read_existing_ports`）保持通过。
- `default_ports_follow_spec_table` 替换为端口池断言；更新测试导入。
- `cargo fmt --check`、`cargo clippy`。
- 本地测试命令：
  `cargo test --features local-tests -- --skip net::runtime_tests --skip net::runtime_service_tests`
  （网络模块用例会自跳过；按仓库约定显式跳过）。

## 7. 风险与已知边界

- **云安全组**：随机端口必须在服务商侧放行才能从公网连上；应用只能自动写服务器
  本机 UFW/iptables。首次部署后若客户端连不上，先查云安全组。写入 CHANGELOG。
- **vless 不再默认 443**：REALITY 的伪装收益削弱；用户已确认接受。
- **中转扩展端口的 DNAT 端口**不产生本地监听，主协议分配感知不到；撞上的概率约为
  「DNAT 条数 / 54816」，本次不处理。分享分配同理。
- **端口池跨重启不记忆**：随机只在首次分配时发生，端口落库；数据库丢失后重部署
  会得到新端口，与现有行为一致。
- **分享 CDN 端口**仍受 Cloudflare 支持列表限制，只有 6 个候选，随机收益有限。

## 8. 验收标准

1. 新协议首次部署的端口落在 10000–65535，且不在 18080–18799。
2. 同一台服务器一次部署多个协议，端口互不重复，且不与远端既有协议端口重复。
3. 重部署不改动已存在协议的端口。
4. 新分享的订阅口/数据口/内部口分别落在各自区间，互不重复，且不与已有分享重复。
5. CDN 自动模式只从 Cloudflare 支持的 6 个端口中选。
6. 上述行为有纯函数测试与 FakeSshServer 集成测试覆盖，本地测试全绿。
