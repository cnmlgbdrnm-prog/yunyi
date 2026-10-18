//! 自动探测服务器自身的全局 IPv6 地址。
//!
//! 为什么走 SSH 问机器自己而不是本机 DNS：服务器地址大多是 IPv4 字面量，根本没有
//! AAAA 可查；即使登记的是域名，机器上真正绑定的地址也可能与 DNS 记录不同。SSH
//! 进去读 `ip -6 addr` 是最直接的证据。域名场景下 DNS 只作兜底（主机名解析出
//! AAAA，但 SSH 因 IPv4 路径不稳而失败时仍能给出候选）。
//!
//! 探测固定走 `host`（原始管理地址），**不走 `effective_host`**：一旦用户已切到
//! IPv6，用生效地址探测等于拿已知答案去提问；更糟的是 IPv6 路径坏掉时连探测都
//! 进不去，用户无法切回 v4 修复。

use crate::domain::Server;
use crate::servers::{load_auth, ServersError, CONNECT_TIMEOUT};
use crate::ssh::{SshError, SshSession};
use crate::state::AppState;
use std::net::{Ipv6Addr, SocketAddr};
use std::time::Duration;

const ADDRS_MARK: &str = "__REA_V6_ADDRS__";
const ROUTE_MARK: &str = "__REA_V6_ROUTE__";
const HOSTNAME_MARK: &str = "__REA_V6_HOSTNAME__";

/// 探测命令本身是毫秒级的（实测香港 38ms 链路含 SSH 往返约 2-5 秒）。30 秒的
/// 通用 PROBE_TIMEOUT 会把「TCP 已连上但会话中途丢包」拖成半分钟无响应——香港
/// 这类丢包链路上实测三次里就有一次卡死。10 秒足够跑完，失败后立刻换新连接重试。
const DETECT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// 丢包链路换一条新 SSH 连接往往就能过；两次足以覆盖单次偶发丢包，
/// 同时把最坏等待限制在「2 × (连接 15s + 探测 10s)」。
const DETECT_ATTEMPTS: usize = 2;

/// 三类来源一次拿全：`ip` 为主；默认路由的源地址最可信（优先返回）；
/// `hostname -I` 与 `ifconfig` 覆盖没有 iproute2 的精简系统。
///
/// 标记必须打在每段输出的**前面**：解析器靠标记切换分段，标记若打在命令后面，
/// 命令的输出会落进上一段。实测踩过：路由行落进地址段后，目标地址
/// `2001:4860:4860::8888` 和网关都被当成候选，主机名段反而被当路由段丢掉。
fn probe_command() -> String {
    format!(
        "echo {ADDRS_MARK}; ip -6 -o addr show scope global 2>/dev/null; \
         echo {ROUTE_MARK}; ip -6 route get 2001:4860:4860::8888 2>/dev/null; \
         echo {HOSTNAME_MARK}; hostname -I 2>/dev/null; \
         ifconfig -a 2>/dev/null"
    )
}

/// 探测并返回候选地址（推荐值在首位）。空结果与 SSH 失败都以 `Err` 返回，
/// 错误文本可直接上屏——凭据永远不会出现在其中。
pub async fn detect_ipv6_addresses(state: &AppState, id: i64) -> Result<Vec<String>, ServersError> {
    let (auth, server) = load_auth(state, id)?;
    let mut last_err: Option<ServersError> = None;
    for _ in 0..DETECT_ATTEMPTS {
        match probe_via_ssh(&server, &auth).await {
            Ok(found) if !found.is_empty() => return Ok(found),
            // 命令跑通了但机器上确实没有地址：重试不会有不同答案。
            Ok(_) => break,
            Err(e) if should_retry(&e) => last_err = Some(e),
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }
    if let Some(found) = dns_fallback(&server.host).await {
        return Ok(found);
    }
    Err(last_err.unwrap_or_else(|| {
        ServersError::Validation("服务器上没有发现全局 IPv6 地址（VPS 可能未分配 IPv6）".into())
    }))
}

/// 只重试链路层偶发失败。认证/密钥/主机密钥错误换连接也一样，白等一轮。
fn should_retry(e: &ServersError) -> bool {
    matches!(
        e,
        ServersError::Ssh(
            SshError::Connect { .. } | SshError::Timeout { .. } | SshError::CommandFailed { .. }
        )
    )
}

async fn probe_via_ssh(
    server: &Server,
    auth: &crate::ssh::AuthMaterial,
) -> Result<Vec<String>, ServersError> {
    let mut session = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        auth,
        CONNECT_TIMEOUT,
    )
    .await?;
    let out = session
        .run("探测 IPv6", &probe_command(), DETECT_PROBE_TIMEOUT)
        .await?;
    let found = parse_probe_output(&out.stdout);
    if found.is_empty() && !out.stderr.trim().is_empty() {
        // 命令本身跑失败（例如系统没有 ip/hostname/ifconfig）时把远端诊断带回来，
        // 否则用户只看到「没有 IPv6」，会误以为 VPS 没分配。
        let tail: String = out.stderr.chars().rev().take(300).collect::<String>();
        let tail: String = tail.chars().rev().collect();
        return Err(ServersError::Validation(format!(
            "远端探测命令失败：{}",
            tail.trim()
        )));
    }
    Ok(found)
}

/// 域名兜底：只取 AAAA。字面量 IPv4 自然查不到 v6，不必特判。
async fn dns_fallback(host: &str) -> Option<Vec<String>> {
    let addrs = tokio::net::lookup_host((host, 0)).await.ok()?;
    let mut out: Vec<String> = Vec::new();
    for sa in addrs {
        if let SocketAddr::V6(v6) = sa {
            let ip = *v6.ip();
            if is_usable(&ip) {
                let text = ip.to_string();
                if !out.contains(&text) {
                    out.push(text);
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 解析探测输出。公开给本地测试（tests/rust 不随源码上传）。
pub(crate) fn parse_probe_output(stdout: &str) -> Vec<String> {
    let mut section = 0u8;
    let mut preferred: Option<Ipv6Addr> = None;
    let mut found: Vec<Ipv6Addr> = Vec::new();
    for line in stdout.lines() {
        match line.trim() {
            ADDRS_MARK => {
                section = 0;
                continue;
            }
            ROUTE_MARK => {
                section = 1;
                continue;
            }
            HOSTNAME_MARK => {
                section = 2;
                continue;
            }
            _ => {}
        }
        if section == 1 {
            // `ip -6 route get` 形如 `2001:... via ... dev eth0 src 2a0f:... metric 1024`。
            // 这一节只认 src：目标地址和网关不是本机地址，绝不能混进候选。
            let mut tokens = line.split_whitespace();
            while let Some(tok) = tokens.next() {
                if tok == "src" {
                    preferred = tokens.next().and_then(parse_addr_token);
                    break;
                }
            }
            continue;
        }
        for tok in line.split_whitespace() {
            if let Some(a) = parse_addr_token(tok) {
                found.push(a);
            }
        }
    }
    // 顺序：默认路由的源地址 → 全球单播 2000::/3 → 其余（ULA 等）。去重保序。
    let mut ordered: Vec<Ipv6Addr> = Vec::new();
    if let Some(p) = preferred {
        ordered.push(p);
    }
    found.sort_by_key(|a| u8::from(!is_global_unicast(a)));
    for a in found {
        if !ordered.contains(&a) {
            ordered.push(a);
        }
    }
    ordered.into_iter().map(|a| a.to_string()).collect()
}

fn parse_addr_token(tok: &str) -> Option<Ipv6Addr> {
    let tok = tok.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']'));
    let tok = tok.strip_prefix("addr:").unwrap_or(tok);
    // ifconfig 可能带 `%eth0` 区域标识；iproute2 带 `/64` 前缀长度
    let tok = tok.split('%').next()?;
    let tok = tok.split('/').next()?;
    let addr: Ipv6Addr = tok.parse().ok()?;
    if is_usable(&addr) {
        Some(addr)
    } else {
        None
    }
}

/// 自动候选只收公网全局单播（2000::/3）。ULA（fd00::/8，常见于 Tailscale/
/// WireGuard 内网）与链路本地也能解析，但只在本机所在的内网/隧道里可达；
/// 自动切过去会让「点一下就能用」变成「点一下管理地址失效」。需要内网地址
/// 的用户可在「编辑」里手动填写。
fn is_usable(a: &Ipv6Addr) -> bool {
    !a.is_loopback()
        && !a.is_unspecified()
        && !a.is_multicast()
        && !is_link_local(a)
        && is_global_unicast(a)
}

fn is_link_local(a: &Ipv6Addr) -> bool {
    a.segments()[0] & 0xffc0 == 0xfe80
}

fn is_global_unicast(a: &Ipv6Addr) -> bool {
    a.segments()[0] & 0xe000 == 0x2000
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）。
#[cfg(all(test, feature = "local-tests"))]
mod ipv6_detect_tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/servers/ipv6_detect_tests.rs"
    ));
}
