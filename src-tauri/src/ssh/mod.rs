//! SSH 适配器：russh 客户端连接、命令执行与真实超时。单命令时长由
//! tokio::time::timeout 约束，超时即中断通道；文件写入走 base64 管道
//! （由调用方 servers/deploy 提供白名单路径）。
//! 输入：主机/端口/用户/认证材料/命令；输出：CommandOutput 或 SshError。
//! 依赖：russh（async 客户端，纯 Rust）。

use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// SSH 适配器错误：连接/认证/命令/超时/密钥格式。
#[derive(Debug, Error)]
pub enum SshError {
    #[error("无法连接 {host}:{port}: {reason}")]
    Connect {
        host: String,
        port: u16,
        reason: String,
    },
    #[error("认证失败: {reason}")]
    Auth { reason: String },
    #[error("{step} 执行失败(exit={exit}): {stderr_tail}")]
    CommandFailed {
        step: String,
        exit: u32,
        stderr_tail: String,
    },
    #[error("{step} 超时（{secs} 秒）")]
    Timeout { step: String, secs: u64 },
    #[error("密钥格式错误: {reason}")]
    KeyFormat { reason: String },
    #[error("主机密钥校验未通过: {reason}")]
    HostKey { reason: String },
}

/// 命令输出。
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u32,
}

/// 认证材料：密码或 OpenSSH PEM 私钥文本。
#[derive(Debug, Clone)]
pub enum AuthMaterial {
    Password(String),
    PrivateKey(String),
}

/// 已信任主机密钥的落盘位置（TOFU，见 [`verify_host_key`]）。
///
/// **进程级事实**：一个进程一个数据目录，与 `jobs::set_emit` 同形，在 main 装配
/// [`crate::state::AppState`] 时设置一次。`None`（测试进程）＝不校验——测试里每个
/// 用例都起自己的 FakeSshServer（端口随机、密钥随机），共用一个库只会互相污染。
static KNOWN_HOSTS: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// 设置主机密钥指纹库路径（main 启动时调用；重复调用忽略）。
pub fn set_known_hosts_path(path: std::path::PathBuf) {
    let _ = KNOWN_HOSTS.set(path);
}

/// 主机密钥 TOFU：首次见到就记住指纹，之后**必须一致**。
///
/// 为什么要有：原先这里恒 `Ok(true)`——任何人只要能拦下 TCP（假 DNS、坏路由、被接管的
/// 中间设备）就能冒充用户的服务器，而我们会把 IP、root 密码、WireGuard 私钥、
/// 节点凭据原样交出去。不预置信任 + 记住第一次（TOFU）是用可用性换安全的最小代价。
///
/// 三条刻意的取舍：
/// - **首次连接绝不能被拦**（文件不存在＝首次）：这是「不预置信任」的另一面，
///   拦了就等于要求用户先手工录指纹。
/// - **读失败 fail closed**：除 `NotFound` 之外的 IO 错误一律拒绝并说明是「读不出来」，
///   而不是当成首次连接——把读失败当首次，等于攻击者只要让指纹库读不出来就能让任意
///   主机密钥被信任一次（本仓反复修过的形态）。
/// - 不匹配时**带上新旧两个指纹**：用户要能自己判断是「服务器重装」还是「有人在中间」，
///   只说「密钥不匹配」等于把排查成本推回去。
///
/// 落盘是读-改-写、没有加锁：中转与落地两条连接是 `tokio::join!` 并发的
/// （relay/mod.rs:117），**首次**建链时两条可能同时记录，后写的会盖掉先写的。
/// 后果是「少记一条」，不是「记错」：那台主机下次连接时按首次再记一遍，永远不会
/// 出现假拒绝，也不会凭空信任谁。这里刻意不引入锁——为一次丢失一条记录的小概率事件
/// 去加锁，换来的只是更复杂的失败模式。
pub(crate) fn verify_host_key(
    store: Option<&std::path::Path>,
    host: &str,
    port: u16,
    fingerprint: &str,
) -> Result<(), String> {
    let Some(path) = store else {
        return Ok(());
    };
    let key = format!("{host}:{port}");
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(), // 真·首次
        Err(e) => {
            return Err(format!(
                "读取主机密钥指纹库失败（{}）：{e}——为安全起见拒绝连接，请检查该文件权限",
                path.display()
            ))
        }
    };
    if let Some(line) = content.lines().find(|l| l.starts_with(&format!("{key} "))) {
        let known = line.split_whitespace().nth(1).unwrap_or("");
        if known == fingerprint {
            return Ok(());
        }
        return Err(format!(
            "主机密钥已变更（{}）：已记录 {known}，本次 {fingerprint}。\
             可能是中间人劫持，也可能是服务器重装过——确认无误后请删掉指纹库里这一行再重试。",
            key
        ));
    }
    // 首次见到这台主机：记下来。写不进去也不拦人（下次仍会当作首次），但要说清楚
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut next = content;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&format!("{key} {fingerprint}\n"));
    std::fs::write(path, next)
        .map_err(|e| format!("记录主机密钥指纹失败（{}）：{e}", path.display()))?;
    Ok(())
}

/// 客户端处理器：把 TOFU 校验结果带出来。
///
/// russh 的 `check_server_key` 只能返回 `bool`，拒绝的**原因**传不出去（握手失败在
/// 上层只表现为一句通用的错误）。所以这里把原因塞进共享槽，由 [`SshSession::connect_via`]
/// 在握手失败后取出来换成中文错误——否则用户看到的是 "Unknown key" 这类无从下手的文本。
struct ClientHandler {
    host: String,
    port: u16,
    store: Option<std::path::PathBuf>,
    reject: Arc<std::sync::Mutex<Option<String>>>,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();
        match verify_host_key(self.store.as_deref(), &self.host, self.port, &fp) {
            Ok(()) => Ok(true),
            Err(reason) => {
                if let Some(slot) = self.reject.lock().ok().as_mut() {
                    **slot = Some(reason);
                }
                Ok(false)
            }
        }
    }
}

/// 已连接的 SSH 会话。
pub struct SshSession {
    handle: russh::client::Handle<ClientHandler>,
}

// russh 的 Handle 不实现 Debug（含活跃连接内部状态），这里只输出结构名
impl std::fmt::Debug for SshSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSession").finish_non_exhaustive()
    }
}

impl SshSession {
    /// 建立连接并认证；conn_timeout 同时约束连接与认证两个阶段。
    /// 拨号经 [`crate::net::local_proxy`]：开启网络时走 mihomo mixed-port（白拿
    /// 一套规则路由——国内服务器自动直连、国外自动走节点），未开启时即直连。
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        auth: &AuthMaterial,
        conn_timeout: Duration,
    ) -> Result<Self, SshError> {
        Self::connect_via(
            host,
            port,
            user,
            auth,
            conn_timeout,
            crate::net::local_proxy::get(),
        )
        .await
    }

    /// 显式代理参数版本（None = 直连）。代理外提是为了可测——进程级登记表在并行
    /// 测试里无法安全复位。conn_timeout 覆盖「拨号（含代理隧道）+ SSH 握手」。
    pub(crate) async fn connect_via(
        host: &str,
        port: u16,
        user: &str,
        auth: &AuthMaterial,
        conn_timeout: Duration,
        proxy: Option<u16>,
    ) -> Result<Self, SshError> {
        let config = Arc::new(russh::client::Config::default());
        let reject: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let handler = ClientHandler {
            host: host.into(),
            port,
            store: KNOWN_HOSTS.get().cloned(),
            reject: reject.clone(),
        };
        let mut handle = tokio::time::timeout(conn_timeout, async {
            // 先建流再交给 russh：connect 的第二个参数是 impl AsyncRead + AsyncWrite，
            // 不限于 TcpStream——HTTP CONNECT 打通后拿回的就是那条 TcpStream。
            let stream = crate::net::local_proxy::dial(host, port, proxy)
                .await
                .map_err(|e| SshError::Connect {
                    host: host.into(),
                    port,
                    reason: e.to_string(),
                })?;
            // 用 connect_stream 而非 connect：后者只收 ToSocketAddrs（内部自建
            // TcpStream），前者才接受任意的 AsyncRead + AsyncWrite——传什么流由
            // 我们决定（russh 的 connect 实现本身就是 connect_stream 的一层包装）。
            russh::client::connect_stream(config, stream, handler)
                .await
                .map_err(|e| match reject.lock().ok().and_then(|g| g.clone()) {
                    // 主机密钥被拒：russh 只给一句通用错误，这里换成带新旧指纹的人话
                    Some(reason) => SshError::HostKey { reason },
                    None => SshError::Connect {
                        host: host.into(),
                        port,
                        reason: e.to_string(),
                    },
                })
        })
        .await
        .map_err(|_| SshError::Timeout {
            step: "连接".into(),
            secs: conn_timeout.as_secs(),
        })??;

        let accepted = match auth {
            AuthMaterial::Password(pw) => {
                tokio::time::timeout(conn_timeout, handle.authenticate_password(user, pw))
                    .await
                    .map_err(|_| SshError::Timeout {
                        step: "认证".into(),
                        secs: conn_timeout.as_secs(),
                    })?
                    .map_err(|e| SshError::Auth {
                        reason: e.to_string(),
                    })?
            }
            AuthMaterial::PrivateKey(pem) => {
                let key =
                    russh::keys::decode_secret_key(pem, None).map_err(|e| SshError::KeyFormat {
                        reason: e.to_string(),
                    })?;
                tokio::time::timeout(
                    conn_timeout,
                    handle.authenticate_publickey(
                        user,
                        russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None),
                    ),
                )
                .await
                .map_err(|_| SshError::Timeout {
                    step: "认证".into(),
                    secs: conn_timeout.as_secs(),
                })?
                .map_err(|e| SshError::Auth {
                    reason: e.to_string(),
                })?
            }
        };
        if !accepted.success() {
            return Err(SshError::Auth {
                reason: "服务端拒绝认证".into(),
            });
        }
        Ok(Self { handle })
    }

    /// 执行命令；超时即中断。非零退出不报错，输出原样返回。
    pub async fn run(
        &mut self,
        step: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Result<CommandOutput, SshError> {
        tokio::time::timeout(timeout, self.run_inner(cmd))
            .await
            .map_err(|_| SshError::Timeout {
                step: step.into(),
                secs: timeout.as_secs(),
            })?
    }

    async fn run_inner(&mut self, cmd: &str) -> Result<CommandOutput, SshError> {
        let mut ch =
            self.handle
                .channel_open_session()
                .await
                .map_err(|e| SshError::CommandFailed {
                    step: "打开通道".into(),
                    exit: 0,
                    stderr_tail: e.to_string(),
                })?;
        ch.exec(true, cmd)
            .await
            .map_err(|e| SshError::CommandFailed {
                step: "发送命令".into(),
                exit: 0,
                stderr_tail: e.to_string(),
            })?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit: Option<u32> = None;
        loop {
            match ch.wait().await {
                Some(russh::ChannelMsg::Data { data }) => {
                    stdout.push_str(&String::from_utf8_lossy(&data));
                }
                Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                    stderr.push_str(&String::from_utf8_lossy(&data));
                }
                Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                    exit = Some(exit_status);
                }
                Some(russh::ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        Ok(CommandOutput {
            stdout,
            stderr,
            exit_code: exit.unwrap_or(255),
        })
    }

    /// 执行命令并要求退出码为 0；失败时取 stderr，若为空则取 stdout 的末尾。
    /// Tor 的 --verify-config 把致命诊断写到 stdout，不能只报空白的 exit=1。
    pub async fn run_ok(
        &mut self,
        step: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Result<CommandOutput, SshError> {
        let out = self.run(step, cmd, timeout).await?;
        if out.exit_code != 0 {
            let output = if out.stderr.trim().is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            let tail: String = output
                .chars()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(SshError::CommandFailed {
                step: step.into(),
                exit: out.exit_code,
                stderr_tail: tail,
            });
        }
        Ok(out)
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/ssh/mods.rs"
));
