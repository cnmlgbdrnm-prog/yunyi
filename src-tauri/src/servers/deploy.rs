//! 落地机一键部署管道：探测→安装→配置→启动→写回。
//! 输入：DeployRequest（job_id/server_id/kind/port/reality_dest）；输出：进度事件与落库结果。
//! 命令序列白名单化：本文件是唯一构造远端命令的位置，可变参数经校验后拼接；
//! 远端文件写入走「echo base64 | base64 -d > 白名单路径」管道。
//! 取消为协作式：步骤边界 check_cancel + 单命令内 select 轮询（50ms）。
//! 依赖：ssh 适配器、servers::load_auth_parts、repos、config 缓存重建、注入式 SS 版本查询。

use crate::db::repos;
use crate::domain::{JobProgress, Node};
use crate::jobs::{DeployRequest, EmitFn, JobRequest, JobRunner};
use crate::servers::{load_auth_parts, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use uuid::Uuid;

pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
pub const CONFIG_TIMEOUT: Duration = Duration::from_secs(60);
/// 启动后的状态验证（is-active / 端口监听）：失败重试等场景下 30s 足够，
/// 与启动命令的 60s 配置超时区分开，避免验证阶段长时间卡死。
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// 远端写文件白名单路径（常量，禁止动态构造）；pub(crate)：relay::build 收敛复用。
pub(crate) const XRAY_CONFIG_PATH: &str = "/usr/local/etc/xray/config.json";
pub(crate) const SS_CONFIG_PATH: &str = "/etc/shadowsocks-rust/config.json";
const SS_UNIT_PATH: &str = "/etc/systemd/system/ssserver.service";

// 协议/命令/配置生成与既有配置解析集中在 deploy_spec；原 `crate::servers::deploy::*`
// 路径经此重导出保持不变（测试与 relay 等模块按旧路径引用）。
pub use crate::servers::deploy_spec::*;

/// 查看某台服务器**当前可用的**已部署协议，以 `nodes` 里的自建节点为准。
///
/// 为什么不再读最新部署摘要：摘要只记录最后一次 job 写回的协议，既会漏掉更早部署、
/// 仍在使用的协议（先补装 SS，再重部署三件套），也会把「配置已写、服务已起，但 job
/// 在写回前被取消」的协议当成没部署。节点行才是客户端真正能拨通的那份事实。
pub fn deployed_protocols(state: &Arc<AppState>, server_id: i64) -> Result<Vec<String>, String> {
    Ok(deployed_protocol_details(state, server_id)?
        .into_iter()
        .map(|d| d.protocol)
        .collect())
}

/// 已部署协议详情：白名单协议名 + 端口 + 完整变体（传输层/安全层/流控）。
///
/// 节点行里混着两类 `source=self-built`：
/// 1. 落地协议（本函数要返回的）；
/// 2. 上游住宅出口（同一台机器上的第二条 Xray 实例），它们也写成 self-built，
///    必须按 `upstreams.node_id` 排除，否则会把“上游出口”误报成“已部署 vless”。
///
/// 中转改写过 `addr` 的节点用 [`crate::domain::direct_addr`] 还原真实落地地址。
pub fn deployed_protocol_details(
    state: &Arc<AppState>,
    server_id: i64,
) -> Result<Vec<DeployedProtocolDetail>, String> {
    let conn = state.db.lock();
    let Some(server) = repos::servers::get(&conn, server_id).map_err(|e| e.to_string())? else {
        return Ok(vec![]);
    };
    let upstream_nodes: HashSet<String> = repos::upstreams::list_by_server(&conn, server_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|u| u.node_id)
        .collect();
    let mut details: Vec<DeployedProtocolDetail> = repos::nodes::list(&conn, &state.creds)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|n| n.source == "self-built")
        .filter(|n| is_supported_protocol(&n.protocol))
        .filter(|n| !upstream_nodes.contains(&n.id))
        .filter(|n| crate::domain::server_owns_addr(&server, &crate::domain::direct_addr(n)))
        .map(|n| {
            describe_deployed_protocol(
                &n.protocol,
                u16::try_from(n.port).unwrap_or_default(),
                &n.params_json,
            )
        })
        .collect();
    details.sort_by(|a, b| {
        protocol_order(&a.protocol)
            .cmp(&protocol_order(&b.protocol))
            .then(a.port.cmp(&b.port))
    });
    // 同一协议只保留端口最小的那条（重复部署会清旧行，这里只是防御历史脏数据）。
    details.dedup_by(|a, b| a.protocol == b.protocol);
    Ok(details)
}

/// 持久放行的端口清单 `(协议, 端口)`：部署管理的节点端口 + 上游出口监听端口。
///
/// 防火墙放行要覆盖的不只是「本次勾选的协议」：同一台机器上还可能有此前部署的
/// 多个入站（例如 ashura 的德国/新加坡/日本/洛杉矶/纽约/亚利桑那广播节点），
/// 它们同样需要持久放行。
///
/// 上游出口（`upstreams.listen_port`）由独立的 xray-upstream 实例监听，重启后
/// iptables 规则同样会丢——所以即使它们不属于「部署管理」（见
/// [`deployed_protocol_details`] 的排除口径），端口也必须并进同一份放行清单。
/// 否则表现是主入站全通、所有广播/住宅出口节点超时（2026-10-23 ashura 重启后实测）。
///
/// 同一 `(协议, 端口)` 只返回一次。
pub fn firewall_ports(
    nodes: &[crate::domain::Node],
    upstreams: &[crate::domain::Upstream],
    server: &crate::domain::Server,
) -> Vec<(String, u16)> {
    let upstream_ids: HashSet<&str> = upstreams
        .iter()
        .filter_map(|u| u.node_id.as_deref())
        .collect();
    let mut out: Vec<(String, u16)> = Vec::new();
    let mut push = |protocol: &str, port: i64| {
        let Ok(port) = u16::try_from(port) else {
            return;
        };
        if !out.iter().any(|(p, q)| p == protocol && *q == port) {
            out.push((protocol.to_string(), port));
        }
    };
    // 上游出口节点在 xray-upstream 里一律是 vless/REALITY 入站（出站才是住宅协议）。
    for u in upstreams {
        push("vless", u.listen_port);
    }
    for n in nodes {
        if n.source != "self-built"
            || !is_supported_protocol(&n.protocol)
            || upstream_ids.contains(n.id.as_str())
            || !crate::domain::server_owns_addr(server, &crate::domain::direct_addr(n))
        {
            continue;
        }
        push(&n.protocol, n.port);
    }
    out
}

/// 部署「复用既有入站」时可选的节点（按协议索引，每协议一条）。
///
/// **必须排除上游出口节点**：同一个协议名（vless）在两类节点上重名，而上游节点
/// 的 sni/公钥属于各自的 xray-upstream 入口。混进来会让主入站复用错公钥或拿错
/// 伪装域名——重复 key 时旧代码的 `collect::<HashMap>` 由哈希顺序决定谁留下，
/// 表现为时好时坏、极难复现。这里统一用「先到先得」且显式排除，行为确定。
pub fn deploy_managed_nodes(
    nodes: Vec<crate::domain::Node>,
    server: &crate::domain::Server,
    upstream_node_ids: &HashSet<String>,
) -> HashMap<String, crate::domain::Node> {
    let mut out = HashMap::new();
    for n in nodes {
        if n.source != "self-built"
            || upstream_node_ids.contains(&n.id)
            || !crate::domain::server_owns_addr(server, &crate::domain::direct_addr(&n))
        {
            continue;
        }
        out.entry(n.protocol.clone()).or_insert(n);
    }
    out
}

/// 从库里收集 [`firewall_ports`] 的输入（读库失败按空表处理：放行是增益，
/// 不能因为读不到节点就让整次部署失败）。
fn deployed_node_ports(
    state: &Arc<AppState>,
    server: &crate::domain::Server,
) -> Vec<(String, u16)> {
    let conn = state.db.lock();
    let upstreams = repos::upstreams::list_by_server(&conn, server.id).unwrap_or_default();
    let nodes = repos::nodes::list(&conn, &state.creds).unwrap_or_default();
    firewall_ports(&nodes, &upstreams, server)
}

/// 端口占用检查：true=端口已被监听（占用），false=空闲可部署。
///
/// 探测的套接字类型**随协议**：Hysteria2/TUIC 只绑 UDP，用 TCP 表探测会永远
/// 报「空闲」，既可能选到已被别的 UDP 服务占用的端口，也会让验证永远失败。
async fn probe_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    protocol: &str,
    port: u16,
) -> Result<bool, String> {
    Ok(run_cmd(
        sess,
        cancel,
        "探测",
        &listen_probe_cmd(protocol, port),
        PROBE_TIMEOUT,
    )
    .await?
    .exit_code
        == 0)
}

/// 从既有入站的 REALITY 私钥推导客户端应使用的公钥。
///
/// 服务器配置里只有私钥，公钥必须与它成对；旧节点参数里的公钥可能是错的——
/// 2026-10-23 ashura 实测：入站私钥推导出 `ZE2j…`，节点行却写着 `HjKX…`（来自
/// 一条已被合并的重复行），vless 直连与中转全部 503，而 trojan 的密钥对是好的。
/// 能推导就以服务器私钥为准；只有旧版 xray 不支持 `x25519 -i` 时才由调用方退回
/// 节点参数（此时保持原有行为，不因推导失败而让部署失败）。
async fn derive_reality_public_key(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    private_key: &str,
) -> Option<String> {
    // 私钥是 base64url；先卡字符集，避免把任意内容拼进远端命令
    if private_key.is_empty()
        || !private_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let out = run_cmd_ok(
        sess,
        cancel,
        "配置",
        &format!("xray x25519 -i {private_key}"),
        CONFIG_TIMEOUT,
    )
    .await
    .ok()?;
    parse_x25519_keys(&out.stdout).ok().map(|(_, pub_k)| pub_k)
}

/// 节点名是否属于本次部署要写回的这条线。
///
/// 除了同名，还要认 `{base} #N`：应用启动时的重名自愈
/// （`nodes::ensure_unique_names`）会把历史重复行改名成 ` #2`、` #3`……，
/// 它们正是上一次「按地址删不到」留下的副本；不认识这个后缀就会把它们
/// 当成用户自己的另一条节点，永远清不掉。用户改过的名字不带这个后缀，不受影响。
pub(crate) fn matches_deployed_name(candidate: &str, base: &str) -> bool {
    if candidate == base {
        return true;
    }
    candidate
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix(" #"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// 从 base 起探测空闲端口(逐 +1),最多 20 个连续候选;全占报错。
async fn allocate_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    protocol: &str,
    base: u16,
) -> Result<u16, String> {
    for n in 0..21u16 {
        let cand = base.saturating_add(n);
        if !probe_port(sess, cancel, protocol, cand).await? {
            return Ok(cand);
        }
    }
    Err(format!("端口 {base} 起 20 个连续端口全被占用"))
}

/// 读取远端已部署配置中的端口表（重部署复用：云侧安全组/防火墙通常只放行首次部署
/// 端口，漂移会直接打不通——实测 trojan 4443→4444 被云商策略性封禁后客户端 SYN 超时）。
/// 文件不存在/退出码非 0 → 无既有端口（首次部署）。
async fn read_existing_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    need_xray: bool,
    need_ss: bool,
    need_singbox: bool,
) -> Result<ExistingRemote, String> {
    let mut ports: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    let mut xray_inbounds: std::collections::HashMap<u16, XrayInboundEntry> =
        std::collections::HashMap::new();
    let mut ss_password: Option<String> = None;
    let mut singbox_hy2_pw: Option<String> = None;
    let mut singbox_tuic_uuid: Option<String> = None;
    let mut singbox_tuic_pw: Option<String> = None;
    if need_xray {
        // 读失败即中止（`?`），理由同下面的 sing-box 分支：退化成「没有既有部署」
        // 就会重新分配端口、重新生成 reality 私钥并整份重渲染 config.json，
        // 而客户端手里还是旧参数；远端失败还会被 `2>/dev/null` 吞掉。
        let existing =
            read_remote_file(sess, cancel, "探测", XRAY_CONFIG_PATH, PROBE_TIMEOUT).await?;
        if let RemoteFile::Content(c) = &existing {
            for (proto, port) in parse_xray_inbound_ports(c) {
                ports.insert(proto, port);
            }
            // 同一份配置里顺带还原凭据：重复部署靠它复用而不是重新生成（见 run_deploy）
            xray_inbounds = parse_xray_inbounds(c);
        }
    }
    if need_ss {
        let existing =
            read_remote_file(sess, cancel, "探测", SS_CONFIG_PATH, PROBE_TIMEOUT).await?;
        if let RemoteFile::Content(c) = &existing {
            if let Some(p) = parse_ss_port(c) {
                ports.insert("ss".into(), p);
            }
            ss_password = parse_ss_password(c);
        }
    }
    if need_singbox {
        // 读失败即中止（`?`）：读不回来就会按「首次部署」重新生成凭据与端口，
        // 而客户端手里还是旧凭据——服务端新密码 + 客户端旧密码 = 静默变砖。
        let existing =
            read_remote_file(sess, cancel, "探测", SINGBOX_CONFIG_PATH, PROBE_TIMEOUT).await?;
        if let RemoteFile::Content(c) = &existing {
            for (proto, port) in parse_singbox_ports(c) {
                ports.insert(proto, port);
            }
            let (hy2, tuic) = parse_singbox_creds(c);
            singbox_hy2_pw = hy2;
            if let Some((uuid, pw)) = tuic {
                singbox_tuic_uuid = Some(uuid);
                singbox_tuic_pw = Some(pw);
            }
        }
    }
    Ok(ExistingRemote {
        ports,
        xray_inbounds,
        ss_password,
        singbox_hy2_pw,
        singbox_tuic_uuid,
        singbox_tuic_pw,
    })
}

/// 远端既有部署的明细：端口表（重部署复用端口）+ 凭据（重部署复用凭据，不轮换）。
///
/// 两件事都要读同一批文件，所以一次读完一起带回来——分两次读会让「探测」阶段
/// 多跑 2~3 条 SSH 命令，也会让两份数据有可能来自不同时刻的配置。
struct ExistingRemote {
    ports: std::collections::HashMap<String, u16>,
    /// 按端口索引的 xray 入站（含 reality 私钥，见 [`parse_xray_inbounds`]）
    xray_inbounds: std::collections::HashMap<u16, XrayInboundEntry>,
    /// shadowsocks 既有密码（复用避免轮换）
    ss_password: Option<String>,
    /// sing-box hysteria2 既有密码（同上，缺失才新生成）
    singbox_hy2_pw: Option<String>,
    /// sing-box tuic 既有 uuid / 密码（同上）
    singbox_tuic_uuid: Option<String>,
    singbox_tuic_pw: Option<String>,
}

/// 生产 runner：按 JobRequest 分发给部署/建链/拆链/设备 peer 执行器。
/// state 经 OnceLock 注入：AppState 构造存在循环依赖（runner 需要 state，
/// state 需要 runner），worker 惰性启动保证首次 submit 前必已注入。
pub fn prod_runner(
    state: Arc<OnceLock<Arc<AppState>>>,
    http: reqwest::Client,
    ss_api_base: String,
    singbox_api_base: String,
) -> Arc<dyn JobRunner> {
    Arc::new(ProdRunner {
        state,
        http,
        ss_api_base,
        singbox_api_base,
    })
}

struct ProdRunner {
    state: Arc<OnceLock<Arc<AppState>>>,
    http: reqwest::Client,
    ss_api_base: String,
    singbox_api_base: String,
}

impl ProdRunner {
    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .get()
            .cloned()
            .ok_or_else(|| "应用状态未就绪".to_string())
    }
}

#[async_trait::async_trait]
impl JobRunner for ProdRunner {
    async fn run(
        &self,
        req: JobRequest,
        cancel: Arc<AtomicBool>,
        emit: EmitFn,
    ) -> Result<(), String> {
        let state = self.state()?;
        match req {
            JobRequest::Deploy(r) => self.run_deploy(&state, r, &cancel, &emit).await,
            JobRequest::Upstream(r) => {
                crate::servers::upstream::run_upstream_job(&state, &r, &cancel, &emit).await
            }
            JobRequest::Privacy(r) => {
                crate::servers::privacy::run_privacy_job(&state, &r, &cancel, &emit).await
            }
            JobRequest::PrivacyRemove(r) => {
                crate::servers::privacy::run_privacy_remove_job(&state, &r, &cancel, &emit).await
            }
            JobRequest::LinkBuild(r) => {
                crate::relay::build::run_build(&state, r, &cancel, &emit).await
            }
            JobRequest::LinkTeardown(r) => {
                crate::relay::build::run_teardown(&state, r, &cancel, &emit).await
            }
            JobRequest::PeerAdd(r) => {
                crate::relay::peers::run_peer_add(&state, r, &cancel, &emit).await
            }
            JobRequest::PeerRemove(r) => {
                crate::relay::peers::run_peer_remove(&state, r, &cancel, &emit).await
            }
            JobRequest::SpeedTest(r) => {
                crate::speed::run_speed_job(&state, r, cancel.clone(), emit.clone()).await
            }
            JobRequest::ServerSpeed(r) => {
                crate::server_speed::run_server_speed_job(state, r, cancel.clone(), emit.clone())
                    .await
            }
            JobRequest::Throughput(r) => {
                crate::server_speed::iperf::run_throughput_job(
                    &state,
                    r,
                    cancel.clone(),
                    emit.clone(),
                )
                .await
            }
        }
    }
}

/// 一次发布查询的结果：版本、文件名、以及**上游给的** sha256。
///
/// `sha256` 为 `None` 表示上游没给 digest（老发布或换了算法）：此时**不校验**，
/// 改为把实测哈希打出来让漂移可见——绝不塞一个恒真的校验冒充「钉住了」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub tag: String,
    pub asset: String,
    pub sha256: Option<String>,
}

/// 从发布资产的 `digest` 字段取 sha256 十六进制值。
/// GitHub 的形状是 `"digest": "sha256:<hex>"`；其它算法或缺失一律 None
/// （`sha256sum -c` 只认 sha256，拿别的算法去校验只会误报）。
fn parse_asset_digest(asset: &serde_json::Value) -> Option<String> {
    asset
        .get("digest")?
        .as_str()?
        .strip_prefix("sha256:")
        .map(|s| s.to_string())
}

/// 上游是否为发布资产提供构建来源证明（GitHub Artifact Attestation）。
///
/// 这是**实测结论**，不是推断（2026-09-27 实测）：
/// - sing-box：`{api}/attestations/sha256:<digest>` 返回 bundle 数组；
///   `gh attestation verify` 对真归档退出 0，对「篡改 1 字节 / 换个仓库 / 随机文件」
///   三种阴性样本一律退出 1——该端点确实在判别，不是恒真。
/// - shadowsocks-rust：同一端点对其真实资产 digest 返回 404，上游不发布。
///
/// 上游若变更发布流程（不再签 attestation），这条策略要跟着改；改错的方向是
/// 「本该中止却放过」，所以宁可让它响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// 上游发布来源证明：digest 上查不到即视为篡改信号，中止部署。
    Attested,
    /// 上游不发布来源证明：无从核验，如实记录，不假装已核验。
    NotPublished,
}

/// 构建来源核验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceOutcome {
    /// 查到 `count` 份 attestation：该 digest 由该仓库的 CI 产出。
    Confirmed { count: usize },
    /// 上游不发布来源证明，未做核验。
    NotPublished,
}

/// digest 查不到 attestation 时的统一错误：404 与 200+空数组是同一个结论。
fn missing_attestation_err(label: &str, sha: &str) -> String {
    format!(
        "{label} 的 digest {sha} 查不到构建来源证明（attestation），已中止部署：\
         该资产不是上游 CI 产出的，可能已被替换。\
         若上游已改为不发布 attestation，请更新 Provenance 策略。"
    )
}

/// 核验发布资产的构建来源，确认该 digest 是否有上游 CI 的 attestation。
///
/// **能挡什么**：偷到 GitHub token 的人能把木马换进 release——他改得了字节，
/// 也改得了 API 报的 digest，但**签不出 attestation**（attestation 由仓库 workflow
/// 的 OIDC 身份签出，PAT 伪造不了）。此时本函数对那个 digest 查不到 attestation，
/// 部署中止。这是现有链路唯一没覆盖的一段：digest 校验挡得住服务端网络上的 MITM
/// （预期值由本机经 SSH 送达），却挡不住「上传资产的人自己就是攻击者」。
///
/// **边界（不假装比实际更强）**：这不是完整 sigstore 验签——不验证 Fulcio 证书链、
/// 不验证 Rekor 包含证明、不校验签名，只确认 GitHub 的 attestations 端点对该
/// repo+digest 返回了非空 bundle 列表。完整验签需要 `sigstore` crate，而它硬依赖
/// `aws-lc-rs`（Windows 上要 NASM）——正是本项目为 russh 刻意移除的依赖
/// （见 Cargo.toml 里 `default-features = false, features = ["ring"]` 的说明）。
/// 因此本核验的信任锚是 GitHub API 本身，与「digest 也取自同一个 API」一致：
/// 没有引入新的信任假设，也没有比它更强。
pub async fn verify_provenance(
    http: &reqwest::Client,
    api_base: &str,
    label: &str,
    policy: Provenance,
    sha256: Option<&str>,
) -> Result<ProvenanceOutcome, String> {
    if policy == Provenance::NotPublished {
        return Ok(ProvenanceOutcome::NotPublished);
    }
    // 没有 digest 就无从核验。上游若一边给 attestation 一边不给 digest，
    // 不能默认「没事」——那正是「假装钉住了」。
    let Some(sha) = sha256 else {
        return Err(format!(
            "{label} 的发布资产没有 digest，无法核验构建来源，已中止部署"
        ));
    };
    let url = format!("{api_base}/attestations/sha256:{sha}");
    let resp = http
        .get(url)
        .header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("核验 {label} 构建来源失败（网络）: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(missing_attestation_err(label, sha));
    }
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("核验 {label} 构建来源失败（GitHub API 错误）: {e}"))?;
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("核验 {label} 构建来源失败（响应解析失败）: {e}"))?;
    // 200 + 空数组是「查得到但为空」的形状，与 404 是同一个结论：
    // 只判 status 会把这种情况误放行。
    let count = j["attestations"].as_array().map_or(0, |a| a.len());
    if count == 0 {
        return Err(missing_attestation_err(label, sha));
    }
    Ok(ProvenanceOutcome::Confirmed { count })
}

/// 查询 sing-box 最新发布资产名（api_base 注入：生产为 GitHub API，测试为 wiremock）。
/// 下载 URL 由调用方以固定前缀拼接；tag 与资产名拼入 URL 前必须过字符集白名单。
pub async fn latest_singbox_asset(
    http: &reqwest::Client,
    api_base: &str,
    target: &str,
) -> Result<ReleaseAsset, String> {
    let url = format!("{api_base}/releases/latest");
    let resp = http
        .get(url)
        .header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("查询 sing-box 最新版本失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API 错误: {e}"))?;
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API 响应解析失败: {e}"))?;
    let tag = j["tag_name"]
        .as_str()
        .ok_or("GitHub API 响应缺少 tag_name")?
        .to_string();
    // sing-box 发布资产名按平台而非 Rust 三元组命名（如 sing-box-1.11.11-linux-amd64.tar.gz）；
    // 把三原组映射到其平台后缀再匹配。
    let suffix = match target {
        "x86_64-unknown-linux-gnu" => "linux-amd64.tar.gz",
        "aarch64-unknown-linux-gnu" => "linux-arm64.tar.gz",
        t => return Err(format!("暂不支持的 sing-box 目标架构: {t}")),
    };
    let assets = j["assets"].as_array().ok_or("GitHub API 响应缺少 assets")?;
    let picked = assets
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
        .ok_or_else(|| format!("未找到 {suffix} 的发布资产"))?;
    let asset = picked["name"]
        .as_str()
        .ok_or("发布资产缺少 name")?
        .to_string();
    if !is_safe_url_fragment(&tag) || !is_safe_url_fragment(&asset) {
        return Err("资产名不合法".into());
    }
    Ok(ReleaseAsset {
        tag,
        asset,
        sha256: parse_asset_digest(picked),
    })
}

/// 查询 shadowsocks-rust 最新发布资产名（api_base 注入：生产为 GitHub API，测试为 wiremock）。
/// 下载 URL 由调用方以固定前缀拼接，本函数只解析 tag 与匹配 target 的资产名；
/// tag 与资产名拼入 URL 前必须过字符集白名单（字母数字点下划线连字符）。
pub async fn latest_ss_asset(
    http: &reqwest::Client,
    api_base: &str,
    target: &str,
) -> Result<ReleaseAsset, String> {
    let url = format!("{api_base}/releases/latest");
    let resp = http
        .get(url)
        .header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("查询 SS 最新版本失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API 错误: {e}"))?;
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API 响应解析失败: {e}"))?;
    let tag = j["tag_name"]
        .as_str()
        .ok_or("GitHub API 响应缺少 tag_name")?
        .to_string();
    let assets = j["assets"].as_array().ok_or("GitHub API 响应缺少 assets")?;
    let suffix = format!("{target}.tar.xz");
    let picked = assets
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(&suffix)))
        .ok_or_else(|| format!("未找到 {target} 的发布资产"))?;
    let asset = picked["name"]
        .as_str()
        .ok_or("发布资产缺少 name")?
        .to_string();
    // 拼 URL 前校验：非法字符（引号、空白等）会被注入到远端下载命令
    if !is_safe_url_fragment(&tag) || !is_safe_url_fragment(&asset) {
        return Err("资产名不合法".into());
    }
    Ok(ReleaseAsset {
        tag,
        asset,
        sha256: parse_asset_digest(picked),
    })
}

/// URL 片段字符集白名单：字母数字点下划线连字符，且非空。
fn is_safe_url_fragment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// 上游没给校验和时，把 `sha256sum` 实测出来的那一行记进部署进度。
///
/// 这是「不假装钉住了」的落地：`ReleaseAsset::sha256` 为 `None` 时下载命令里没有
/// 校验（见 [`binary_install_cmd`]），只能把实测哈希**如实记下来**——版本漂移
/// 至少留在部署记录里，而不是无声无息。已经校验过的情形什么都不做（校验失败时
/// 命令本身就失败了，根本走不到这里）。
fn emit_measured_hash(
    emit: &impl Fn(&str, &str, i64),
    archive: &str,
    rel: &ReleaseAsset,
    out: &crate::ssh::CommandOutput,
) {
    if rel.sha256.is_some() {
        return;
    }
    let measured = out
        .stdout
        .lines()
        .find(|l| l.contains(archive))
        .unwrap_or("（远端未回显 sha256sum 输出）")
        .trim();
    emit(
        "安装",
        &format!(
            "上游 {} 未提供校验和，实测 {}: {measured}",
            rel.tag, rel.asset
        ),
        46,
    );
}

/// 把**实际装上去的那个可执行文件**的实测 sha256 记进部署进度。
///
/// 这是 `scripts/fetch-tor.mjs` 那条「归档 + 解出来的 exe」双哈希链在这一侧的
/// 对应物：tor 那边的 exe 哈希是常量（版本写死），而这里的版本是运行时才定的
/// （`releases/latest`），钉不住预期值——那就把实测值留下来，至少让「服务器上装的
/// 到底是哪一份」查得到，而不是只有一个「装好了」。
/// 远端没回显时也要出声：静默通过等于这条留痕根本不存在。
fn emit_installed_bin_hash(
    emit: &impl Fn(&str, &str, i64),
    bin: &str,
    out: &crate::ssh::CommandOutput,
) {
    let measured = out
        .stdout
        .lines()
        .find(|l| l.contains(bin))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("（远端未回显 sha256sum 输出）");
    emit("安装", &format!("已安装 {bin} sha256: {measured}"), 48);
}

// 远端文件原语拆到 servers::remote_file（deploy.rs 已顶到单文件 1500 行上限）。
// 经此重导出，relay/privacy 按旧路径的引用保持不变。
pub(crate) use crate::servers::remote_file::{read_remote_file, write_remote_file, RemoteFile};

/// 取消感知的单命令执行：取消标志置位即中断（不等命令超时）。
/// pub(crate)：relay::build/peers 建链拆链与设备下发复用。
pub(crate) async fn run_cmd(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    cmd: &str,
    timeout: Duration,
) -> Result<crate::ssh::CommandOutput, String> {
    tokio::select! {
        out = sess.run(step, cmd, timeout) => out.map_err(|e| e.to_string()),
        _ = async {
            loop {
                if cancel.load(Ordering::SeqCst) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => Err("已取消".into()),
    }
}

/// 等服务进入 active 的等待预算：服务实际启动在毫秒级，这里给足余量只为扛住
/// 重启时的过渡态；超时再报错。
pub const ACTIVATE_BUDGET: Duration = Duration::from_secs(10);

/// 等服务进入 active，按**状态**判成败。
///
/// 为什么不能直接 `run_cmd_ok("systemctl is-active …")`：`is-active` 在服务没起来时
/// **本身就返回非零**（未激活 = 退出码 3），`run_cmd_ok` 会先一步把它当成「命令执行
/// 失败」抛出去——错误只剩一句「启动 执行失败(exit=3): 」加一片空白，而紧接着那句
/// 精心写的「xxx 服务未进入 active 状态: …」永远执行不到（2026-09-18 实测踩过）。
///
/// 分状态处理：`active` 成功；`failed` 是终态，再等也不会自己起来，立刻把状态报出去；
/// 其余（activating / inactive / 空）算过渡态，轮询到预算耗尽再报最后一次读到的状态。
pub(crate) async fn wait_service_active(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    unit: &str,
) -> Result<(), String> {
    let cmd = format!("systemctl is-active {unit}");
    let deadline = std::time::Instant::now() + ACTIVATE_BUDGET;
    let mut last = String::new();
    loop {
        // 用 run_cmd 而非 run_cmd_ok：未激活时非零退出是**预期内**的探测结果
        let out = run_cmd(sess, cancel, "启动", &cmd, VERIFY_TIMEOUT).await?;
        let state = out.stdout.trim().to_string();
        if state == "active" {
            return Ok(());
        }
        if state == "failed" {
            return Err(format!("{unit} 服务启动失败（systemctl：failed）"));
        }
        if !state.is_empty() {
            last = state;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{unit} 服务未进入 active 状态（systemctl：{}）",
                if last.is_empty() { "无输出" } else { &last }
            ));
        }
        // 取消感知的 200ms 间隔
        for _ in 0..4 {
            if cancel.load(Ordering::SeqCst) {
                return Err("已取消".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// 取消感知 + 要求退出码为 0。
/// pub(crate)：relay::build/peers 建链拆链与设备下发复用。
pub(crate) async fn run_cmd_ok(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    cmd: &str,
    timeout: Duration,
) -> Result<crate::ssh::CommandOutput, String> {
    tokio::select! {
        out = sess.run_ok(step, cmd, timeout) => out.map_err(|e| e.to_string()),
        _ = async {
            loop {
                if cancel.load(Ordering::SeqCst) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => Err("已取消".into()),
    }
}

impl ProdRunner {
    /// 部署主线。步骤：探测(5→20)→安装(30→45)→配置(55)→启动(80)→优化(88→90，仅勾选 BBR)
    /// →放行(92)→写回(95→100)。进度单调递增；全协议验证失败在「优化」之前中止，不发 95/100。
    /// 多协议：一次会话内按选中协议并行完成安装/配置/启动/验证；
    /// 单协议失败仅标记 failed_protocols，其余协议继续验证与写回（失败隔离）。
    async fn run_deploy(
        &self,
        state: &Arc<AppState>,
        req: DeployRequest,
        cancel: &Arc<AtomicBool>,
        emit: &EmitFn,
    ) -> Result<(), String> {
        let server = {
            let conn = state.db.lock();
            repos::servers::get(&conn, req.server_id)
                .map_err(|e| e.to_string())?
                .ok_or("服务器不存在")?
        };
        let need_xray = ["vless", "trojan", "vmess"]
            .iter()
            .any(|p| req.protocols.iter().any(|x| x == p));
        let need_ss = req.protocols.iter().any(|x| x == "ss");
        let need_singbox = ["hysteria2", "tuic"]
            .iter()
            .any(|p| req.protocols.iter().any(|x| x == p));
        // 出站地址族：与入口（addr_family / effective_host）完全独立，
        // 只决定 VPS→目标域名时优先/限定哪一族。字面 IP 目标不受影响。
        let egress = EgressFamily::from_db(&server.egress_family);
        let emit_progress = |step: &str, message: &str, progress: i64| {
            emit(JobProgress {
                job_id: req.job_id,
                kind: "deploy".into(),
                step: step.to_string(),
                message: message.to_string(),
                progress,
                error: None,
            });
        };
        emit_progress("探测", "连接服务器", 5);
        let (auth, _) =
            load_auth_parts(&state.db, &state.creds, req.server_id).map_err(|e| e.to_string())?;
        let host = crate::domain::effective_host(&server).to_string();
        let mut sess = SshSession::connect(
            &host,
            server.port as u16,
            &server.user,
            &auth,
            CONNECT_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;

        // 1 探测（dry-run，不落配置）
        check_cancel(cancel)?;
        let arch = run_cmd_ok(&mut sess, cancel, "探测", "uname -m", PROBE_TIMEOUT)
            .await?
            .stdout;
        let arch = arch.trim().to_string();
        let target = match arch.as_str() {
            "x86_64" => "x86_64-unknown-linux-gnu",
            "aarch64" => "aarch64-unknown-linux-gnu",
            other => return Err(format!("暂不支持的架构: {other}")),
        };
        check_cancel(cancel)?;
        let caps = run_cmd(
            &mut sess,
            cancel,
            "探测",
            CAPABILITY_PROBE_SCRIPT,
            PROBE_TIMEOUT,
        )
        .await?
        .stdout;
        let (os, has_systemd) = parse_capability(&caps);
        if !has_systemd {
            return Err(format!("该系统未运行 systemd（{os}），暂不支持自动部署"));
        }
        check_cancel(cancel)?;
        // 组件安装状态：只探测选中协议需要的组件（对未选组件不产生命令）
        let xray_installed = if need_xray {
            run_cmd(&mut sess, cancel, "探测", "command -v xray", PROBE_TIMEOUT)
                .await?
                .exit_code
                == 0
        } else {
            true
        };
        let ss_installed = if need_ss {
            run_cmd(
                &mut sess,
                cancel,
                "探测",
                "command -v ssserver",
                PROBE_TIMEOUT,
            )
            .await?
            .exit_code
                == 0
        } else {
            true
        };
        let singbox_installed = if need_singbox {
            run_cmd(
                &mut sess,
                cancel,
                "探测",
                "command -v sing-box",
                PROBE_TIMEOUT,
            )
            .await?
            .exit_code
                == 0
        } else {
            true
        };
        // 端口分配：已部署协议复用远端既有端口（不重新探测——防漂移撞防火墙/安全组）；
        // 新协议才探测分配（占用偏移逐 +1）。任一失败即中止（不落配置）。
        let existing =
            read_existing_ports(&mut sess, cancel, need_xray, need_ss, need_singbox).await?;
        let mut ports: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
        for proto in &req.protocols {
            let base = default_port(proto);
            if base == 0 {
                return Err(format!("未知协议: {proto}"));
            }
            let assigned = match existing.ports.get(proto.as_str()) {
                Some(p) => *p,
                None => allocate_port(&mut sess, cancel, proto, base).await?,
            };
            ports.insert(proto.as_str(), assigned);
        }
        emit_progress("探测", "探测完成", 20);
        check_cancel(cancel)?;

        // 2 安装（缺才装）
        if need_xray && !xray_installed {
            emit_progress("安装", "安装 Xray（可能耗时数分钟）", 30);
            run_cmd_ok(
                &mut sess,
                cancel,
                "安装",
                &xray_install_cmd(),
                INSTALL_TIMEOUT,
            )
            .await?;
        }
        if need_ss && !ss_installed {
            emit_progress("安装", "安装 shadowsocks-rust（可能耗时数分钟）", 30);
            let rel = latest_ss_asset(&self.http, &self.ss_api_base, target).await?;
            // 实测：shadowsocks-rust 不发布 attestation（该端点对其真实 digest 返回 404），
            // 所以这里必然走到 NotPublished——如实说「无从核验」，不冒充「已核验」。
            // 上游哪天开始签 attestation，把这条策略改成 Attested 即可启用整条核验路径。
            match verify_provenance(
                &self.http,
                &self.ss_api_base,
                "shadowsocks-rust",
                Provenance::NotPublished,
                rel.sha256.as_deref(),
            )
            .await?
            {
                ProvenanceOutcome::NotPublished => emit_progress(
                    "安装",
                    "上游 shadowsocks-rust 不发布构建来源证明，本步无从核验",
                    32,
                ),
                ProvenanceOutcome::Confirmed { count } => emit_progress(
                    "安装",
                    &format!("构建来源已证实（{count} 份 attestation，digest 匹配）"),
                    32,
                ),
            }
            let url = format!(
                "https://github.com/shadowsocks/shadowsocks-rust/releases/download/{}/{}",
                rel.tag, rel.asset
            );
            let cmd = ss_install_cmd(&url, rel.sha256.as_deref());
            let out = run_cmd_ok(&mut sess, cancel, "安装", &cmd, INSTALL_TIMEOUT).await?;
            emit_measured_hash(&emit_progress, "ss.tar.xz", &rel, &out);
            emit_installed_bin_hash(&emit_progress, "ssserver", &out);
        }
        if need_singbox && !singbox_installed {
            emit_progress("安装", "安装 sing-box（可能耗时数分钟）", 30);
            let rel = latest_singbox_asset(&self.http, &self.singbox_api_base, target).await?;
            // 实测：sing-box 发布 attestation。查不到就中止——偷 token 换包的人
            // 改得了字节与 digest，但签不出 attestation（见 verify_provenance 说明）。
            match verify_provenance(
                &self.http,
                &self.singbox_api_base,
                "sing-box",
                Provenance::Attested,
                rel.sha256.as_deref(),
            )
            .await?
            {
                ProvenanceOutcome::Confirmed { count } => emit_progress(
                    "安装",
                    &format!("构建来源已证实（{count} 份 attestation，digest 匹配）"),
                    32,
                ),
                ProvenanceOutcome::NotPublished => {
                    emit_progress("安装", "上游 sing-box 未发布构建来源证明，本步无从核验", 32)
                }
            }
            let url = format!(
                "https://github.com/SagerNet/sing-box/releases/download/{}/{}",
                rel.tag, rel.asset
            );
            let cmd = singbox_install_cmd(&url, rel.sha256.as_deref());
            let out = run_cmd_ok(&mut sess, cancel, "安装", &cmd, INSTALL_TIMEOUT).await?;
            emit_measured_hash(&emit_progress, "singbox.tar.gz", &rel, &out);
            emit_installed_bin_hash(&emit_progress, "sing-box", &out);
        }
        check_cancel(cancel)?;
        // 已装/新装统一文案（双分支相同，clippy if_same_then_else）
        emit_progress("安装", "组件就绪", 45);
        check_cancel(cancel)?;

        // 3 配置：按协议组生成配置并写入白名单路径
        emit_progress("配置", "生成配置", 55);
        // 本机库里已有的节点（按协议索引）：复用入站时要拿它补 reality 公钥——公钥只在
        // 客户端这一侧（服务器配置只存私钥）。读库失败按空表处理：复用是增益，读不回来
        // 就退化成新生成，不能让一次部署因为「读不到旧节点」而失败。
        let existing_nodes: HashMap<String, Node> = {
            let conn = state.db.lock();
            let nodes = repos::nodes::list(&conn, &state.creds).unwrap_or_default();
            let upstream_ids: HashSet<String> = repos::upstreams::list_by_server(&conn, server.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|u| u.node_id)
                .collect();
            deploy_managed_nodes(nodes, &server, &upstream_ids)
        };
        // 每协议记录 (protocol, port, params_json) 供写回；失败收集 failed_protocols
        let mut success_protocols: Vec<(String, u16, String)> = vec![];
        let mut failed_protocols: Vec<String> = vec![];

        if need_xray {
            // 每个 Reality 入站独立生成 x25519 密钥对（vless 与 trojan 不得共用：
            // 实测同私有密钥多入站并存时 Xray v26 reality 握手全线失败，客户端一律
            // EOF/503）；vmess(WS) 不涉 Reality，空密钥即可。
            // 兜底域名与上游出口共用同一个常量，避免两条链路各写一份而漂移——
            // 漂移的表现是「界面显示 A、服务器写 B」，且失败只在客户端侧可见。
            //
            // 但**只在没有既有节点 sni 时**才用默认值：重部署（例如仅切出口）若把
            // 用户自定义的伪装域名重置成默认值，就是一次静默的配置回退——服务器
            // 与节点两侧虽然一致，却不再是用户选的那个域名。
            let reality_dest = match &req.reality_dest {
                Some(d) => d.clone(),
                None => req
                    .protocols
                    .iter()
                    .find_map(|p| {
                        existing_nodes.get(p).and_then(|n| {
                            crate::servers::reality_dest::sni_of_params(&n.params_json)
                        })
                    })
                    .or_else(|| {
                        ["vless", "trojan"].iter().find_map(|p| {
                            existing_nodes.get(*p).and_then(|n| {
                                crate::servers::reality_dest::sni_of_params(&n.params_json)
                            })
                        })
                    })
                    .unwrap_or_else(|| crate::servers::upstream::DEFAULT_REALITY_DEST.into()),
            };
            let mut entries: Vec<XrayInboundEntry> = Vec::new();
            for p in ["vless", "trojan", "vmess"] {
                if !req.protocols.iter().any(|x| x == p) {
                    continue;
                }
                let port = ports[p];
                // **幂等**：这个端口上已有入站 → 原样复用它的凭据与 reality 私钥，只按
                // 本次请求更新伪装域名。
                //
                // 为什么非复用不可：凭据一改就是「两侧短暂不一致」——新凭据先落到服务器
                // 配置，要等到写回阶段才进客户端节点行，中间任何失败（重启 xray 掐断
                // 承载这次部署的隧道、SSH 抖动、端口验证失败、用户取消）都会让客户端
                // 攥着旧凭据去连一个只认新凭据的入站：**节点变砖，界面上还看不出来**
                // （行还在、还启用在用）。2026-09-20 真机：job 356 停在 80%，ashura 三个
                // 协议全废，只能重部署一次才恢复。复用之后，一次失败的重复部署只是
                // 「没改成」，不会再毁掉已经能用的节点。
                //
                // 公钥不在服务器配置里（Xray 只存私钥），只能从既有节点参数取；取不到就
                // 不复用，退化成新生成——宁可多写一次新凭据，也不要复用出一个公钥为空的
                // reality 入站（那同样是全废，且更难查）。
                if let Some(old) = existing.xray_inbounds.get(&port) {
                    // 私钥才是服务器侧的事实源；见 derive_reality_public_key 的说明。
                    let old_pk = if p == "vmess" {
                        None
                    } else {
                        derive_reality_public_key(&mut sess, cancel, &old.private_key)
                            .await
                            .or_else(|| {
                                existing_nodes
                                    .get(p)
                                    .and_then(|n| public_key_of(&n.params_json))
                            })
                    };
                    if p == "vmess" || old_pk.is_some() {
                        let mut e = old.clone();
                        e.reality_dest = reality_dest.clone();
                        e.public_key = old_pk.unwrap_or_default();
                        entries.push(e);
                        continue;
                    }
                }
                let (priv_k, pub_k) = if p == "vmess" {
                    (String::new(), String::new())
                } else {
                    let keys_out =
                        run_cmd_ok(&mut sess, cancel, "配置", "xray x25519", CONFIG_TIMEOUT)
                            .await?
                            .stdout;
                    parse_x25519_keys(&keys_out)?
                };
                let short_id: String = (0..8)
                    .map(|_| format!("{:x}", rand::random::<u8>() % 16))
                    .collect();
                entries.push(XrayInboundEntry {
                    proto: p,
                    port: ports[p],
                    uuid: Uuid::new_v4().to_string(),
                    password: Uuid::new_v4().to_string(),
                    short_id,
                    private_key: priv_k,
                    public_key: pub_k,
                    reality_dest: reality_dest.clone(),
                    ws_path: "/ray".into(),
                });
            }
            let cfg = xray_config_json_multi(&entries, egress);
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                XRAY_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            for e in &entries {
                let params = match e.proto {
                    "vless" => serde_json::json!({
                        "uuid": e.uuid, "security": "reality", "type": "tcp",
                        "flow": "xtls-rprx-vision", "sni": e.reality_dest,
                        "public-key": e.public_key, "short-id": e.short_id, "fingerprint": "chrome"
                    })
                    .to_string(),
                    "trojan" => serde_json::json!({
                        "password": e.password, "security": "reality", "type": "tcp",
                        "sni": e.reality_dest, "public-key": e.public_key,
                        "short-id": e.short_id, "fingerprint": "chrome"
                    })
                    .to_string(),
                    _ => serde_json::json!({
                        "uuid": e.uuid, "alterId": "0", "cipher": "auto",
                        "network": "ws", "path": "/ray"
                    })
                    .to_string(),
                };
                success_protocols.push((e.proto.to_string(), e.port, params));
            }
        }
        // 客户端用哪一族地址连这台机器，UDP/SS 入站就必须监听哪一族：
        // sing-box 与 shadowsocks-rust 的 `0.0.0.0` 都是**纯 IPv4**（Xray 的同一
        // 写法却是双栈）。香港这类登记了 IPv6 的机器会因此让 ss/h2/tuic 完全
        // 收不到包（2026-10-23 真机：ss 在 0.0.0.0:8388，客户端连 [v6]:8388 503）。
        let inbound_listen = if host.contains(':') { "::" } else { "0.0.0.0" };
        if need_ss {
            // 同 xray 那段：既有密码原样复用，重复部署不轮换（轮换 = 两侧短暂不一致，
            // 中途失败就把节点变砖）。只有首次部署或读不回配置时才新生成。
            let key: [u8; 32] = rand::random();
            let pw = existing.ss_password.clone().unwrap_or_else(|| {
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key)
            });
            let ss_port = ports["ss"];
            let cfg = ss_config_json(ss_port, &pw, egress, inbound_listen);
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SS_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SS_UNIT_PATH,
                &ss_systemd_unit(),
                CONFIG_TIMEOUT,
            )
            .await?;
            let params = serde_json::json!({ "cipher": "2022-blake3-aes-256-gcm", "password": pw })
                .to_string();
            success_protocols.push(("ss".into(), ss_port, params));
        }
        if need_singbox {
            // 同 xray/ss 两段：既有凭据原样复用，重复部署不轮换（轮换 = 服务端新凭据 +
            // 客户端旧凭据 = 静默变砖，界面还显示「部署成功」）。只有首次部署、
            // 或既有配置里读不回该项时才新生成。
            let hy2_pw = existing
                .singbox_hy2_pw
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let tuic_uuid = existing
                .singbox_tuic_uuid
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let tuic_pw = existing
                .singbox_tuic_pw
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            // 自签证书（幂等：已存在跳过）。缺 openssl 时按包管理器安装；
            // 全无包管理器 → 脚本回 NO_OPENSSL，映射成可读错误（而非裸 exit code）。
            // 超时用 INSTALL_TIMEOUT(600s) 而非 CONFIG_TIMEOUT(60s)：该命令内含
            // `apt-get update -qq && apt-get install -y openssl`，慢镜像下单是索引
            // 刷新就可能超 60s，会把一条罕见但合法的路径变成「配置阶段的不明超时」。
            let out = run_cmd(
                &mut sess,
                cancel,
                "配置",
                &singbox_cert_cmd(),
                INSTALL_TIMEOUT,
            )
            .await?;
            if out.stdout.contains("NO_OPENSSL") {
                return Err(
                    "该系统找不到 openssl，也没有可识别的包管理器（apt-get/dnf/yum/pacman/zypper/apk），无法生成 sing-box 自签证书"
                        .into(),
                );
            }
            if out.exit_code != 0 {
                return Err(format!(
                    "生成自签证书失败(exit={}): {}",
                    out.exit_code,
                    out.stderr.trim_end()
                ));
            }
            // 按选中集生成 inbounds：`ports` 只装选中协议，未选中的键不存在。
            // 必须用 then(|| …) 惰性闭包——then_some 的参数是急切求值，
            // ports["hysteria2"] 会在调用前就索引并 panic（单 L2 即触发）。
            let hy2_selected = req.protocols.iter().any(|p| p == "hysteria2");
            let tuic_selected = req.protocols.iter().any(|p| p == "tuic");
            // 出口写法随 sing-box 版本分支：1.12+ 用出站 domain_resolver，
            // 更早版本用 domain_strategy。探测失败按旧版处理——把新版字段写进
            // 旧版配置会让 sing-box 直接 FATAL，比「出口不生效」严重得多。
            let ver_out = run_cmd(
                &mut sess,
                cancel,
                "配置",
                "/usr/local/bin/sing-box version",
                PROBE_TIMEOUT,
            )
            .await;
            // 取消不能被上面的「失败不阻断」吞掉
            check_cancel(cancel)?;
            let singbox_ver = match ver_out {
                Ok(out) if out.exit_code == 0 => parse_singbox_version(&out.stdout),
                _ => None,
            };
            if singbox_ver.is_none() {
                // 探测失败时配置里不写任何出口字段（两种写法各有版本会 FATAL，
                // 见 deploy_spec 的实测注释）：要让用户知道这次出口没生效。
                emit_progress(
                    "配置",
                    "无法识别 sing-box 版本，本次未写入出口地址族（服务保持可用，可重试部署）",
                    55,
                );
            }
            let cfg = singbox_config_json(
                hy2_selected.then(|| (ports["hysteria2"], hy2_pw.as_str())),
                tuic_selected.then(|| (ports["tuic"], tuic_uuid.as_str(), tuic_pw.as_str())),
                egress,
                singbox_ver,
                inbound_listen,
            );
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SINGBOX_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SINGBOX_UNIT_PATH,
                &singbox_systemd_unit(),
                CONFIG_TIMEOUT,
            )
            .await?;
            if req.protocols.contains(&"hysteria2".to_string()) {
                success_protocols.push((
                    "hysteria2".into(),
                    ports["hysteria2"],
                    serde_json::json!({
                        "password": hy2_pw, "skip-cert-verify": "1"
                    })
                    .to_string(),
                ));
            }
            if req.protocols.contains(&"tuic".to_string()) {
                success_protocols.push((
                    "tuic".into(),
                    ports["tuic"],
                    serde_json::json!({
                        "uuid": tuic_uuid, "password": tuic_pw, "skip-cert-verify": "1"
                    })
                    .to_string(),
                ));
            }
        }
        if success_protocols.is_empty() {
            return Err("没有生成任何协议配置".into());
        }
        check_cancel(cancel)?;

        // 4 启动：逐 unit 激活并验证；端口验证失败 → 该协议标记失败（不中断其余）
        emit_progress("启动", "启动服务", 80);
        if need_xray {
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                "systemctl enable --now xray",
                CONFIG_TIMEOUT,
            )
            .await?;
            // restart 的退出码不作判据：它偶发抖一下（实测 exit=3、stderr 空）不代表
            // 服务没起来，真正的判据是下面等出来的状态。
            run_cmd(
                &mut sess,
                cancel,
                "启动",
                "systemctl restart xray",
                CONFIG_TIMEOUT,
            )
            .await?;
            wait_service_active(&mut sess, cancel, "xray").await?;
        }
        if need_ss {
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                "systemctl daemon-reload && systemctl enable --now ssserver",
                CONFIG_TIMEOUT,
            )
            .await?;
            // 只 enable --now 不会让已在跑的服务重新读配置——重部署写下的新
            // ipv6_first 会停在磁盘上不生效（与服务已 active 的假象并存）。
            // 与 xray 一样：restart 的退出码不作判据，判据是下面的状态等待。
            run_cmd(
                &mut sess,
                cancel,
                "启动",
                "systemctl restart ssserver",
                CONFIG_TIMEOUT,
            )
            .await?;
            wait_service_active(&mut sess, cancel, "ssserver").await?;
        }
        if need_singbox {
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                "systemctl daemon-reload && systemctl enable --now singbox",
                CONFIG_TIMEOUT,
            )
            .await?;
            // 同上：sing-box 配置里的 domain_resolver / domain_strategy 必须
            // 重启进程才生效，否则「切了出口但行为没变」。
            run_cmd(
                &mut sess,
                cancel,
                "启动",
                "systemctl restart singbox",
                CONFIG_TIMEOUT,
            )
            .await?;
            wait_service_active(&mut sess, cancel, "singbox").await?;
        }
        // 逐协议端口验证：失败隔离（其余协议继续,仅标记）
        for (proto, port, _params) in &success_protocols {
            // UDP 协议（Hysteria2/TUIC）只在 UDP 表里可见，用 `ss -tln` 验永远失败。
            let listening = run_cmd(
                &mut sess,
                cancel,
                "启动",
                &listen_probe_cmd(proto, *port),
                VERIFY_TIMEOUT,
            )
            .await?
            .exit_code
                == 0;
            if !listening {
                failed_protocols.push(proto.clone());
            }
        }
        check_cancel(cancel)?;

        // 4.4 全协议失败守卫：必须在 BBR 之前。BBR 会真实改写内核参数（sysctl -w +
        // 追加 /etc/sysctl.conf），若先开 BBR 再因「全部协议验证失败」返回 Err，
        // 内核已被动过而 repos::deploys::insert 从未执行，用户拿到一个 failed 部署
        // 且不知道内核被改过。部分失败不走这里：那条路径的摘要（含 bbr 字段）在
        // Err 之前已落库，不受影响。
        let ok_protocols: Vec<&(String, u16, String)> = success_protocols
            .iter()
            .filter(|(p, _, _)| !failed_protocols.contains(p))
            .collect();
        if ok_protocols.is_empty() {
            return Err(format!("所有协议验证失败: {}", failed_protocols.join(", ")));
        }

        // 4.5 BBR（可选）：插在端口验证之后、写回之前——协议服务确认起来了才动内核参数，
        // BBR 出问题不可能被误判成部署失败。任何失败都不上抛：协议已部署成功，
        // BBR 属优化项，加 `?` 会让一次成功的部署在最后一步变成 failed，所以要收集成字符串。
        //
        // 本阶段的**命令内部**不感知取消：server_bbr::check_bbr / enable_bbr 走裸
        // SshSession::run，脚本没有取消标志，只能靠命令自身超时兜底（检测 15s + 开启 30s
        // ⇒ 最坏窗口约 45s）。这里不打断执行中的命令（SSH 粒度决定，见模块头）。
        //
        // 但 BBR 段**之后**必须重新看一眼取消位（下面 5 之前那句）：否则用户在这 45 秒里
        // 点的取消就是空操作——部署照样写回节点并报成功，取消等于没点（#14）。
        // 代价是可能停在「远端服务已起、BBR 已生效，而本地没有节点行」：这是**有意选的**。
        // 用户显式点了取消，应用不该替他决定「其实挺好的，就当没取消吧」；重部署一次即可
        // 收敛，而用户被无视是无从挽回的。同理，取消落在更早的步骤时也早就这样了
        // （配置已写、服务已起，只是不写回），这里保持一致而不是破例。
        let mut bbr_note: Option<String> = None;
        if req.enable_bbr {
            emit_progress("优化", "检测 BBR", 88);
            match crate::server_bbr::check_bbr(&mut sess).await {
                // 已是 bbr：不下发开启脚本（脚本虽幂等，但没必要多跑一次 sysctl 写）
                Ok(algo) if algo == "bbr" => bbr_note = Some("BBR 已启用".to_string()),
                Ok(algo) => {
                    emit_progress("优化", &format!("当前 {algo}，开启 BBR"), 90);
                    bbr_note = Some(match crate::server_bbr::enable_bbr(&mut sess).await {
                        Ok(m) => m,
                        Err(e) => format!("开启失败：{e}"),
                    });
                }
                Err(e) => bbr_note = Some(format!("检测失败：{e}")),
            }
        }

        // 步骤边界：BBR 段可能刚跑了几十秒，这期间用户点的取消必须在这里被看见（#14）。
        // 写回是「应用认为这次部署成立」的那一刻，取消要在它之前生效。
        check_cancel(cancel)?;

        // 4.6 放行：把本次验证通过的端口写进**持久**防火墙规则。
        //
        // 必须在写回之前：写回之后界面就把它当成可用节点，而端口没放行时用户看到
        // 的是「节点在、测速 0ms」，只能人工上服务器查防火墙。2026-10-23 ashura
        // 重启后，8443/2053/2083/2096/8002/8989 只因运行时 iptables 规则放行过，
        // UFW 的持久规则里没有 → 默认 DROP 全挡，正是这个缺口。
        // 放行失败直接让部署失败：留下一个连不上的节点比失败更糟。
        emit_progress("放行", "持久放行节点端口", 92);
        // 本次验证通过的端口 + 这台机器上已部署的其它节点端口（广播 IP 多入站），
        // 一次全写进去；firewall_allow_cmd 内部去重。
        let existing_ports = deployed_node_ports(state, &server);
        let mut allow_ports: Vec<(&str, u16)> = ok_protocols
            .iter()
            .map(|(protocol, port, _)| (protocol.as_str(), *port))
            .collect();
        allow_ports.extend(
            existing_ports
                .iter()
                .map(|(protocol, port)| (protocol.as_str(), *port)),
        );
        run_cmd_ok(
            &mut sess,
            cancel,
            "放行",
            &firewall_allow_cmd(&allow_ports),
            VERIFY_TIMEOUT,
        )
        .await?;
        check_cancel(cancel)?;

        // 5 写回：仅写成功协议节点(含端口验证通过);failed 在成功节点写回后随 error 上报
        // （ok_protocols 已在上方 4.4 守卫处算出，非空已保证）
        emit_progress("写回", "写入节点与部署记录", 95);
        {
            let conn = state.db.lock();
            let upstream_ids: HashSet<String> =
                repos::upstreams::list_by_server(&conn, req.server_id)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter_map(|u| u.node_id)
                    .collect();
            for (proto, port, params) in &ok_protocols {
                let name = format!("{}-{}", server.name, proto);
                // 同名旧行必须**原地更新**，不能按地址删了重建：
                // - 切过地址族的机器（香港/ashura 登记 v6，旧行 addr 还是 v4）按新地址
                //   删不到旧行，重建后库里出现两个同名节点，mihomo 热重载直接 400
                //   `proxy <name> is the duplicate name`（2026-10-23 真机）；
                // - 旧行上的 relay_rule_id 与用户改名也跟着丢。
                // 只认「同服务器 + 同协议 + 同名」；用户改过名的节点不算，另起一行。
                // ` #N` 是重名自愈给历史副本加的后缀，同样属于这条线（见
                // matches_deployed_name 的说明）。
                let rows: Vec<Node> = repos::nodes::list(&conn, &state.creds)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|n| {
                        n.source == "self-built"
                            && n.protocol == *proto
                            && matches_deployed_name(&n.name, &name)
                            && crate::domain::server_owns_addr(&server, &n.addr)
                            && !upstream_ids.contains(&n.id)
                    })
                    .collect();
                // 优先保留挂了中转的那行：链路是按节点行 id/端口建的，换行即丢挂载。
                let keep = rows
                    .iter()
                    .position(|n| n.relay_rule_id.is_some())
                    .unwrap_or(0);
                let mut node = if rows.is_empty() {
                    Node {
                        id: Uuid::new_v4().simple().to_string(),
                        name: name.clone(),
                        source: "self-built".into(),
                        protocol: proto.clone(),
                        addr: String::new(),
                        port: 0,
                        params_json: String::new(),
                        group_name: "自建".into(),
                        sub_id: None,
                        relay_rule_id: None,
                        enabled: true,
                        latency: None,
                        bandwidth: None,
                        last_checked_at: None,
                        last_check_error: None,
                        direct_latency: None,
                        hop_relay_ms: None,
                        hop_landing_ms: None,
                        hop_tunnel_ms: None,
                    }
                } else {
                    rows[keep].clone()
                };
                node.addr = crate::domain::effective_host(&server).to_string();
                node.port = *port as i64;
                node.params_json = params.clone();
                node.enabled = true;
                for (i, extra) in rows.iter().enumerate() {
                    if i != keep {
                        repos::nodes::delete(&conn, &extra.id).map_err(|e| e.to_string())?;
                    }
                }
                repos::nodes::upsert(&conn, &node, &state.creds).map_err(|e| e.to_string())?;
            }
            // 自动补挂中转：建链只改指「当时已存在」的节点，之后部署上来的节点
            // relay_rule_id 恒为 None，客户端就按直连拨——落地机直连不通时表现为
            // 测速 504，节点行上却毫无线索。这里补上，并把挂不上的原因写进摘要。
            let deployed: Vec<(String, i64)> = ok_protocols
                .iter()
                .map(|(p, port, _)| (p.clone(), *port as i64))
                .collect();
            let attaches =
                crate::relay::attach_deployed_nodes(&conn, &server, &deployed, &state.creds)
                    .unwrap_or_else(|e| {
                        // 补挂失败不该让整次部署失败：节点已在库，链路问题照实报出来
                        vec![crate::relay::AttachOutcome {
                            node: server.name.clone(),
                            attached: false,
                            detail: format!("自动补挂中转时出错：{e}"),
                        }]
                    });
            let ports_map: serde_json::Map<String, serde_json::Value> = ok_protocols
                .iter()
                .map(|(p, port, _)| (p.clone(), serde_json::Value::from(*port)))
                .collect();
            let mut summary = serde_json::json!({
                "protocols": ok_protocols.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(),
                "ports": serde_json::Value::Object(ports_map),
                "relay": attaches.iter().map(|a| serde_json::json!({
                    "node": a.node, "attached": a.attached, "detail": a.detail
                })).collect::<Vec<_>>()
            });
            // BBR 结果（有才写）：摘要只作部署历史记录，协议列表以节点行为准，
            // 因此附加字段不会影响 UI 的已部署协议展示
            if let Some(note) = &bbr_note {
                summary["bbr"] = serde_json::Value::from(note.clone());
            }
            let summary = summary.to_string();
            repos::deploys::insert(&conn, req.server_id, "deploy", &summary, "ok")
                .map_err(|e| e.to_string())?;
        }
        state
            .subs
            .rebuild_config()
            .await
            .map_err(|e| e.to_string())?;
        // 只重建导出缓存不够：新节点必须同时进入**正在运行**的客户端。否则出口
        // 下拉框（读库）里能选中新节点，切换时 mihomo 回
        // `HTTP 400 Selector update error: proxy not exist`（2026-10-23 实测）。
        // 网络没开时这步是 no-op（ConfigApply::NotRunning）。
        crate::net::runtime::apply_config_change(state)
            .await
            .map_err(|e| format!("部署已完成，但更新运行中的客户端失败：{e}"))?;
        if failed_protocols.is_empty() {
            // BBR 结果并进 100% 文案：用户不点开摘要也能看到
            let done = match &bbr_note {
                Some(n) => format!("部署完成（{n}）"),
                None => "部署完成".to_string(),
            };
            emit_progress("写回", done.as_str(), 100);
            Ok(())
        } else {
            let msg = format!("部分协议失败: {}", failed_protocols.join(", "));
            emit(JobProgress {
                job_id: req.job_id,
                kind: "deploy".into(),
                step: "写回".into(),
                message: msg.clone(),
                progress: 100,
                error: Some(msg.clone()),
            });
            // 把失败协议名带回任务错误里：以前只回一句「部分协议部署失败」，
            // 任务表里查不到到底是哪几个，用户只知道「勾了却没部署上」。
            Err(msg)
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("已取消".into())
    } else {
        Ok(())
    }
}
