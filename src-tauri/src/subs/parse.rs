//! 订阅解析器：识别 base64 URI 列表与 Clash YAML 两种格式，解析为统一 ParsedNode。
//! URI 列表支持 vmess/ss/ssr/trojan/vless/hysteria2 六类协议；Clash 来源节点保留
//! 原始 YAML 片段（clash_raw），配置生成时原样输出以免协议信息失真。

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
