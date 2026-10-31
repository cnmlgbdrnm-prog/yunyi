//! 只读 SSH 探针：连接、执行采集脚本、返回原始输出或结构化失败。
use crate::crypto::CredentialService;
use crate::db::Db;
use crate::domain::{effective_host, Server};
use crate::ssh::SshSession;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

const PROBE_BODY: &str = r#"echo '##uptime'; cat /proc/uptime 2>/dev/null
echo '##load'; cat /proc/loadavg 2>/dev/null
echo '##cpu1'; grep '^cpu ' /proc/stat 2>/dev/null
sleep 1
echo '##cpu2'; grep '^cpu ' /proc/stat 2>/dev/null
echo '##mem'; cat /proc/meminfo 2>/dev/null
echo '##disk'; df -P -k / 2>/dev/null
echo '##cores'; nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo 2>/dev/null
echo '##bbr'; sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null
echo '##svc'; systemctl is-active xray ssserver singbox 2>/dev/null
echo '##wg'; wg show interfaces 2>/dev/null
rc=$?
echo '##wg_rc'; echo "$rc"
"#;

/// 没有配置三网地区时返回与旧版逐字一致的脚本；配置后在资源采集段前插入三条并发
/// curl，在 `##done` 前插入 `##ct/##cu/##cm` 输出。curl 与 `sleep 1` 重叠。
pub fn probe_script(region: Option<&str>) -> String {
    let hosts = region.and_then(|r| super::carrier::target_hosts(r).ok());
    let mut out = String::new();
    if let Some(hosts) = &hosts {
        out.push_str(&super::carrier::probe_setup(hosts));
    }
    out.push_str(PROBE_BODY);
    if hosts.is_some() {
        out.push_str(super::carrier::probe_tail());
    }
    out.push_str("echo '##done'\n");
    out
}

#[derive(Debug, Clone)]
pub struct MonitorTarget {
    pub server: Server,
    pub expected_services: Vec<String>,
    pub deployment_known: bool,
    pub wg_expected: bool,
    /// 该台服务器配置的三网地区 code；None = 不测试。
    pub carrier_region: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProbeOutcome {
    Connected {
        connect_ms: u64,
        stdout: String,
        command_error: Option<String>,
    },
    Unreachable {
        error: String,
    },
    LocalError {
        error: String,
    },
}

#[async_trait]
pub trait MonitorProbe: Send + Sync {
    async fn probe(&self, target: &MonitorTarget) -> ProbeOutcome;
}

pub struct RealProbe {
    pub db: Arc<Db>,
    pub creds: Arc<CredentialService>,
}

#[async_trait]
impl MonitorProbe for RealProbe {
    async fn probe(&self, target: &MonitorTarget) -> ProbeOutcome {
        let (auth, _) =
            match crate::servers::load_auth_parts(&self.db, &self.creds, target.server.id) {
                Ok(v) => v,
                Err(e) => {
                    return ProbeOutcome::LocalError {
                        error: format!("读取服务器凭据失败: {e}"),
                    }
                }
            };
        let started = Instant::now();
        let mut session = match SshSession::connect(
            effective_host(&target.server),
            target.server.port as u16,
            &target.server.user,
            &auth,
            CONNECT_TIMEOUT,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return ProbeOutcome::Unreachable {
                    error: e.to_string(),
                }
            }
        };
        let connect_ms = started.elapsed().as_millis() as u64;
        let script = probe_script(target.carrier_region.as_deref());
        match session
            .run("服务器健康采集", &script, COMMAND_TIMEOUT)
            .await
        {
            Ok(out) => ProbeOutcome::Connected {
                connect_ms,
                stdout: out.stdout,
                command_error: if out.exit_code == 0 {
                    None
                } else {
                    Some(format!("采集脚本退出码 {}", out.exit_code))
                },
            },
            Err(e) => ProbeOutcome::Connected {
                connect_ms,
                stdout: String::new(),
                command_error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(all(test, feature = "local-tests"))]
mod tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/rust/server_monitor/probe_tests.rs"
    ));
}
