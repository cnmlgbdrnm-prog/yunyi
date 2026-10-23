//! 中转链路服务：探测（probe）、接管（adopt）、链路 CRUD 与状态查询。
//! 探测与状态查询为只读 SSH 命令；adopt 为纯 DB 写回（不改服务器配置）；
//! 建链/拆链/设备下发在 build.rs 与 peers.rs（经 job 队列）。
//! 输入：RelayRule/AdoptLinkInput/规则 id；输出：ProbeResult/LinkStatus/规则行。
//! 凭据经 servers::load_auth 解密即用即弃；公钥可入事件与展示，私钥/PSK 不出。

pub mod build;
pub mod config;
pub mod extend;
pub mod peers;
pub mod status_poll;
// 设备 peer 命令层以 relay:: 平级路径消费（与 build/peers 模块内函数同一命名面）
pub use peers::{add_device_peer, get_peer_config, list_device_peers, remove_device_peer};

use crate::db::repos;
use crate::db::Db;
use crate::domain::{AdoptLinkInput, DnatPair, LinkStatus, ProbeResult, RelayRule, Server};
use crate::servers::{load_auth, ServersError, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::{AuthMaterial, SshSession};
use crate::state::AppState;
use rusqlite::Connection;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Semaphore;

pub const WG_IFACE: &str = "wg0";
/// 远端 wg0 配置路径（白名单常量，禁止动态构造）。
pub const WG_CONF_PATH: &str = "/etc/wireguard/wg0.conf";
/// rebuild 前备份路径。
pub const WG_BAK_PATH: &str = "/etc/wireguard/wg0.conf.bak";
/// 状态查询单命令超时（规格 §8：状态查询 15s）。
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(15);
/// 状态查询并发上限：同 P1 同步并发模式，防批量刷新打爆两端 SSH。
/// static 而非 const：Semaphore 含内部可变性，const 借用触发 borrow_interior_mutable_const。
static STATUS_SEM: Semaphore = Semaphore::const_new(2);

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("{0}")]
    Validation(String),
    #[error("链路不存在")]
    NotFound,
    #[error("数据库错误: {0}")]
    Db(String),
    #[error("凭据加密错误: {0}")]
    Crypto(String),
    #[error("{0}")]
    Ssh(String),
    #[error("系统不支持 WireGuard: {0}")]
    WgNotSupported(String),
    #[error("{step} 执行失败: {stderr_tail}")]
    WgCommandFailed { step: String, stderr_tail: String },
    #[error("链路未建立")]
    LinkNotBuilt,
    #[error("链路已建立，请先拆链")]
    LinkBuilt,
    #[error("端口 {port} 已被占用")]
    PortInUse { port: u16 },
    #[error("握手超时")]
    HandshakeTimeout,
    #[error("设备地址已耗尽")]
    PeerIpExhausted,
    #[error("{side} 探测失败")]
    ProbeFailed { side: String },
}

pub(crate) fn map_servers_error(e: ServersError) -> RelayError {
    match e {
        ServersError::Validation(s) => RelayError::Validation(s),
        ServersError::NotFound => RelayError::NotFound,
        ServersError::Db(s) => RelayError::Db(s),
        ServersError::Crypto(s) => RelayError::Crypto(s),
        ServersError::Ssh(s) => RelayError::Ssh(s.to_string()),
        ServersError::HasDependents => RelayError::Validation("服务器存在关联记录".into()),
        ServersError::LocalKey(s) => RelayError::Validation(s),
    }
}

/// 建链前探测：两端只读 SSH 命令回读 WG 现状，不落任何配置。
/// 单端连接失败 → 该端 *_reachable=false 不报错（结果对象带回）；两端都连不上才 Err。
pub async fn probe_link(
    state: &AppState,
    relay_server_id: i64,
    landing_server_id: i64,
) -> Result<ProbeResult, RelayError> {
    let (relay_auth, relay_srv) = load_auth(state, relay_server_id).map_err(map_servers_error)?;
    let (landing_auth, landing_srv) =
        load_auth(state, landing_server_id).map_err(map_servers_error)?;
    let mut result = ProbeResult {
        relay_reachable: false,
        landing_reachable: false,
        relay_wg: false,
        landing_wg: false,
        handshake: false,
        relay_pubkey: String::new(),
        landing_pubkey: String::new(),
        relay_wg_ip: None,
        landing_wg_ip: None,
        wg_iface: None,
        tailscale: false,
        wg_port: None,
        subnet: None,
        forward_port: None,
        dnat_pairs: Vec::new(),
        message: String::new(),
    };
    let (relay_conn, landing_conn) = tokio::join!(
        SshSession::connect(
            &relay_srv.host,
            relay_srv.port as u16,
            &relay_srv.user,
            &relay_auth,
            CONNECT_TIMEOUT,
        ),
        SshSession::connect(
            &landing_srv.host,
            landing_srv.port as u16,
            &landing_srv.user,
            &landing_auth,
            CONNECT_TIMEOUT,
        ),
    );
    let mut relay_sp: Option<SideProbe> = None;
    let mut landing_sp: Option<SideProbe> = None;
    if let Ok(mut sess) = relay_conn {
        result.relay_reachable = true;
        relay_sp = Some(probe_side(&mut sess, true).await);
    }
    if let Ok(mut sess) = landing_conn {
        result.landing_reachable = true;
        landing_sp = Some(probe_side(&mut sess, false).await);
    }
    if !result.relay_reachable && !result.landing_reachable {
        return Err(RelayError::ProbeFailed {
            side: "中转与落地两端".into(),
        });
    }
    if let Some(sp) = &relay_sp {
        fill_side(&mut result, sp, true);
    }
    if let Some(sp) = &landing_sp {
        fill_side(&mut result, sp, false);
    }
    // 互认判定：中转 peer 行含落地本端公钥 且 落地 peer 行含中转本端公钥；
    // 握手判定：两端 latest-handshakes 对应行 epoch 均非零。
    let mutual = relay_sp
        .as_ref()
        .map(|sp| sp.peers.iter().any(|(k, _)| k == &result.landing_pubkey))
        .unwrap_or(false)
        && landing_sp
            .as_ref()
            .map(|sp| sp.peers.iter().any(|(k, _)| k == &result.relay_pubkey))
            .unwrap_or(false);
    let relay_hs = relay_sp
        .as_ref()
        .and_then(|sp| sp.handshakes.get(&result.landing_pubkey))
        .copied()
        .unwrap_or(0)
        > 0;
    let landing_hs = landing_sp
        .as_ref()
        .and_then(|sp| sp.handshakes.get(&result.relay_pubkey))
        .copied()
        .unwrap_or(0)
        > 0;
    result.handshake = mutual && relay_hs && landing_hs;
    result.message = probe_message(&result);
    Ok(result)
}

/// 单端探测的临时结果。
struct SideProbe {
    wg: bool,
    /// 命中的接口名（多接口时按命中规则选取，不假设 wg0）
    iface: Option<String>,
    pubkey: String,
    /// 命中接口的 allowed-ips 输出（10.9.x.y/N 令牌 subnet 解析用；
    /// 不用 dump——dump 首行含本端私钥，不得过日志/缓冲区）。
    allowed_ips_raw: String,
    /// 命中接口的 peer 行（公钥 → allowed-ips）
    peers: Vec<(String, String)>,
    handshakes: HashMap<String, i64>,
    /// 命中接口的真实 IPv4 地址（`ip -o addr show` 解析，P5.1：
    /// 手动链路的 imported 规则不再硬凑 {prefix}.1/.2 合成值）
    inet: Option<String>,
    /// 命中接口的网段（地址 CIDR 归一：`10.66.66.1/30` → `10.66.66.0/30`）——
    /// 接管表单「网段」的预填值，链路网段的权威来源
    subnet: Option<String>,
    listen_port: Option<i64>,
    /// 中转侧 DNAT 表（原样回读；落地侧恒空）
    dnat_pairs: Vec<DnatPair>,
    tailscale: bool,
}

/// 单端命令序列：wg show interfaces 枚举接口（失败/空 → 无 WG 并跳过其余）
/// → 逐接口 public-key + latest-handshakes 选命中接口 → allowed-ips
/// → listen-port（中转）→ iptables DNAT 回读（中转）。
/// 接口名不假设 wg0：手动建的链路可用 wg1 等任意名称（真机验收发现）。
/// 命令错误/非零退出一律按「无该数据」处理，探测不因单命令失败报错。
async fn probe_side(sess: &mut SshSession, is_relay: bool) -> SideProbe {
    let mut sp = SideProbe {
        wg: false,
        iface: None,
        pubkey: String::new(),
        allowed_ips_raw: String::new(),
        peers: Vec::new(),
        handshakes: HashMap::new(),
        inet: None,
        subnet: None,
        listen_port: None,
        dnat_pairs: Vec::new(),
        tailscale: detect_tailscale(sess).await,
    };
    let Ok(ifaces_out) = sess.run("探测", "wg show interfaces", PROBE_TIMEOUT).await else {
        return sp;
    };
    if ifaces_out.exit_code != 0 {
        return sp;
    }
    // 远端输出，进命令前过白名单（不留则当它不存在——比拿它拼命令安全）
    let ifaces: Vec<String> = ifaces_out
        .stdout
        .split_whitespace()
        .filter(|n| crate::relay::build::valid_iface_name(n))
        .map(str::to_string)
        .collect();
    if ifaces.is_empty() {
        return sp;
    }
    // 逐接口取公钥与握手表
    let mut candidates: Vec<(String, String, HashMap<String, i64>)> = Vec::new();
    for iface in &ifaces {
        let Some(pk_out) = iface_cmd(sess, iface, "public-key").await else {
            continue;
        };
        let pubkey = pk_out.trim().to_string();
        if pubkey.is_empty() {
            continue;
        }
        let handshakes = iface_cmd(sess, iface, "latest-handshakes")
            .await
            .map(|out| parse_latest_handshakes(&out))
            .unwrap_or_default();
        candidates.push((iface.clone(), pubkey, handshakes));
    }
    // 命中规则：有握手 epoch>0 的接口优先（活着的链路），其中 wg0 优先；否则 wg0；否则第一个
    let Some(pick) = candidates
        .iter()
        .find(|(i, _, hs)| i == WG_IFACE && hs.values().any(|e| *e > 0))
        .or_else(|| {
            candidates
                .iter()
                .find(|(_, _, hs)| hs.values().any(|e| *e > 0))
        })
        .or_else(|| candidates.iter().find(|(i, _, _)| i == WG_IFACE))
        .or(candidates.first())
    else {
        return sp;
    };
    sp.wg = true;
    sp.iface = Some(pick.0.clone());
    sp.pubkey = pick.1.clone();
    sp.handshakes = pick.2.clone();
    if let Some(out) = iface_cmd(sess, &pick.0, "allowed-ips").await {
        sp.allowed_ips_raw = out;
        sp.peers = parse_wg_dump(&sp.allowed_ips_raw);
    }
    // 真实接口地址：adopt/readopt 落库用（P5.1 根因——手工链路 WG 地址
    // 曾硬凑 {prefix}.1/.2，与真实接口地址不符时对测连黑洞地址超时）
    if let Ok(out) = sess
        .run(
            "探测",
            &format!("ip -o addr show {}", pick.0),
            PROBE_TIMEOUT,
        )
        .await
    {
        if out.exit_code == 0 {
            sp.inet = parse_iface_inet(&out.stdout);
            // 网段同源（同一次回读）：接口地址带前缀，归一成网络地址
            sp.subnet = parse_iface_cidr(&out.stdout)
                .and_then(|(addr, prefix)| network_cidr(&addr, prefix));
        }
    }
    if is_relay {
        if let Some(out) = iface_cmd(sess, &pick.0, "listen-port").await {
            sp.listen_port = parse_listen_port(&out);
        }
        // DNAT 回读作为接管表单的转发端口/落地端口默认值；探测阶段不知出口网卡，egress_if 传空 = 任意网卡
        if let Ok(out) = sess
            .run("探测", "iptables -t nat -S PREROUTING", PROBE_TIMEOUT)
            .await
        {
            if out.exit_code == 0 {
                sp.dnat_pairs = parse_dnat_pairs(&out.stdout, "");
            }
        }
    }
    sp
}

/// 单接口命令封装：错误/非零退出返回 None（探测不因单命令失败报错）。
async fn iface_cmd(sess: &mut SshSession, iface: &str, sub: &str) -> Option<String> {
    match sess
        .run("探测", &format!("wg show {iface} {sub}"), PROBE_TIMEOUT)
        .await
    {
        Ok(out) if out.exit_code == 0 => Some(out.stdout),
        _ => None,
    }
}

/// Tailscale 检测：ip -br link 输出含 tailscale 接口名即视为存在（仅提示，不参与链路判断）。
async fn detect_tailscale(sess: &mut SshSession) -> bool {
    matches!(
        sess.run("探测", "ip -br link", PROBE_TIMEOUT).await,
        Ok(out) if out.exit_code == 0 && out.stdout.contains("tailscale")
    )
}

/// 把单端探测结果并入 ProbeResult。
fn fill_side(result: &mut ProbeResult, sp: &SideProbe, is_relay: bool) {
    if is_relay {
        result.relay_wg = sp.wg;
        result.relay_pubkey = sp.pubkey.clone();
        result.relay_wg_ip = sp.inet.clone();
        result.wg_port = sp.listen_port;
        result.dnat_pairs = sp.dnat_pairs.clone();
        // forward_port 是这张表的派生视图：首个入口端口（口径不变）
        result.forward_port = result.dnat_pairs.first().map(|p| p.forward_port);
    } else {
        result.landing_wg = sp.wg;
        result.landing_pubkey = sp.pubkey.clone();
        result.landing_wg_ip = sp.inet.clone();
    }
    // 接口名以中转侧优先（接管默认值展示）；relay 先并入，因此非空时不覆盖
    if result.wg_iface.is_none() {
        result.wg_iface = sp.iface.clone();
    }
    result.tailscale |= sp.tailscale;
    // subnet：接口自身 CIDR 优先（真机手工链路是 `Address = 10.66.66.1/30`）；
    // 地址读不到时才退回 allowed-ips 的 10.9.x 令牌（本工具自建链路的形态）。中转侧优先
    if result.subnet.is_none() {
        result.subnet = sp
            .subnet
            .clone()
            .or_else(|| parse_subnet_from_dump(&sp.allowed_ips_raw));
    }
}

/// 组装探测说明：失败原因列表（脱敏）+ 命中接口与 Tailscale 提示。
fn probe_message(result: &ProbeResult) -> String {
    let mut reasons = Vec::new();
    if !result.relay_reachable {
        reasons.push("中转机不可达".to_string());
    }
    if !result.landing_reachable {
        reasons.push("落地机不可达".to_string());
    }
    if result.relay_reachable && !result.relay_wg {
        reasons.push("中转机未检测到 WireGuard 接口".to_string());
    }
    if result.landing_reachable && !result.landing_wg {
        reasons.push("落地机未检测到 WireGuard 接口".to_string());
    }
    if result.relay_wg && result.landing_wg && !result.handshake {
        reasons.push("两端未握手".to_string());
    }
    if result.tailscale {
        reasons.push("检测到 Tailscale 接口（不参与本工具链路判断）".to_string());
    }
    if let Some(iface) = &result.wg_iface {
        reasons.push(format!("检测到的 WireGuard 接口: {iface}"));
    }
    reasons.join("；")
}

/// 接管：对已存在的中转链路补充规则行（不改服务器配置）。
/// P5.1：imported 规则必须落库**真实** WG 接口地址（接管前双侧探测
/// `ip -o addr show` 读取；任一读不到 → 报错），不再硬凑 {prefix}.1/.2
/// 合成值——手工链路的合成值与真实接口地址不符时，对测/节点走 WG 会连
/// 黑洞地址超时（老板真机根因）。
/// 落地机已有 status=failed 的半成品行（建链中途取消/失败）时先删除再接管：
/// 恢复路径——否则用户永远卡在「已存在链路」且拆链会去动服务器。
pub async fn adopt_link(state: &AppState, input: AdoptLinkInput) -> Result<RelayRule, RelayError> {
    validate_adopt_input(&input)?;
    // 真实地址探测（imported 必真）：探测读不到 → 拒绝接管并给可读原因
    let probe = probe_link(state, input.relay_server_id, input.landing_server_id).await?;
    let relay_wg_ip = probe.relay_wg_ip.clone().ok_or_else(|| {
        RelayError::Validation(
            "无法读取 wg 接口地址（中转侧）：请确认 WireGuard 已启用且接口有 IPv4 地址".into(),
        )
    })?;
    let landing_wg_ip = probe.landing_wg_ip.clone().ok_or_else(|| {
        RelayError::Validation(
            "无法读取 wg 接口地址（落地侧）：请确认 WireGuard 已启用且接口有 IPv4 地址".into(),
        )
    })?;
    let conn = state.db.lock();
    // 中转机必须存在。地址本身用不上了（挂载不再写 addr），留着这行是兜「库里没这行」
    // ——探测已覆盖连通性，这里只查存在。
    repos::servers::get(&conn, input.relay_server_id)
        .map_err(|e| RelayError::Db(e.to_string()))?
        .ok_or(RelayError::NotFound)?;
    let landing = repos::servers::get(&conn, input.landing_server_id)
        .map_err(|e| RelayError::Db(e.to_string()))?
        .ok_or(RelayError::NotFound)?;
    // 判重按 (落地机, 落地端口)：一台落地机上可以有多个服务、多条链路，
    // 落地端口是它们的身份。只按落地机判会把第二个服务误判成「已存在链路」
    let existing = repos::relay::find_by_landing_port(
        &conn,
        input.landing_server_id,
        input.landing_port as i64,
    )
    .map_err(|e| RelayError::Db(e.to_string()))?;
    match existing {
        Some(r) if r.status == "failed" => {
            // failed 半成品：级联清设备行并删行（网段随之释放），随后按新输入接管
            repos::relay::delete(&conn, r.id).map_err(|e| RelayError::Db(e.to_string()))?;
        }
        Some(r) => {
            return Err(RelayError::Validation(format!(
                "该落地机的落地端口 {} 已有链路（{}），请先拆链或删除",
                input.landing_port, r.name
            )));
        }
        None => {}
    }
    // 同一条隧道上的多条链路共用网段是正常的；只有跨隧道重网段才拦
    if repos::relay::subnet_taken_elsewhere(
        &conn,
        &input.subnet,
        input.relay_server_id,
        input.landing_server_id,
    )
    .map_err(|e| RelayError::Db(e.to_string()))?
    {
        return Err(RelayError::Validation(
            "网段已被另一条隧道占用（同一中转/落地对上的链路共用网段是允许的）".into(),
        ));
    }
    // 公钥以**本次探测回读**为准：接管本来就是「回读现有配置」，两端公钥探测已经读到了。
    // 调用方没带（旧前端、脚本、或前端用的还是上次探测的结果）时不该落 NULL——
    // 缺公钥的行状态查询要退化成整个接口聚合，真实拆除会被直接拒。
    let relay_pubkey = input
        .relay_pubkey
        .clone()
        .or_else(|| nonempty(&probe.relay_pubkey).map(str::to_string));
    let landing_pubkey = input
        .landing_pubkey
        .clone()
        .or_else(|| nonempty(&probe.landing_pubkey).map(str::to_string));
    let rule = RelayRule {
        id: 0,
        name: input.name.clone(),
        relay_server_id: input.relay_server_id,
        landing_server_id: input.landing_server_id,
        client_mode: input.client_mode.clone(),
        wg_port: input.wg_port as i64,
        forward_port: input.forward_port.map(|p| p as i64),
        landing_port: input.landing_port as i64,
        subnet: input.subnet.clone(),
        relay_wg_ip,
        landing_wg_ip,
        relay_pubkey,
        landing_pubkey,
        converged: input.converged,
        status: "imported".into(),
        // 接管的是既有人工链路：语义按 single（端口对端口）记账
        forward_mode: "single".into(),
    };
    let id = repos::relay::insert(&conn, &rule).map_err(|e| RelayError::Db(e.to_string()))?;
    // 节点改指须带真实行 id（rule 构造时 id=0，insert 后才落库）
    let rule = RelayRule { id, ..rule };
    reroute_landing_nodes(&conn, &rule, &landing, &state.creds)?;
    Ok(rule)
}

/// 落地节点挂上中转链路：**只落 relay_rule_id，绝不碰 addr/port**。
///
/// 生效地址由 [`crate::config::effective_node`] 在生成侧一处算出——`config/mod.rs`
/// 的不变量是「数据始终存直连地址」。这里若连地址一起写，会坏掉两件事：
/// ① 关掉中转时那个被写坏的原地址会浮上来（本地曾实测：节点标着直连、地址却是中转口）；
/// ② `find_for_landing_server` 是按「落地机地址 + 端口」找节点的，地址一改下次就找不到它，
/// 转而新建一个——就是重名节点的来源。
///
/// 落地节点按「落地机 host + 服务端口（landing_port）」匹配，不是 SSH 端口。
/// adopt 与 readopt（failed 行一键恢复）共用。
fn reroute_landing_nodes(
    conn: &Connection,
    rule: &RelayRule,
    landing: &crate::domain::Server,
    creds: &crate::crypto::CredentialService,
) -> Result<(), RelayError> {
    let nodes = repos::nodes::find_for_landing_server(conn, landing, rule.landing_port, creds)
        .map_err(|e| RelayError::Db(e.to_string()))?;
    for n in nodes {
        repos::nodes::set_relay(conn, &n.id, Some(rule.id))
            .map_err(|e| RelayError::Db(e.to_string()))?;
    }
    Ok(())
}

/// 空串归一为 None（探测无接口时公钥为空串，入库应为 NULL）。
fn nonempty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// 重新接管：failed 半成品行（建链中断残留）且两端实际已握手时，一键回读
/// 公钥并置 imported——恢复路径，免去重新走建链向导（行内参数已齐全）。
pub async fn readopt_link(state: &AppState, rule_id: i64) -> Result<RelayRule, RelayError> {
    let rule = {
        let conn = state.db.lock();
        repos::relay::get(&conn, rule_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?
    };
    if rule.status != "failed" {
        return Err(RelayError::Validation(
            "仅失败的半成品链路可重新接管".into(),
        ));
    }
    let probe = probe_link(state, rule.relay_server_id, rule.landing_server_id).await?;
    if !probe.handshake {
        return Err(RelayError::Validation(format!(
            "两端未握手，无法接管：{}",
            probe.message
        )));
    }
    let conn = state.db.lock();
    repos::relay::set_pubkeys(
        &conn,
        rule.id,
        nonempty(&probe.relay_pubkey),
        nonempty(&probe.landing_pubkey),
    )
    .map_err(|e| RelayError::Db(e.to_string()))?;
    // P5.1：恢复为 imported 时同步真实接口地址（读到即更新；探测已要求两端握手，
    // 接口必有 IPv4——读不到保留库值不阻断恢复路径）
    repos::relay::set_wg_ips(
        &conn,
        rule.id,
        probe.relay_wg_ip.as_deref(),
        probe.landing_wg_ip.as_deref(),
    )
    .map_err(|e| RelayError::Db(e.to_string()))?;
    repos::relay::update_status(&conn, rule.id, "imported")
        .map_err(|e| RelayError::Db(e.to_string()))?;
    repos::servers::get(&conn, rule.relay_server_id)
        .map_err(|e| RelayError::Db(e.to_string()))?
        .ok_or(RelayError::NotFound)?;
    let landing = repos::servers::get(&conn, rule.landing_server_id)
        .map_err(|e| RelayError::Db(e.to_string()))?
        .ok_or(RelayError::NotFound)?;
    reroute_landing_nodes(&conn, &rule, &landing, &state.creds)?;
    repos::relay::get(&conn, rule_id)
        .map_err(|e| RelayError::Db(e.to_string()))?
        .ok_or(RelayError::NotFound)
}

fn validate_adopt_input(input: &AdoptLinkInput) -> Result<(), RelayError> {
    if !matches!(input.client_mode.as_str(), "forward" | "peer") {
        return Err(RelayError::Validation(
            "client_mode 只能是 forward 或 peer".into(),
        ));
    }
    if input.client_mode == "forward" && input.forward_port.is_none() {
        return Err(RelayError::Validation(
            "forward 模式必须提供 forward_port".into(),
        ));
    }
    let ports = [
        Some(input.wg_port),
        input.forward_port,
        Some(input.landing_port),
    ]
    .into_iter()
    .flatten();
    for p in ports {
        if !(1..=65535).contains(&p) {
            return Err(RelayError::Validation("端口必须在 1-65535".into()));
        }
    }
    if !is_valid_subnet(&input.subnet) {
        return Err(RelayError::Validation(
            "网段须为合法网段（形如 10.9.0.0/24，地址取网络地址）".into(),
        ));
    }
    if input.client_mode == "peer" && !is_device_subnet(&input.subnet) {
        return Err(RelayError::Validation(
            "WG 对等模式须用 x.y.z.0/24 网段（设备地址按末段分配）；点对点小网段的链路请选端口转发模式".into(),
        ));
    }
    Ok(())
}

/// 网段格式校验：合法 IPv4 网段（地址须为网络地址，前缀 8-32）。
///
/// **不再限定 10.9.x.0/24**：接管的对象是别人手工搭的链路，网段是人家的
/// （真机 203.0.113.127 ↔ 203.0.113.17 用的是 10.66.66.0/30）。只认 10.9.x
/// 会让「两端已握手，可接管」的链路必然卡在格式校验上——接管成了空头承诺。
/// 本工具自建链路的分配仍从 10.9.x.0/24 起（见 `relay::config::alloc_subnet`），形态不变。
fn is_valid_subnet(s: &str) -> bool {
    let Some((addr, prefix)) = s.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    (8..=32).contains(&prefix) && network_cidr(addr, prefix).as_deref() == Some(s)
}

/// 能否挂设备（WG 对等模式）：必须 x.y.z.0/24。
/// 设备地址由 [`crate::relay::config::alloc_peer_ip`] 按「网段前缀 + 末段编号」分配
/// （.3、.4…），只有 /24 形态成立。点对点小网段（手工链路的 /30 等）接管没问题，
/// 但挂设备会算出网段外的地址——在接管时报清楚，别等下发 conf 才炸。
fn is_device_subnet(s: &str) -> bool {
    s.ends_with(".0/24")
}

pub fn list_relay_rules(state: &AppState) -> Result<Vec<RelayRule>, RelayError> {
    list_relay_rules_inner(&state.db).map_err(|e| RelayError::Db(e.to_string()))
}

/// 节点可挂的中转规则（纯函数）：规则须已建成（imported/built 且 converged），且
/// **落地端口与节点服务端口一致**——转发是端口对端口（如 4443→8388 只服务 ss：
/// vmess 挂它会送错端到 ssserver，Delay 必 503）。多条命中取第一条（同端口场景）
pub fn select_rule_for_node(node_port: i64, rules: &[RelayRule]) -> Option<RelayRule> {
    rules
        .iter()
        .find(|r| {
            r.converged
                && (r.status == "imported" || r.status == "built")
                && r.landing_port == node_port
        })
        .cloned()
}

/// 一条链路给节点的路由目标 `(addr, port)`：模式 A 指中转机公网入口，模式 B 指
/// WG 内网口。链路不可用（未建成 / 未收敛）→ None —— 判据与
/// [`select_rule_for_node`] 完全一致，两处漂移会让「挂得上」与「拨得通」对不上。
pub fn route_target(rule: &RelayRule, relay_host: &str) -> Option<(String, i64)> {
    if !rule.converged || !matches!(rule.status.as_str(), "imported" | "built") {
        return None;
    }
    Some(if rule.client_mode == "forward" {
        (
            relay_host.to_string(),
            rule.forward_port.unwrap_or(rule.landing_port),
        )
    } else {
        (rule.landing_wg_ip.clone(), rule.landing_port)
    })
}

/// 节点经该链路出网时的落点 `(addr, port)`：整机转发用节点自己的端口号，
/// 其余沿用 [`route_target`]。链路不可用 → None。
pub fn route_for(rule: &RelayRule, relay_host: &str, node_port: i64) -> Option<(String, i64)> {
    if rule.forward_mode == "same_port" {
        let (addr, _) = route_target(rule, relay_host)?;
        return Some((addr, node_port));
    }
    route_target(rule, relay_host)
}

/// 本次建链请求的转发粒度：**请求里给了以请求为准**，没给则沿用已有规则的粒度、
/// 新建才回落 `'single'`。
///
/// 为什么不能直接 `unwrap_or("single")`：重试按钮、老前端、任何不带该字段的调用方
/// 都会把一条已建成的 same_port 链路降级成 single——2026-09-18 实测正是这样把
/// 刚建好的整机转发打回原状，而 DNAT 还留着，链路变成半拉子。
pub fn resolve_forward_mode(existing: Option<&str>, requested: Option<&str>) -> String {
    match requested {
        Some(m) => m.to_string(),
        None => existing.unwrap_or("single").to_string(),
    }
}

/// 这条链路该对落地机上的**哪些节点**动手（收敛、改指共用）。
///
/// `same_port` 取该宿主的全部自建节点：改指走的是节点自己的端口号，而
/// `find_for_landing_server` 按「落地端口」过滤——整机转发下落地端口只是其中一个节点，
/// 用它会把另外几个协议漏掉（实测：112 的 4443/8443 没被改指，依旧直连打不通）。
/// 宿主判定走 [`crate::domain::direct_addr`]，节点改指后 `addr` 已变成中转机，
/// 只有 `params_json.direct.addr` 还留着落地机地址。
pub fn landing_nodes_for_rule(
    conn: &Connection,
    landing: &crate::domain::Server,
    rule: &RelayRule,
    creds: &crate::crypto::CredentialService,
) -> rusqlite::Result<Vec<crate::domain::Node>> {
    if rule.forward_mode == "same_port" {
        repos::nodes::find_self_built_by_server(conn, landing, creds)
    } else {
        repos::nodes::find_for_landing_server(conn, landing, rule.landing_port, creds)
    }
}

/// 节点挂不上中转时**具体缺什么**（部署结果与节点页共用）。
///
/// 为什么要写这么细：原来只有一句「暂无对应的中转入口」，用户看到它得自己反推是端口
/// 不匹配、链路没收敛、还是模式选错。2026-09-18 真机上正是这个含糊把排查拖了很久。
pub fn explain_missing_route(landing_rules: &[RelayRule], node_port: i64) -> String {
    if landing_rules.is_empty() {
        return format!(
            "这台落地机还没有中转链路：节点（端口 {node_port}）当前是直连，直连不通时无从转发"
        );
    }
    if let Some(peer) = landing_rules.iter().find(|r| r.client_mode == "peer") {
        return format!(
            "该落地机现有链路是模式 B（WG 对等，#{}）：它给设备接入用，不转发端口——\
             节点（端口 {node_port}）要中转需要一条以 {node_port} 为落地端口的模式 A 链路",
            peer.id
        );
    }
    let unusable = |r: &RelayRule| !matches!(r.status.as_str(), "imported" | "built");
    if landing_rules.iter().all(unusable) {
        return format!(
            "该落地机的链路还没建成（当前状态 {}）",
            landing_rules[0].status
        );
    }
    if landing_rules.iter().all(|r| !r.converged) {
        return format!(
            "该落地机的链路尚未收敛（落地机服务还在公网监听）：节点（端口 {node_port}）不会走中转，\
             重新建链时勾选「收敛」即可"
        );
    }
    if landing_rules.iter().any(|r| r.forward_mode == "same_port") {
        return format!(
            "该落地机的整机转发链路还没就绪（状态 {}）",
            landing_rules[0].status
        );
    }
    let covered: Vec<String> = landing_rules
        .iter()
        .map(|r| r.landing_port.to_string())
        .collect();
    format!(
        "该落地机已有链路只覆盖落地端口 {}，本节点是 {}：需要一条以 {} 为落地端口的模式 A 链路",
        covered.join("/"),
        node_port,
        node_port
    )
}

/// 部署后自动挂载的逐节点结果（节点名 / 是否挂上 / 挂上给入口、没挂上给原因）。
pub struct AttachOutcome {
    pub node: String,
    pub attached: bool,
    pub detail: String,
}

/// 只绑 UDP 的协议：Hysteria2 / TUIC。转发式中转只下发 TCP DNAT，承载不了它们。
fn protocol_is_udp(protocol: &str) -> bool {
    matches!(protocol, "hysteria2" | "tuic")
}

/// 部署写回后把刚落库的节点挂到该落地机可用的中转链路上（「节点后于建链部署」的补挂）。
///
/// 为什么必须有这一步：建链只在**当时**把节点改指（`build.rs` 第 9 步），之后部署上来的
/// 节点 `relay_rule_id` 恒为 None，客户端便按直连拨号——落地机直连不通时表现为测速 504，
/// 而节点行上一点线索都没有。挂不上的一律带原因返回，不静默。
pub fn attach_deployed_nodes(
    conn: &Connection,
    landing: &Server,
    deployed: &[(String, i64)],
    creds: &crate::crypto::CredentialService,
) -> Result<Vec<AttachOutcome>, RelayError> {
    let all = repos::relay::list(conn).map_err(|e| RelayError::Db(e.to_string()))?;
    let servers = repos::servers::list(conn).map_err(|e| RelayError::Db(e.to_string()))?;
    let landing_rules: Vec<RelayRule> = all
        .into_iter()
        .filter(|r| r.landing_server_id == landing.id)
        .collect();
    let mut out = Vec::new();
    for (protocol, port) in deployed {
        let node_name = format!("{}-{}", landing.name, protocol);
        let usable = |r: &RelayRule| {
            r.converged
                && matches!(r.status.as_str(), "imported" | "built")
                && (r.forward_mode == "same_port" || r.landing_port == *port)
        };
        let hit = landing_rules.iter().find(|r| usable(r)).and_then(|r| {
            let relay_host = servers
                .iter()
                .find(|s| s.id == r.relay_server_id)
                .map(|s| s.host.clone())?;
            route_for(r, &relay_host, *port).map(|t| (r, t))
        });
        match hit {
            // 转发式中转只下发 TCP DNAT（`-p tcp`），UDP 协议挂上去只会一直超时。
            // 历史误挂要顺手清掉：2053 这类端口号恰好与 TUIC 相同，曾把 tuic 挂到
            // 一条只能过 TCP 的扩展链路上，摘要却写「已挂上」。
            Some((r, _)) if protocol_is_udp(protocol) && r.client_mode == "forward" => {
                let nodes = repos::nodes::find_for_landing_server(conn, landing, *port, creds)
                    .map_err(|e| RelayError::Db(e.to_string()))?;
                for n in nodes {
                    if n.relay_rule_id == Some(r.id) {
                        repos::nodes::set_relay(conn, &n.id, None)
                            .map_err(|e| RelayError::Db(e.to_string()))?;
                    }
                }
                out.push(AttachOutcome {
                    node: node_name,
                    attached: false,
                    detail: format!(
                        "{protocol} 是 UDP 协议，转发式中转只承载 TCP（链路 #{}），保持直连",
                        r.id
                    ),
                });
            }
            Some((r, (addr, target_port))) => {
                let nodes = repos::nodes::find_for_landing_server(conn, landing, *port, creds)
                    .map_err(|e| RelayError::Db(e.to_string()))?;
                for n in nodes {
                    // 只落标记，不改地址（同 reroute_landing_nodes 的说明）：改写 addr
                    // 会让 find_for_landing_server 下次部署找不到这个节点，转而新建一个重名节点。
                    repos::nodes::set_relay(conn, &n.id, Some(r.id))
                        .map_err(|e| RelayError::Db(e.to_string()))?;
                }
                out.push(AttachOutcome {
                    node: node_name,
                    attached: true,
                    detail: format!("经 {addr}:{target_port}（链路 #{}）", r.id),
                });
            }
            None => out.push(AttachOutcome {
                node: node_name,
                attached: false,
                detail: explain_missing_route(&landing_rules, *port),
            }),
        }
    }
    Ok(out)
}

/// 库内读（供无 AppState 的调用方：合并配置重建/导出缓存等）。
pub fn list_relay_rules_inner(db: &Db) -> rusqlite::Result<Vec<RelayRule>> {
    repos::relay::list(&db.lock())
}

/// 删除规则行：仅 status ∈ {not-built, failed}；built/imported 须先拆链。
/// failed 为建链半成品（允许直接删行重置；服务器残留配置不清理——
/// 若半成品写入了服务器配置，应先走拆链清理）。
pub fn delete_link_rule(state: &AppState, rule_id: i64) -> Result<(), RelayError> {
    let conn = state.db.lock();
    let rule = repos::relay::get(&conn, rule_id)
        .map_err(|e| RelayError::Db(e.to_string()))?
        .ok_or(RelayError::NotFound)?;
    if !matches!(rule.status.as_str(), "not-built" | "failed") {
        return Err(RelayError::LinkBuilt);
    }
    repos::relay::delete(&conn, rule_id).map_err(|e| RelayError::Db(e.to_string()))?;
    Ok(())
}

/// 链路状态：两端 transfer + latest-handshakes（轻量查询，不走队列）。
/// 单端连接失败视为该端不可达（字段保持默认，不置整体失败）；已连接后的命令失败
/// 置 ok=false + 脱敏错误文本，不抛错。
pub async fn refresh_link_status(state: &AppState, rule_id: i64) -> Result<LinkStatus, RelayError> {
    let sem = &STATUS_SEM;
    let _permit = sem
        .acquire()
        .await
        .map_err(|e| RelayError::Db(e.to_string()))?;
    let rule = {
        let conn = state.db.lock();
        repos::relay::get(&conn, rule_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?
    };
    let mut st = LinkStatus {
        ok: true,
        relay_handshake_secs_ago: None,
        landing_handshake_secs_ago: None,
        relay_rx_bytes: 0,
        relay_tx_bytes: 0,
        landing_rx_bytes: 0,
        landing_tx_bytes: 0,
        checked_at: chrono::Utc::now().to_rfc3339(),
        error: None,
    };
    let relay = load_auth(state, rule.relay_server_id).map_err(map_servers_error)?;
    let landing = load_auth(state, rule.landing_server_id).map_err(map_servers_error)?;
    // 各端按对端公钥定位 peer（历史行公钥 NULL 时回退 wg0 聚合）
    let (relay_res, landing_res) = tokio::join!(
        query_side(&relay.1, &relay.0, rule.landing_pubkey.as_deref()),
        query_side(&landing.1, &landing.0, rule.relay_pubkey.as_deref()),
    );
    let mut relay_inet: Option<String> = None;
    let mut landing_inet: Option<String> = None;
    match relay_res {
        Ok(Some((hs, rx, tx, inet))) => {
            st.relay_handshake_secs_ago = hs;
            st.relay_rx_bytes = rx;
            st.relay_tx_bytes = tx;
            relay_inet = inet;
        }
        Ok(None) => {}
        Err(e) => {
            st.ok = false;
            st.error = Some(e);
        }
    }
    match landing_res {
        Ok(Some((hs, rx, tx, inet))) => {
            st.landing_handshake_secs_ago = hs;
            st.landing_rx_bytes = rx;
            st.landing_tx_bytes = tx;
            landing_inet = inet;
        }
        Ok(None) => {}
        Err(e) => {
            st.ok = false;
            st.error = Some(e);
        }
    }
    // P5.1 静默自愈：imported 规则且真实地址与库不符 → 顺手修正（老板点一次
    // 「刷新状态」即修好，无需重新接管）；built 规则由 App 生成配置，合成值
    // 自洽——不动。COALESCE 语义：只更新读到且不符的侧。
    if rule.status == "imported" {
        let fix_relay = relay_inet
            .as_ref()
            .filter(|ip| ip.as_str() != rule.relay_wg_ip)
            .map(String::as_str);
        let fix_landing = landing_inet
            .as_ref()
            .filter(|ip| ip.as_str() != rule.landing_wg_ip)
            .map(String::as_str);
        if fix_relay.is_some() || fix_landing.is_some() {
            repos::relay::set_wg_ips(&state.db.lock(), rule.id, fix_relay, fix_landing)
                .map_err(|e| RelayError::Db(e.to_string()))?;
        }
    }
    Ok(st)
}

/// 单端状态查询：连接失败 → Ok(None)（该端不可达）；已连接后命令失败 → Err。
/// 接口枚举后按对端公钥定位 peer（expected_pubkey=None 的历史行回退 wg0 聚合）；
/// 握手秒差 = now - 该 peer 握手 epoch，负值（时钟偏斜）按 None 处理。
/// 返回第 4 元组为命中接口真实 IPv4 地址（P5.1 静默自愈用；读不到 → None）。
async fn query_side(
    srv: &Server,
    auth: &AuthMaterial,
    expected_pubkey: Option<&str>,
) -> Result<Option<(Option<i64>, i64, i64, Option<String>)>, String> {
    let mut sess =
        match SshSession::connect(&srv.host, srv.port as u16, &srv.user, auth, CONNECT_TIMEOUT)
            .await
        {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
    let ifaces_out = sess
        .run("状态", "wg show interfaces", STATUS_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if ifaces_out.exit_code != 0 {
        return Err("wg interfaces 命令失败".into());
    }
    // 远端输出，进命令前过白名单（不留则当它不存在——比拿它拼命令安全）
    let ifaces: Vec<String> = ifaces_out
        .stdout
        .split_whitespace()
        .filter(|n| crate::relay::build::valid_iface_name(n))
        .map(str::to_string)
        .collect();
    if ifaces.is_empty() {
        return Err("未检测到 WireGuard 接口".into());
    }
    // 命中接口：按对端公钥找 peer；无公钥历史行回退 wg0（本工具建链固定 wg0）
    let target = match expected_pubkey {
        Some(pk) => {
            let mut found = None;
            for iface in &ifaces {
                let hs = sess
                    .run(
                        "状态",
                        &format!("wg show {iface} latest-handshakes"),
                        STATUS_TIMEOUT,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                if hs.exit_code != 0 {
                    continue;
                }
                if parse_latest_handshakes(&hs.stdout).contains_key(pk) {
                    found = Some(iface.clone());
                    break;
                }
            }
            found.ok_or_else(|| "未在对端 peer 列表中找到目标公钥".to_string())?
        }
        None => ifaces
            .iter()
            .find(|i| *i == WG_IFACE)
            .or(ifaces.first())
            .unwrap()
            .clone(),
    };
    let transfer = sess
        .run(
            "状态",
            &format!("wg show {target} transfer"),
            STATUS_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;
    if transfer.exit_code != 0 {
        return Err("wg transfer 命令失败".into());
    }
    let hs = sess
        .run(
            "状态",
            &format!("wg show {target} latest-handshakes"),
            STATUS_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;
    if hs.exit_code != 0 {
        return Err("wg latest-handshakes 命令失败".into());
    }
    // P5.1：同一接口回读真实 IPv4 地址（refresh 静默自愈用；失败 → None 不报错）
    let inet = match sess
        .run("状态", &format!("ip -o addr show {target}"), STATUS_TIMEOUT)
        .await
    {
        Ok(out) if out.exit_code == 0 => parse_iface_inet(&out.stdout),
        _ => None,
    };
    let transfer_map = parse_transfer(&transfer.stdout);
    let hs_map = parse_latest_handshakes(&hs.stdout);
    // 按对端公钥取该 peer 的握手与流量；历史行（无公钥）沿用多 peer 聚合（rx 最大的行为主方向）
    let latest_epoch = expected_pubkey
        .and_then(|pk| hs_map.get(pk).copied())
        .unwrap_or_else(|| hs_map.values().copied().max().unwrap_or(0));
    let (rx, tx) = expected_pubkey
        .and_then(|pk| transfer_map.get(pk).copied())
        .unwrap_or_else(|| {
            transfer_map
                .values()
                .max_by_key(|(r, _)| *r)
                .copied()
                .unwrap_or((0, 0))
        });
    let secs_ago = if latest_epoch > 0 {
        let diff = chrono::Utc::now().timestamp() - latest_epoch;
        if diff >= 0 {
            Some(diff)
        } else {
            None
        }
    } else {
        None
    };
    Ok(Some((secs_ago, rx, tx, inet)))
}

/// 解析 `ip -o addr show <iface>` 输出：取第一个 inet（IPv4）行的地址（去 /N），
/// 忽略 inet6 与 link 行；无 inet → None。行格式如
/// `3: wg1    inet 10.9.0.2/24 brd 10.9.0.255 scope global wg1`。
/// 多行输入（`ip -o addr show | grep wg`）同样适用：逐行找第一个 inet token。
pub fn parse_iface_inet(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let pos = tokens.iter().position(|t| *t == "inet")?;
        let addr = tokens.get(pos + 1)?.split('/').next().unwrap_or("");
        (!addr.is_empty()).then(|| addr.to_string())
    })
}

/// 解析 `ip -o addr show <iface>` 的首个 inet 行 → (地址, 前缀长度)。
/// 链路网段的权威定义在这里：`Address = 10.66.66.1/30` 就是这条隧道。
/// 从 peer 的 AllowedIPs 反推是猜——手工链路常把它写成默认路由
/// （真机实测中转侧 `AllowedIPs = 0.0.0.0/0` 包住整个出口），里面根本没有隧道网段。
pub fn parse_iface_cidr(output: &str) -> Option<(String, u8)> {
    output.lines().find_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let pos = tokens.iter().position(|t| *t == "inet")?;
        let (addr, prefix) = tokens.get(pos + 1)?.split_once('/')?;
        let prefix: u8 = prefix.parse().ok()?;
        (!addr.is_empty() && prefix <= 32).then(|| (addr.to_string(), prefix))
    })
}

/// 地址 + 前缀 → 网络地址 CIDR（`10.66.66.1/30` → `10.66.66.0/30`）。
/// 主机位非 0 的输入归一化成网络地址；地址不合法/前缀越界返回 None。
pub fn network_cidr(addr: &str, prefix: u8) -> Option<String> {
    if prefix > 32 {
        return None;
    }
    let octets: Vec<u8> = addr
        .split('.')
        .map(str::parse::<u8>)
        .collect::<Result<_, _>>()
        .ok()?;
    if octets.len() != 4 {
        return None;
    }
    let ip = u32::from_be_bytes([octets[0], octets[1], octets[2], octets[3]]);
    // prefix=0 时移位 32 位是 UB（debug 下 panic），单独短路
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let [a, b, c, d] = (ip & mask).to_be_bytes();
    Some(format!("{a}.{b}.{c}.{d}/{prefix}"))
}

// ---------- 解析纯函数（Task 4 建链执行器复用） ----------

/// 解析 `wg show <iface> dump` 或 `allowed-ips` 输出的 peer 行：每行 \t 分割，
/// 首字段公钥、第 4 字段（索引 3）allowed_ips；跳过无 tab 行。
/// 探测路径只用 allowed-ips（dump 首行含本端私钥，不得过日志/缓冲区）。
pub fn parse_wg_dump(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter(|l| l.contains('\t'))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let allowed = f.get(3).copied().unwrap_or("").to_string();
            (f[0].to_string(), allowed)
        })
        .collect()
}

/// 解析 `wg show wg0 latest-handshakes`：key<TAB>epoch 行 → map（epoch 解析失败的行跳过）。
pub fn parse_latest_handshakes(stdout: &str) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let mut it = line.split('\t');
        let (Some(k), Some(e)) = (it.next(), it.next()) else {
            continue;
        };
        if let Ok(epoch) = e.trim().parse::<i64>() {
            map.insert(k.to_string(), epoch);
        }
    }
    map
}

/// 解析 `wg show wg0 listen-port`：首行端口，无输出/非数字返回 None。
pub fn parse_listen_port(stdout: &str) -> Option<i64> {
    stdout.lines().next()?.trim().parse().ok()
}

/// 解析 `wg show wg0 transfer`：key<TAB>rx<TAB>tx 行 → map。
pub fn parse_transfer(stdout: &str) -> HashMap<String, (i64, i64)> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let mut it = line.split('\t');
        let (Some(k), Some(rx), Some(tx)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if let (Ok(r), Ok(t)) = (rx.trim().parse::<i64>(), tx.trim().parse::<i64>()) {
            map.insert(k.to_string(), (r, t));
        }
    }
    map
}

/// 解析 iptables DNAT 回读 → 转发对表（入口端口 / 目标 IP / 目标端口）。
/// 只认同时含 `--dport ` 与 `--to-destination ` 的行；egress_if 非空时须含 `-i <iface>`。
///
/// 两个端口都要：`--dport 4443 ... --to-destination 10.66.66.2:8388` 里，
/// 前半是客户端连的中转口（forward_port），后半是落地机自己的服务口
/// （landing_port）。接管表单的「落地端口」就靠它自动带出来。
pub fn parse_dnat_pairs(iptables_stdout: &str, egress_if: &str) -> Vec<DnatPair> {
    iptables_stdout
        .lines()
        .filter_map(|line| {
            if !line.contains("--dport ") || !line.contains("--to-destination ") {
                return None;
            }
            if !egress_if.is_empty() && !line.contains(&format!("-i {egress_if}")) {
                return None;
            }
            let forward_port: i64 = line
                .split("--dport ")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            // rsplit_once 而非 split_once：IPv6 目标是 `[::1]:80` 形态
            let target = line
                .split("--to-destination ")
                .nth(1)?
                .split_whitespace()
                .next()?;
            let (target_ip, target_port) = target.rsplit_once(':')?;
            let target_port: i64 = target_port.parse().ok()?;
            Some(DnatPair {
                forward_port,
                target_ip: target_ip.to_string(),
                target_port,
            })
        })
        .collect()
}

/// 从 dump 原始输出取首个 `10.9.x.y/N` 令牌并派生网段 `10.9.x.0/24`。
fn parse_subnet_from_dump(dump_raw: &str) -> Option<String> {
    dump_raw.split_whitespace().find_map(|tok| {
        let prefix = tok.trim_end_matches("/32").trim_end_matches("/24");
        let mut parts = prefix.split('.');
        if parts.next() == Some("10") && parts.next() == Some("9") {
            if let Some(x) = parts.next() {
                if x.parse::<u8>().is_ok() {
                    return Some(format!("10.9.{x}.0/24"));
                }
            }
        }
        None
    })
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/relay/mods.rs"
));
