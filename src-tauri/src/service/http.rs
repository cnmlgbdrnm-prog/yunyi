//! 最小 HTTP/1.1 客户端（仅回环、仅明文）。
//!
//! 为什么不用 reqwest：服务以 SYSTEM 运行，且跑在 Tauri **之外**（main.rs 在拉起
//! Tauri 之前就分流到服务模式），进程里没有 tokio 运行时。为一次 `PUT /configs`
//! 拖进 reqwest 加一个隐式运行时，等于给一个 SYSTEM 服务平添攻击面。控制器的流量
//! 就是 127.0.0.1 上的明文 HTTP，手写够用且每一行都可审。
//!
//! 输入输出：PUT 一个 JSON body → HTTP 状态码。**失败一律 Err，绝不猜**——
//! 静默当成 200 会让「热重载失败」伪装成成功，开关回弹但没人知道为什么。
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

/// `PUT {addr}{path}`，带 Bearer token 与 JSON body，返回状态码。
pub(super) fn put_json(
    addr: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<u16, String> {
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

    let mut buf = String::new();
    let mut chunk = [0u8; 1024];
    // 状态行在首段里；读不到内容（对端直接关）就是错误，不能当成功
    let n = stream
        .read(&mut chunk)
        .map_err(|e| format!("读响应失败: {e}"))?;
    if n == 0 {
        return Err("控制器没回任何内容就关闭了连接".to_string());
    }
    buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
    parse_status(&buf)
}
