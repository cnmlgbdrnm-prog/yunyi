//! 订阅解析器：识别 base64 URI 列表、Clash YAML 与 sing-box JSON 三种格式，
//! 解析为统一 ParsedNode。URI 列表支持 vmess/ss/ssr/trojan/vless/hysteria2 六类协议；
//! Clash 来源节点保留原始 YAML 片段（clash_raw），配置生成时原样输出以免协议信息失真。
//! sing-box JSON 解析 `outbounds` 里的代理出站（selector/urltest/direct/block 跳过）。

use crate::subs::SubError;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::Map;
use serde_json::Value;

/// 订阅格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubFormat {
    /// 每行一个协议 URI（可整体 base64 包裹）
    UriList,
    /// Clash/Mihomo YAML（proxies 列表）
    ClashYaml,
    /// sing-box JSON（outbounds 列表）
    SingBoxJson,
}

/// 解析后的统一节点表示。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNode {
    pub name: String,
    pub protocol: String,
    pub addr: String,
    pub port: i64,
    pub params: Map<String, Value>,
    /// Clash YAML 来源的原始 proxy 片段（YAML 字符串）
    pub clash_raw: Option<String>,
}

/// 识别订阅内容格式。
pub fn detect(bytes: &[u8]) -> Result<SubFormat, SubError> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_start();
    if trimmed.starts_with("proxies:") {
        return Ok(SubFormat::ClashYaml);
    }
    if looks_like_uri_list(&text) {
        return Ok(SubFormat::UriList);
    }
    // sing-box 客户端配置：顶层是 JSON 对象且带 outbounds 数组。
    // 必须真正解析一次，不能只看首字符 `{`——否则损坏的 JSON 会被判成
    // sing-box 而报“没有出站”，掩盖真正的语法错误。JSON 语法错误直接报
    // ParseFailed，而不是笼统的“不支持的订阅格式”（用户复制链接常截断）。
    if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| SubError::ParseFailed(format!("订阅返回的 JSON 无法解析: {e}")))?;
        if value.get("outbounds").and_then(Value::as_array).is_some() {
            return Ok(SubFormat::SingBoxJson);
        }
        return Err(SubError::ParseFailed(
            "订阅返回的 JSON 缺少 outbounds 列表".into(),
        ));
    }
    // 整体 base64 包裹的 URI 列表
    if let Ok(decoded) = STANDARD.decode(trimmed.replace(['\r', '\n'], "")) {
        let inner = String::from_utf8_lossy(&decoded);
        if looks_like_uri_list(&inner) {
            return Ok(SubFormat::UriList);
        }
    }
    Err(SubError::UnsupportedFormat)
}

fn looks_like_uri_list(text: &str) -> bool {
    text.lines().filter(|l| !l.trim().is_empty()).all(|l| {
        [
            "vmess://",
            "vless://",
            "trojan://",
            "hysteria2://",
            "tuic://",
            "ss://",
            "ssr://",
        ]
        .iter()
        .any(|p| l.trim_start().starts_with(p))
    }) && !text.trim().is_empty()
}

/// 解析订阅内容为节点列表。
pub fn parse(bytes: &[u8], format: SubFormat) -> Result<Vec<ParsedNode>, SubError> {
    match format {
        SubFormat::ClashYaml => parse_clash_yaml(bytes),
        SubFormat::UriList => parse_uri_list(bytes),
        SubFormat::SingBoxJson => parse_singbox_json(bytes),
    }
}

fn parse_uri_list(bytes: &[u8]) -> Result<Vec<ParsedNode>, SubError> {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    // 整体 base64 包裹时先解开
    if let Ok(decoded) = STANDARD.decode(text.replace(['\r', '\n'], "")) {
        let inner = String::from_utf8_lossy(&decoded);
        if looks_like_uri_list(&inner) {
            text = inner.into_owned();
        }
    }
    let mut out = vec![];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(parse_uri(line)?);
    }
    Ok(out)
}

fn parse_uri(uri: &str) -> Result<ParsedNode, SubError> {
    // 按字符截断而非字节，避免截断点落在多字节字符中间导致 panic
    let err = || {
        SubError::ParseFailed(format!(
            "无法解析 URI: {}",
            uri.chars().take(32).collect::<String>()
        ))
    };
    if let Some(b64) = uri.strip_prefix("vmess://") {
        let json_bytes = STANDARD.decode(b64).map_err(|_| err())?;
        let v: Value = serde_json::from_slice(&json_bytes).map_err(|_| err())?;
        let mut params = Map::new();
        params.insert("uuid".into(), v.get("id").cloned().unwrap_or_default());
        params.insert(
            "alterId".into(),
            v.get("aid").cloned().unwrap_or_else(|| Value::from("0")),
        );
        params.insert(
            "cipher".into(),
            v.get("scy").cloned().unwrap_or_else(|| Value::from("auto")),
        );
        if let Some(net) = v.get("net") {
            params.insert("network".into(), net.clone());
        }
        if let Some(tls) = v.get("tls") {
            params.insert("tls".into(), tls.clone());
        }
        if let Some(host) = v.get("host") {
            params.insert("host".into(), host.clone());
        }
        if let Some(path) = v.get("path") {
            params.insert("path".into(), path.clone());
        }
        let port: i64 = v
            .get("port")
            .and_then(|p| p.as_str().and_then(|s| s.parse().ok()))
            .or_else(|| v.get("port").and_then(|p| p.as_i64()))
            .ok_or_else(err)?;
        return Ok(ParsedNode {
            name: v
                .get("ps")
                .and_then(|p| p.as_str())
                .unwrap_or("未命名节点")
                .to_string(),
            protocol: "vmess".into(),
            addr: v
                .get("add")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string(),
            port,
            params,
            clash_raw: None,
        });
    }
    if let Some(rest) = uri.strip_prefix("ss://") {
        // 去掉 #名称 与 ?插件参数；机场订阅的 fragment 常为百分号编码，
        // 与 trojan/vless 分支同一解码器（& = + 保持原义），空则回落默认名
        let (body, name) = match rest.split_once('#') {
            Some((b, n)) => {
                let name = percent_decode(n);
                (
                    b,
                    if name.is_empty() {
                        "未命名节点".to_string()
                    } else {
                        name
                    },
                )
            }
            None => (rest, "未命名节点".to_string()),
        };
        let body = body.split('?').next().unwrap_or(body);
        let (userinfo, hostport) = match body.rsplit_once('@') {
            Some(parts) => (parts.0.to_string(), parts.1.to_string()),
            // 传统格式：整体 base64 后再切 userinfo@host
            None => {
                let decoded = URL_SAFE.decode(body).map_err(|_| err())?;
                let full = String::from_utf8_lossy(&decoded);
                let (u, h) = full.rsplit_once('@').ok_or_else(err)?;
                (u.to_string(), h.to_string())
            }
        };
        let decoded_ui = decode_ss_userinfo(&userinfo)?;
        let ui = String::from_utf8_lossy(&decoded_ui);
        let (cipher, password) = ui.split_once(':').ok_or_else(err)?;
        let (addr, port) = hostport.rsplit_once(':').ok_or_else(err)?;
        let mut params = Map::new();
        params.insert("cipher".into(), Value::from(cipher.to_string()));
        params.insert("password".into(), Value::from(password.to_string()));
        params.insert("udp".into(), Value::from(true));
        return Ok(ParsedNode {
            name,
            protocol: "ss".into(),
            addr: addr.to_string(),
            port: port.parse().map_err(|_| err())?,
            params,
            clash_raw: None,
        });
    }
    if let Some(b64) = uri.strip_prefix("ssr://") {
        // SSR 链接整体 base64 包裹：外层解出 host:port:protocol:method:obfs:base64(password)，
        // 查询段（obfsparam/protoparam/remarks/group）各值再独立 base64
        let decoded_bytes = decode_ss_userinfo(b64).map_err(|_| err())?;
        let decoded = String::from_utf8_lossy(&decoded_bytes);
        // 主体与查询以 ? 分隔；标准链接主体以 / 收尾（ssr://<b64>/?obfsparam=...）
        let (body, query) = match decoded.split_once('?') {
            Some((b, q)) => (b.trim_end_matches('/'), Some(q)),
            None => (decoded.as_ref(), None),
        };
        // host:port:protocol:method:obfs:base64pw 六段，缺段即无法解析（IPv6 不在 SSR 生态）
        let parts: Vec<&str> = body.splitn(6, ':').collect();
        if parts.len() < 6 {
            return Err(err());
        }
        let addr = parts[0];
        let port: i64 = parts[1].parse().map_err(|_| err())?;
        let protocol = parts[2];
        let method = parts[3];
        let obfs = parts[4];
        // password 字段内层再次 base64；解码失败必须报错并带原因，不静默放原值
        let password = decode_ss_userinfo(parts[5]).map_err(|_| {
            SubError::ParseFailed(format!(
                "无法解析 URI: {}（ssr 密码字段解码失败）",
                uri.chars().take(32).collect::<String>()
            ))
        })?;
        let password = String::from_utf8_lossy(&password).into_owned();
        let mut name = "未命名节点".to_string();
        let mut params = Map::new();
        if let Some(query) = query {
            for pair in query.split('&').filter(|p| !p.is_empty()) {
                if let Some((key, value)) = pair.split_once('=') {
                    // 查询参数值内层 base64 解码，失败保留原值（机场订阅存在非标准编码，不致命）
                    let value = decode_ss_userinfo(value)
                        .map(|v| String::from_utf8_lossy(&v).into_owned())
                        .unwrap_or_else(|_| value.to_string());
                    if key == "remarks" {
                        name = if value.is_empty() {
                            "未命名节点".to_string()
                        } else {
                            value.clone()
                        };
                    }
                    params.insert(key.into(), Value::from(value));
                }
            }
        }
        params.insert("protocol".into(), Value::from(protocol.to_string()));
        params.insert("method".into(), Value::from(method.to_string()));
        params.insert("obfs".into(), Value::from(obfs.to_string()));
        params.insert("password".into(), Value::from(password));
        return Ok(ParsedNode {
            name,
            protocol: "ssr".into(),
            addr: addr.to_string(),
            port,
            params,
            clash_raw: None,
        });
    }
    if uri.starts_with("trojan://")
        || uri.starts_with("vless://")
        || uri.starts_with("hysteria2://")
        || uri.starts_with("tuic://")
    {
        let parsed = url::Url::parse(uri).map_err(|_| err())?;
        let protocol = if uri.starts_with("trojan://") {
            "trojan"
        } else if uri.starts_with("hysteria2://") {
            "hysteria2"
        } else if uri.starts_with("tuic://") {
            "tuic"
        } else {
            "vless"
        };
        let host = parsed.host_str().ok_or_else(err)?.to_string();
        let port = parsed.port().map(i64::from).unwrap_or(443);
        // 机场订阅的 fragment 常为百分号编码（url 库也会把非 ASCII 片段编码），必须解码后作为节点名
        let name = percent_decode(parsed.fragment().unwrap_or(""));
        let name = if name.is_empty() {
            "未命名节点".to_string()
        } else {
            name
        };
        let userinfo = percent_decode(parsed.username());
        let mut params = Map::new();
        // vless 用 uuid 键;trojan/hysteria2 的 userinfo 是 password;
        // tuic 的 userinfo 是 `uuid:pw` 两段,拆出 uuid 与 password
        if protocol == "vless" {
            params.insert("uuid".into(), Value::from(userinfo));
        } else if protocol == "tuic" {
            params.insert("uuid".into(), Value::from(userinfo));
            if let Some(pw) = parsed.password() {
                params.insert("password".into(), Value::from(percent_decode(pw)));
            }
        } else {
            params.insert("password".into(), Value::from(userinfo));
        }
        for (k, v) in parsed.query_pairs() {
            params.insert(k.into_owned(), Value::from(v.into_owned()));
        }
        return Ok(ParsedNode {
            name,
            protocol: protocol.into(),
            addr: host,
            port,
            params,
            clash_raw: None,
        });
    }
    Err(err())
}

/// 纯百分号解码：只解 %XX，& = + 保持原义（fragment / userinfo 的 RFC 3986 语义）。
/// 不可用 form_urlencoded（查询串语义：& 切分、= 切键值、+ 转空格）——节点名与密码
/// 中的原义 & = + 会被静默损坏。
fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// ss userinfo 解码：SIP002 生成器常剥离 base64 填充 `=`，带填充失败后回退无填充引擎。
fn decode_ss_userinfo(s: &str) -> Result<Vec<u8>, SubError> {
    let err = || {
        SubError::ParseFailed(format!(
            "无法解码 ss 用户信息: {}",
            s.chars().take(32).collect::<String>()
        ))
    };
    URL_SAFE
        .decode(s)
        .or_else(|_| URL_SAFE_NO_PAD.decode(s))
        .map_err(|_| err())
}

/// 解析 sing-box 客户端配置里的 `outbounds`。
///
/// 只收代理出站：selector/urltest/direct/block/dns 等没有 `server`/`server_port`
/// 的出站一律跳过。参数键统一折算成 config/mod.rs 与 config/singbox.rs 认识的
/// 内部键名（如 ws 的 path/host、REALITY 的 public-key），保证「导入再导出」
/// 不丢字段。
fn parse_singbox_json(bytes: &[u8]) -> Result<Vec<ParsedNode>, SubError> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| SubError::ParseFailed(format!("sing-box JSON 解析失败: {e}")))?;
    let outbounds = root
        .get("outbounds")
        .and_then(Value::as_array)
        .ok_or_else(|| SubError::ParseFailed("缺少 outbounds 列表".into()))?;
    let mut out = vec![];
    for ob in outbounds {
        let Some(map) = ob.as_object() else {
            continue;
        };
        let Some((protocol, params)) = singbox_proxy_params(map) else {
            continue;
        };
        let addr = sb_str(map, "server").unwrap_or("").to_string();
        let port = sb_port(map).unwrap_or(0);
        if addr.is_empty() || port <= 0 {
            continue;
        }
        let name = sb_str(map, "tag")
            .filter(|s| !s.is_empty())
            .unwrap_or("未命名节点")
            .to_string();
        out.push(ParsedNode {
            name,
            protocol,
            addr,
            port,
            params,
            clash_raw: None,
        });
    }
    if out.is_empty() {
        return Err(SubError::ParseFailed(
            "sing-box 配置里没有可导入的代理出站".into(),
        ));
    }
    Ok(out)
}

/// 单个 sing-box 出站 → 内部协议名与参数；非代理出站返回 None。
fn singbox_proxy_params(ob: &Map<String, Value>) -> Option<(String, Map<String, Value>)> {
    let ty = sb_str(ob, "type")?;
    let mut p = Map::new();
    let protocol = match ty {
        "vless" => {
            p.insert("uuid".into(), Value::from(sb_str(ob, "uuid").unwrap_or("")));
            if let Some(flow) = sb_str(ob, "flow") {
                if !flow.is_empty() {
                    p.insert("flow".into(), Value::from(flow));
                }
            }
            if let Some(reality) = sb_apply_tls(ob, &mut p) {
                p.insert(
                    "security".into(),
                    Value::from(if reality { "reality" } else { "tls" }),
                );
            }
            sb_apply_transport(ob, &mut p, "type");
            "vless"
        }
        "vmess" => {
            p.insert("uuid".into(), Value::from(sb_str(ob, "uuid").unwrap_or("")));
            // sing-box 的 security 是加密方式（auto/aes-128-gcm…），内部键为 cipher；
            // TLS 开关单独用 tls 字符串，不能复用 security——否则导出时会把
            // "tls" 当成加密方式。
            let security = sb_str(ob, "security").unwrap_or("auto");
            p.insert("cipher".into(), Value::from(security));
            p.insert(
                "alterId".into(),
                Value::from(sb_int(ob, "alter_id").unwrap_or(0)),
            );
            if sb_apply_tls(ob, &mut p).is_some() {
                p.insert("tls".into(), Value::from("tls"));
            }
            sb_apply_transport(ob, &mut p, "network");
            "vmess"
        }
        "trojan" => {
            p.insert(
                "password".into(),
                Value::from(sb_str(ob, "password").unwrap_or("")),
            );
            if let Some(reality) = sb_apply_tls(ob, &mut p) {
                p.insert(
                    "security".into(),
                    Value::from(if reality { "reality" } else { "tls" }),
                );
            }
            sb_apply_transport(ob, &mut p, "type");
            "trojan"
        }
        "shadowsocks" => {
            p.insert(
                "cipher".into(),
                Value::from(sb_str(ob, "method").unwrap_or("")),
            );
            p.insert(
                "password".into(),
                Value::from(sb_str(ob, "password").unwrap_or("")),
            );
            "ss"
        }
        "hysteria2" => {
            p.insert(
                "password".into(),
                Value::from(sb_str(ob, "password").unwrap_or("")),
            );
            for (sk, dk) in [("up_mbps", "up"), ("down_mbps", "down")] {
                if let Some(v) = sb_int(ob, sk) {
                    p.insert(dk.into(), Value::from(v));
                }
            }
            if let Some(obfs) = ob.get("obfs").and_then(Value::as_object) {
                if let Some(kind) = sb_str(obfs, "type") {
                    p.insert("obfs".into(), Value::from(kind));
                }
                if let Some(pw) = sb_str(obfs, "password") {
                    p.insert("obfs-password".into(), Value::from(pw));
                }
            }
            // hysteria2 恒为 TLS；这里只吸收 sni/跳过校验等可选字段
            sb_apply_tls(ob, &mut p);
            "hysteria2"
        }
        "tuic" => {
            p.insert("uuid".into(), Value::from(sb_str(ob, "uuid").unwrap_or("")));
            p.insert(
                "password".into(),
                Value::from(sb_str(ob, "password").unwrap_or("")),
            );
            if let Some(cc) = sb_str(ob, "congestion_control") {
                p.insert("congestion-control".into(), Value::from(cc));
            }
            if let Some(mode) = sb_str(ob, "udp_relay_mode") {
                p.insert("udp-relay-mode".into(), Value::from(mode));
            }
            sb_apply_tls(ob, &mut p);
            "tuic"
        }
        "wireguard" => {
            p.insert(
                "private-key".into(),
                Value::from(sb_str(ob, "private_key").unwrap_or("")),
            );
            p.insert(
                "public-key".into(),
                Value::from(sb_str(ob, "peer_public_key").unwrap_or("")),
            );
            if let Some(psk) = sb_str(ob, "peer_pre_shared_key") {
                p.insert("preshared-key".into(), Value::from(psk));
            }
            // local_address 允许字符串或数组；内部只保留首个地址
            if let Some(ip) = ob.get("local_address").and_then(|v| match v {
                Value::Array(a) => a.first().and_then(Value::as_str),
                Value::String(s) => Some(s.as_str()),
                _ => None,
            }) {
                p.insert("ip".into(), Value::from(ip));
            }
            if let Some(mtu) = sb_int(ob, "mtu") {
                p.insert("mtu".into(), Value::from(mtu));
            }
            "wireguard"
        }
        _ => return None,
    };
    Some((protocol.to_string(), p))
}

/// 吸收 sing-box `tls` 对象：sni/跳过校验/uTLS 指纹/REALITY 公钥与 short_id。
/// 返回 Some(reality)：TLS 已启用；布尔值表示是否 REALITY。未启用返回 None。
fn sb_apply_tls(ob: &Map<String, Value>, p: &mut Map<String, Value>) -> Option<bool> {
    let tls = ob.get("tls").and_then(Value::as_object)?;
    if !tls.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    if let Some(sni) = sb_str(tls, "server_name") {
        if !sni.is_empty() {
            p.insert("sni".into(), Value::from(sni));
        }
    }
    if tls
        .get("insecure")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        p.insert("skip-cert-verify".into(), Value::from("true"));
    }
    if let Some(fp) = tls
        .get("utls")
        .and_then(Value::as_object)
        .and_then(|u| sb_str(u, "fingerprint"))
    {
        if !fp.is_empty() {
            p.insert("fingerprint".into(), Value::from(fp));
        }
    }
    let reality = tls.get("reality").and_then(Value::as_object);
    let reality_on = reality
        .map(|r| r.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .unwrap_or(false);
    if let Some(r) = reality {
        if let Some(key) = sb_str(r, "public_key") {
            if !key.is_empty() {
                p.insert("public-key".into(), Value::from(key));
            }
        }
        if let Some(sid) = sb_str(r, "short_id") {
            if !sid.is_empty() {
                p.insert("short-id".into(), Value::from(sid));
            }
        }
    }
    Some(reality_on)
}

/// 吸收 sing-box `transport` 对象：ws/grpc 折算回内部键。
/// 内部两套键名：vmess 用 `network`，vless/trojan 用 `type`。
fn sb_apply_transport(ob: &Map<String, Value>, p: &mut Map<String, Value>, key: &str) {
    let Some(t) = ob.get("transport").and_then(Value::as_object) else {
        return;
    };
    let Some(kind) = sb_str(t, "type") else {
        return;
    };
    if kind == "tcp" {
        return;
    }
    p.insert(key.into(), Value::from(kind));
    match kind {
        "ws" => {
            if let Some(path) = sb_str(t, "path") {
                if !path.is_empty() {
                    p.insert("path".into(), Value::from(path));
                }
            }
            // headers.Host 可能是字符串或数组；内部只保留首个
            if let Some(host) = t.get("headers").and_then(Value::as_object).and_then(|h| {
                h.get("Host")
                    .or_else(|| h.get("host"))
                    .and_then(|v| match v {
                        Value::Array(a) => a.first().and_then(Value::as_str),
                        Value::String(s) => Some(s.as_str()),
                        _ => None,
                    })
            }) {
                if !host.is_empty() {
                    p.insert("host".into(), Value::from(host));
                }
            }
        }
        "grpc" => {
            if let Some(sn) = sb_str(t, "service_name") {
                if !sn.is_empty() {
                    p.insert("serviceName".into(), Value::from(sn));
                }
            }
        }
        _ => {}
    }
}

fn sb_str<'a>(ob: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ob.get(key).and_then(Value::as_str)
}

/// sing-box 数值字段：宽容接受数字与数字字符串。
fn sb_int(ob: &Map<String, Value>, key: &str) -> Option<i64> {
    ob.get(key).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|n| n as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn sb_port(ob: &Map<String, Value>) -> Option<i64> {
    sb_int(ob, "server_port")
}

fn parse_clash_yaml(bytes: &[u8]) -> Result<Vec<ParsedNode>, SubError> {
    let doc: serde_yaml::Value =
        serde_yaml::from_slice(bytes).map_err(|e| SubError::ParseFailed(e.to_string()))?;
    let proxies = doc
        .get("proxies")
        .and_then(|v| v.as_sequence())
        .ok_or_else(|| SubError::ParseFailed("缺少 proxies 列表".into()))?;
    let mut out = vec![];
    for p in proxies {
        let map = p
            .as_mapping()
            .ok_or_else(|| SubError::ParseFailed("proxy 不是映射".into()))?;
        let name = map
            .get(serde_yaml::Value::String("name".into()))
            .and_then(|v| v.as_str())
            .unwrap_or("未命名节点");
        let protocol = map
            .get(serde_yaml::Value::String("type".into()))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let addr = map
            .get(serde_yaml::Value::String("server".into()))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let port = map
            .get(serde_yaml::Value::String("port".into()))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        out.push(ParsedNode {
            name: name.to_string(),
            protocol: protocol.to_string(),
            addr: addr.to_string(),
            port,
            params: Map::new(),
            clash_raw: Some(
                serde_yaml::to_string(p).map_err(|e| SubError::ParseFailed(e.to_string()))?,
            ),
        });
    }
    Ok(out)
}
