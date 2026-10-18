# 连接日志「流量」列：拆上行/下行 + 会话总计 设计

- 日期：2026-10-02
- 状态：**待实现**
- 依赖：无。不新增实体、不改配置格式、不改 IPC 命令名（只在既有快照结构体上加两个字段）

**目标：** 让用户看到「本次开启网络以来**经 mihomo** 的累计上下行」，而不再只有「单条连接的
累计」——后者天生只有几百 KB（一条 HTTP/2 连接往往只承载一两个请求），用户读成了「流量怎么
这么少」。

**范围：** `net/connlog.rs` 的取数与快照结构；`ConnLogPanel.tsx` 的列与顶部条；两侧类型与测试；
`connlog_tests.rs` 新增假 mihomo 服务器夹具。

**不做：**

- **不做实时速率。** mihomo `/traffic` 给 `up`/`down`（bytes/s），但用户要的是会话累计。速率是
  另一个控件的事（YAGNI）
- **不做「本次已开启时长」。** 唯一现成的「开始时间」是采样器启动时刻，而采样器会在「再点一次
  开启网络」时重启（`net/runtime.rs:429-431`）而 mihomo 不重启——时长会归零、流量不会，界面就
  开始撒谎。要做对得去取 mihomo 子进程的创建时间（Win32 `GetProcessTimes`），是另一件事。
  用户已确认先不做
- **不按进程/域名聚合。** 连接日志的身份是「连接证据」（判决 + 理由 + 判据），改成用量排行会
  削掉它的泄漏检测能力。用户已明确选「顶部总计 + 行拆上行/下行」
- 不动判决、理由、`rowIdentity`、`—` 约定、`COLUMNS` 之外的表格结构

## 1. 需求前提

1. 用户读「流量」列时想知道的是「**本次**用了多少」，而当前实现是**单条连接**的累计——两者
   不是同一层的量，前者天生大不起来
2. 上行与下行必须**分开**：合成一个数会掩盖「只上传不下载」（或反之）的异常形态
3. 会话累计的边界必须是「开启网络 → 关闭网络」，**重新开启才重新记录**
4. 数字的来源与边界必须能在界面上自证（本仓库一贯要求：判据要能自证）

## 2. 现状（实现前已核对）

1. 「流量」列渲染 `bytesText(row.upload + row.download)`（`src/components/ConnLogPanel.tsx:338`）
   ——**上下行相加**，且是**单条连接**的累计
2. 两个字段来自 mihomo `/connections` 每条连接的 `upload`/`download`
   （`src-tauri/src/net/connlog.rs:385-386`）
3. **未经 mihomo 的行这两列是硬编码 0**（`connlog.rs:805-806`），而 `bytesText(0)` 返回 `—`
   （`ConnLogPanel.tsx:138`）。所以泄漏/疑点行显示 `—` **是结论本身**，与它左边三列同一套语义
   （`connlog.rs:801`）——本改动不需要为它们写任何特殊分支
4. mihomo `/connections` 响应**顶层**还有 `uploadTotal`/`downloadTotal`（官方 API 文档确认），
   但当前解析器只取 `connections` 数组（`connlog.rs:341`），`fetch_connections` 的返回值
   （`:1093-1113`）把总量在这一步丢弃
5. `fetch_connections` 签名为 `Result<Vec<MihomoConn>, String>`；`ConnLogSnapshot`（`:251`）、
   `Shared`（`:874`）、`snapshot_of`（`:1047`）都没有总量字段
6. **实测（2026-10-02，本机 sidecar mihomo，`-d` 指向固定临时目录，经混合端口下 3 MB）**：

   | 时刻 | uploadTotal | downloadTotal |
   |---|---|---|
   | 起进程 | 0 | 0 |
   | 经混合端口下 3 MB（`MATCH,DIRECT`） | 79 | 3 000 128 |
   | **杀进程后用同一 `-d` 重启** | **0** | **0** |

   落盘目录只有 `cache.db`（fake-ip / selected 缓存），**流量不落盘**。两条推论：
   ① 总量是**进程生命周期**累计；② **DIRECT 流量也计入**，不只是走节点的
7. mihomo 进程生命周期＝一次「开启网络 → 关闭网络」：`stop` 时 `kill()`
   （`net/runtime.rs:863`），服务托管时走 `StopNet`（`service/mihomo.rs:171`）；`start` 每次重新
   spawn（`net/runtime.rs:615`）。故 §2.6 的总量**天然**就是用户要的会话口径
8. `bytesText` **没有 GB 档**（`ConnLogPanel.tsx:137-142`），会话总量轻易超 1 GB，会渲染成
   `1234.5 MB`
9. **测试基建实情**：`connlog_tests.rs` 里**没有**假 mihomo 服务器。既有的失败路径用例是靠
   「端口 1 上必然没人监听」（`:970-973`）造出来的。故本设计的关键断言需要一个新夹具（§4.1）

## 3. 设计

### 3.1 数据来源：mihomo 顶层总量

新增纯函数：

```rust
/// `/connections` 顶层的会话累计。字段缺失/形状不认识给 0——与 `parse_connections`
/// 同款宽容：一条畸形响应不该把采样任务打死。
pub fn parse_totals(v: &serde_json::Value) -> MihomoTotals   // { upload: u64, download: u64 }
```

`fetch_connections` 的返回值改为 `Result<(Vec<MihomoConn>, MihomoTotals), String>`，在同一次响应上
把总量一并取出（**不额外发请求**）。

### 3.2 为什么它正好是「会话累计」

见 §2.6 与 §2.7：总量随进程归零、不落盘，而进程恰好活在一次网络会话里。**因此不需要任何
累加器**——自己累加反而要处理采样器重启、连接消失、丢帧这些麻烦，且更容易算错。这正是用户
那条「如果重新开启才重新记录」的免费实现。

### 3.3 取数：取后一次读取，**绝不相加**

`sample_once` 每次采样读两次 mihomo（前后夹住，理由见 `connlog.rs:1057-1063`），两次各有各的
totals。它们**是累计快照，不是增量**——相加会得到约两倍的假值。取后一次（`after`，最新的一帧）。

这是整个改动最容易写错的一处：`before + after` 是个看着很合理、结果翻倍的错误。用测试钉死
（§4.2）。

### 3.4 失败路径：保留上一轮，不清零

`drive` 的 `Err` 分支（`connlog.rs:1018-1037`）保留上一轮的行，理由是「连接日志的价值在历史」。
totals 同理：一次查询失败不该把用户正在看的数字抹成 0。`stop()`（`:943`）才清零，与清行同步
——「网络关了」与「这一秒没问到」是两件事。

### 3.5 前端

- `COLUMNS`（`ConnLogPanel.tsx:39`）的 `'流量'` → `'上行'`、`'下行'` 两列
- 行内（`:338`）：`bytesText(row.upload)` / `bytesText(row.download)`。硬编码 0 的泄漏/疑点行
  两侧自然都是 `—`，无需特殊处理
- 顶部一条常驻：`本次会话（经 mihomo）  ↑ 12.4 MB   ↓ 183.7 MB`
- `bytesText` 补 GB 档（`n >= 1 GiB` → `X.XX GB`）
- **`sampling === false` 时整条隐藏。** 不在采样＝不在读 mihomo，留着一个冻住的数字看着可信
  其实过期——正是本仓库最讨厌的那类「未知冒充已知」（对照 `net/runtime.rs` 里 `service_state()`
  返回 `Option` 而非「没在跑」的那处修正）

### 3.6 诚实性：标签必须写「经 mihomo」

mihomo 的累计**只统计经过它的流量**。绕过 mihomo 的泄漏流量它根本看不见——§2.3 那些显示 `—`
的行就是证据。所以文案不能写「本次总流量」，必须写「**经 mihomo** 累计」。另外：系统代理模式
下只有听混合端口的程序计入，TUN 模式才全部计入。这两句直接进界面文案，不靠用户猜。

## 4. 验证

### 4.1 新增夹具：假 mihomo 控制器

`connlog_tests.rs` 里加一个最小 HTTP 服务器（`std::net::TcpListener` + 一个线程）：读到
`\r\n\r\n` 即回一段固定的 `/connections` JSON，带正确的 `Content-Length`。**只服务这一个
端点**，不认识的一律 404——夹具要能自证，不能默默回落成「什么都能答」。

这个夹具是必需的，不是可选：§3.3 那条断言（取后一次而非相加）只有在**真发两次请求**的路径上
才钉得住，纯函数测不到。

### 4.2 用例

| 用例 | 位置 | 钉住的行为 |
|---|---|---|
| `parse_totals_reads_the_real_response_shape` | `connlog_tests` | 真实响应形状下取到两个数 |
| `parse_totals_defaults_to_zero_on_missing_fields` | `connlog_tests` | 缺字段 → 0，不 panic |
| `sampler_reports_session_totals_in_the_snapshot` | `connlog_tests` | 总量经 `fetch_connections` 进到快照（端到端） |
| `sampler_does_not_double_count_totals` | `connlog_tests` | **取后一次而非两次相加**（夹具固定返回同一个值，相加即翻倍） |
| `failed_sample_keeps_previous_totals` | `connlog_tests` | 查询失败后总量不被抹成 0 |
| `stopping_the_sampler_clears_totals` | `connlog_tests` | `stop()` 清零 |
| 顶部条渲染上下行两个数 | `ConnLogPanel.test.tsx` | 总计条存在且两数分开 |
| 行内两列分别是 upload / download | `ConnLogPanel.test.tsx` | 不再相加 |
| `sampling === false` 时总计条不出现 | `ConnLogPanel.test.tsx` | 未知不冒充已知 |
| 0 → `—`（两侧各一） | `ConnLogPanel.test.tsx` | 沿用既有约定 |
| `bytesText` 的 GB 档 | `ConnLogPanel.test.tsx` | 1234.5 MB 不再是唯一形态 |

**反向验证（本仓库规矩）**：§3.3 那条用例写完要先把它**故意破坏**（改成相加）确认变红，再改
回来——RED 必须是这条用例真的抓到了，不是恰好通过。

### 4.3 不覆盖的部分（如实登记）

- 真实 mihomo 的端到端：需要真跑起 sidecar 并造流量。§2.6 的实测已单独做过一次，但不进 CI
- TUN 模式下的总量口径：与系统代理模式同为「经 mihomo」，逻辑上无分支可测

## 5. 已知代价

1. **总计不包含绕过 mihomo 的流量。** 这是 mihomo 的固有边界，不是缺陷。已由 §3.6 的文案
   显式说明，不让用户误以为它是整机流量
2. **多一个假服务器夹具的维护面。** 它的响应是手写常量，mihomo 改了响应形状它不会自己发现。
   缓解：`parse_connections` 那条既有用例（`connlog_tests.rs:807`）本就以「真实响应形状」为准，
   两者的夹具必须一起改
3. **顶部条在「采样器自杀」时一并消失**（`sampling === false`）。此时网络可能还开着、数字其实
   仍然有效——但我们已经不再读 mihomo，无法保证它下一刻还对。宁可不说，也不给一个冻住的数字

## 6. 参考

- mihomo 官方 API 文档 `/connections` 与 `/traffic` 的字段说明：
  <https://wiki.metacubex.one/en/api>
- §2.6 的实测命令与原始输出：本机 sidecar `binaries/mihomo.exe`，`mixed-port: 17890`、
  `external-controller: 127.0.0.1:19090`、规则 `MATCH,DIRECT`，经 `curl -x` 下 3 MB 本地文件；
  重启前后各读一次 `/connections`
