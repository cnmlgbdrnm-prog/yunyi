//! 测速域服务：mihomo sidecar 生命周期（sidecar）、测速引擎（engine）与
//! 测速执行器（本文件）。执行器职责：sidecar 就绪 → 节点装载 → 按并发窗口分块
//! 并发测速（JoinSet）→ 每节点写库 + 事件；块间检查取消，单节点失败不判任务失败，
//! ssr 节点（mihomo 不支持 SSR）跳过测速并标记原因，仅 sidecar 启动失败/前置校验
//! 失败才 Err。事件复用 job://progress（kind='speed'）。
//!
//! 带宽测速为**自适应下载量**：先拉小量（256KB）估速，再按估速决定是否继续确认，
//! 且**确认量按估值反推**（见 CONFIRM_SECS）——慢链路少要、快链路多要，两端都能在
//! 超时内真的拉完，测出真实带宽而不是回退估值。请求的 `bytes=` 恒等于实际读取量：
//! 只截断读取而不改请求的话，服务端仍按大文件发送，会触发测速源按请求量限流（429）。

use crate::db::repos;
use crate::db::Db;
use crate::domain::{JobProgress, Node, NodeSpeed, SpeedTestReq};
use crate::jobs::EmitFn;
use crate::server_speed::{RealConnect, SshConnect};
use crate::speed::engine::{MihomoEngine, SpeedEngine};
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub mod engine;
pub mod sidecar;

// main.rs 退出清理入口（stop_all 本体在 sidecar 内）
pub use sidecar::stop_all;

/// 延迟测试目标（HTTP 204，最轻量可达性判定）。
pub const DELAY_URL: &str = "http://www.gstatic.com/generate_204";

/// 出口 IP 回显服务（纯文本正文，无 JSON 解析）。
pub const EGRESS_IP_URL: &str = "https://api.ipify.org";

/// 回显正文 → 出口 IP。正文不是 IP 时必须拒绝：运营商插页/门户劫持会返回一段 HTML，
/// 当成出口 IP 落库会把「链路其实不通」伪装成一条成功的记录。
pub fn parse_egress_ip(body: &str) -> Result<String, String> {
    let ip = body.trim();
    if ip.parse::<std::net::IpAddr>().is_err() {
        return Err(format!(
            "出口返回的不是 IP：{}",
            ip.chars().take(80).collect::<String>()
        ));
    }
    Ok(ip.to_string())
}

/// 出口 IP 探测：经 mihomo 注入该出口写回的节点，请求 ipify 读回显。
///
/// **刻意不在中转机上 curl** —— 那要求把住宅代理的密码塞进远端命令行
/// （`/proc/<pid>/cmdline`、shell 历史都会沾上），而中转机常是别人托管的机器。
/// 经本地 mihomo 还顺带验证了**完整链路**（客户端 → 中转机 → 住宅代理），
/// 比「中转机能连上代理」更接近用户真正关心的事。
///
/// 失败不抛给调用方以外的地方：结果落 `last_check_error`（脱敏）后原样返回 Err，
/// 由 UI 红字展示——它**不影响**部署结论（配置早已落地、节点也已写回）。
pub async fn probe_egress_ip(state: &AppState, upstream_id: i64) -> Result<String, String> {
    let (node_id, node) = {
        let conn = state.db.lock();
        let u = repos::upstreams::get(&conn, upstream_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "出口不存在".to_string())?;
        let nid = u
            .node_id
            .clone()
            .ok_or_else(|| "该出口尚未部署（没有节点），先部署再探测".to_string())?;
        let n = repos::nodes::get(&conn, &nid, &state.creds)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "节点不存在".to_string())?;
        (nid, n)
    };

    let result = async {
        let (controller_port, mixed_port, token) = sidecar::ensure_started(state).await?;
        let engine: Arc<dyn SpeedEngine> = Arc::new(MihomoEngine::new(
            controller_port,
            mixed_port,
            token,
            sidecar::sidecar_config_path(state),
        ));
        // 与测速同口径：经中转/生效地址由 relay 规则决定
        let relays = crate::relay::list_relay_rules(state).unwrap_or_default();
        let servers = crate::db::repos::servers::list(&state.db.lock()).unwrap_or_default();
        let privacies = crate::db::repos::privacies::list_all(&state.db.lock()).unwrap_or_default();
        // 带隐私：开了隐私的线在这里会被拒绝注入，从而不会被直连探测
        let proxy = crate::config::node_proxy_json_with_privacy(
            &node, &node_id, &relays, &servers, &privacies,
        )
        .map_err(|e| e.to_string())?;
        engine.put_proxy(&node_id, proxy).await?;
        let fetched = engine
            .fetch_text(mixed_port, EGRESS_IP_URL, Duration::from_secs(20))
            .await;
        // 无论成败都清理注入，避免污染后续测速的节点表
        let _ = engine.delete_proxy(&node_id).await;
        parse_egress_ip(&fetched?)
    }
    .await;

    {
        let conn = state.db.lock();
        let _ = match &result {
            Ok(ip) => repos::upstreams::set_probe(&conn, upstream_id, Some(ip), None),
            Err(e) => repos::upstreams::set_probe(&conn, upstream_id, None, Some(e)),
        };
    }
    result
}
/// 带宽测试下载端点（不含查询参数）：URL 一律经 `download_url` 拼，别在别处再写一份。
pub const SPEED_TEST_ENDPOINT: &str = "https://speed.cloudflare.com/__down";
/// 服务器带宽测试（server_speed）的固定下载目标：10MB。
/// 节点测速的确认量**不用它**——改按链路估值反推（见 CONFIRM_SECS）。
pub const SIZE_LIMIT: usize = 10 * 1024 * 1024;
/// 单节点带宽下载超时。
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// 慢节点小量估速：256KB。极慢节点（<此阈值拉不满且在超时内拉完前量也算有值）用它兜底。
pub const SIZE_LIMIT_SMALL: usize = 256 * 1024;
/// 小量估速后判定为快节点的阈值：256KB 在 <2s 拉完即视为快，继续拉大量确认。
pub const FAST_PROBE_SECS: f64 = 2.0;
/// 确认阶段目标时长：按探针估值反推每流请求量，使确认约在此时间内跑完。
/// 「请求量 × 路数 / 链路速率」就是确认阶段耗时——固定请求量意味着固定耗时门槛，
/// 链路慢于该门槛就永远拉不完，只能回退估值（测不出真实带宽）；快于它又白拉。
/// 所以请求量必须随链路快慢伸缩，而不是钉死一个数。
const CONFIRM_SECS: f64 = 8.0;
/// 确认阶段每流请求量下限：刚过快阈值的链路反推值偏小（128KB/s → 每流 128KB），
/// 8 路并行 + 高 RTT 时还没越过慢启动就拉完了，会低估。
pub const CONFIRM_MIN_PER_STREAM: usize = 256 * 1024;
/// 确认阶段每流请求量上限：封顶单次测速总量（8 × 4MB = 32MB）。测速源按请求量限流，
/// 不封顶的话一条快链路的反推值可达数百 MB，一次就把额度打光。
pub const CONFIRM_MAX_PER_STREAM: usize = 4 * 1024 * 1024;
/// 带宽测速并行流数：8 路并行取 Σ字节/墙钟。单连接受 RTT×窗口限制（国际线路
/// 只跑几 Mbps），聚合值才反映真实拉取带宽；但不能用各流速率求和——流完成时间
/// 不齐时求和会显著高估，必须按总字节/总墙钟。
pub const STREAMS: usize = 8;
/// 延迟测试超时（ms，控制器侧）。
const DELAY_TIMEOUT_MS: u64 = 5000;
/// 分跳测量的采样次数：与服务器延迟同语义（串行 N 次取最小，排掉偶发排队）。
const HOP_SAMPLES: u8 = 3;
/// 节点延迟采样次数：同上，串行 N 次取最小。单次采样噪声极大（同一节点相邻两次
/// 200ms / 1347ms，2026-09-20 实测），一次采样等于把测速结果交给运气。
pub(crate) const DELAY_SAMPLES: u8 = 3;
/// 在中转机上执行分跳命令的超时。
const HOP_CMD_TIMEOUT: Duration = Duration::from_secs(20);

/// 测速执行器入口（队列 runner 调用）：确保 sidecar 就绪后以真实引擎执行。
pub async fn run_speed_job(
    state: &AppState,
    req: SpeedTestReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
) -> Result<(), String> {
    let (controller_port, mixed_port, token) = sidecar::ensure_started(state).await?;
    let engine: Arc<dyn SpeedEngine> = Arc::new(MihomoEngine::new(
        controller_port,
        mixed_port,
        token,
        sidecar::sidecar_config_path(state),
    ));
    // 分跳里的「中转→落地」要在中转机上执行，故生产装配真实 SSH 连接器
    let ssh: Arc<dyn SshConnect> = Arc::new(RealConnect {
        db: state.db.clone(),
        creds: state.creds.clone(),
    });
    let result = run_speed_job_with(state, req, cancel, emit, engine, mixed_port, ssh).await;
    // 测速完把出口切到**实测最快**的节点（仅当用户选的是「自动选择」；见该函数注释：
    // 组是 url-test，按延迟挑，而延迟最低的未必最快）。失败按尽力而为——切不动是网络
    // 客户端的事，测速数据已经落库，不该因此把一次成功的测速报成失败。
    if result.is_ok() {
        let _ = crate::net::runtime::apply_fastest_measured(state).await;
    }
    result
}

/// 注入引擎的执行器本体：不变量测试用 FakeEngine 驱动；生产由 run_speed_job 构造
/// MihomoEngine。返回 Err 仅发生在取消或前置失败；单节点失败不中断整体。
pub(crate) async fn run_speed_job_with(
    state: &AppState,
    req: SpeedTestReq,
    cancel: Arc<AtomicBool>,
    emit: EmitFn,
    engine: Arc<dyn SpeedEngine>,
    mixed_port: u16,
    ssh: Arc<dyn SshConnect>,
) -> Result<(), String> {
    // 节点装载：按 node_ids 顺序装载；缺失节点以「节点不存在」结果占位（不中断整体）
    let mut nodes: Vec<(String, Option<Node>)> = Vec::with_capacity(req.node_ids.len());
    {
        let conn = state.db.lock();
        for id in &req.node_ids {
            let n = repos::nodes::get(&conn, id, &state.creds)
                .map_err(|e| format!("读取节点失败: {e}"))?;
            nodes.push((id.clone(), n));
        }
    }
    let concurrency = req.concurrency.max(1) as usize;
    // 链路上下文：中转规则/服务器表决定生效地址与分跳目标（与上网客户端/导出同口径）
    let relay_ctx = RelayCtx {
        rules: crate::relay::list_relay_rules(state).unwrap_or_default(),
        servers: crate::db::repos::servers::list(&state.db.lock()).unwrap_or_default(),
        ssh,
    };
    let mut done: i64 = 0;
    for chunk in nodes.chunks(concurrency) {
        // 块间取消检查：置位后未开始的块全部跳过
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".to_string());
        }
        let mut set = tokio::task::JoinSet::new();
        for (node_id, node) in chunk {
            let db = state.db.clone();
            let engine = engine.clone();
            let cancel = cancel.clone();
            let node_id = node_id.clone();
            let node = node.clone();
            let relay_ctx = relay_ctx.clone();
            set.spawn(async move {
                test_one(db, engine, mixed_port, node_id, node, cancel, relay_ctx).await
            });
        }
        while let Some(res) = set.join_next().await {
            let (node_id, error) = res.map_err(|e| format!("测速任务异常: {e}"))?;
            done += 1;
            emit(JobProgress {
                job_id: req.job_id,
                kind: "speed".into(),
                step: "node".into(),
                message: node_id,
                progress: done,
                error,
            });
        }
    }
    // 终态事件：全部节点完成后发射（前端据 progress==100 判定整批结束）；
    // 取消返回 Err 不发射终态
    emit(JobProgress {
        job_id: req.job_id,
        kind: "speed".into(),
        step: "完成".into(),
        message: String::new(),
        progress: 100,
        error: None,
    });
    Ok(())
}

/// 单节点测速：注入 → 延迟 → 带宽（延迟失败跳过）→ 清理 → 写库。
/// 返回 (node_id, 单节点错误或 None)；缺失节点直接占位错误，不触碰引擎。
/// 单节点测速的链路上下文：中转规则/服务器表决定生效地址与分跳目标；SSH 连接器用于在
/// **中转机**上执行「中转→落地」那一跳（三段里只有它必须跳到远端执行）。
/// 三者打包传，免得 test_one 的位置参数继续膨胀。
#[derive(Clone)]
struct RelayCtx {
    rules: Vec<crate::domain::RelayRule>,
    servers: Vec<crate::domain::Server>,
    ssh: Arc<dyn SshConnect>,
}

/// 分跳裸 TCP RTT（ms，各 1 个往返）。任一段失败只留 None，不影响其余段。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Hops {
    relay: Option<i64>,
    landing: Option<i64>,
    tunnel: Option<i64>,
}

/// 测三段分跳；节点没挂「已建成」的中转规则 → None（分跳只对中转链路有意义）。
///
/// 「本机→中转机」必须打中转机**自己终结**的端口（这里用 SSH 端口）。规则里的
/// forward_port 是 iptables DNAT 转发入口，连上去实际穿过隧道落到落地机，量到的是
/// 整段路程而非本机到中转机那一跳。
async fn probe_hops(ctx: &RelayCtx, node: &Node) -> Option<Hops> {
    let (rule, relay, landing) = crate::config::node_relay_chain(node, &ctx.rules, &ctx.servers)?;
    let hop_relay =
        crate::server_speed::phase1::tcp_latency_ms(&relay.host, relay.port as u16, HOP_SAMPLES)
            .await
            .ok();
    // 后两段都需要落地机：缺失（规则里引用的服务器已被删）→ 只留本机→中转那一段。
    let (hop_landing, tunnel) = match landing {
        Some(l) => (
            crate::server_speed::phase1::tcp_latency_ms(&l.host, l.port as u16, HOP_SAMPLES)
                .await
                .ok(),
            tunnel_rtt_ms(ctx, relay, &rule.landing_wg_ip, rule.landing_port).await,
        ),
        None => (None, None),
    };
    Some(Hops {
        relay: hop_relay,
        landing: hop_landing,
        tunnel,
    })
}

/// 中转机→落地机的 TCP 往返：必须**在中转机上**执行，目标用隧道内地址（中转流量真正
/// 走的是隧道，不是落地机公网 IP）。curl 的 `%{time_connect}` 即到 TCP 握手完成的
/// 时间 = 1 个往返。注意 ss 服务器收到 HTTP 会直接断开，curl 退出码非 0 **属正常**，
/// 所以只解析 stdout、不看退出码。
async fn tunnel_rtt_ms(
    ctx: &RelayCtx,
    relay: &crate::domain::Server,
    landing_wg_ip: &str,
    landing_port: i64,
) -> Option<i64> {
    let mut sess = ctx.ssh.connect(relay).await.ok()?;
    let cmd = format!(
        "for i in $(seq {HOP_SAMPLES}); do curl -o /dev/null -s -w '%{{time_connect}}\\n' \
         --max-time 5 \"http://{landing_wg_ip}:{landing_port}/\"; done"
    );
    let out = sess.run("分跳测速", &cmd, HOP_CMD_TIMEOUT).await.ok()?;
    min_time_connect_ms(&out.stdout)
}

/// 从 curl `%{time_connect}` 输出（秒，每行一个）取最小值转毫秒。
/// 非数字与 0 一律跳过：连不上时 curl 打 `0.000000`，照收会把「连不上」记成 0ms。
fn min_time_connect_ms(stdout: &str) -> Option<i64> {
    let mut min: Option<f64> = None;
    for v in stdout.lines().filter_map(|l| l.trim().parse::<f64>().ok()) {
        if v > 0.0 {
            min = Some(min.map_or(v, |m: f64| m.min(v)));
        }
    }
    min.map(|s| (s * 1000.0).round() as i64)
}

async fn test_one(
    db: Arc<Db>,
    engine: Arc<dyn SpeedEngine>,
    mixed_port: u16,
    node_id: String,
    node: Option<Node>,
    cancel: Arc<AtomicBool>,
    ctx: RelayCtx,
) -> (String, Option<String>) {
    let Some(node) = node else {
        return (node_id, Some("节点不存在".to_string()));
    };
    // ssr 协议：mihomo 已移除 SSR 支持，跳过该节点测速并标记原因（不触碰引擎、不中断整批）
    if node.protocol == "ssr" {
        return (node_id, Some("mihomo 不支持 SSR，跳过测速".to_string()));
    }
    // §11：**开了隐私的线一律跳过测速**。测速引擎拨的是节点的 (addr, port)，
    // 也就是 VPS 公网 IP——那正是隐私模式要藏起来的东西。让软件自己的健康检查
    // 把它暴露给本地网络，是这套功能最讽刺的失败方式。宁可显示「未测试」。
    let privacies = crate::db::repos::privacies::list_all(&db.lock()).unwrap_or_default();
    if crate::servers::privacy::effective_privacy(&node.id, &privacies).is_some() {
        return (
            node_id,
            Some("隐私线路不参与直连测速（探测会暴露 VPS 公网 IP），已跳过".to_string()),
        );
    }
    // 注入：节点形态与导出 YAML 一致（name 覆盖为 node.id，唯一且无 URL 转义问题）；
    // 挂中转规则的节点在此即按中转地址注入（与用户测速看到的直连地址一一对应）
    let proxy = match crate::config::node_proxy_json_with_privacy(
        &node,
        &node_id,
        &ctx.rules,
        &ctx.servers,
        &privacies,
    ) {
        Ok(p) => p,
        Err(e) => return (node_id, Some(format!("节点配置生成失败: {e}"))),
    };
    // 分跳（仅挂中转的节点）：三段各 1 个裸 TCP 往返，与 latency 口径不同、不可相加，
    // 只回答「哪一跳烂」。任一段失败留 None，不影响其余段与主路径。
    let hops = probe_hops(&ctx, &node).await.unwrap_or_default();
    // 中转对比：挂了中转规则的节点，先按「直连形态」（节点原地址）测一次延迟。它与随后
    // 的经中转值**同口径**（都是 mihomo URL 测试，含 DNS/建连/请求响应/代理开销），所以
    // 可以直接相减得出中转收益；裸 TCP 分段 RTT 不是同一个量（每段仅 1 个 RTT），不能这么比。
    // 失败只记 NULL：直连不通不代表经中转不可用，绝不能影响主路径的结果。
    let mut direct_latency: Option<i64> = None;
    if node.relay_rule_id.is_some() {
        if let Ok(direct) = crate::config::node_proxy_json(&node, &node_id, &[], &[]) {
            if engine.put_proxy(&node_id, direct).await.is_ok() {
                direct_latency = engine
                    .delay(&node_id, DELAY_URL, DELAY_TIMEOUT_MS)
                    .await
                    .ok();
            }
        }
    }
    let mut latency: Option<i64> = None;
    let mut bandwidth: Option<i64> = None;
    let mut error: Option<String> = None;
    if let Err(e) = engine.put_proxy(&node_id, proxy).await {
        error = Some(e);
    }
    if error.is_none() {
        // 延迟：串行 N 次**取最小**，与分跳/服务器延迟同一约定。单次采样在这条链路上
        // 噪声大到没法比节点——同一节点相邻两次测速 200ms 与 1347ms（2026-09-20 实测），
        // 而首采还要额外担掉建连开销，正是最该被丢掉的那个样本。排队只会加时间不会减，
        // 所以这里的正确估计是最小值而不是均值。
        //
        // 全部失败才算失败：一次抖动不该把整个节点判死（带宽那步会跟着被跳过）。
        let mut last_err: Option<String> = None;
        for _ in 0..DELAY_SAMPLES {
            match engine.delay(&node_id, DELAY_URL, DELAY_TIMEOUT_MS).await {
                Ok(ms) => latency = Some(latency.map_or(ms, |m: i64| m.min(ms))),
                Err(e) => last_err = Some(e),
            }
        }
        if latency.is_none() {
            error = last_err;
        }
    }
    // 延迟失败跳过带宽：仍记录 latency=NULL + 错误（update_speed 语义）。
    // 带宽为自适应下载量（8 路并行聚合）：慢节点先小量估速，快节点再拉大量确认。
    // 聚合任务层兜底 DOWNLOAD_TIMEOUT 保证 30s 内必返回。
    if error.is_none() {
        match tokio::time::timeout(
            DOWNLOAD_TIMEOUT,
            download_adaptive(
                &engine,
                mixed_port,
                SPEED_TEST_ENDPOINT,
                DOWNLOAD_TIMEOUT,
                cancel,
            ),
        )
        .await
        {
            Ok(Ok(bps)) => bandwidth = Some(bps.round() as i64),
            Ok(Err(e)) => error = Some(e),
            Err(_) => error = Some("下载超时".to_string()),
        }
    }
    // 尽力清理注入的代理（失败不覆盖已有错误：指标已取得，残留由下次 PUT 覆盖）
    let _ = engine.delete_proxy(&node_id).await;
    let metrics = NodeSpeed {
        latency,
        direct_latency,
        hop_relay_ms: hops.relay,
        hop_landing_ms: hops.landing,
        hop_tunnel_ms: hops.tunnel,
        bandwidth,
        error: error.clone(),
    };
    if let Err(e) = repos::nodes::update_speed(&db.lock(), &node_id, &metrics) {
        // 写库失败拼接保留原诊断（延迟/下载错误不可被覆盖）
        error = Some(match error.take() {
            Some(orig) => format!("{orig}；写库失败: {e}"),
            None => format!("写库失败: {e}"),
        });
    }
    (node_id, error)
}

/// 自适应带宽测速：先小量（SIZE_LIMIT_SMALL）估速，判定快慢后决定是否拉大量确认。
/// 返回估算的 bytes/s。
///
///   - 小量吞吐 ≥ 256KB/2s（快节点）→ 继续拉大量（SIZE_LIMIT）确认真实上限；
///   - 小量吞吐低（慢节点，聚合内部按实际到位字节估算不报错）→ 直接按小量估算返回，
///     不再要求拉满 10MB；极慢/断流（无任何字节到位）才报超时/失败。
///
/// 慢节点出口带宽有限（如 <1MB/s）时，10MB 在 30s 内拉不满 → 旧逻辑报「下载超时」；
/// 自适应改为小量命中即估算带宽落库，不再因目标文件过大而误报慢节点不可测。
async fn download_adaptive(
    engine: &Arc<dyn SpeedEngine>,
    mixed_port: u16,
    url: &str,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<f64, String> {
    // 快慢阈值：256KB 在 <FAST_PROBE_SECS(2s) 内拉满 = Σ字节/墙钟 ≥ 256KB/2s → 快节点。
    let fast_threshold = SIZE_LIMIT_SMALL as f64 / FAST_PROBE_SECS;
    // 阶段一：小量估速。聚合内部 8 流同时拉，慢节点超时后按实际到位字节估算（不因
    // 拉不满 256KB 而报错，只要 Σ字节>0）；极慢断流（Σ字节=0）才返回错误。
    let probe = tokio::time::timeout(
        timeout,
        download_aggregate(
            engine,
            mixed_port,
            url,
            SIZE_LIMIT_SMALL,
            timeout,
            cancel.clone(),
        ),
    )
    .await
    .map_err(|_| "下载超时".to_string())??;
    if probe >= fast_threshold {
        // 快节点：按探针估值反推确认量，确认它在 CONFIRM_SECS 量级内跑得完。
        // 固定请求量（曾是每流 10MB）等于给「多久拉完」钉死门槛：8×10MB 要在 30s 内
        // 完成需 2.67MB/s，而快阈值只要求 128KB/s——中间这段（实测经中转的链路
        // 2.4MB/s 正落在其中）判为"快"却永远拉不满，于是只能回退估值，**测不出正常值**；
        // 快链路又白拉 80MB，把测速源额度打光（见 CONFIRM_MAX_PER_STREAM）。
        // 反推后确认阶段耗时≈CONFIRM_SECS，快慢链路都能真的拉完并测出数。
        // 真失败（断流/限流/超时）仍回退阶段一估值：探针刚成功，说明链路可用。
        let confirm_size = ((probe * CONFIRM_SECS / STREAMS as f64) as usize)
            .clamp(CONFIRM_MIN_PER_STREAM, CONFIRM_MAX_PER_STREAM);
        match tokio::time::timeout(
            timeout,
            download_aggregate(engine, mixed_port, url, confirm_size, timeout, cancel),
        )
        .await
        {
            Ok(Ok(bps)) => Ok(bps),
            // Ok(Err) = 8 路全失败/全超时（Σ字节为 0）；Err = 聚合整体超时
            Ok(Err(_)) | Err(_) => Ok(probe),
        }
    } else {
        // 慢节点：小量估算即为其真实带宽（继续拉大量只会超时），直接返回。
        Ok(probe)
    }
}

/// 拼本次下载请求 URL：`bytes=` 必须等于本次实际读取量（size_limit）。
///
/// 服务端按 `bytes=` 决定发送多少，客户端只截断读取而不改请求，在服务端看仍是
/// 「大请求被打断」而非小请求——实测该形状会被 speed.cloudflare.com 按请求量限流
/// （8 路 `bytes=10485760` 全 429，同样 8 路 `bytes=262144` 全 200）。探针自称小量
/// 却请求 10MB，既触发限流，也让每次测速白白向服务端多要 8 个大文件。
pub fn download_url(endpoint: &str, size_limit: usize) -> String {
    format!("{endpoint}?bytes={size_limit}")
}

/// 8 路并行聚合带宽测速：每路经 mixed-port 代理拉取同一测试文件，每个流自身拉取上限
/// 为 size_limit（快节点确认大量/down_adaptive 小量估速共用），**请求量与之保持一致**
/// （见 download_url）。返回 Σ已下载字节 / 总墙钟（bytes/s）。各流自身超时容错：
/// 超时/失败流计 0 字节，全部流失败才返回第一个错误；取消 → '已取消'。虚拟时钟冻结
/// （测试环境墙钟为 0）时按 1s 归一：聚合值等于 Σ字节，可确定性断言。
async fn download_aggregate(
    engine: &Arc<dyn SpeedEngine>,
    mixed_port: u16,
    url: &str,
    size_limit: usize,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<f64, String> {
    let url = download_url(url, size_limit);
    let started = tokio::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..STREAMS {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let url = url.clone();
        set.spawn(async move {
            engine
                .download_bytes(mixed_port, &url, size_limit, timeout, cancel)
                .await
        });
    }
    let mut total_bytes: f64 = 0.0;
    let mut errors: Vec<String> = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(bytes)) => total_bytes += bytes,
            Ok(Err(e)) => errors.push(e),
            Err(e) => errors.push(format!("测速流异常: {e}")),
        }
    }
    if total_bytes <= 0.0 || !total_bytes.is_finite() {
        return Err(errors
            .into_iter()
            .next()
            .unwrap_or_else(|| "下载失败".to_string()));
    }
    let wall = started.elapsed().as_secs_f64();
    let wall = if wall > 0.0 { wall } else { 1.0 };
    Ok(total_bytes / wall)
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/speed/mods.rs"
));
