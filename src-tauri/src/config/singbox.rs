//! sing-box 全协议导出模块：启用节点 → sing-box config.json（配置工厂的第二出口，
//! 与 build_yaml 的 Clash YAML 出口并列）。输出为完整顶层结构（log/dns/inbounds/
//! outbounds/route），节点 outbounds 按名称排序保证确定性（serde_json Map 为
//! BTreeMap，键序亦确定）。
//!
//! 出站字段名以 sing-box 官方文档（v1.11.4，2026-08-14 context7 核实）为准：
//! shadowsocks(server/server_port/method/password)、vmess(server/server_port/uuid/
//! security/alter_id)、vless(server/server_port/uuid/flow)、trojan(server/server_port/
//! password)、hysteria2(server/server_port/password/up_mbps/down_mbps/
//! obfs{type,password}/tls{enabled,server_name,insecure})。
//!
//! 来源策略：clash_raw 是 Clash 字段体系，不能透传——解析 YAML 片段后按映射表取
//! 基础字段（type 改名 ss→shadowsocks、server、port→server_port、cipher→method 仅 ss、
//! password/uuid 直传），其余键忽略不映射（原始 params_json 不丢失）；URI 来源按
//! 协议映射基础字段。传输层映射 ws/grpc 的 transport 对象（VMess 取 network 键，
//! VLESS/Trojan 取 type 键）；tls 映射
//! enabled/server_name/insecure，REALITY 节点另映射 `tls.reality` 与 uTLS 指纹——
//! 少了它们客户端握手会直接失败。DNS 采用 sing-box 1.12+ 的对象形式（字符串形式在
//! 1.13 起 FATAL）。未知键一律忽略。
//!
//! ssr 协议（sing-box 自 1.6.0 起移除 SSR 支持）与 build_yaml 同策略跳过并计数，
//! clash_raw type 为 ssr 同样不豁免。

use crate::config::ConfigError;
use crate::domain::{Node, Privacy};
use serde_json::{Map, Value};

/// selector 组 tag：route.final 指向该组（与 Clash 导出「自动选择」同名同构）。
/// pub(crate)：导出空配置守卫依据 tag 排除两组基础设施出站（commands::ensure_singbox_has_nodes）。
pub(crate) const SELECTOR_TAG: &str = "自动选择";
/// urltest 组 tag：含全部节点 tag，selector 指向该组。
pub(crate) const URLTEST_TAG: &str = "自动测速";
/// mixed 入站监听端口（常量，后续可配置化——P7 不做 UI 配置）。
const MIXED_LISTEN_PORT: i64 = 7890;
/// hysteria2 up/down Mbps 缺省值（URI 未携带时）。
const DEFAULT_MBPS: i64 = 100;

/// 生成 sing-box config.json；节点按名称排序（与 build_yaml 一致），
/// 返回 (json 字符串, 跳过的 ssr 节点数)。调用方：subs/service.rs rebuild_config
/// 重建缓存（同 build_yaml），导出命令从缓存读。
#[cfg(all(test, feature = "local-tests"))]
pub fn build_singbox(nodes: &[Node]) -> Result<(String, usize), ConfigError> {
    build_singbox_with_privacy(nodes, &[])
}

/// 同 [`build_singbox`]，但带上隐私开关：开了隐私的线**跳过并计数**。
///
/// 为什么跳过而不是映射：v1 的隐私只支持 mihomo 侧（靠 `dialer-proxy` 链到本机 Tor）。
/// 在 sing-box 里照原样映射会生成一条**直连 VPS 公网 IP** 的配置——那正是隐私模式
/// 要藏起来的东西，比不导出危险得多。与 ssr 同策略。
pub fn build_singbox_with_privacy(
    nodes: &[Node],
    privacies: &[Privacy],
) -> Result<(String, usize), ConfigError> {
    let mut enabled: Vec<&Node> = nodes.iter().filter(|n| n.enabled).collect();
    enabled.sort_by(|a, b| a.name.cmp(&b.name));

    let mut outbounds = vec![];
    let mut skipped_ssr = 0usize;
    for n in &enabled {
        match node_outbound(n, privacies)? {
            Some(o) => outbounds.push(o),
            None => skipped_ssr += 1,
        }
    }
    let node_tags: Vec<String> = outbounds
        .iter()
        .map(|o| {
            o["tag"]
                .as_str()
                .expect("本模块生成的出站必有 tag")
                .to_string()
        })
        .collect();
    outbounds.push(url_test_group(&node_tags));
    outbounds.push(select_group());

    let root = serde_json::json!({
        "log": { "level": "info" },
        // sing-box 1.12+ 只接受对象形式的 DNS server；字符串形式在 1.13 起直接
        // FATAL（真机用 1.13.20 / 1.14.3 验证）。用 IP 避免 TLS 域名需要额外
        // domain_resolver。
        "dns": { "servers": [
            { "type": "udp", "server": "1.1.1.1" },
            { "type": "udp", "server": "8.8.8.8" },
        ] },
        "inbounds": [{
            "type": "mixed",
            "listen": "127.0.0.1",
            "listen_port": MIXED_LISTEN_PORT,
        }],
        "outbounds": outbounds,
        "route": { "final": SELECTOR_TAG },
    });
    serde_json::to_string_pretty(&root)
        .map(|json| (json, skipped_ssr))
        .map_err(|e| ConfigError::BuildFailed(e.to_string()))
}

/// 单节点 → sing-box 出站 JSON，tag 恒为节点名。返回 None 表示 ssr 跳过
/// （build_singbox 计数，与 build_yaml 的 ssr 预判语义一致）。
fn node_outbound(n: &Node, privacies: &[Privacy]) -> Result<Option<Value>, ConfigError> {
    // ssr：sing-box 与 mihomo 均已移除 SSR 支持，映射会丢失协议伪装层（静默失真），
    // 一律跳过并计数（含 clash_raw 片段）
    if n.protocol == "ssr" {
        return Ok(None);
    }
    // 隐私线：v1 只支持 mihomo 侧的 Tor 走法。在 sing-box 里照原样映射会生成一条
    // **直连 VPS 公网 IP** 的配置——那正是隐私模式要藏的东西，比不导出危险得多。
    if crate::servers::privacy::effective_privacy(&n.id, privacies).is_some() {
        return Ok(None);
    }
    let params: Value = serde_json::from_str(&n.params_json).map_err(|e| {
        ConfigError::BuildFailed(format!("节点 {} params_json 解析失败: {e}", n.name))
    })?;
    if let Some(raw_str) = params.get("clash_raw").and_then(|v| v.as_str()) {
        return raw_outbound(raw_str, &n.name);
    }
    let outbound = match n.protocol.as_str() {
        "ss" => shadowsocks_outbound(n, &params),
        "vmess" => vmess_outbound(n, &params),
        "vless" => vless_outbound(n, &params),
        "trojan" => trojan_outbound(n, &params),
        "hysteria2" => hysteria2_outbound(n, &params),
        "tuic" => tuic_outbound(n, &params),
        "wireguard" => wireguard_outbound(n, &params),
        other => {
            return Err(ConfigError::BuildFailed(format!(
                "未知协议且无 clash_raw: {other}"
            )))
        }
    };
    Ok(Some(outbound))
}

/// clash_raw YAML 片段 → sing-box 出站。Clash 与 sing-box 字段体系不同不能透传：
/// 只按映射表取基础字段（type 改名、server、port→server_port、cipher→method 仅 ss、
/// password/uuid 直传），其余键忽略。返回 None 表示 raw type 为 ssr（跳过计数）。
fn raw_outbound(raw: &str, tag: &str) -> Result<Option<Value>, ConfigError> {
    let frag: serde_yaml::Value = serde_yaml::from_str(raw)
        .map_err(|e| ConfigError::BuildFailed(format!("clash_raw 解析失败: {e}")))?;
    let m = frag
        .as_mapping()
        .ok_or_else(|| ConfigError::BuildFailed("clash_raw 不是映射".into()))?;
    let raw_type = m
        .get(serde_yaml::Value::String("type".into()))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ConfigError::BuildFailed("clash_raw 缺少 type 字段".into()))?;
    let sb_type = match raw_type {
        "ss" => "shadowsocks",
        "ssr" => return Ok(None),
        "vmess" | "vless" | "trojan" | "hysteria2" => raw_type,
        other => {
            return Err(ConfigError::BuildFailed(format!(
                "clash_raw 含未知协议: {other}"
            )))
        }
    };
    let mut out = Map::new();
    out.insert("type".into(), Value::String(sb_type.into()));
    out.insert("tag".into(), Value::String(tag.into()));
    for (raw_key, sb_key) in [("server", "server"), ("port", "server_port")] {
        if let Some(v) = m.get(serde_yaml::Value::String(raw_key.into())) {
            out.insert(
                sb_key.into(),
                serde_json::to_value(v).map_err(|e| {
                    ConfigError::BuildFailed(format!("clash_raw {raw_key} 序列化失败: {e}"))
                })?,
            );
        }
    }
    if raw_type == "ss" {
        if let Some(v) = m.get(serde_yaml::Value::String("cipher".into())) {
            out.insert(
                "method".into(),
                serde_json::to_value(v).map_err(|e| {
                    ConfigError::BuildFailed(format!("clash_raw cipher 序列化失败: {e}"))
                })?,
            );
        }
    }
    for key in ["password", "uuid"] {
        if let Some(v) = m.get(serde_yaml::Value::String(key.into())) {
            out.insert(
                key.into(),
                serde_json::to_value(v).map_err(|e| {
                    ConfigError::BuildFailed(format!("clash_raw {key} 序列化失败: {e}"))
                })?,
            );
        }
    }
    Ok(Some(Value::Object(out)))
}

fn shadowsocks_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("shadowsocks".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    m.insert("method".into(), Value::String(str_param(params, "cipher")));
    m.insert(
        "password".into(),
        Value::String(str_param(params, "password")),
    );
    Value::Object(m)
}

fn vmess_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("vmess".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    m.insert("uuid".into(), Value::String(str_param(params, "uuid")));
    // security：URI 原生键 security 优先，回落 cipher（vmess URI 的加密键为 cipher），缺省 auto
    let mut security = str_param(params, "security");
    if security.is_empty() {
        security = str_param(params, "cipher");
    }
    if security.is_empty() {
        security = "auto".into();
    }
    m.insert("security".into(), Value::String(security));
    m.insert(
        "alter_id".into(),
        Value::from(int_param(params, "alterId", 0)),
    );
    // tls 基础：URI 原生 tls 键为 "tls" 时启用
    if str_param(params, "tls") == "tls" {
        m.insert("tls".into(), tls_basic(params));
    }
    if let Some(t) = outbound_transport(params, "network") {
        m.insert("transport".into(), t);
    }
    Value::Object(m)
}

fn vless_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("vless".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    m.insert("uuid".into(), Value::String(str_param(params, "uuid")));
    // flow 仅当 URI 携带时输出
    let flow = str_param(params, "flow");
    if !flow.is_empty() {
        m.insert("flow".into(), Value::String(flow));
    }
    // tls 基础：security=tls/reality 启用
    if matches!(str_param(params, "security").as_str(), "tls" | "reality") {
        m.insert("tls".into(), tls_basic(params));
    }
    if let Some(t) = outbound_transport(params, "type") {
        m.insert("transport".into(), t);
    }
    Value::Object(m)
}

fn trojan_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("trojan".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    m.insert(
        "password".into(),
        Value::String(str_param(params, "password")),
    );
    if matches!(str_param(params, "security").as_str(), "tls" | "reality") {
        m.insert("tls".into(), tls_basic(params));
    }
    if let Some(t) = outbound_transport(params, "type") {
        m.insert("transport".into(), t);
    }
    Value::Object(m)
}

fn hysteria2_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("hysteria2".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    m.insert(
        "password".into(),
        Value::String(str_param(params, "password")),
    );
    // up/down：URI 携带 Mbps 值（数字或字符串）则用，否则默认 100
    m.insert("up_mbps".into(), mbps_param(params, "up"));
    m.insert("down_mbps".into(), mbps_param(params, "down"));
    // obfs：obfs 键非空时输出 {type, password}；obfs-password 缺省则省略 password
    let obfs = str_param(params, "obfs");
    if !obfs.is_empty() {
        let mut o = Map::new();
        o.insert("type".into(), Value::String(obfs));
        let obfs_pw = str_param(params, "obfs-password");
        if !obfs_pw.is_empty() {
            o.insert("password".into(), Value::String(obfs_pw));
        }
        m.insert("obfs".into(), Value::Object(o));
    }
    // hysteria2 恒为 TLS（QUIC+TLS 设计）：tls 对象始终输出
    m.insert("tls".into(), tls_basic(params));
    Value::Object(m)
}

/// TUIC 出站：uuid/password/username 直传,congestion_control 与 udp_relay_mode 可带,恒含 tls。
fn tuic_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("tuic".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    m.insert("uuid".into(), Value::String(str_param(params, "uuid")));
    m.insert(
        "password".into(),
        Value::String(str_param(params, "password")),
    );
    let cc = str_param(params, "congestion-control");
    if !cc.is_empty() {
        m.insert("congestion_control".into(), Value::String(cc));
    }
    let udp_relay = str_param(params, "udp-relay-mode");
    if !udp_relay.is_empty() {
        m.insert("udp_relay_mode".into(), Value::String(udp_relay));
    }
    m.insert("tls".into(), tls_basic(params));
    Value::Object(m)
}

/// WireGuard 出站：server/server_port + 密钥/地址字段(缺省省略,域名可解析由用户负责)。
fn wireguard_outbound(n: &Node, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("wireguard".into()));
    m.insert("tag".into(), Value::String(n.name.clone()));
    m.insert("server".into(), Value::String(n.addr.clone()));
    m.insert("server_port".into(), Value::from(n.port));
    let ip = str_param(params, "ip");
    if !ip.is_empty() {
        m.insert("local_address".into(), Value::String(ip));
    }
    let priv_key = str_param(params, "private-key");
    if !priv_key.is_empty() {
        m.insert("private_key".into(), Value::String(priv_key));
    }
    let pub_key = str_param(params, "public-key");
    if !pub_key.is_empty() {
        m.insert("peer_public_key".into(), Value::String(pub_key));
    }
    let psk = str_param(params, "preshared-key");
    if !psk.is_empty() {
        m.insert("peer_pre_shared_key".into(), Value::String(psk));
    }
    Value::Object(m)
}

/// 内部传输参数 → sing-box transport 对象。
///
/// 键名分两套：VMess 用 `network`，VLESS/Trojan 用 `type`，由调用方传入。
/// 只映射 ws/grpc——其它传输（http/quic 等）本项目尚无生成路径，宁可不输出，
/// 也不伪造 sing-box 不认识的字段。
fn outbound_transport(params: &Value, network_key: &str) -> Option<Value> {
    let network = str_param(params, network_key);
    match network.as_str() {
        "ws" => {
            let mut t = Map::new();
            t.insert("type".into(), Value::String("ws".into()));
            let path = str_param(params, "path");
            if !path.is_empty() {
                t.insert("path".into(), Value::String(path));
            }
            let host = str_param(params, "host");
            if !host.is_empty() {
                t.insert("headers".into(), serde_json::json!({ "Host": host }));
            }
            Some(Value::Object(t))
        }
        "grpc" => {
            let mut t = Map::new();
            t.insert("type".into(), Value::String("grpc".into()));
            // URI 原生键为 serviceName；Clash 来源的 clash_raw 走另一条映射路径
            let service = str_param(params, "serviceName");
            if !service.is_empty() {
                t.insert("service_name".into(), Value::String(service));
            }
            Some(Value::Object(t))
        }
        _ => None,
    }
}

/// tls 基础对象：enabled 恒 true，server_name 取 sni，insecure 由
/// skip-cert-verify/allowInsecure/insecure 任一为 "true"/"1" 判定；只输出可确定的字段。
///
/// REALITY 节点必须带 `tls.reality`（公钥/short_id），否则 sing-box 会把它当普通
/// TLS 握手，连接在证书阶段就失败。uTLS 指纹也只在参数里明确给出时输出。
fn tls_basic(params: &Value) -> Value {
    let mut t = Map::new();
    t.insert("enabled".into(), Value::Bool(true));
    // VMess 的 SNI 按 mihomo 字段叫 servername，兼容读取两种键
    let mut sni = str_param(params, "sni");
    if sni.is_empty() {
        sni = str_param(params, "servername");
    }
    if !sni.is_empty() {
        t.insert("server_name".into(), Value::String(sni));
    }
    if skip_cert_verify_true(params) {
        t.insert("insecure".into(), Value::Bool(true));
    }
    let mut fingerprint = str_param(params, "fingerprint");
    if fingerprint.is_empty() {
        fingerprint = str_param(params, "client-fingerprint");
    }
    if !fingerprint.is_empty() {
        t.insert(
            "utls".into(),
            serde_json::json!({ "enabled": true, "fingerprint": fingerprint }),
        );
    }
    if str_param(params, "security") == "reality" {
        let mut r = Map::new();
        r.insert("enabled".into(), Value::Bool(true));
        let public_key = str_param(params, "public-key");
        if !public_key.is_empty() {
            r.insert("public_key".into(), Value::String(public_key));
        }
        let short_id = str_param(params, "short-id");
        if !short_id.is_empty() {
            r.insert("short_id".into(), Value::String(short_id));
        }
        t.insert("reality".into(), Value::Object(r));
    }
    Value::Object(t)
}

/// 跳过证书校验判定：URI 原生键 skip-cert-verify/allowInsecure/insecure，值 "true"/"1" 为真。
fn skip_cert_verify_true(params: &Value) -> bool {
    ["skip-cert-verify", "allowInsecure", "insecure"]
        .iter()
        .any(|k| matches!(str_param(params, k).as_str(), "true" | "1"))
}

/// urltest 组：含全部节点 tag，selector 经其自动测速选优。
fn url_test_group(tags: &[String]) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("urltest".into()));
    m.insert("tag".into(), Value::String(URLTEST_TAG.into()));
    let refs: Vec<Value> = tags.iter().map(|t| Value::String(t.clone())).collect();
    m.insert("outbounds".into(), Value::Array(refs));
    Value::Object(m)
}

/// selector 组：route.final 指向，下级仅 urltest 组（与 Clash 导出「自动选择」同构）。
fn select_group() -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("selector".into()));
    m.insert("tag".into(), Value::String(SELECTOR_TAG.into()));
    m.insert(
        "outbounds".into(),
        Value::Array(vec![Value::String(URLTEST_TAG.into())]),
    );
    Value::Object(m)
}

fn str_param(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 整数参数：数字直取；字符串解析失败或缺失用 default（vmess alterId 等）。
fn int_param(params: &Value, key: &str, default: i64) -> i64 {
    if let Some(v) = params.get(key) {
        if let Some(n) = v.as_i64() {
            return n;
        }
        if let Some(s) = v.as_str() {
            if let Ok(n) = s.parse() {
                return n;
            }
        }
    }
    default
}

/// Mbps 参数（hysteria2 up/down）：数字直取；字符串先按整数解析再按小数；缺失或
/// 不可解析用 DEFAULT_MBPS。
fn mbps_param(params: &Value, key: &str) -> Value {
    if let Some(v) = params.get(key) {
        if let Some(n) = v.as_i64() {
            return Value::from(n);
        }
        if let Some(n) = v.as_f64() {
            return Value::from(n);
        }
        if let Some(s) = v.as_str() {
            if let Ok(n) = s.parse::<i64>() {
                return Value::from(n);
            }
            if let Ok(n) = s.parse::<f64>() {
                return Value::from(n);
            }
        }
    }
    Value::from(DEFAULT_MBPS)
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/config/singbox_mods.rs"
));
