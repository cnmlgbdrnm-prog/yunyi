//! 阶段1 服务器测速原语：本机 TCP 延迟（3 采样最小，不依赖服务器工具）与
//! SSH exec curl 下载带宽（解析 %{speed_download} bytes/s）。
//! 编排（批量窗口/写库/事件）在 mod.rs（Task 4 接线）。

use crate::ssh::SshSession;
use std::time::Duration;

/// 单次 TCP 建连超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 串行 samples 次 TCP connect 计时（单次 ≤5s 超时），返回最小 ms。
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
