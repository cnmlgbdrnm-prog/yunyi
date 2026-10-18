# vpn-ctl P5 服务器测速设计规格

> 日期：2026-08-14。P4 节点测速已完成并合并 master（ec4f7b2）。本规格定义**服务器测速**（机器本身的网络质量），节点测速保留不动。

## 1. 范围与验收

- 服务器页单台/批量测速：本机→服务器 TCP 延迟 + 服务器自身下载带宽，结果实时推送并持久化。
- 服务器间吞吐实测：任意两台服务器（默认经 WG 隧道地址）iperf3 打 15 秒，吞吐持久化。
- 服务器缺 iperf3 时可一键安装（按系统选包管理器）。
- **验收**：老板 3 台服务器各出延迟/带宽；ashura↔VM-irO0oMfP 出 WG 隧道吞吐；失败原因可见；重启后结果仍在；测速可取消；App 退出无残留连接。

## 2. 已确认决策（老板拍板 2026-08-14）

| 项 | 决策 |
|---|---|
| 测速目标 | **服务器**（节点测速保留） |
| 阶段 1 | 本机延迟（TCP 连 SSH 端口 3 采样取最小）+ 服务器下载带宽（SSH 内 curl 拉 10MB 测速文件） |
| 阶段 2 | 服务器间吞吐：**装 iperf3**（老板批准，侵入可接受） |
| 延迟方法 | 不依赖服务器工具：本机 TCP 建连计时 |
| 带宽方法 | SSH exec `curl -o /dev/null -s -w '%{speed_download}'` 解析 bytes/s |
| 吞吐方法 | 目标机 `iperf3 -s --one-off` 后台起服务端，源机 `iperf3 -c {目标} -t {秒} -J` 解析 JSON `end.sum_sent.bits_per_second` |

## 3. 架构

```
src-tauri/src/server_speed/
  mod.rs        # 阶段1 编排：批量窗口、tcp_latency + curl_bandwidth、写库、事件
  phase1.rs     # tcp_latency_ms（3 采样最小）+ curl_bandwidth（解析 speed_download）
  iperf.rs      # 阶段2 编排：起服务端/跑客户端/解析 JSON/写 throughput 表 + 安装命令选择
  phase1_tests.rs / iperf_tests.rs / tests.rs
jobs: JobRequest::ServerSpeed(ServerSpeedReq) + JobRequest::Throughput(ThroughputReq)  # kind='server_speed'
events: job://progress 复用（kind='server_speed'；单服务器事件 step='server' message=server_id；终态 step='完成'/progress=100 或 '结束'+error——与 P4 语义一致）
db: v5 迁移 — servers 加 latency/bandwidth/last_checked_at/last_check_error；新建 server_throughput 表；v6（P5.7）— server_throughput 加 full_output 失败诊断列
ssh: 复用 SshSession（russh），阶段1 的 curl 与阶段2 的 iperf 全走既有 exec 路径
前端: Servers 页 + api/serverSpeed/serverThroughput/installIperf3
```

- **测速即诊断**：服务器没有 curl/iperf3、连接失败、超时，全部给可读中文原因，不静默。

## 4. 数据模型（v5 + v6）

```sql
ALTER TABLE servers ADD COLUMN latency INTEGER;
ALTER TABLE servers ADD COLUMN bandwidth INTEGER;
ALTER TABLE servers ADD COLUMN last_checked_at TEXT;
ALTER TABLE servers ADD COLUMN last_check_error TEXT;

CREATE TABLE server_throughput (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  from_server_id INTEGER NOT NULL,
  to_server_id INTEGER NOT NULL,
  bits_per_second INTEGER,
  checked_at TEXT NOT NULL,
  error TEXT
);

-- v6（P5.7）：失败行完整客户端输出（stdout+stderr+退出码，8KB 截断），供真机诊断
ALTER TABLE server_throughput ADD COLUMN full_output TEXT;
```

- servers.latency（ms）/bandwidth（bytes/s）/last_checked_at/last_check_error 语义同 nodes。
- server_throughput 一次对测一行：成功 bits_per_second + checked_at；失败 error（含时间）+ full_output（仅客户端已产出输出时；SSH 超时/取消为 NULL）。

## 5. 核心流程

### 5.1 阶段 1：单台测速

1. 延迟：本机对 `server.host:server.port` 做 3 次 TCP connect 计时（每次 ≤5s 超时），取最小 ms；全部失败 → latency NULL + 错误「TCP 连接失败: {host}:{port} {原因}」。
2. 带宽：SSH exec `curl -o /dev/null -s -w '%{speed_download}' --max-time 30 {SPEED_TEST_URL}`（URL 同 P4 常量，10MB）；stdout 解析 f64 → bytes/s；curl 缺失（stderr 含 command not found / 退出 127）→「服务器缺少 curl，无法测下载带宽」；超时 →「下载测速超时（30s）」。
3. 结果写 servers 表 + 事件（step='server'，message=server_id，error=单台错误|null）。**单台失败不判任务失败**；取消中止在途 SSH。

### 5.2 阶段 2：服务器间吞吐

1. **起服前清理（P5.3 无依赖化 + P5.4 数字校验）**：向目标机独立 exec 清理段——PID 文件 kill 为**主清理**且经 case 数字校验（`SPID=$(cat /tmp/vpnctl-iperf3.pid 2>/dev/null); case "$SPID" in ''|*[!0-9]*) ;; *) kill $SPID 2>/dev/null ;; esac`，空/含非数字（含预写 `123 456` 等脏内容）一律不 kill，防多用户共享机的本地 DoS 向量），随后 `rm -f` + `pkill -f 'iperf3 -s'` 兜底清历史孤儿（pkill/timeout 缺失时 `|| true` 吞掉，主路径纯 shell 内建零依赖）。独立 exec 而非并入启动脚本（pkill -f 会匹配承载脚本的 sh 包装自身 cmdline 导致自杀），客户端侧留 1s 退场窗口。
2. **端口池智能回退（P5.3 老板诉求）**：按 `IPERF_PORTS = [5201, 5202, 5203, 5204, 5205]` 依次起服，单端口一条命令完成「后台起 `iperf3 -s --one-off -p {PORT}` + SPID 落 PID 文件 + 1s `kill -0` 探活」：存活 → `echo IPERF_SERVER_READY:$SPID` 即用该端口；已死且 `iperf3 -v` 存在 → `IPERF_SERVER_BIND_FAILED` 换下一端口；`iperf3 -v` 也不在 → `IPERF_MISSING` **只首端口短路**（缺 iperf3 换端口无意义）→「目标机未安装 iperf3，请点「安装 iperf3」」。全池 BIND_FAILED → Err「对测端口 5201-5205 均被占用，请稍后重试（最后错误: …）」（**去掉 P5.2 的 per-port timeout 取证**，智能回退取代诊断）。空输出兼容视为就绪。就绪事件 message 明示「{目标路径} 端口 {PORT}」。
3. **客户端前二次存活检查（P5.5）**：READY 后、打流前，目标机会话再探活（`kill -0` 经 PID 文件，同 case 数字校验）——DEAD →「目标机 iperf3 服务端已退出（启动后崩溃或被清理）」不跑客户端（SSH 瞬断保守判 DEAD，客户端反正跑不了）；存活则照跑。
4. 源机 SSH exec `iperf3 -c {目标地址} -t {秒} -p {实际端口} -J`（默认 15s，范围 5–30，**端口用回退后的实际端口**）；解析 JSON **顶层 `error` 字段优先透出（P5.5）**——`{"error":"..."}` →「iperf3 错误: {原文}」（老板真机曾报笼统「缺少吞吐数据」掩盖真实原因）；无 error 再解析 `end.sum_sent.bits_per_second`（缺失回退 sum_received），再缺 → **老版流级聚合回退（P5.7）**：`end.streams[].sender.bits_per_second` 求和（任一 sender 存在），否则 receiver 求和（老版 iperf3 3.0.x 无顶层聚合）；全无 →「iperf3 输出缺少吞吐数据（exit={退出码}）」——**退出码永远附带（P5.6）**，stdout 前 120 字符预览与 stderr 首行非空才追加（格式「，stdout: {预览}」「，stderr: {首行}」；老版 iperf3 把错误文本打在 stdout，曾因只查 stderr 出现双空裸文案无法定位），兜底分支再附「版本: {start.version}」（P5.7，JSON 结构随版本演进）；**bits_per_second 取值必须浮点兼容（P5.8）**——iperf3 输出 JSON 浮点（真机 3.16 实测 `93274106.282310233`），`as_i64()` 只认整数曾致浮点静默 None 误报「缺少吞吐数据」（P5.5-P5.7 未中的真根因），一律经 bps_of（i64 优先、f64 截断取整）；失败行另将完整 stdout/stderr 落 full_output 列（64KB 截断（P5.8：8KB 曾把 15 秒 JSON 的 end 段截掉），UI 不展示全文）。
5. **目标地址选择**：两机存在已建 relay_rule（status built/imported）时按方向取目标机的 WG 地址——from=中转/to=落地 → landing_wg_ip，反向 → relay_wg_ip（测 WG 隧道真实路径，双向均可用）；无规则 → 目标公网 host。地址在事件 message 中明示。
6. **imported 规则实测优先**（P5.1）：客户端执行前向目标机 `ip -o addr show | grep wg | head -1` 实测接口地址（接口名未知的退化命令），实测到 → 用实测（事件「经 WG 隧道 {ip}（实测）」）；解析不到 → 回退库值（「（记录值）」），防库值失真（曾硬凑 {prefix}.1/.2）连黑洞地址超时。built 规则合成值自洽直接用。
7. 成功后写 server_throughput + 终态事件（message 带 Mbps 值并明示「（端口 {实际端口}）」）；任一环节失败 → error 可读 + job failed。
8. **失败自诊断映射**（P5.1 + P5.6 同查 stdout）：客户端 **stderr 或 stdout** 含 `Connection refused` →「目标拒绝连接（未监听 {实际端口}：检查目标机 iperf3/防火墙/地址）」（老版 iperf3 把该错误打在 stdout，只查 stderr 会漏判成「缺少吞吐数据」）；SSH 超时 →「连接目标超时：WG 隧道或目标地址不可达（先在中转页刷新状态确认握手）」，不再裸抛「对测 超时（N 秒）」；写库失败行同文案。
9. **服务端清理（P5.2 结构 + P5.3 无依赖段 + P5.4 校验）**：`--one-off` 单次测试后自退；run_throughput_with 内层 async 块承载业务、外层**无条件**执行同一清理段——成功/客户端失败/超时/取消/起服失败五条路径均清理，外层以就绪解析出的 SPID 精确 kill 自家服务端（start_server 失败时 SPID 为空，PID 文件 + pkill 兜底仍执行）；历史孤儿由起服前预清理自愈（曾真实发生：超时对测的 `--one-off` 服务端未等到客户端，长期占用 5201 致后续 BIND_FAILED）。

### 5.3 安装 iperf3

- 独立命令 `install_iperf3_cmd(server_id)`（非 job，UI busy 态等返回）：
  - server.system 含 ubuntu/debian → `apt-get update -qq && apt-get install -y iperf3`；
  - 含 centos/rhel/fedora → `yum install -y iperf3`（探测 dnf 则用 dnf）；alpine → `apk add iperf3`；
  - system 为空/未知 → 依次 `command -v apt-get`/`dnf`/`yum`/`apk` 探测首个存在的包管理器；都没有 →「无法识别服务器系统，请手动安装 iperf3」。
- 装完验证 `iperf3 -v` 退出码 0；失败带 stderr 原因。

### 5.4 取消

- 复用 cancel_job_cmd：worker cancel flag；在途 SSH exec 随 future drop 关闭通道（russh session 级取消，P3 已有模式）。
- 不因取消影响服务器上的 `iperf3 -s`：取消提前返回仍走外层无条件 pkill（P5.2 全路径清理保证），--one-off 自退兜底。

## 6. 命令与事件

| 命令 | 签名 | 说明 |
|---|---|---|
| `server_speed_cmd` | `(server_ids: Vec<i64>, concurrency: u8) -> job_id` | 阶段1 批量/单发统一入口，并发 1–5 默认 3 |
| `server_throughput_cmd` | `(from_server_id, to_server_id, seconds) -> job_id` | 阶段2 对测（seconds 5–30 默认 15） |
| `install_iperf3_cmd` | `(server_id) -> ()` | 一键安装（busy 态） |
| `list_throughput_cmd` | `() -> Vec<ThroughputTest>` | 历史对测结果（UI 展示最近一条） |
| `cancel_job_cmd` | 已有 | 取消 |

事件复用 `job://progress`：kind='server_speed'；阶段1 单台事件 step='server'、message=server_id、error=单台错误；阶段2 步骤事件 step='iperf'、message=阶段文案；终态 step='完成'（progress=100，message 带吞吐 Mbps）或 '结束'（error）。前端终态判定只看 step=='完成'||'结束'。

## 7. 测试策略

- phase1：tcp_latency_ms 用本地 TcpListener 真连（3 采样、最小、全败错误、超时）；curl_bandwidth 用 FakeSshServer（russh 回环）注入 stdout/stderr/退出码（正常值、127、超时）；解析器纯函数枚举。
- iperf：parse JSON 金样（sum_sent/sum_received 回退/缺失字段）；安装命令按 system 映射 + 空 system 探测序列；对测编排用 FakeSshServer 双机脚本（起服务端/客户端/解析/写表/事件）。
- 不变量：批量并发窗口≤concurrency、单台失败隔离、取消中止在途（FakeSshServer block_on/release 已有旋钮）、超时不挂死（tokio::time::pause）、终态事件语义（与 P4 同型）。
- IPC：tauri::test 三个命令 camelCase 线格式。
- 前端：Servers 页组件测试（单发/批量/并发/取消/事件行更新/安装按钮 busy/吞吐对测选择与结果展示）。
- 真实 iperf3 往返与防火墙可达性 → 终审手工验收（老板 3 台真机）。

## 8. UI（服务器页）

- 行尾新增列：延迟 ms / 下载带宽 MB/s / 上次测速时间（本地时区，复用 checkedAtFormat）/ 失败原因红字。
- 行内「测速」按钮（busy）；缺 iperf3/curl 时错误下方给出「安装 iperf3」按钮（busy，装完提示）。
- 工具栏：「批量测速」（全部服务器）+ 并发下拉（1/3/5 默认 3）；「吞吐对测」：源/目标两个下拉（默认取已建链路的 中转机→落地机）+ 秒数（默认 15）+ 最近对测结果展示。
- kind='server_speed' 事件驱动行状态实时更新；终态后 refresh。
- 无 emoji；所有按钮 busy/禁用态；错误带原因。

## 9. 风险与边界

- 每台服务器一次下载测速产生约 10MB 流量；iperf3 15s 打满带宽（约几十 MB 级），UI 明示秒数。
- 服务器防火墙需放行 5201（仅对测时）；被挡时错误提示检查防火墙。
- 安装 iperf3 需要服务器能访问软件源；失败原因透传。
- 服务器上没有 curl 的机器下载带宽测不了（不装 curl，给出明确原因）——后续如需可加 wget 回退。

## 10. BBR 检测与开启（老板追加 2026-08-14）

- 服务器页每台服务器可「检测 BBR」：SSH exec `sysctl -n net.ipv4.tcp_congestion_control`，展示当前算法（bbr / cubic / 其他 / 错误原因）。**惰性触发**（行内按钮），页面加载绝不自动 SSH。
- 非 bbr 时出现「开启 BBR」按钮：执行开启脚本（`grep -qw bbr` 检查 tcp_available_congestion_control——该文件是**算法名**空间（`reno cubic bbr`，与检测读出的 "bbr" 同一命名），词边界防误配，覆盖内建/已加载 → modprobe tcp_bbr 兜底模块化发行版 → sysctl 两个参数 → 幂等写入 /etc/sysctl.conf → 回读验证），成功显示「已开启 bbr」；内建/已加载均不可用（modprobe 失败）→「内核不支持 BBR」可读错误。（终审两轮修订：`/proc/modules` 双端锚定无法匹配模块行且内建无行；第一轮误用模块名 tcp_bbr 匹配算法名空间恒假——守卫目标必须是算法名 `bbr`。）
- 命令：`check_bbr_cmd(server_id) -> String`（当前算法名）、`enable_bbr_cmd(server_id) -> String`（结果文案）；非 job 直接异步命令，UI busy 态等待。
- BBR 只管 TCP；WG/UDP 流量不受影响（README 说明）。
- 与测速配套：开启前后各测一次下载带宽可对比验证 BBR 效果。
