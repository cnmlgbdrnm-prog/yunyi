//! 最小 HTTP/1.1 客户端（仅回环、仅明文）。
//!
//! 为什么不用 reqwest：服务以 SYSTEM 运行，且跑在 Tauri **之外**（main.rs 在拉起
//! Tauri 之前就分流到服务模式），进程里没有 tokio 运行时。为一次 `PUT /configs`
//! 拖进 reqwest 加一个隐式运行时，等于给一个 SYSTEM 服务平添攻击面。控制器的流量
//! 就是 127.0.0.1 上的明文 HTTP，手写够用且每一行都可审。
//!
//! 输入输出：PUT 一个 JSON body → HTTP 状态码 + 响应体片段。**失败一律 Err，绝不猜**——
//! 静默当成 200 会让「热重载失败」伪装成成功，开关回弹但没人知道为什么。
//! 带响应体是为了让 mihomo 的 400 不再是光秃秃的 HTTP 400：它的
//! `{"message":"proxy xxx is the duplicate name"}` 才是可诊断的那句话。
//! 依赖：std::net / std::io / serde_json。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 控制器是本机进程，卡住只可能是它挂了。5s 足够，且不能让服务线程无限等。
const TIMEOUT: Duration = Duration::from_secs(5);

/// 只解析状态行。非 2xx 由调用方决定怎么报——这里不替它判断。
pub(super) fn parse_status(head: &str) -> Result<u16, String> {
    let line = head.lines().next().ok_or("响应为空")?;
    let mut parts = line.split_whitespace();
    let version = parts.next().ok_or("状态行缺协议版本")?;
    if !version.starts_with("HTTP/") {
        return Err(format!("响应不是 HTTP: {line}"));
    }
    parts
        .next()
        .ok_or_else(|| format!("状态行缺状态码: {line}"))?
        .parse::<u16>()
        .map_err(|e| format!("状态码不是数字: {e}"))
}

/// 响应体最多保留这么多字符：错误信息只要够说明原因，不把整份配置灌进日志。
const BODY_SNIPPET_CHARS: usize = 300;
/// 整个响应（头 + 体）的读取上限，防止对端不回 EOF 时无限读。
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// `PUT {addr}{path}`，带 Bearer token 与 JSON body，返回 (状态码, 响应体片段)。
pub(super) fn put_json(
    addr: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<(u16, String), String> {
    let payload = serde_json::to_string(body).map_err(|e| format!("序列化请求体失败: {e}"))?;
    let mut stream =
        TcpStream::connect(addr).map_err(|e| format!("连接控制器 {addr} 失败: {e}"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    // Connection: close 让对端答完就关，省掉自己处理分块/keep-alive
    let req = format!(
        "PUT {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{payload}",
        len = payload.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    // 读到头 + 体（Connection: close，正常情况对端答完就关）。允许分多次读：
    // 控制器报错时状态行与 JSON 体可能落在不同的 TCP 段里。
    let mut raw = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() >= MAX_RESPONSE_BYTES {
                    break;
                }
                // 有 Content-Length 时按长度收口，避免等服务端关连接拖到读超时
                if content_length_complete(&raw) {
                    break;
                }
            }
            // 已经读到响应就先用现有内容；一个字都没有才算通信失败
            Err(_) if !raw.is_empty() => break,
            Err(e) => return Err(format!("读响应失败: {e}")),
        }
    }
    if raw.is_empty() {
        return Err("控制器没回任何内容就关闭了连接".to_string());
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let code = parse_status(head)?;
    let snippet: String = body.trim().chars().take(BODY_SNIPPET_CHARS).collect();
    Ok((code, snippet))
}

/// 响应是否已按 Content-Length 收全（头里没有该字段则永远返回 false，交给 EOF/上限收口）。
fn content_length_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let len = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    });
    matches!(len, Some(n) if body.len() >= n)
}
