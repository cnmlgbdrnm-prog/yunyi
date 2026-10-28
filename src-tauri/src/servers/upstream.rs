//! 上游出口：中转机上**专用的第二个 Xray 实例**，把买来的静态住宅 IP 代理
//! （SOCKS5/HTTP，没有 SSH 也无从部署）接成落地。一条出口 = 一个 VLESS+REALITY
//! inbound + 一个 socks5/http outbound + 一条 routing 配对，多出口靠 inboundTag 共存。
//!
//! 为什么是独立实例而不是塞进 deploy 管的那份配置：`deploy` 是**整文件重渲染**
//! （`merge_deploy_protocols` + `read_existing_ports`），混进去之后任何一次
//! 「重新部署协议/补装协议」都可能把用户的住宅线路静默搞断，且极难排查。
//! 独立实例 = 拆除干净（停 unit + 删文件删 unit），与协议管理 IPC 零耦合。
//!
//! 客户端侧协议不变（VLESS+REALITY）——socks5/http **永远不进节点导出面**，
//! 所以 `config/mod.rs`、`config/singbox.rs`、订阅解析一行都不用改。
//!
//! 密码纪律：入库走 seal；不进 `domain::Upstream`（该结构体要序列化给前端）；
//! 不进任何远端命令行（出口 IP 探测因此走本地 mihomo，不走中转机 curl）。

use crate::db::repos;
use crate::domain::{JobProgress, Node, Upstream, UpstreamEntryOption, UpstreamEntryOptions};
use crate::jobs::{DeployRequest, EmitFn, UpstreamReq};
use crate::servers::deploy::{
    parse_capability, parse_x25519_keys, run_cmd, run_cmd_ok, wait_service_active,
    write_remote_file, DeployProgress, ProdRunner, CAPABILITY_PROBE_SCRIPT, CONFIG_TIMEOUT,
    INSTALL_TIMEOUT,
};
use crate::servers::deploy_spec::{describe_deployed_protocol, protocol_order, UpstreamRoute};
use crate::servers::{clean_text, load_auth, ServersError, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// 专用实例的远端路径（白名单常量）。
/// **不得**写 `/usr/local/etc/xray/config.json` —— 那是 deploy 的地盘。
pub const UPSTREAM_CONFIG_PATH: &str = "/usr/local/etc/xray-upstream/config.json";
pub const UPSTREAM_UNIT_PATH: &str = "/etc/systemd/system/xray-upstream.service";
/// systemd 服务名（与 unit 文件名一致）。
pub const UPSTREAM_SERVICE: &str = "xray-upstream";
/// 上游协议白名单（与前端选项同步）。
pub const PROTOS: &[&str] = &["socks5", "http"];
/// 可以复用主入口的协议。SS 故意不在内：它由独立的 ssserver 承载，
/// shadowsocks-rust 没有「按用户分流到不同出站」的能力，复用 SS 入口
/// 无法把该用户的流量送进住宅代理（会静默走直连，比不支持更糟）。
pub const REUSABLE_PROTOS: &[&str] = &["vless", "trojan", "vmess", "hysteria2", "tuic"];
/// 默认 REALITY 伪装域名（与前端 src/utils/realityDest.ts 的 DEFAULT_REALITY_DEST 同值，
/// 两条链路不得各写一份——漂移了就是「界面显示 A、服务器写 B」）。
/// 已知代价：本值 www.microsoft.com 在本项目实测中 REALITY 握手必失败（服务端报
/// invalid connection、客户端 EOF/503）。默认值按既定选择保持原样，UI 在该值上给警告；
/// 换一个可用域名只需改这一处常量（下拉里有 19 个实测可用的候选）。
pub const DEFAULT_REALITY_DEST: &str = "www.microsoft.com";
/// 默认入站端口。
pub const DEFAULT_LISTEN_PORT: u16 = 443;

/// 部署后「本机 → 中转机入站」可达性核验写下的错误前缀。
///
/// 这次核验成功时必须清掉上一次同来源的红字：服务器入口地址族从 v6 切回 v4
/// 后重新部署，端口已经通了，但旧错误若不清会一直挂着（用户看到的就是
/// 「明明能连，界面还是红字」）。出口 IP 探测写的是另一条链路（中转机 →
/// 住宅代理）的失败原因，不能用这次入站核验的成功去覆盖它。
const INBOUND_PROBE_ERROR_PREFIX: &str = "从本机连不上 ";

/// 本次入站可达性核验成功时，是否该清掉旧的 `last_check_error`。
fn should_clear_stale_inbound_error(old: Option<&str>) -> bool {
    old.is_some_and(|e| e.starts_with(INBOUND_PROBE_ERROR_PREFIX))
}

/// REALITY 身份材料：首建生成后固定，重建复用——否则客户端节点每次都要重新导入。
/// 公钥一并存下来（而不是每次从私钥推导），重建时就不必依赖 `xray x25519 -i`
/// 这个随版本变化的参数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Material {
    pub uuid: String,
    pub private_key: String,
    pub public_key: String,
    pub short_id: String,
    /// 复用入口时的独立口令（Trojan/Hysteria2 直接用 uuid；TUIC 需要 uuid+口令）。
    /// 旧行没有这个键（serde default 空串），使用时按 uuid 兜底并补写。
    #[serde(default)]
    pub password: String,
}

impl Material {
    /// 复用入口的认证口令：老材料可能只有 uuid（或整份 `{}`），这里统一兜底。
    pub fn entry_password(&self) -> String {
        if self.password.is_empty() {
            self.uuid.clone()
        } else {
            self.password.clone()
        }
    }
}

/// 添加/编辑输入（IPC 扁平参数形态）。
#[derive(Debug, Clone)]
pub struct UpstreamInput {
    pub server_id: i64,
    pub name: String,
    pub proto: String,
    pub host: String,
    pub port: i64,
    pub username: String,
    pub password: String,
    /// 旧专用实例的监听端口；复用入口时由命令层置 0。
    pub listen_port: i64,
    pub reality_dest: String,
    /// 复用入口：选中的已部署主节点 id（优先于 `entry_proto`）。
    pub entry_node_id: Option<String>,
    /// 复用入口协议；与服务端解析出的节点协议二选一。
    pub entry_proto: String,
}

/// 编辑既有出口：改的都在这，改完**必须重新部署**才在远端生效。
///
/// 两处刻意的取舍：
/// - **名称可改**：名称参与写回节点的命名（`<服务器名>-<出口名>`）。保存只写库；
///   重新部署时部署 job 会按 `node_id` 用新名字覆盖那只节点，不是一次纯本地编辑。
/// - **密码是 `Option`**：`None` = 不动。凭据经 IPC 不回显（前端表单拿不到原值），
///   留空就只能是「不改」——照空串写回去等于把已存的口令擦掉。
#[derive(Debug, Clone)]
pub struct UpstreamEdit {
    pub name: String,
    pub proto: String,
    pub host: String,
    pub port: i64,
    pub username: String,
    pub password: Option<String>,
    pub listen_port: i64,
    pub reality_dest: String,
    pub entry_node_id: Option<String>,
    pub entry_proto: String,
}

/// 出口在专用实例内的入站 tag（`up-{id}`）。
pub fn tag_of(id: i64) -> String {
    format!("up-{id}")
}

/// 入站 tag → 出站 tag（`up-1` → `out-up-1`）。渲染时手里只有 tag，没有 id。
pub fn outbound_tag(inbound_tag: &str) -> String {
    format!("out-{inbound_tag}")
}

/// 这台机器上**除上游之外**已占用的端口：deploy 管的协议入站 + 中转链路的
/// `wg_port`/`forward_port`。调用方在锁外拼好（relay 那部分查库时自己加锁）。
///
/// 为什么非查不可：上游是**独立第二个 Xray 实例**，与 deploy 那份配置各占各的端口，
/// 而 deploy 的入站端口不写在别处、只体现在「该机节点的 `port`」上。漏查的后果不是
/// 报错而是**静默**——2026-09-21 实测：监听端口填了 443（界面**默认预填**值），正是
/// 同机 `ashura-vless` 在用的端口，第二个实例 bind 失败立刻退出，job 却报 done/100%，
/// 客户端连过去被主实例用另一套 REALITY 密钥拒掉，节点表现为
/// 「延迟测试失败: HTTP 503 {"message":"An error occurred in the delay test"}」。
///
/// `nodes` 必须是**该机节点**（addr == 服务器地址）；`upstream_node_ids` 是这台机器上
/// 各上游自己写回的节点——它们的端口就是上游入站端口，由 [`validate`] 的另一条规则
/// 管（出口之间不得重复），重复计入会把「改自己端口」判成冲突。
pub fn ports_in_use_elsewhere(nodes: &[Node], upstream_node_ids: &[String]) -> Vec<i64> {
    let mut v: Vec<i64> = nodes
        .iter()
        .filter(|n| !upstream_node_ids.iter().any(|id| id == &n.id))
        .map(|n| n.port)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 首选端口被占时依次改用的候选。
///
/// 挑这几个而不是上来就随机：它们是 HTTPS 的备用端口，云安全组/防火墙通常已经放行，
/// 而随机端口很可能没放行——那样「自动挑一个」只是把问题从「端口撞车」换成「连不上」，
/// 更难查。全被占才退到随机段。
const ALTERNATE_PORTS: &[i64] = &[8443, 2096, 2083, 2087, 2053, 2086, 9443, 8080];

/// 挑一个不与 `taken` 冲突的监听端口：首选能用就用首选，被占就按 [`ALTERNATE_PORTS`]
/// 顺序找一个，都被占则随机。**绝不动已占用的那个**——上游是独立实例，抢端口只会把
/// 「新出口连不上」换成「老出口被挤掉」，后者更难查也更伤（用户已有线路会莫名断）。
pub fn pick_listen_port(preferred: i64, taken: &[i64]) -> i64 {
    let free = |p: i64| (1..=65535).contains(&p) && !taken.contains(&p);
    if free(preferred) {
        return preferred;
    }
    if let Some(p) = ALTERNATE_PORTS.iter().copied().find(|p| free(*p)) {
        return p;
    }
    let mut rng = rand::thread_rng();
    for _ in 0..200 {
        let p: i64 = rand::Rng::gen_range(&mut rng, 20000..=60000);
        if free(p) {
            return p;
        }
    }
    // 随机挑 200 次都撞（只在 taken 大得离谱时才会发生）：原样返回，交给 validate 报错
    preferred
}

/// 解析后的入口选择。`None` = 旧专用实例（保持存量语义）；
/// `Some` = 复用主配置里的同协议入站。`node_id=None` 表示该协议尚未部署，
/// 部署 job 会先补部署再回写端口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryTarget {
    pub node_id: Option<String>,
    pub proto: String,
    /// 已部署节点的端口；尚未部署时为 0。
    pub port: i64,
}

/// 校验输入；`existing` 为同一中转机上已有的出口（用于监听端口唯一性），
/// `reserved` 为本机**非上游**已占用的端口（见 [`ports_in_use_elsewhere`]）。
/// 编辑场景由调用方把被编辑行从 `existing` 中排除。
///
/// `entry=Some` 时走复用入口校验：不再校验/占用监听端口与伪装域名——这两项
/// 只属于旧专用实例，复用入口的端口由主协议决定（见 `entry_port`）。
pub fn validate(
    input: &UpstreamInput,
    existing: &[Upstream],
    reserved: &[i64],
    entry: Option<&EntryTarget>,
) -> Result<(), ServersError> {
    let name = clean_text(&input.name);
    if name.is_empty() {
        return Err(ServersError::Validation("出口名称不能为空".into()));
    }
    // 节点名是 `<服务器名>-<出口名>`：同机出口重名会让写回的两个节点同名，
    // mihomo 会以 duplicate name 拒绝整份配置。新增与改名共用这条校验。
    if existing.iter().any(|u| clean_text(&u.name) == name) {
        return Err(ServersError::Validation(format!(
            "出口名称「{name}」已被这台机器上的其他出口使用"
        )));
    }
    if !PROTOS.contains(&input.proto.as_str()) {
        return Err(ServersError::Validation(format!(
            "上游协议只支持 {}",
            PROTOS.join("/")
        )));
    }
    if clean_text(&input.host).is_empty() {
        return Err(ServersError::Validation("上游地址不能为空".into()));
    }
    if !(1..=65535).contains(&input.port) {
        return Err(ServersError::Validation(
            "上游端口必须在 1-65535 之间".into(),
        ));
    }
    if let Some(entry) = entry {
        if !REUSABLE_PROTOS.contains(&entry.proto.as_str()) {
            return Err(ServersError::Validation(format!(
                "入口协议 {} 不支持复用（可选 {}）",
                entry.proto,
                REUSABLE_PROTOS.join("/")
            )));
        }
        return Ok(());
    }
    if !(1..=65535).contains(&input.listen_port) {
        return Err(ServersError::Validation(
            "监听端口必须在 1-65535 之间".into(),
        ));
    }
    if existing.iter().any(|u| u.listen_port == input.listen_port) {
        return Err(ServersError::Validation(format!(
            "监听端口 {} 已被这台中转机上的其他出口占用",
            input.listen_port
        )));
    }
    if reserved.contains(&input.listen_port) {
        return Err(ServersError::Validation(format!(
            "监听端口 {} 已被这台机器上的其它服务占用（deploy 管的协议入站或中转链路的 \
             WireGuard 端口都在此列），换一个再试",
            input.listen_port
        )));
    }
    if clean_text(&input.reality_dest).is_empty() {
        return Err(ServersError::Validation("伪装域名不能为空".into()));
    }
    Ok(())
}

/// 把「选中的入口节点 / 入口协议」解析成可落库的 `EntryTarget`。
///
/// 优先用节点 id：节点必须属于这台服务器、是 deploy 写回的主节点，且不是另一条
/// 上游出口自己写回的节点（同协议同名，选错了会把上游节点当作主入口）。只给协议名
/// （尚未部署）时返回 `port=0`，由下一次部署补端口。
pub fn resolve_entry(
    state: &AppState,
    server_id: i64,
    entry_node_id: Option<&str>,
    entry_proto: &str,
) -> Result<Option<EntryTarget>, String> {
    let node_id = entry_node_id.map(str::trim).filter(|s| !s.is_empty());
    let proto = clean_text(entry_proto);
    if node_id.is_none() && proto.is_empty() {
        return Ok(None);
    }
    if proto == "ss" {
        return Err(
            "SS 使用独立的 ssserver，无法按用户把流量送进住宅代理，不能作为复用入口".into(),
        );
    }
    let conn = state.db.lock();
    let server = repos::servers::get(&conn, server_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "服务器不存在".to_string())?;
    let upstream_node_ids: HashSet<String> = repos::upstreams::list_by_server(&conn, server_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|u| u.node_id)
        .collect();
    if let Some(id) = node_id {
        let node = repos::nodes::get(&conn, id, &state.creds)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "选择的入口节点不存在（可能已被重新部署替换）".to_string())?;
        if node.source != "self-built"
            || upstream_node_ids.contains(&node.id)
            || !crate::domain::server_owns_addr(&server, &crate::domain::direct_addr(&node))
        {
            return Err("选择的入口节点不属于这台服务器的主协议部署".into());
        }
        if node.protocol == "ss" {
            return Err(
                "SS 使用独立的 ssserver，无法按用户把流量送进住宅代理，不能作为复用入口".into(),
            );
        }
        if !REUSABLE_PROTOS.contains(&node.protocol.as_str()) {
            return Err(format!(
                "入口协议 {} 不支持复用（可选 {}）",
                node.protocol,
                REUSABLE_PROTOS.join("/")
            ));
        }
        return Ok(Some(EntryTarget {
            node_id: Some(node.id),
            proto: node.protocol,
            port: node.port,
        }));
    }
    if !REUSABLE_PROTOS.contains(&proto.as_str()) {
        return Err(format!(
            "入口协议 {proto} 不支持复用（可选 {}）",
            REUSABLE_PROTOS.join("/")
        ));
    }
    Ok(Some(EntryTarget {
        node_id: None,
        proto,
        port: 0,
    }))
}

/// 「入口协议」下拉的数据源：已部署、可复用的主节点 + 支持复用的协议白名单。
///
/// 与 `deployed_protocol_details` 用同一套归属判据（self-built、服务器地址、
/// 排除上游出口节点），保证「已部署协议」页签里能看到的，这里就能选到。
pub fn entry_options(state: &AppState, server_id: i64) -> Result<UpstreamEntryOptions, String> {
    let conn = state.db.lock();
    let server = repos::servers::get(&conn, server_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "服务器不存在".to_string())?;
    let upstream_node_ids: HashSet<String> = repos::upstreams::list_by_server(&conn, server_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|u| u.node_id)
        .collect();
    let nodes = repos::nodes::list(&conn, &state.creds).map_err(|e| e.to_string())?;
    let mut deployed: Vec<UpstreamEntryOption> = nodes
        .into_iter()
        .filter(|n| n.source == "self-built")
        .filter(|n| REUSABLE_PROTOS.contains(&n.protocol.as_str()))
        .filter(|n| !upstream_node_ids.contains(&n.id))
        .filter(|n| crate::domain::server_owns_addr(&server, &crate::domain::direct_addr(n)))
        .filter_map(|n| {
            let port = u16::try_from(n.port).ok()?;
            let detail = describe_deployed_protocol(&n.protocol, port, &n.params_json);
            Some(UpstreamEntryOption {
                node_id: n.id,
                protocol: n.protocol,
                port,
                label: format!("{} · {}", detail.label, port),
            })
        })
        .collect();
    deployed.sort_by(|a, b| {
        protocol_order(&a.protocol)
            .cmp(&protocol_order(&b.protocol))
            .then(a.port.cmp(&b.port))
    });
    // 主配置每协议只有一个入站；同协议的历史残留节点只保留端口最小的一条。
    deployed.dedup_by(|a, b| a.protocol == b.protocol);
    Ok(UpstreamEntryOptions {
        deployed,
        supported: REUSABLE_PROTOS.iter().map(|p| p.to_string()).collect(),
    })
}

/// 补齐复用入口需要的身份材料（uuid + 独立口令），不依赖 SSH。
///
/// 旧专用实例的材料含 x25519 私钥/公钥，复用入口一个都用不上；反过来，复用入口
/// 新建的材料没有私钥，切回专用实例时由 [`ensure_material`] 识别并重新生成。
pub(crate) fn material_for_reuse(
    conn: &rusqlite::Connection,
    id: i64,
    material_json: &str,
) -> Result<Material, String> {
    if let Ok(mut m) = serde_json::from_str::<Material>(material_json) {
        if !m.uuid.trim().is_empty() {
            if m.password.is_empty() {
                m.password = Uuid::new_v4().to_string();
                repos::upstreams::set_material(
                    conn,
                    id,
                    &serde_json::to_string(&m).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            }
            return Ok(m);
        }
    }
    let m = Material {
        uuid: Uuid::new_v4().to_string(),
        private_key: String::new(),
        public_key: String::new(),
        short_id: String::new(),
        password: Uuid::new_v4().to_string(),
    };
    repos::upstreams::set_material(
        conn,
        id,
        &serde_json::to_string(&m).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(m)
}

/// 读取这台服务器上所有「复用主入口」的出口，供主配置渲染注入。
///
/// 返回的住宅代理口令是**明文**，只在本次渲染/部署的内存里存活。旧专用实例
/// （`entry_proto` 空）不在此列——它们仍由 `run_upstream_job` 的独立配置处理。
pub fn load_routes(state: &AppState, server_id: i64) -> Result<Vec<UpstreamRoute>, String> {
    let conn = state.db.lock();
    let rows = repos::upstreams::list_by_server(&conn, server_id).map_err(|e| e.to_string())?;
    let mut routes = Vec::new();
    for u in rows {
        if u.entry_proto.is_empty() || !REUSABLE_PROTOS.contains(&u.entry_proto.as_str()) {
            continue;
        }
        let (sealed_pw, material_json) = repos::upstreams::secrets(&conn, u.id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "出口不存在".to_string())?;
        let material = material_for_reuse(&conn, u.id, &material_json)?;
        let outbound_port = u16::try_from(u.port)
            .map_err(|_| format!("出口 {} 的上游端口非法: {}", u.name, u.port))?;
        routes.push(UpstreamRoute {
            id: u.id,
            entry_proto: u.entry_proto.clone(),
            entry_port: u16::try_from(u.entry_port).unwrap_or(0),
            credential: material.uuid.clone(),
            credential_password: material.entry_password(),
            outbound_proto: u.proto.clone(),
            outbound_host: u.host.clone(),
            outbound_port,
            outbound_username: u.username.clone(),
            outbound_password: crate::crypto::open_param(&state.creds, &sealed_pw),
        });
    }
    Ok(routes)
}

/// 用主入口节点的参数 + 出口自己的身份材料，生成写回客户端的节点参数。
///
/// 关键点：**凭据必须是出口自己的**（`material.uuid`/口令），不能抄主入口那条
/// 节点的 uuid——否则所有复用同一入口的出口会在服务端变成同一个用户，路由规则
/// `user=up-N` 无法区分，出口 IP 会互相串。
pub fn reused_node_params(
    entry_proto: &str,
    main_params: &str,
    material: &Material,
) -> Result<String, String> {
    let main: serde_json::Value =
        serde_json::from_str(main_params).unwrap_or_else(|_| serde_json::json!({}));
    let s = |key: &str| -> String {
        main.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let value = |key: &str, fallback: &str| -> serde_json::Value {
        main.get(key)
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(fallback.to_string()))
    };
    let params = match entry_proto {
        "vless" => serde_json::json!({
            "uuid": material.uuid,
            "security": value("security", "reality"),
            "type": value("type", "tcp"),
            "flow": value("flow", "xtls-rprx-vision"),
            "sni": s("sni"),
            "public-key": s("public-key"),
            "short-id": s("short-id"),
            "fingerprint": value("fingerprint", "chrome"),
        }),
        "trojan" => serde_json::json!({
            "password": material.uuid,
            "security": value("security", "reality"),
            "type": value("type", "tcp"),
            "sni": s("sni"),
            "public-key": s("public-key"),
            "short-id": s("short-id"),
            "fingerprint": value("fingerprint", "chrome"),
        }),
        "vmess" => {
            let mut p = serde_json::json!({
                "uuid": material.uuid,
                "alterId": "0",
                "cipher": "auto",
                "network": value("network", "ws"),
                "path": value("path", "/ray"),
            });
            if let Some(sni) = main.get("sni") {
                p["sni"] = sni.clone();
            }
            p
        }
        "hysteria2" => serde_json::json!({
            "password": material.uuid,
            "obfs": value("obfs", "salamander"),
            "obfs-password": s("obfs-password"),
            "skip-cert-verify": "1",
        }),
        "tuic" => serde_json::json!({
            "uuid": material.uuid,
            "password": material.entry_password(),
            "skip-cert-verify": "1",
        }),
        other => {
            return Err(format!(
                "入口协议 {other} 不支持复用（可选 {}）",
                REUSABLE_PROTOS.join("/")
            ))
        }
    };
    Ok(params.to_string())
}

/// 一条出口渲染所需的全量信息（`password` 为**明文**，仅供本次渲染，不得落库或外传）。
pub struct RenderEntry {
    pub tag: String,
    pub listen_port: u16,
    pub reality_dest: String,
    pub material: Material,
    pub proto: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// 渲染专用实例的**整份**配置：每个出口一个入站 + 一个出站，routing 按 tag 配对。
///
/// 整文件重渲染在这里是安全的（不需要 relay 那套「按公钥定位、段级替换」）——
/// 因为整个实例由本工具独占，配置完全由 DB 派生，天然幂等。
pub fn render_config(entries: &[RenderEntry]) -> String {
    let inbounds: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "listen": "0.0.0.0",
                "port": e.listen_port,
                "protocol": "vless",
                "tag": e.tag,
                "settings": {
                    "clients": [{ "id": e.material.uuid, "flow": "xtls-rprx-vision" }],
                    "decryption": "none"
                },
                "streamSettings": {
                    "network": "tcp",
                    "security": "reality",
                    "realitySettings": {
                        "dest": format!("{}:443", e.reality_dest),
                        "serverNames": [e.reality_dest],
                        "privateKey": e.material.private_key,
                        "shortIds": [e.material.short_id]
                    }
                },
                // routeOnly 不是可选项，删了会静默打死一类客户端（2026-09-22 实测）：
                // 不加它时 Xray 会把目标**从 IP 改写成探测到的域名**，于是下游（住宅/上游
                // SOCKS）必须自己去解析这个名字。凡客户端发的 SNI 解析不出来——Tor 的 OR
                // 连接每条都发 `www.<17个随机字符>` ——下游就在 ~2.3 秒的 DNS 超时后静默关断，
                // 客户端只看到 unexpected eof。Tor 因此永远卡在 Bootstrapped 10%。
                //
                // 判据是同链路下的 SNI 对照：apple/microsoft/bing/google 全通，a.com 与随机
                // 域名全断，与「能否解析」一一对应；把 SNI 整个删掉则完整握手。
                //
                // 加了 routeOnly 后域名仍照常供路由规则匹配，只是**不再改写真实目标**；
                // 顺带堵住一个隐私口子——不加时被访问域名会以明文域名形式下发给上游代理，
                // 而上游出口存在的意义恰恰是「让上游只看到 IP」。
                "sniffing": { "enabled": true, "destOverride": ["http", "tls", "quic"], "routeOnly": true }
            })
        })
        .collect();

    let outbounds: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            // socks5 在 Xray 里叫 "socks"；http 就是 "http"。
            // settings 是**扁平结构**（address/port/user/pass），不是入站那种
            // servers[] 数组——写错会让 Xray 报配置错误且难以定位。
            serde_json::json!({
                "protocol": if e.proto == "http" { "http" } else { "socks" },
                "tag": outbound_tag(&e.tag),
                "settings": {
                    "address": e.host,
                    "port": e.port,
                    "user": e.username,
                    "pass": e.password
                }
            })
        })
        .collect();

    let rules: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "inboundTag": [e.tag],
                "outboundTag": outbound_tag(&e.tag)
            })
        })
        .collect();

    serde_json::json!({
        "inbounds": inbounds,
        "outbounds": outbounds,
        // 显式给内部解析器配上游：REALITY 的 dest 是域名，解析不该依赖本机
        // resolver 的实现。2026-10-28 圣克拉拉 2096 的 503 根因是 Xray 26.10.21
        // 与内置 mihomo 1.19.10 的 REALITY 不兼容，不是空 DNS；这里只是把解析路径
        // 钉死。不写 queryStrategy：专用实例的出口族没有传进来，默认 UseIP 对
        // v4-only 与 v6-only 服务器都能解析出可用的 dest。
        "dns": { "servers": ["8.8.8.8", "1.1.1.1"] },
        // AsIs：域名保持未解析交给上游代理**远端解析**——既避开 GFW 的 DNS 污染，
        // 也避免 DNS 泄漏到本机。
        "routing": { "domainStrategy": "AsIs", "rules": rules }
    })
    .to_string()
}

/// 专用实例的 systemd unit（工具自写，不依赖发布包内容；照 ss_systemd_unit 的写法）。
pub fn render_unit() -> String {
    format!(
        "[Unit]\nDescription=Xray upstream egress (managed by vpn-ctl)\nAfter=network.target\n\n\
         [Service]\nExecStart=/usr/local/bin/xray run -config {UPSTREAM_CONFIG_PATH}\n\
         Restart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

/// 下发客户端的节点参数：与 deploy 的 REALITY 节点同构（deploy.rs 的 vless 分支）。
/// **不含私钥**——私钥只在服务端。
pub fn node_params(material: &Material, reality_dest: &str) -> String {
    serde_json::json!({
        "uuid": material.uuid,
        "security": "reality",
        "type": "tcp",
        "flow": "xtls-rprx-vision",
        "sni": reality_dest,
        "public-key": material.public_key,
        "short-id": material.short_id,
        "fingerprint": "chrome"
    })
    .to_string()
}

// ---------------------------------------------------------------- 执行器（SSH）

/// 出口齐备度 → 服务动作。
#[derive(Debug, PartialEq, Eq)]
pub enum ServiceAction {
    /// 还有出口：写配置并重启
    Restart,
    /// 一条出口都不剩：停服务、删配置与 unit（不残留监听端口）
    StopAndClean,
}

pub fn service_action(existing: usize) -> ServiceAction {
    if existing == 0 {
        ServiceAction::StopAndClean
    } else {
        ServiceAction::Restart
    }
}

/// Xray 安装命令。**收敛到 `deploy::xray_install_cmd`**：这里原先自己列了一份
/// `raw/main/…` 的 URL（当时的理由只是 deploy 侧那个常量不带 pub），于是 #25
/// 「安装脚本指向可变分支」在本模块又出现一次——只修一半等于没修。
/// 现在两边同源：固定 commit + 脚本内容哈希校验都由那一处提供。
pub fn install_xray_cmd() -> String {
    crate::servers::deploy::xray_install_cmd()
}

/// 取该出口的身份材料。`material_json` 仍是占位 `{}`（首建）→ 用远端 `xray x25519`
/// 生成一套（含公钥）并落库；已有 → 直接反序列化复用。
/// 「添加出口」是纯本地操作（不需要 SSH），所以材料一律在这里补生成。
async fn ensure_material(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    state: &AppState,
    id: i64,
) -> Result<Material, String> {
    let existing_json = {
        let conn = state.db.lock();
        repos::upstreams::secrets(&conn, id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "出口不存在".to_string())?
            .1
    };
    // 复用入口的材料没有 REALITY 私钥/公钥（只有 uuid/口令）；切回专用实例时
    // 必须重新生成，否则会写出一个公钥为空的 REALITY 入站，所有客户端握手失败。
    if let Ok(m) = serde_json::from_str::<Material>(&existing_json) {
        if !m.private_key.trim().is_empty() && !m.public_key.trim().is_empty() {
            return Ok(m);
        }
    }
    let stdout = run_cmd_ok(sess, cancel, "配置", "xray x25519", CONFIG_TIMEOUT)
        .await?
        .stdout;
    let (private_key, public_key) = parse_x25519_keys(&stdout)?;
    let short_id: String = (0..8)
        .map(|_| format!("{:x}", rand::random::<u8>() % 16))
        .collect();
    let m = Material {
        uuid: Uuid::new_v4().to_string(),
        private_key,
        public_key,
        short_id,
        password: Uuid::new_v4().to_string(),
    };
    let json = serde_json::to_string(&m).map_err(|e| e.to_string())?;
    {
        let conn = state.db.lock();
        repos::upstreams::set_material(&conn, id, &json).map_err(|e| e.to_string())?;
    }
    Ok(m)
}

/// 上游出口 job 的总入口。
///
/// 两条路径：
/// - **复用主入口**（`entry_proto` 非空）：调用主部署主线把 `up-<id>` 用户/出站/路由
///   渲染进主配置，并写回客户端节点；若出口选的是尚未部署的协议，主部署顺手创建它。
/// - **旧专用实例**（`entry_proto` 空）：仍走 `xray-upstream` 独立配置（存量兼容）。
///
/// 删除复用出口后行已不在 DB，`req.cleanup_protocols` 让主部署把残留用户摘掉；
/// 即使一条复用出口都不剩，也会跑一次主部署清理，然后停掉旧专用实例。
pub(crate) async fn run_upstream_job(
    state: &Arc<AppState>,
    req: &UpstreamReq,
    cancel: &Arc<AtomicBool>,
    emit: &EmitFn,
    // None 只在单测里出现（旧专用实例路径不碰主部署）；生产 dispatch 必为 Some。
    deploy_runner: Option<&ProdRunner>,
) -> Result<(), String> {
    // 1 该机全部出口：按入口模式分组。主部署只负责复用行，旧实例只负责 legacy 行。
    emit(JobProgress {
        job_id: req.job_id,
        kind: "upstream".into(),
        step: "探测".into(),
        message: "读取中转机与出口配置".into(),
        progress: 2,
        error: None,
    });
    let (reused, deployed, has_legacy) = {
        let conn = state.db.lock();
        let rows = repos::upstreams::list_by_server(&conn, req.server_id)
            .map_err(|e| format!("读取出口失败: {e}"))?;
        let has_legacy = rows.iter().any(|u| u.entry_proto.is_empty());
        let reused: Vec<Upstream> = rows
            .into_iter()
            .filter(|u| !u.entry_proto.is_empty())
            .collect();
        drop(conn);
        let deployed =
            crate::servers::deploy::deployed_protocols(state, req.server_id).unwrap_or_default();
        (reused, deployed, has_legacy)
    };
    // 主部署要保留的协议：已部署的主协议 ∪ 复用出口的入口协议。后者保证「选了尚未
    // 部署的协议」时会被创建；前者保证其它已部署协议不会在整文件重渲染时被删掉。
    let mut protocols = deployed.clone();
    for u in &reused {
        if !protocols.contains(&u.entry_proto) {
            protocols.push(u.entry_proto.clone());
        }
    }
    // 删除复用出口：行已不在 DB，但它的入口协议仍是主协议；只要它还部署着，
    // 就必须重渲染一次才能摘掉 up-<id> 用户与住宅出站。
    let cleanup_needed = req
        .cleanup_protocols
        .iter()
        .any(|p| deployed.iter().any(|d| d == p));
    let progress_base = if !reused.is_empty() || cleanup_needed {
        // 旧专用实例还要在 75 之后处理；没有 legacy 行时主部署可以一直走到 95，
        // 最后 95→100 留给「停用旧实例（如有）」。
        let to = if has_legacy { 75 } else { 95 };
        let runner =
            deploy_runner.ok_or_else(|| "内部错误：复用入口需要主部署执行器".to_string())?;
        runner
            .run_deploy_with(
                state,
                DeployRequest {
                    job_id: req.job_id,
                    server_id: req.server_id,
                    protocols,
                    // None = 复用既有节点里的自定义伪装域名，不回落默认值。
                    reality_dest: None,
                    enable_bbr: false,
                },
                cancel,
                emit,
                DeployProgress {
                    kind: "upstream",
                    from: 5,
                    to,
                },
            )
            .await?;
        to
    } else {
        0
    };
    // 2 旧专用实例（存量）。rows 为空时它只做停用/清理，不装 Xray、不碰主配置。
    run_legacy_upstream_job(state.as_ref(), req, cancel.as_ref(), emit, progress_base).await
}

/// 旧专用实例（`xray-upstream`）的部署/拆除执行体。
/// `progress_base` 把本阶段的 5→100 映射到总进度的后半段（主部署已占用前半段）。
async fn run_legacy_upstream_job(
    state: &AppState,
    req: &UpstreamReq,
    cancel: &AtomicBool,
    emit_raw: &EmitFn,
    progress_base: i64,
) -> Result<(), String> {
    // EmitFn 吃的是 JobProgress（jobs/mod.rs），这里包一层三元组签名，与 deploy 的
    // emit_progress 同款；闭包直接叫 emit，函数体里的调用点无需再区分。
    let emit = |step: &str, message: &str, progress: i64| {
        let mapped = progress_base + progress * (100 - progress_base) / 100;
        emit_raw(JobProgress {
            job_id: req.job_id,
            kind: "upstream".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress: mapped,
            error: None,
        });
    };
    let check_cancel = || -> Result<(), String> {
        if cancel.load(Ordering::SeqCst) {
            Err("已取消".to_string())
        } else {
            Ok(())
        }
    };

    // 1 装载：中转机凭据 + 该机全部出口
    emit("探测", "读取中转机与出口配置", 5);
    let (auth, server, rows) = {
        let (auth, server) =
            load_auth(state, req.server_id).map_err(|e| format!("读取服务器失败: {e}"))?;
        let conn = state.db.lock();
        let rows: Vec<Upstream> = repos::upstreams::list_by_server(&conn, req.server_id)
            .map_err(|e| format!("读取出口失败: {e}"))?
            .into_iter()
            // 复用主入口的行由主部署负责，绝不进专用实例渲染。
            .filter(|u| u.entry_proto.is_empty())
            .collect();
        (auth, server, rows)
    };

    let mut sess = SshSession::connect(
        crate::domain::effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 没有 legacy 出口：专用实例若存在就停掉并清理，然后结束。不装 Xray、
    // 不探测 systemd——这台机器现在可能只跑复用主入口的出口。
    if rows.is_empty() {
        emit("清理", "停用旧的专用上游实例（如有）", 80);
        let _ = run_cmd(
            &mut sess,
            cancel,
            "清理",
            &format!("systemctl disable --now {UPSTREAM_SERVICE}"),
            CONFIG_TIMEOUT,
        )
        .await;
        let _ = run_cmd(
            &mut sess,
            cancel,
            "清理",
            &format!(
                "rm -f {UPSTREAM_CONFIG_PATH} {UPSTREAM_UNIT_PATH} && systemctl daemon-reload"
            ),
            CONFIG_TIMEOUT,
        )
        .await;
        emit("完成", "上游出口已就绪", 100);
        return Ok(());
    }

    // 2 探测：OS 与 systemd
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
        return Err(format!("该系统未运行 systemd（{os}），暂不支持"));
    }
    check_cancel()?;

    // 3 安装（缺或版本不符才装）。与主部署同一条版本探测：Xray 26.10.21 的
    // REALITY 与内置 mihomo 不兼容，已装新版的机器也要降级到钉住的 26.3.27。
    let xray_installed = run_cmd(
        &mut sess,
        cancel,
        "探测",
        &crate::servers::deploy_spec::xray_installed_probe_cmd(),
        PROBE_TIMEOUT,
    )
    .await?
    .exit_code
        == 0;
    if !xray_installed {
        emit("安装", "安装 Xray（可能耗时数分钟）", 30);
        run_cmd_ok(
            &mut sess,
            cancel,
            "安装",
            &install_xray_cmd(),
            INSTALL_TIMEOUT,
        )
        .await?;
    }
    check_cancel()?;

    // 4 身份材料：首建生成、此后复用（否则客户端节点每次重建都要重新导入）
    let mut material_of = std::collections::HashMap::new();
    for u in &rows {
        material_of.insert(u.id, ensure_material(&mut sess, cancel, state, u.id).await?);
    }
    check_cancel()?;

    // 5 渲染 + 下发
    match service_action(rows.len()) {
        ServiceAction::Restart => {
            emit("配置", "渲染专用实例配置", 55);
            // 解密密码并组装渲染条目（明文只在本次调用内存中存活）
            let mut entries = Vec::with_capacity(rows.len());
            for u in &rows {
                let (sealed_pw, _) = {
                    let conn = state.db.lock();
                    repos::upstreams::secrets(&conn, u.id)
                        .map_err(|e| format!("读取凭据失败: {e}"))?
                        .ok_or_else(|| "出口不存在".to_string())?
                };
                entries.push(RenderEntry {
                    tag: tag_of(u.id),
                    listen_port: u.listen_port as u16,
                    reality_dest: u.reality_dest.clone(),
                    material: material_of
                        .get(&u.id)
                        .cloned()
                        .ok_or_else(|| "身份材料缺失".to_string())?,
                    proto: u.proto.clone(),
                    host: u.host.clone(),
                    port: u.port as u16,
                    username: u.username.clone(),
                    password: crate::crypto::open_param(&state.creds, &sealed_pw),
                });
            }
            // 隐私保护**不进这份配置**：开关版只在 Tor 那侧加一层洋葱门，
            // 指向这条线原本就在监听的端口，Xray 配置一个字节都不用动。
            // （早先的「隐私入站同实例重渲染」方案已作废，见 servers/privacy.rs 顶部说明。）
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                UPSTREAM_CONFIG_PATH,
                &render_config(&entries),
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                UPSTREAM_UNIT_PATH,
                &render_unit(),
                CONFIG_TIMEOUT,
            )
            .await?;
            // 放行监听端口：**自己开的端口自己放行**。服务器的 INPUT 默认丢弃时，
            // 少这一步的表现是客户端「延迟测试超时」（HTTP 504）——而实例、配置、进程
            // 全是好的，只是包进不来（2026-09-21 实测：8864 上 xray 在听，本机连它超时，
            // 手工加上这条 iptables 规则后立刻连通）。幂等，已放行则不动。
            check_cancel()?;
            emit("放行", "放行监听端口", 75);
            // 与协议部署同一条持久化放行命令（ufw 优先、netfilter-persistent 保存）：
            // `ensure_open_cmd` 只写运行时 iptables，重启后规则全丢——2026-10-23
            // ashura 重启后主入站（部署时已改持久化）全通、六个上游出口节点全超时，
            // 正是这条差异造成的。
            let ports: Vec<(&str, u16)> = rows
                .iter()
                .filter_map(|u| u16::try_from(u.listen_port).ok())
                .map(|port| ("vless", port))
                .collect();
            let allow_cmd = crate::servers::deploy_spec::firewall_allow_cmd(&ports);
            if !allow_cmd.is_empty() {
                if let Err(e) =
                    run_cmd_ok(&mut sess, cancel, "放行", &allow_cmd, CONFIG_TIMEOUT).await
                {
                    // 这里不判失败：可能没装 iptables，也可能本就由别处（云侧镜像/ufw）放行。
                    // 真正的判据是最后那次「从本机连它」——那才是客户端要走的路径。
                    emit(
                        "放行",
                        &format!("放行命令未成功（{e}），继续，稍后用可达性核验判定"),
                        76,
                    );
                }
            }
            check_cancel()?;
            emit("启动", "启动专用实例", 80);
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                &format!(
                    "systemctl daemon-reload && systemctl enable --now {UPSTREAM_SERVICE} \
                     && systemctl restart {UPSTREAM_SERVICE}"
                ),
                CONFIG_TIMEOUT,
            )
            .await?;
            // 启动命令返回 0 **不代表实例起来了**：Type=simple 的服务只要进程被 spawn
            // 就返回 0，Xray 因端口被占立刻退出时它照样返回 0——于是 job 报 done/100%、
            // 出口行看着一切正常，而远端没有实例在服务（2026-09-21 实测：监听端口填了
            // 443，撞上同机 deploy 管的协议入站；客户端连过去被那个实例用另一套 REALITY
            // 密钥拒掉，节点表现为「延迟测试失败: HTTP 503」）。按状态判成败。
            emit("启动", "核验实例状态", 88);
            if let Err(e) = wait_service_active(&mut sess, cancel, UPSTREAM_SERVICE).await {
                return Err(format!(
                    "专用实例没起来（{e}）。最常见的原因是监听端口已被本机其它服务占用——\
                     deploy 管的协议入站也占端口，本机节点端口就是它：{ports:?}。\
                     改一个空闲端口后重新部署即可。",
                    ports = rows.iter().map(|u| u.listen_port).collect::<Vec<_>>()
                ));
            }
            // 远端「实例活着」还不等于客户端连得上：**从本机连一次它的端口**，那才是
            // 客户端要走的路径。连不上时实例/配置/进程全是对的，唯一错的是包进不来，
            // 而它以前只表现为客户端那句「延迟测试失败: HTTP 504」——让用户对着一个
            // 超时自己猜是端口、密钥还是线路。原因写到行上（UI 红字），不判 job 失败
            // （也可能只是本机网络抖了一下）。
            emit("核验", "从本机探测端口可达性", 90);
            let mut unreachable: Vec<(i64, String)> = Vec::new();
            for u in &rows {
                if let Err(e) = crate::server_speed::phase1::tcp_latency_ms(
                    crate::domain::effective_host(&server),
                    u.listen_port as u16,
                    1,
                )
                .await
                {
                    unreachable.push((
                        u.id,
                        format!(
                            "{INBOUND_PROBE_ERROR_PREFIX}{}:{}（{e}）：服务器上实例与配置正常，\
                             卡在**入站放行**——本工具已尝试在服务器上放行该端口，若仍不通就查\
                             云服务商的安全组",
                            crate::domain::effective_host(&server),
                            u.listen_port
                        ),
                    ));
                }
            }
            {
                let conn = state.db.lock();
                for u in &rows {
                    let msg = unreachable
                        .iter()
                        .find(|(id, _)| *id == u.id)
                        .map(|(_, m)| m.as_str());
                    // egress_ip 原样带回：这次核验与它无关，别把已有读数抹掉
                    let egress = u.egress_ip.as_deref();
                    match msg {
                        Some(msg) => {
                            let _ = repos::upstreams::set_probe(&conn, u.id, egress, Some(msg));
                        }
                        // 本次可达：只清「同一来源」的旧错误。出口 IP 探测的失败原因
                        // 来自另一条链路，不能被入站核验的成功抹掉。
                        None if should_clear_stale_inbound_error(u.last_check_error.as_deref()) => {
                            let _ = repos::upstreams::set_probe(&conn, u.id, egress, None);
                        }
                        None => {}
                    }
                }
            }
        }
        ServiceAction::StopAndClean => {
            emit("拆除", "停用并清理专用实例", 80);
            // 停服务 + 清文件都不上抛：服务本就可能没起过（从没部署成功就删了出口），
            // 此时 disable 会失败，但那不影响「最终状态是干净的」这个结论。
            let _ = run_cmd(
                &mut sess,
                cancel,
                "拆除",
                &format!("systemctl disable --now {UPSTREAM_SERVICE}"),
                CONFIG_TIMEOUT,
            )
            .await;
            let _ = run_cmd(
                &mut sess,
                cancel,
                "拆除",
                &format!(
                    "rm -f {UPSTREAM_CONFIG_PATH} {UPSTREAM_UNIT_PATH} && systemctl daemon-reload"
                ),
                CONFIG_TIMEOUT,
            )
            .await;
        }
    }
    check_cancel()?;

    // 6 写回节点：按 upstreams.node_id 定位。**不得**按 protocol+addr 删——
    // 同一中转机上所有出口的节点协议与地址都相同，会把兄弟出口的节点一起删掉。
    let mut wrote_nodes = false;
    if service_action(rows.len()) == ServiceAction::Restart {
        emit("写回", "写入节点", 95);
        // 锁只在这个块里：MutexGuard 不是 Send，跨 await 过不去（下面的配置热更新要 await）
        let conn = state.db.lock();
        for u in &rows {
            let material = material_of
                .get(&u.id)
                .cloned()
                .ok_or_else(|| "身份材料缺失".to_string())?;
            let node = Node {
                id: u
                    .node_id
                    .clone()
                    .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
                name: format!("{}-{}", server.name, u.name),
                source: "self-built".into(),
                protocol: "vless".into(),
                addr: crate::domain::effective_host(&server).to_string(),
                port: u.listen_port,
                params_json: node_params(&material, &u.reality_dest),
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
            };
            repos::nodes::upsert(&conn, &node, &state.creds).map_err(|e| e.to_string())?;
            if u.node_id.is_none() {
                repos::upstreams::set_node_id(&conn, u.id, &node.id).map_err(|e| e.to_string())?;
            }
        }
        drop(conn);
        wrote_nodes = true;
    }
    if wrote_nodes {
        // 节点变了，**运行中的客户端必须跟上**：否则新节点只存在于库里和测速侧车里
        // （侧车按需注入节点，所以「自动验证」能测通），而 7890 上那份配置还是「开网络」
        // 那一刻生成的——下拉里能选到它，一切换 mihomo 就回
        // `HTTP 400 Selector update error: proxy not exist`（2026-09-21 实测）。
        //
        // 与订阅同步那种后台路径不同：这是一次用户显式动作（添加/重新部署出口），
        // 重启客户端的代价换「点了就能用」是划算的。
        let _ = state.subs.rebuild_config().await;
        let _ = crate::net::runtime::apply_config_change(state).await;
    }

    emit("完成", "上游出口已就绪", 100);
    Ok(())
}
