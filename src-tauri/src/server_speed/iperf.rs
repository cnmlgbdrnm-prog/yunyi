//! 阶段2 服务器间吞吐对测与 iperf3 安装：起服前先无依赖清理（P5.3——老板真机疑缺
//! pkill/timeout，主清理走 PID 文件 kill，pkill 仅兜底尽力而为）；目标机按端口池
//! 5201-5205 依次后台起 `iperf3 -s --one-off` 并就绪自检（READY:SPID 即用该端口、
//! BIND_FAILED 换下一端口、MISSING 首端口短路）；READY 后客户端前二次存活检查
//! （P5.5——服务端可能已崩溃/被清理，DEAD 提前报可读文案）；源机 `iperf3 -c -J`
//! 解析吞吐 JSON（顶层 error 字段优先透出真实原因，P5.5）；目标地址优先走已建
//! 链路 WG 隧道地址（imported 实测优先）；失败/超时映射可读文案（拒绝/隧道
//! 不可达），不再裸抛英文 stderr 或「对测 超时」；清理由 run_throughput_with
//! 外层清理点覆盖全部退出路径（成功/失败/超时/取消）；安装命令按服务器系统
//! 选择包管理器。
//! 事件 step='iperf'，终态由 job 包装统一发射。

use crate::db::repos;
use crate::domain::{JobProgress, ThroughputReq};
use crate::jobs::EmitFn;
use crate::relay::parse_iface_inet;
use crate::server_speed::{wait_cancel, CONNECT_TIMEOUT};
use crate::ssh::{CommandOutput, SshError, SshSession};
use crate::state::AppState;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// 对测端口池：按序尝试回退（P5.3 老板诉求——端口占用不许只报错，要智能）。
pub const IPERF_PORTS: &[u16] = &[5201, 5202, 5203, 5204, 5205];

/// 默认/首端口（= IPERF_PORTS[0]，历史引用与拒绝文案沿用）。
pub const IPERF_PORT: u16 = 5201;

/// 目标机 PID 文件：起服脚本把服务端 PID 落盘，清理按 PID 精确 kill——
/// 不依赖 pkill/timeout（老板真机疑似缺失，预清理可能从未生效的根因）。
pub const IPERF_PID_FILE: &str = "/tmp/vpnctl-iperf3.pid";

/// 远端 shell 单引号转义：**实现已收敛到 [`crate::util::shell_quote`]**，这里只转发——
/// 同一个仓里原先只有这一处有转义函数，别处需要时只好裸拼（#10）。客户端的
/// `server.host` 仍经此进入命令，调用点不变。
pub fn shell_quote(s: &str) -> String {
    crate::util::shell_quote(s)
}

/// 解析 iperf3 `-J` 输出：顶层 `error` 字段（失败时 `{"error":"..."}`）优先透出
/// 真实原因（P5.5——此前只找 end.sum_sent/sum_received，把真实原因吞成
/// 「缺少吞吐数据」）；无 error 再走 `end.sum_sent.bits_per_second`（缺失回退
/// sum_received）；再没有走流级统计聚合（P5.7，见 legacy_stream_bps）；
/// 都没有 → 「缺少吞吐数据」。
pub fn parse_iperf_json(s: &str) -> Result<i64, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("iperf3 输出不是 JSON: {e}"))?;
    if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
        return Err(format!("iperf3 错误: {err}"));
    }
    let sent = v.pointer("/end/sum_sent/bits_per_second").and_then(bps_of);
    let recv = v
        .pointer("/end/sum_received/bits_per_second")
        .and_then(bps_of);
    sent.or(recv)
        .or_else(|| legacy_stream_bps(&v))
        .ok_or_else(|| "iperf3 输出缺少吞吐数据".to_string())
}

/// iperf3 `bits_per_second` 是 JSON 浮点（真机 3.16 实测
/// `93274106.282310233`）——旧实现 `as_i64()` 只认整数，浮点值静默 None
/// 致「缺少吞吐数据」误报（P5.8 根因）；兼容 i64（历史整数金样）与 f64
/// （真机常态），f64 截断为整数 bps（显示精度 Mbps 足够）。
fn bps_of(x: &serde_json::Value) -> Option<i64> {
    x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))
}

/// 老版 iperf3（3.0.x）流级统计聚合（P5.7）：顶层 sum_sent/sum_received 缺失时，
/// 统计只在 `end.streams[].sender.bits_per_second`（反向测试在 receiver）——
/// 任一 sender 值存在则 sender 求和，否则 receiver 求和；全无 → None。
/// 取值经 bps_of（浮点兼容，P5.8）。
fn legacy_stream_bps(v: &serde_json::Value) -> Option<i64> {
    let streams = v.pointer("/end/streams")?.as_array()?;
    let mut sender_total = 0i64;
    let mut sender_seen = false;
    let mut recv_total = 0i64;
    let mut recv_seen = false;
    for st in streams {
        if let Some(x) = st.pointer("/sender/bits_per_second").and_then(bps_of) {
            sender_total += x;
            sender_seen = true;
        }
        if let Some(x) = st.pointer("/receiver/bits_per_second").and_then(bps_of) {
            recv_total += x;
            recv_seen = true;
        }
    }
    if sender_seen {
        Some(sender_total)
    } else if recv_seen {
        Some(recv_total)
    } else {
        None
    }
}

/// iperf3 客户端版本（`start.version`，如 `iperf 3.0.3`）：JSON 结构随版本演进，
/// 失败诊断必须知道对面实际版本。无/非字符串 → None。
fn iperf_version(v: &serde_json::Value) -> Option<&str> {
    v.pointer("/start/version")?.as_str()
}

/// 解析就绪标记中的服务端 PID：`IPERF_SERVER_READY:12345` → `12345`。
/// 只取标记后连续 ASCII 数字（纯数字进 shell 无注入面）；无标记/无数字 → None。
pub fn parse_iperf_ready_pid(stdout: &str) -> Option<String> {
    let rest = stdout.split("IPERF_SERVER_READY:").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then_some(digits)
}

/// 安装命令映射（小写包含匹配）；空/未知系统 → 单条探测脚本按
/// apt-get→dnf→yum→apk 顺序选中第一个存在的包管理器安装，全无则
/// echo NO_PACKAGE_MANAGER 且 exit 1。
pub fn iperf_install_cmd(system: &str) -> String {
    let s = system.trim().to_lowercase();
    if s.contains("ubuntu") || s.contains("debian") {
        return "apt-get update -qq && apt-get install -y iperf3".into();
    }
    if s.contains("centos") || s.contains("rhel") || s.contains("fedora") {
        return "if command -v dnf >/dev/null 2>&1; then dnf install -y iperf3; else yum install -y iperf3; fi".into();
    }
    if s.contains("alpine") {
        return "apk add iperf3".into();
    }
    r#"if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && apt-get install -y iperf3;
elif command -v dnf >/dev/null 2>&1; then dnf install -y iperf3;
elif command -v yum >/dev/null 2>&1; then yum install -y iperf3;
elif command -v apk >/dev/null 2>&1; then apk add iperf3;
else echo "NO_PACKAGE_MANAGER"; exit 1; fi"#
        .into()
}

/// 阶段2 对测编排（Task 4 只做 JobRequest 分发，直接调本函数）：
/// 两机装载 → 目标机后台起服务端 → 源机打流（select 轮询 cancel）→ 解析写表 →
/// pkill 兜底。单次对测整体失败判任务失败（与阶段1 单台隔离语义不同）。
pub async fn run_throughput_job(
    state: &AppState,
    req: ThroughputReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
) -> Result<(), String> {
    // 1. 服务器装载 + 目标地址选择：两机存在 built/imported 链路（双向）→ 取对应
    //    WG 隧道地址（from 为中转 → landing_wg_ip；from 为落地 → relay_wg_ip），
    //    否则公网 host。事件明示目标路径。
    //    P5.1：imported 规则库值可能失真（曾硬凑 {prefix}.1/.2）——客户端执行前向
    //    目标机实测接口地址，实测优先；解析不到回退库值并明示「（记录值）」。
    let (from_id, to_id, rule_addr, fallback_host) = {
        let conn = state.db.lock();
        let _from = repos::servers::get(&conn, req.from_server_id)
            .map_err(|e| format!("读取服务器失败: {e}"))?
            .ok_or_else(|| "源服务器不存在".to_string())?;
        let to = repos::servers::get(&conn, req.to_server_id)
            .map_err(|e| format!("读取服务器失败: {e}"))?
            .ok_or_else(|| "目标服务器不存在".to_string())?;
        let rule = repos::relay::find_by_pair(&conn, req.from_server_id, req.to_server_id)
            .map_err(|e| format!("读取链路失败: {e}"))?;
        match rule {
            Some(r) => {
                // from 为中转侧 → 目标取 landing_wg_ip；from 为落地侧 → relay_wg_ip
                let wg_ip = if r.relay_server_id == req.from_server_id {
                    r.landing_wg_ip.clone()
                } else {
                    r.relay_wg_ip.clone()
                };
                (
                    req.from_server_id,
                    req.to_server_id,
                    Some((wg_ip, r.status == "imported")),
                    to.host.clone(),
                )
            }
            None => (req.from_server_id, req.to_server_id, None, to.host.clone()),
        }
    };
    // 2. SSH 会话（凭据解密复用 servers::load_auth；凭据不进日志/错误文本）
    let (auth_to, to_srv) = crate::servers::load_auth(state, to_id).map_err(|e| e.to_string())?;
    let (auth_from, from_srv) =
        crate::servers::load_auth(state, from_id).map_err(|e| e.to_string())?;
    let mut to_sess = SshSession::connect(
        &to_srv.host,
        to_srv.port as u16,
        &to_srv.user,
        &auth_to,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut from_sess = SshSession::connect(
        &from_srv.host,
        from_srv.port as u16,
        &from_srv.user,
        &auth_from,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    // 3. 目标地址定案：imported 规则 → 目标机实测（接口名未知，退化命令枚举 wg 接口）；
    //    built 规则合成值自洽直接用；无规则 → 公网 host。
    let (target, addr_desc) = match rule_addr {
        Some((wg_ip, is_imported)) if is_imported => {
            let measured = match to_sess
                .run(
                    "对测",
                    "ip -o addr show | grep wg | head -1",
                    CONNECT_TIMEOUT,
                )
                .await
            {
                Ok(out) if out.exit_code == 0 => parse_iface_inet(&out.stdout),
                _ => None,
            };
            match measured {
                Some(ip) => (ip.clone(), format!("经 WG 隧道 {ip}（实测）")),
                None => (wg_ip.clone(), format!("经 WG 隧道 {wg_ip}（记录值）")),
            }
        }
        Some((wg_ip, _)) => (wg_ip.clone(), format!("经 WG 隧道 {wg_ip}（记录值）")),
        None => (fallback_host.clone(), format!("经公网 {}", fallback_host)),
    };
    run_throughput_with(
        state,
        req,
        cancel,
        emit,
        &mut to_sess,
        &mut from_sess,
        target,
        addr_desc,
    )
    .await
}

/// 对测主体（注入会话，供 FakeSshServer 测试驱动）：
/// 目标机后台起服务端 → 源机打流（select 轮询 cancel）→ 解析写表 → pkill 兜底。
/// 生产入口 run_throughput_job 完成装载与连接后委托本函数。
#[allow(clippy::too_many_arguments)] // 测试注入入口（会话/目标/地址描述），打包成结构体反增间接层
pub(crate) async fn run_throughput_with(
    state: &AppState,
    req: ThroughputReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
    to_sess: &mut SshSession,
    from_sess: &mut SshSession,
    target: String,
    addr_desc: String,
) -> Result<(), String> {
    // 内层块承载业务；外层无条件清理兜底（P5.2 结构保留）：start_server 失败
    // （BIND_FAILED/全池失败）也必须清理——孤儿 iperf3 不跨对测遗留。外层用
    // 就绪解析出的 SPID 精确 kill 自家服务端（P5.3：SPID 跨块传出供精确清理），
    // start_server 失败时 spid 为空（`kill` 无参报错被吞，PID 文件 + pkill 兜底
    // 仍执行）。客户端内部兜底保留（run_client_and_finish 被测试直接注入调用），
    // 清理段幂等无害。
    let mut spid = String::new();
    let result = async {
        let (port, got) = start_server(to_sess, emit.clone(), &req, &addr_desc).await?;
        spid = got;
        // P5.5 二次存活检查：READY 后到打流前服务端可能已崩溃/被清理，DEAD 提前
        // 报可读文案不白跑客户端。空输出（旧脚本/未知实现）按存活兼容。
        let alive = match to_sess
            .run("存活检查", &alive_check_cmd(), Duration::from_secs(10))
            .await
        {
            Ok(o) => !o.stdout.contains("DEAD"),
            Err(_) => false,
        };
        if !alive {
            return Err("目标机 iperf3 服务端已退出（启动后崩溃或被清理）".to_string());
        }
        let secs = req.seconds.clamp(5, 30) as u64;
        run_client_and_finish(
            state,
            req,
            cancel,
            emit,
            to_sess,
            from_sess,
            target,
            addr_desc,
            port,
            Duration::from_secs(secs + 30),
        )
        .await
    }
    .await;
    cleanup(to_sess, &spid).await;
    result
}

/// 起服探活脚本（单端口）：后台起 `--one-off` 服务端 → SPID 落 PID 文件 →
/// sleep 1 探活。存活=就绪（echo IPERF_SERVER_READY:SPID，SPID 供精确清理）；
/// 已死再分「装了但启动失败」（BIND_FAILED）与「根本没装」（MISSING）。
/// `kill -0` 只探存活不发信号，全部 shell 内建，无外部工具依赖。
fn start_cmd(port: u16) -> String {
    format!(
        "iperf3 -s --one-off -p {port} </dev/null >/dev/null 2>&1 & SPID=$!; \
         echo $SPID > {IPERF_PID_FILE}; sleep 1; \
         if kill -0 $SPID 2>/dev/null; then echo IPERF_SERVER_READY:$SPID; \
         elif iperf3 -v >/dev/null 2>&1; then echo IPERF_SERVER_BIND_FAILED; exit 1; \
         else echo IPERF_MISSING; exit 1; fi"
    )
}

/// 无依赖清理段（起服前预清理与全路径兜底共用）：kill 客户端持有的 SPID（精确
/// 清理自家服务端；spid 为空时 `kill` 无参报错被吞）→ kill PID 文件里的 PID →
/// rm → pkill 兜底清历史孤儿（pkill/timeout 缺失时 `|| true` 吞掉，尽力而为）。
/// PID 文件内容经 case 数字校验（P5.4）：`$(cat)` 命令替换剥离尾换行，空/含任何
/// 非数字字符（含 `123 456` 多数字、脏预写内容）一律不 kill——本地 DoS 向量：
/// root 下裸 `kill $(cat ...)` 可能被预写文件诱导误杀任意 PID。
/// pub(crate) 供金样测试直接断言脚本结构。
pub(crate) fn clean_cmd(spid: &str) -> String {
    format!(
        "kill {spid} 2>/dev/null || true; \
         SPID=$(cat {IPERF_PID_FILE} 2>/dev/null); case \"$SPID\" in ''|*[!0-9]*) ;; *) kill $SPID 2>/dev/null ;; esac; \
         rm -f {IPERF_PID_FILE}; pkill -f 'iperf3 -s' 2>/dev/null || true"
    )
}

/// 客户端前二次存活检查脚本（P5.5）：READY 之后到打流之前服务端可能已崩溃或被
/// 清理（`--one-off` 等客户端、启动后异常退出），提前报可读文案不白跑客户端。
/// PID 文件内容沿用 P5.4 case 数字校验——空/非纯数字一律 DEAD，不把脏内容喂给
/// kill -0；纯数字且存活 → ALIVE。`kill -0` 只探存活不发信号。
/// pub(crate) 供金样测试直接断言脚本结构。
pub(crate) fn alive_check_cmd() -> String {
    format!(
        "SPID=$(cat {IPERF_PID_FILE} 2>/dev/null); case \"$SPID\" in \
         ''|*[!0-9]*) echo DEAD ;; *) kill -0 $SPID 2>/dev/null && echo ALIVE || echo DEAD ;; esac"
    )
}

/// 目标机按端口池起服务端 + 就绪自检（P5.3 智能回退）：READY → 用该端口返回
/// (端口, SPID)；BIND_FAILED → 换下一端口；MISSING 只首端口短路（缺 iperf3 换
/// 端口无意义）；无标记（空输出/旧脚本）按已就绪兼容取当前端口。全池失败 →
/// 聚合文案（含 5201-5205 与最后错误）。就绪 → 阶段事件明示目标地址与端口。
/// 独立成函数：依赖服务器响应的步骤必须在真实时间完成（FakeSshServer 响应在
/// 虚拟时钟暂停下会被饿死），超时/取消测试先真实调用本函数再暂停时钟。
pub(crate) async fn start_server(
    sess: &mut SshSession,
    emit: EmitFn,
    req: &ThroughputReq,
    addr_desc: &str,
) -> Result<(u16, String), String> {
    // 起服前清理（独立 exec）：`--one-off` 服务端等到客户端才退，此前对测遗留的
    // 服务端会占死端口。独立 exec 必要：段内含 pkill 模式串，pkill -f 会匹配到
    // 承载它的 sh 包装（cmdline 含脚本全文）并 SIGTERM 之；独立 exec 下包装死亡
    // 无害（结果忽略），并入启动脚本则启动行永不执行。
    let _ = sess
        .run("预清理", &clean_cmd(""), Duration::from_secs(10))
        .await;
    // 给旧进程退场 + 释放端口留窗口（脚本内 sleep 同理会被 pkill 误伤，放客户端侧）
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut last_err = String::new();
    for port in IPERF_PORTS {
        let out = sess
            .run("启动服务端", &start_cmd(*port), Duration::from_secs(15))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(spid) = parse_iperf_ready_pid(&out.stdout) {
            emit(JobProgress {
                job_id: req.job_id,
                kind: "server_speed".into(),
                step: "iperf".into(),
                message: format!("{addr_desc} 端口 {port}"),
                progress: 1,
                error: None,
            });
            return Ok((*port, spid));
        }
        if out.stdout.contains("IPERF_MISSING") {
            return Err("目标机未安装 iperf3，请点「安装 iperf3」".to_string());
        }
        if out.stdout.contains("IPERF_SERVER_BIND_FAILED") {
            last_err = format!("端口 {port} 被占用或启动失败");
            continue;
        }
        // 无标记（空输出/未知实现）→ 按已就绪兼容，用当前端口
        emit(JobProgress {
            job_id: req.job_id,
            kind: "server_speed".into(),
            step: "iperf".into(),
            message: format!("{addr_desc} 端口 {port}"),
            progress: 1,
            error: None,
        });
        return Ok((*port, String::new()));
    }
    Err(format!(
        "对测端口 {}-{} 均被占用，请稍后重试（最后错误: {last_err}）",
        IPERF_PORTS.first().copied().unwrap_or(IPERF_PORT),
        IPERF_PORTS.last().copied().unwrap_or(IPERF_PORT),
    ))
}

/// 源机打流 + 结果处理：select 轮询 cancel（在途取消即中止）→ 解析 JSON →
/// 写表（失败行 error 带原因）→ pkill 兜底 → 结果事件。
/// run_timeout 为对测整体超时（生产 secs+30s；测试注入短值——FakeSshServer 响应
/// 在虚拟时钟暂停下被饿死，超时用例必须在真实时间驱动）。
#[allow(clippy::too_many_arguments)] // 测试注入入口（含 run_timeout 覆盖超时），打包成结构体反增间接层
pub(crate) async fn run_client_and_finish(
    state: &AppState,
    req: ThroughputReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
    to_sess: &mut SshSession,
    from_sess: &mut SshSession,
    target: String,
    addr_desc: String,
    port: u16,
    run_timeout: Duration,
) -> Result<(), String> {
    let from_id = req.from_server_id;
    let to_id = req.to_server_id;
    let secs = req.seconds.clamp(5, 30) as u64;
    // target 可能来自 server.host（仅非空校验）：经 shell_quote 进入远端 shell 命令，
    // 杜绝含 ;/$()/引号的主机名成为源机（通常 root）上的命令注入（BLOCK 信任边界）
    let cmd = format!("iperf3 -c {} -t {secs} -p {port} -J", shell_quote(&target));
    let iperf_fut = from_sess.run("对测", &cmd, run_timeout);
    tokio::pin!(iperf_fut);
    let out = tokio::select! {
        r = &mut iperf_fut => match r {
            Ok(o) => o,
            Err(e) => {
                // P5.1 自诊断：SSH 超时 = 目标不可达（WG 黑洞地址/防火墙丢包/地址失真），
                // 映射为用户可读文案，不再裸抛「对测 超时（N 秒）」；其他传输错误原样保留
                let msg = match &e {
                    SshError::Timeout { .. } => {
                        "连接目标超时：WG 隧道或目标地址不可达（先在中转页刷新状态确认握手）"
                            .to_string()
                    }
                    other => other.to_string(),
                };
                record_fail(state, from_id, to_id, &msg, None);
                cleanup(to_sess, "").await;
                return Err(msg);
            }
        },
        _ = wait_cancel(cancel) => {
            record_fail(state, from_id, to_id, "已取消", None);
            cleanup(to_sess, "").await;
            return Err("已取消".to_string());
        }
    };
    // 解析 JSON → 写表（失败行 error 带原因）→ 结果事件
    let bps = match parse_iperf_json(&out.stdout) {
        Ok(b) => b,
        Err(e) => {
            // P5.5：iperf3 顶层 error 字段（「iperf3 错误: …」）优先透出真实原因，
            // 不再吞成「缺少吞吐数据」。P5.6：其余解析失败**永远**附上退出码与
            // stdout/stderr 预览——老版 iperf3 的错误文本打在 stdout（真机曾出现
            // stdout/stderr 双空、退出码被丢、无法定位的场景），拒绝检测同查 stdout。
            // P5.7：兜底文案再附 iperf3 客户端版本（JSON 结构随版本演进）；完整
            // stdout/stderr 另落 full_output 列（8KB 截断）供真机诊断。
            let msg = if e.starts_with("iperf3 错误:") {
                e
            } else if out.stderr.contains("Connection refused")
                || out.stdout.contains("Connection refused")
            {
                // P5.1 自诊断：iperf3 客户端连不上目标端口 → 未监听/防火墙拦截，
                // 映射为可读拒绝文案（不再裸抛英文 stderr），端口用实际端口
                format!("目标拒绝连接（未监听 {port}：检查目标机 iperf3/防火墙/地址）")
            } else {
                let stdout_preview: String = out.stdout.trim().chars().take(120).collect();
                let stderr_head = out.stderr.trim_end();
                let mut msg = format!("{e}（exit={}）", out.exit_code);
                if !stdout_preview.is_empty() {
                    msg.push_str(&format!("，stdout: {stdout_preview}"));
                }
                if !stderr_head.is_empty() {
                    msg.push_str(&format!(
                        "，stderr: {}",
                        stderr_head.lines().next().unwrap_or("").trim()
                    ));
                }
                if let Some(ver) = serde_json::from_str::<serde_json::Value>(&out.stdout)
                    .ok()
                    .as_ref()
                    .and_then(iperf_version)
                {
                    msg.push_str(&format!("，版本: {ver}"));
                }
                msg
            };
            record_fail(
                state,
                from_id,
                to_id,
                &msg,
                Some(&collect_full_output(&out)),
            );
            cleanup(to_sess, "").await;
            return Err(msg);
        }
    };
    let _ = repos::throughput::insert(&state.db.lock(), from_id, to_id, Some(bps), None, None);
    cleanup(to_sess, "").await;
    let mbps = bps as f64 / 1e6;
    emit(JobProgress {
        job_id: req.job_id,
        kind: "server_speed".into(),
        step: "iperf".into(),
        message: format!("{addr_desc} 端口 {port} 吞吐 {mbps:.1} Mbps"),
        progress: 2,
        error: None,
    });
    // 终态事件（成功路径）：前端据 step='完成' + progress=100 判定整批结束；
    // 失败/取消返回 Err，由 worker 补发 '结束'（两路径互斥）
    emit(JobProgress {
        job_id: req.job_id,
        kind: "server_speed".into(),
        step: "完成".into(),
        message: format!("吞吐 {mbps:.2} Mbps（端口 {port}）"),
        progress: 100,
        error: None,
    });
    Ok(())
}

/// 一键安装 iperf3：已安装直接返回；按系统选包管理器安装（超时 300s）；
/// 无包管理器/安装失败 → 可读错误带原因；装完 iperf3 -v 验证。
pub async fn install_iperf3_on(state: &AppState, server_id: i64) -> Result<String, String> {
    let (auth, s) = crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(&s.host, s.port as u16, &s.user, &auth, CONNECT_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if sess
        .run_ok("检查 iperf3", "iperf3 -v", Duration::from_secs(10))
        .await
        .is_ok()
    {
        return Ok("已安装".to_string());
    }
    let cmd = iperf_install_cmd(&s.system);
    let out = sess
        .run("安装 iperf3", &cmd, Duration::from_secs(300))
        .await
        .map_err(|e| e.to_string())?;
    if out.stdout.contains("NO_PACKAGE_MANAGER") {
        return Err("无法识别服务器系统，请手动安装 iperf3".to_string());
    }
    if out.exit_code != 0 {
        return Err(format!(
            "安装失败(exit={}): {}",
            out.exit_code,
            out.stderr.trim_end()
        ));
    }
    sess.run_ok("验证 iperf3", "iperf3 -v", Duration::from_secs(10))
        .await
        .map_err(|e| e.to_string())?;
    Ok("安装完成".to_string())
}

/// 失败/取消对测写库（throughput 表保留痕迹，UI 可展示）；
/// full_output 仅在客户端已产出输出时传入（SSH 超时/取消无输出，传 None）。
fn record_fail(state: &AppState, from_id: i64, to_id: i64, msg: &str, full_output: Option<&str>) {
    let _ = repos::throughput::insert(
        &state.db.lock(),
        from_id,
        to_id,
        None,
        Some(msg),
        full_output,
    );
}

/// 完整客户端输出落库（P5.7）：error 预览只有 120 字，JSON 结构差异（如老版
/// 流级统计）必须靠全文确诊。iperf3 `-J` 输出只含 IP/端口/统计，无凭据；
/// 64KB 截断防异常大输出撑爆行存储（P5.8：15 秒 -J 输出实测 16KB+，8KB 会把
/// end 段截掉，完整 JSON 含 end 才可确诊）。
pub(crate) fn collect_full_output(out: &CommandOutput) -> String {
    const FULL_OUTPUT_CAP: usize = 65536;
    let full = format!(
        "exit={}\nstdout:\n{}\nstderr:\n{}",
        out.exit_code,
        out.stdout.trim_end(),
        out.stderr.trim_end()
    );
    if full.chars().count() <= FULL_OUTPUT_CAP {
        full
    } else {
        let head: String = full.chars().take(FULL_OUTPUT_CAP).collect();
        format!("{head}…（已截断）")
    }
}

/// 目标机兜底清理（忽略退出码）：与预清理同一段——kill SPID（精确，可为空）→
/// kill PID 文件 → rm → pkill 兜底（缺失时尽力而为）。
async fn cleanup(sess: &mut SshSession, spid: &str) {
    let _ = sess
        .run("清理服务端", &clean_cmd(spid), Duration::from_secs(10))
        .await;
}
