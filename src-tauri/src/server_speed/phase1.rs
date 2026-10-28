//! 阶段1 服务器测速原语：SSH 端响应耗时（服务器页只采一次，不依赖服务器工具）与
//! SSH exec curl 下载带宽（解析 %{speed_download} bytes/s）。
//! 编排（批量窗口/写库/事件）在 mod.rs（Task 4 接线）。

use crate::ssh::SshSession;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// 单次 TCP 建连超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 从拨号开始到收到远端 SSH 标识的耗时，包含当前路径及服务器首响应开销。
/// TUN/透明代理可能在远端连接建立前就完成本地 TCP 握手，不能用 connect 成功代替远端响应。
/// 仅交换协议标识，不认证、不运行命令。失败不回退到会产生假低延迟的 TCP 计时。
pub async fn ssh_response_latency_ms(host: &str, port: u16, samples: u8) -> Result<i64, String> {
    ssh_response_latency_with_timeout(host, port, samples, CONNECT_TIMEOUT).await
}

pub(crate) async fn ssh_response_latency_with_timeout(
    host: &str,
    port: u16,
    samples: u8,
    timeout: Duration,
) -> Result<i64, String> {
    let mut best: Option<i64> = None;
    let mut last_error = String::new();
    for _ in 0..samples.max(1) {
        let start = std::time::Instant::now();
        let probe = async {
            let mut stream = tokio::net::TcpStream::connect((host, port)).await?;
            stream.write_all(b"SSH-2.0-vpn_ctl_latency\r\n").await?;
            // RFC 4253 允许标识前有说明行；总量限 8KiB，避免异常端口无限吐数据。
            let mut reader = BufReader::new(stream.take(8192));
            let mut line = Vec::new();
            loop {
                line.clear();
                if reader.read_until(b'\n', &mut line).await? == 0 {
                    return Err(std::io::Error::other(
                        "未收到有效 SSH 标识（连接关闭或响应超过 8KiB）",
                    ));
                }
                if line.starts_with(b"SSH-") {
                    // 必须收到完整标识行；截断/非 SSH2 服务不能被当成有效样本。
                    let complete = line.ends_with(b"\n") && line.len() <= 255;
                    let text = String::from_utf8_lossy(&line);
                    let version = text.trim_end_matches(['\r', '\n']);
                    let software = version
                        .strip_prefix("SSH-2.0-")
                        .or_else(|| version.strip_prefix("SSH-1.99-"));
                    if complete
                        && !line.contains(&0)
                        && software.is_some_and(|s| !s.is_empty() && !s.starts_with(' '))
                    {
                        return Ok(());
                    }
                    return Err(std::io::Error::other("远端 SSH 标识无效"));
                }
            }
        };
        match tokio::time::timeout(timeout, probe).await {
            Ok(Ok(())) => {
                let ms = start.elapsed().as_millis() as i64;
                best = Some(best.map_or(ms, |previous| previous.min(ms)));
            }
            Ok(Err(e)) => last_error = e.to_string(),
            Err(_) => last_error = "等待远端 SSH 响应超时".into(),
        }
    }
    best.ok_or_else(|| format!("SSH 响应测速失败: {host}:{port} {last_error}"))
}

/// 串行 samples 次 TCP connect 计时（单次 ≤5s 超时），返回最小 ms。
/// TUN 下可能只测到本地握手；服务器页的端响应检测不能使用这个值。
/// 全部失败 → Err(「TCP 连接失败: {host}:{port} {最后一个错误}」)。
pub async fn tcp_latency_ms(host: &str, port: u16, samples: u8) -> Result<i64, String> {
    let mut min: Option<i64> = None;
    let mut last_err = String::new();
    for _ in 0..samples.max(1) {
        let start = std::time::Instant::now();
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        {
            Ok(Ok(_stream)) => {
                let ms = start.elapsed().as_millis() as i64;
                min = Some(match min {
                    Some(m) => m.min(ms),
                    None => ms,
                });
            }
            Ok(Err(e)) => last_err = e.to_string(),
            Err(_) => last_err = "连接超时".to_string(),
        }
    }
    match min {
        Some(ms) => Ok(ms),
        None => Err(format!("TCP 连接失败: {host}:{port} {last_err}")),
    }
}

/// SSH exec curl 测下载带宽：stdout 为 %{speed_download}（bytes/s）。
/// 退出 127 或 stderr 含 "not found"（缺 curl）→ 明确中文错误；
/// 其余非零退出与超时错误照 SshError 形态透出（Display 已可读，直接 String 化）。
pub async fn curl_bandwidth(
    session: &mut SshSession,
    url: &str,
    timeout: Duration,
) -> Result<f64, String> {
    let cmd = format!("curl -o /dev/null -s -w '%{{speed_download}}' --max-time 30 {url}");
    let out = session
        .run("下载测速", &cmd, timeout)
        .await
        .map_err(|e| e.to_string())?;
    if out.exit_code == 127 || out.stderr.contains("not found") {
        return Err("服务器缺少 curl，无法测下载带宽".to_string());
    }
    if out.exit_code != 0 {
        return Err(format!(
            "下载测速 执行失败(exit={}): {}",
            out.exit_code,
            out.stderr.trim_end()
        ));
    }
    parse_speed_download(&out.stdout)
}

/// 解析 curl %{speed_download} 输出：trim 后 f64，≤0 或 NaN/inf 视为失败。
pub fn parse_speed_download(stdout: &str) -> Result<f64, String> {
    let s = stdout.trim();
    let v: f64 = s.parse().map_err(|_| format!("无法解析下载速度: {s:?}"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err(format!("下载速度异常: {s:?}"));
    }
    Ok(v)
}
