//! 本机 Tor sidecar：隐私保护（Tor Onion 入口）那一段的**客户端**。
//!
//! 链路：mihomo → 这里的 SOCKS5(`127.0.0.1:9050`) → Tor 网络 → `<onion>:<port>`。
//!
//! 没有它，隐私线生成的 `dialer-proxy: __privacy_tor__` 就指向一个没人监听的端口——
//! 那条线**连不上**。这是刻意的 fail-closed：宁可连不上，也不能悄悄回落到直连 VPS。
//!
//! 二进制照 mihomo sidecar 的同一套规矩：`scripts/fetch-tor.mjs` 下载 Tor Expert
//! Bundle、钉死 sha256、作为 tauri 资源打进安装包，首次使用时复制进数据目录。
//!
//! **端口固定 9050 而不是随机**：生成的 mihomo 配置里写死了 `127.0.0.1:9050`
//! （[`crate::servers::privacy::TOR_SOCKS_PORT`]），换个端口就要重建配置。
//! 代价是可能与用户自己的 Tor 撞端口：9050 已被占用时先核实监听进程是 tor.exe。

use crate::state::AppState;
use sha2::{Digest, Sha256};

/// 数据目录名（相对 data_dir）。
pub const TOR_DIR: &str = "tor";
/// tor.exe 文件名。
pub const TOR_EXE: &str = "tor.exe";
/// Tor Expert Bundle 15.0.25 里 `tor.exe` 的 sha256（2026-10-18 官方归档下载后本机
/// 计算，与 `scripts/fetch-tor.mjs` 的校验链同值）。换版本时两处必须人工同步。
pub const TOR_EXE_SHA256: &str = "8c38513b3c4584154158e7c584f4144849dc679a64cd679553d72dad28230c54";

/// 内置二进制的候选路径（按序取第一个存在的文件）：打包资源 → dev 下的 binaries/。
pub fn candidate_paths(resource: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = resource {
        paths.push(p.to_path_buf());
    }
    if cfg!(debug_assertions) {
        paths.push(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join("tor")
                .join(TOR_EXE),
        );
    }
    paths
}

/// 运行中的 Tor：端口供作业回连自测用，child 供退出清理。
pub struct RunningTor {
    pub socks_port: u16,
    pub child: Option<std::process::Child>,
    /// true = 进程是我们起的（我们要负责杀）；false = 复用了别人已经在听的 9050。
    pub owned: bool,
    /// 上次成功引导使用的普通节点；配置热重载后专用组可能重置，必须重新选回它。
    pub upstream_name: Option<String>,
}

/// 渲染本机 torrc。
///
/// 只写**必须**的几行，刻意不写的东西都有理由：
/// - 不写 `ControlPort`：Tor 默认「无认证开放控制端口」会打印一条吓人的警告，
///   而我们要的进度从日志里读就够了，犯不上多开一个能被本机任意进程改配置的口子；
/// - 不写 `GeoIPFile`：那是给按国家选路用的出口策略数据，纯客户端用不上。
///   不写会有两条 "Path for GeoIPFile (<default>) is relative" 的警告，无害；
///   带上则是 26MB 的安装包体积，不划算。
///
/// `upstream` 非空时把 Tor 自己的出站也挂到那个 SOCKS5 上（见 [`UpstreamProxy`]）。
pub fn render_torrc(tor_dir: &std::path::Path, socks_port: u16, upstream: Option<&str>) -> String {
    let mut out = String::new();
    // torrc 的双引号会把 Windows 反斜杠解析为转义（C:\Users 中的 \U 会报错）。
    // Tor 在 Windows 上接受正斜杠路径；保留引号以支持用户名中的空格。
    let data_path = tor_dir.join("data").to_string_lossy().replace('\\', "/");
    out.push_str(&format!("SocksPort 127.0.0.1:{socks_port}\n"));
    out.push_str(&format!("DataDirectory \"{data_path}\"\n"));
    // Windows 版 Tor 会把 `Log notice file "C:/..."` 的引号当作文件名的一部分，
    // 启动时因 Invalid argument 退出。把 stdout 重定向到 notice.log 才能兼容带空格路径。
    out.push_str("Log notice stdout\n");
    // 纯客户端：不跑出口中继、不跑目录镜像
    out.push_str("ClientOnly 1\n");
    if let Some(proxy) = upstream {
        out.push_str(&format!("Socks5Proxy {proxy}\n"));
    }
    out
}

/// Tor 自己的出站要不要走本机已有的代理。
///
/// 实测（2026-09-22 本机）：直连 Tor 网络 4 个目录权威**全部不通**，`tor.exe` 起来
/// 之后一行 bootstrap 进度都没有——受限网络里 Tor 的公网入口是被封的。挂到本机
/// 已通的 SOCKS5 上是这里唯一还能走通的路。
#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamProxy {
    /// 挂在 `127.0.0.1:{port}` 这个 SOCKS5 上（通常是本机 mihomo 的混合端口）。
    Socks { port: u16 },
}

impl UpstreamProxy {
    pub fn addr(&self) -> Option<String> {
        match self {
            UpstreamProxy::Socks { port } => Some(format!("127.0.0.1:{port}")),
        }
    }
}

/// 上游代理**由调用方传进来**，不在这里回读 `state.net`。
///
/// 这不是洁癖：`ensure_started` 持有 `tor` 锁，而回读 `net` 锁的方向是
/// `tor → net`——一旦有人在 `net` 锁内调它（`net::runtime::start()` 的端口候选循环、
/// `apply_config_change()` 全程持锁），就是教科书式的 AB-BA 自死锁。
/// 改成参数之后这条路径彻底不碰 `net` 锁，两个调用点又本来就把端口拿在手里
/// （`runtime.rs` 的 `mixed_port` 局部变量），没有一处需要为了拿端口去加锁。
///
/// 为什么要挂 mihomo：**这不是优化，是这台机器上唯一走得通的路**（2026-09-22 本机实测）：
/// 直连 Tor 网络时目录权威全部不通、`tor.exe` 起来后一行 bootstrap 进度都没有；
/// 挂到本机 mihomo 的 SOCKS5 上才走得到 `Bootstrapped 100%`。
/// 端口来自 mihomo 专供 Tor 的回环 SOCKS listener，固定走不含隐私线的自动组。
/// 因此用户把普通出口切到隐私线时，Tor 自己不会经该线回到 9050 形成环路。
pub fn upstream_for(tor_upstream_port: u16) -> UpstreamProxy {
    UpstreamProxy::Socks {
        port: tor_upstream_port,
    }
}

/// 从 Tor 日志里取出最后一次 bootstrap 进度（`Bootstrapped 45% ...`）。
pub fn parse_bootstrap(log: &str) -> Option<u8> {
    let mut last = None;
    for line in log.lines() {
        if let Some(idx) = line.find("Bootstrapped ") {
            let rest = &line[idx + "Bootstrapped ".len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse::<u8>() {
                last = Some(p);
            }
        }
    }
    last
}

/// 确保 dir 内有校验通过的 tor.exe 及其同目录 DLL。
pub fn ensure_binary(
    dir: &std::path::Path,
    resource: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let src = candidate_paths(resource)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "未找到内置 Tor 二进制，请先运行 npm run fetch:tor（dev 模式落盘 src-tauri/binaries/tor/tor.exe）"
                .to_string()
        })?;
    if !binary_ok(&src) {
        return Err(format!(
            "内置 Tor 二进制校验失败（sha256 不匹配: {}）",
            src.display()
        ));
    }
    // 当前固定版本为静态二进制；若资源目录提供 DLL，也一起复制到可执行文件旁。
    copy_runtime_files(&src, dir)?;
    let exe = dir.join(TOR_EXE);
    if !binary_ok(&exe) {
        return Err(format!(
            "Tor 二进制校验失败（sha256 不匹配: {}）",
            exe.display()
        ));
    }
    Ok(exe)
}

fn copy_runtime_files(src_exe: &std::path::Path, dir: &std::path::Path) -> Result<(), String> {
    let src_dir = src_exe.parent().ok_or("Tor 二进制没有父目录")?;
    for entry in std::fs::read_dir(src_dir).map_err(|e| format!("读取 Tor 资源目录失败: {e}"))?
    {
        let entry = entry.map_err(|e| format!("读取 Tor 资源失败: {e}"))?;
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let is_dll = src
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"));
        if !is_dll && src != src_exe {
            continue;
        }
        let dst = dir.join(entry.file_name());
        if sha256_hex(&src) != sha256_hex(&dst) {
            std::fs::copy(&src, &dst)
                .map_err(|e| format!("复制 Tor 运行文件 {} 失败: {e}", src.display()))?;
        }
    }
    Ok(())
}

fn binary_ok(path: &std::path::Path) -> bool {
    path.is_file() && sha256_hex(path).as_deref() == Some(TOR_EXE_SHA256)
}

fn sha256_hex(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex::encode(Sha256::digest(&bytes)))
}

/// 端口上有没有人在听（用来认出「用户自己的 Tor 已经在 9050 上」）。
pub fn socks_port_alive(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

fn existing_tor_listener(port: u16) -> Result<bool, String> {
    if !socks_port_alive(port) {
        return Ok(false);
    }
    let image = crate::net::winprobe::listener_image(port)?;
    if image
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case("tor.exe"))
    {
        Ok(true)
    } else {
        Err(format!(
            "9050 已被 {} 占用，无法确认它是 Tor",
            image.as_deref().unwrap_or("未知进程")
        ))
    }
}

/// 确保 Tor 在跑，返回 SOCKS 端口。已运行（我们的或别人的）直接返回。
///
/// 锁全程持有：与 mihomo sidecar 同款理由——「检查槽位 → 启动」之间的间隙会让
/// 并发调用各起一个进程，留下一个没人管的孤儿 Tor。
pub async fn ensure_started(state: &AppState, upstream: UpstreamProxy) -> Result<u16, String> {
    let mut guard = state.tor.lock().await;
    // 崩溃自愈：进程中途退出后槽位仍是 Some，复用死端口会全部失败
    if let Some(t) = guard.as_mut() {
        if let Some(child) = t.child.as_mut() {
            if child
                .try_wait()
                .map_err(|e| format!("检查 Tor 进程状态失败: {e}"))?
                .is_some()
            {
                guard.take();
            } else {
                return Ok(t.socks_port);
            }
        } else if existing_tor_listener(t.socks_port)? {
            return Ok(t.socks_port);
        } else {
            guard.take();
        }
    }

    let port = crate::servers::privacy::TOR_SOCKS_PORT;
    // 9050 也可能是别的 SOCKS 服务；只复用确认为 tor.exe 的监听进程。
    if existing_tor_listener(port)? {
        *guard = Some(RunningTor {
            socks_port: port,
            child: None,
            owned: false,
            upstream_name: None,
        });
        return Ok(port);
    }

    let tor_dir = state.data_dir.join(TOR_DIR);
    let resource: Option<std::path::PathBuf> = state
        .tor_resource
        .read()
        .map_err(|_| "Tor 资源锁读取失败".to_string())?
        .clone();
    let exe = ensure_binary(&tor_dir, resource.as_deref())?;

    let torrc_path = tor_dir.join("torrc");
    std::fs::write(
        &torrc_path,
        render_torrc(&tor_dir, port, upstream.addr().as_deref()),
    )
    .map_err(|e| format!("写入 torrc 失败: {e}"))?;

    let child = spawn_tor(&exe, &torrc_path, &tor_dir)?;

    *guard = Some(RunningTor {
        socks_port: port,
        child: Some(child),
        owned: true,
        upstream_name: None,
    });
    Ok(port)
}

fn spawn_tor(
    exe: &std::path::Path,
    torrc_path: &std::path::Path,
    tor_dir: &std::path::Path,
) -> Result<std::process::Child, String> {
    // File::create 每次截断旧日志，避免上次的 Bootstrapped 100% 被误认为本次就绪。
    // stdout 同时包含配置解析阶段的错误；即使 Tor 在日志系统初始化前退出也能排障。
    let log = std::fs::File::create(tor_dir.join("notice.log"))
        .map_err(|e| format!("创建 Tor 日志失败: {e}"))?;
    let stderr = log
        .try_clone()
        .map_err(|e| format!("复制 Tor 日志句柄失败: {e}"))?;
    crate::util::quiet(std::process::Command::new(exe))
        .args(["-f"])
        .arg(torrc_path)
        .current_dir(tor_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .map_err(|e| format!("启动 Tor 失败: {e}"))
}

/// 停掉我们自己起的 Tor 并清空槽位（App 退出清理）。复用别人的 9050 时**不杀**。
pub async fn stop_all(state: &AppState) {
    let mut guard = state.tor.lock().await;
    if let Some(mut t) = guard.take() {
        if t.owned {
            if let Some(child) = t.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

// ------------------------------------------------------------ 状态与看门狗

/// 本次会话 Tor（隐私线入口）的状态。
///
/// **增益功能的状态，不是「网络能不能用」的状态**：任何非 [`TorState::Ready`] 的取值
/// 都**不**表示「开启网络」失败了。与 `firewall` 同一只脚站立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorState {
    /// 没开网络，或这份配置里没有隐私线
    Off,
    /// 后台 bootstrap 中（detail 里带进度）
    Starting,
    /// 自己启动的 Tor 已引导完成。
    Ready,
    /// 复用了本机现有 Tor；引导状态不由本应用管理。
    External,
    /// 起不来：二进制缺失、进程退出、超时
    Failed,
}

impl TorState {
    pub fn as_str(self) -> &'static str {
        match self {
            TorState::Off => "off",
            TorState::Starting => "starting",
            TorState::Ready => "ready",
            TorState::External => "external",
            TorState::Failed => "failed",
        }
    }
}

/// 所有普通上游合计的 bootstrap 上限。单条 45 秒，TLS 握手反复失败时提前换线；
/// 超时后**如实报失败**而不是无限等。
pub const BOOTSTRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const ROUTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// 读一次 bootstrap 进度（0..=100）。日志还没出现 / 读不到 → None。
pub fn bootstrap_progress(state: &AppState) -> Option<u8> {
    let log = std::fs::read_to_string(state.data_dir.join(TOR_DIR).join("notice.log")).ok()?;
    parse_bootstrap(&log)
}

/// 日志最后一行非空内容（截 120 字符）。失败原因就在那行里——比如
/// `Address already in use`（9050 被抢）、`Problem bootstrapping`。
/// 把这句话带给用户，比「Tor 启动失败」有用得多。
fn last_log_line(state: &AppState) -> String {
    let Ok(log) = std::fs::read_to_string(state.data_dir.join(TOR_DIR).join("notice.log")) else {
        return String::new();
    };
    let last = log
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if last.is_empty() {
        return String::new();
    }
    let cut: String = last.chars().take(120).collect();
    format!("（日志末行：{cut}）")
}

/// 懒启动 Tor 并在后台跟到就绪。**幂等**（已有跟踪在飞就直接返回），**永不上抛**。
///
/// **只允许在 `net` 锁之外调用**：看门狗要回写状态（拿 `net` 锁），而
/// [`ensure_started`] 持 `tor` 锁——两边同时发生就是 AB-BA 死锁。
///
/// 失败绝不打掉「开启网络」：本函数返回 `()`、不 await 任务、不上抛任何错误，
/// 任务内部的一切失败都落成 [`TorState::Failed`] + 一句人话 detail 给界面看。
/// 调用点都在「网络已经起来、系统代理已经写好」之后。
pub fn spawn_bootstrap(
    state: &AppState,
    upstream: UpstreamProxy,
    ctrl_port: u16,
    token: String,
    candidates: Vec<String>,
) {
    let Some(arc) = state.shared() else {
        // 装配期（self_ref 还没注入）。不该发生：调用点都在网络起来之后。
        return;
    };
    // 已在跟踪：重复点「开启网络」不该把 bootstrap 从头再来一遍
    if state
        .tor_watch
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let fut = async move {
        if let Err(e) = watchdog(&arc, upstream, ctrl_port, &token, candidates).await {
            // 状态已经写进实例给界面看了，这里只留一行 stderr 供排障
            eprintln!("[vpn-ctl] Tor 未就绪: {e}");
        }
        arc.tor_watch
            .store(false, std::sync::atomic::Ordering::SeqCst);
    };
    // 与 jobs::JobQueue::submit 同款分流：主线程（无 Tokio 运行时）上 tokio::spawn
    // 会 panic 且跨不过 FFI 边界 → 进程 abort。当前两个调用点都在 async 上下文里，
    // 保留分流以防将来出现同步调用点。
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(fut);
    } else {
        tauri::async_runtime::spawn(fut);
    }
}

/// 优先尝试用户当前选中的普通出口；隐私线与自动组不在候选内，绝不形成自环。
fn prioritize_route(mut candidates: Vec<String>, preferred: Option<&str>) -> Vec<String> {
    if let Some(index) = preferred.and_then(|name| candidates.iter().position(|n| n == name)) {
        let first = candidates.remove(index);
        candidates.insert(0, first);
    }
    candidates
}

async fn route_order(ctrl_port: u16, token: &str, candidates: Vec<String>) -> Vec<String> {
    let result = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{ctrl_port}/proxies"))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;
    let preferred = match result {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(value) => value["proxies"][crate::config::MANUAL_GROUP]["now"]
                .as_str()
                .map(str::to_string),
            Err(_) => None,
        },
        Err(_) => None,
    };
    prioritize_route(candidates, preferred.as_deref())
}

async fn select_route(ctrl_port: u16, token: &str, name: &str) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .put(format!(
            "http://127.0.0.1:{ctrl_port}/proxies/{}",
            crate::config::TOR_UPSTREAM_GROUP
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name }))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("切换 Tor 上游失败: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect();
        Err(format!("切换 Tor 上游被拒（HTTP {status}）: {body}"))
    }
}

/// 看门狗：HTTP 测速通过不等于 Tor TLS 能握手，逐条尝试普通节点直到引导完成。
async fn watchdog(
    state: &AppState,
    upstream: UpstreamProxy,
    ctrl_port: u16,
    token: &str,
    candidates: Vec<String>,
) -> Result<(), String> {
    if !crate::net::runtime::publish_tor(state, TorState::Starting, Some("正在启动 Tor…".into()))
        .await
    {
        return Ok(()); // 网络已经关闭
    }
    if candidates.is_empty() {
        let detail = "Tor 隐私线需要至少一条已启用的普通节点作为上游出口".to_string();
        let _ =
            crate::net::runtime::publish_tor(state, TorState::Failed, Some(detail.clone())).await;
        return Err(detail);
    }

    // 配置热重载可能把 select 组重置到第一项；先选回已引导成功的上游。
    let active_route = {
        let mut slot = state.tor.lock().await;
        slot.as_mut().and_then(|t| {
            (t.owned
                && t.child
                    .as_mut()
                    .is_some_and(|child| matches!(child.try_wait(), Ok(None))))
            .then(|| t.upstream_name.clone())
            .flatten()
        })
    };
    if bootstrap_progress(state) == Some(100) {
        if let Some(name) = active_route.filter(|name| candidates.contains(name)) {
            if select_route(ctrl_port, token, &name).await.is_ok() {
                let _ = crate::net::runtime::publish_tor(state, TorState::Ready, None).await;
                return Ok(());
            }
        }
    }

    let routes = route_order(ctrl_port, token, candidates).await;
    let global_deadline = tokio::time::Instant::now() + BOOTSTRAP_TIMEOUT;
    let mut errors = Vec::new();
    for (index, name) in routes.iter().enumerate() {
        if tokio::time::Instant::now() >= global_deadline {
            break;
        }
        let label = format!("Tor 上游 {}/{}：{name}", index + 1, routes.len());
        if !crate::net::runtime::publish_tor(
            state,
            TorState::Starting,
            Some(format!("正在尝试 {label}")),
        )
        .await
        {
            return Ok(());
        }
        // 只停本应用持有的 Tor；它的失败重试不能重启 mihomo 或修改系统代理。
        stop_all(state).await;
        if let Err(e) = select_route(ctrl_port, token, name).await {
            errors.push(format!("{label}：{e}"));
            continue;
        }
        if state.net.lock().await.is_none() {
            return Ok(()); // 切换分组期间用户关网，不能再起一个孤儿 Tor
        }
        if let Err(e) = ensure_started(state, upstream.clone()).await {
            errors.push(format!("{label}：{e}"));
            continue;
        }
        let net_running = state.net.lock().await.is_some();
        if !net_running {
            stop_all(state).await;
            return Ok(());
        }
        let ours = state
            .tor
            .lock()
            .await
            .as_ref()
            .map(|t| t.owned)
            .unwrap_or(false);
        if !ours {
            let _ = crate::net::runtime::publish_tor(
                state,
                TorState::External,
                Some("使用本机现有 Tor；引导状态未核验".into()),
            )
            .await;
            return Ok(());
        }

        let route_deadline = (tokio::time::Instant::now() + ROUTE_TIMEOUT).min(global_deadline);
        let mut last = None;
        loop {
            let net_running = state.net.lock().await.is_some();
            if !net_running {
                stop_all(state).await;
                return Ok(()); // 用户关闭了网络；并发起的 Tor 也在这里兜底清理
            }
            let exited = {
                let mut slot = state.tor.lock().await;
                match slot.as_mut().and_then(|t| t.child.as_mut()) {
                    Some(child) => match child.try_wait() {
                        Ok(Some(code)) => Some(format!("Tor 进程已退出（{code}）")),
                        Ok(None) => None,
                        Err(e) => Some(format!("检查 Tor 进程失败: {e}")),
                    },
                    None => Some("Tor 进程已停止".to_string()),
                }
            };
            if let Some(reason) = exited {
                errors.push(format!("{label}：{reason}{}", last_log_line(state)));
                break;
            }
            match bootstrap_progress(state) {
                Some(100) => {
                    if let Some(t) = state.tor.lock().await.as_mut() {
                        t.upstream_name = Some(name.clone());
                    }
                    let _ = crate::net::runtime::publish_tor(state, TorState::Ready, None).await;
                    return Ok(());
                }
                Some(progress) if Some(progress) != last => {
                    last = Some(progress);
                    if !crate::net::runtime::publish_tor(
                        state,
                        TorState::Starting,
                        Some(format!("{label} · 已建立链路 {progress}%")),
                    )
                    .await
                    {
                        return Ok(());
                    }
                }
                _ => {}
            }
            let log = std::fs::read_to_string(state.data_dir.join(TOR_DIR).join("notice.log"))
                .unwrap_or_default();
            if last.unwrap_or(0) <= 10 && log.matches("Problem bootstrapping").count() >= 4 {
                errors.push(format!(
                    "{label}：Tor TLS 握手持续失败{}",
                    last_log_line(state)
                ));
                break;
            }
            if tokio::time::Instant::now() >= route_deadline {
                errors.push(format!(
                    "{label}：{} 秒内未就绪{}",
                    ROUTE_TIMEOUT.as_secs(),
                    last_log_line(state)
                ));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    }

    stop_all(state).await;
    let detail = format!(
        "Tor 已尝试 {} 条普通上游仍未就绪；{}",
        errors.len(),
        errors.last().map(String::as_str).unwrap_or("引导超时")
    );
    if crate::net::runtime::publish_tor(state, TorState::Failed, Some(detail.clone())).await {
        Err(detail)
    } else {
        Ok(())
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/tor/mods.rs"
));
