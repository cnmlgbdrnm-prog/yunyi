//! mihomo sidecar 生命周期管理：二进制定位/校验/复制、config.yaml 渲染、随机端口与
//! token 分配、懒启动与退出清理。输入：AppState（data_dir/sidecar_resource/sidecar
//! 槽位）；输出：RunningSidecar 句柄或中文可读错误。依赖：reqwest（控制器健康轮询）、
//! sha2/hex（二进制校验）、rand（token）；spawn/kill 均用 std::process 快速同步调用，
//! 不引入 tokio process feature。

use crate::state::AppState;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// sidecar 数据目录名（相对 data_dir）。
pub const SIDECAR_DIR: &str = "sidecar";
/// sidecar 可执行文件名。
pub const SIDECAR_EXE: &str = "mihomo.exe";
/// mihomo-windows-amd64.exe v1.19.10 的 sha256（2026-08-14 官方 release 下载后本机
/// 计算，与 scripts/fetch-sidecar.mjs 的 exe digest 校验链一致）。
pub const SIDECAR_EXE_SHA256: &str =
    "a4feef7e4f8cbaf19e2079b3caa3e23821dfe419ad2e39e0539b843bd2d4d1be";

/// 内置二进制候选路径（按序取第一个存在的文件）：
/// 1. 打包资源路径（Resource 解析，安装目录真实文件）；
/// 2. debug 构建时 src-tauri/binaries/mihomo.exe（`npm run fetch:sidecar` 落盘位置；
///    dev 下 Resource 解析到 target/debug 的不存在路径，必须回退到 manifest 目录）；
/// 3. 两者都缺时为空列表 → 调用方报「未找到」提示语。
pub fn candidate_paths(resource: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = resource {
        paths.push(p.to_path_buf());
    }
    if cfg!(debug_assertions) {
        paths.push(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join(SIDECAR_EXE),
        );
    }
    paths
}

/// 运行中的 sidecar：端口/token 供测速引擎与代理下载使用，child 供退出清理。
pub struct RunningSidecar {
    pub controller_port: u16,
    pub mixed_port: u16,
    pub token: String,
    pub child: std::process::Child,
}

/// 渲染 sidecar config.yaml：mixed-port/external-controller 仅绑 127.0.0.1，secret 为
/// 随机 token，proxies 置空（节点按需经 /configs 热重载注入——mihomo 无
/// 运行时增删 proxy 的 API，注入 = 重载整份配置）。
pub fn render_sidecar_config(controller_port: u16, mixed_port: u16, token: &str) -> String {
    format!(
        "mixed-port: {mixed_port}\nexternal-controller: 127.0.0.1:{controller_port}\nsecret: {token}\nproxies: []\n"
    )
}

/// 注入 sidecar 的一条代理：监听端口由引擎分配，供该节点独占测速。
pub struct SidecarProxySpec<'a> {
    /// 代理名（节点 id 或测速专用别名），必须与 `proxy.name` 一致。
    pub name: &'a str,
    pub proxy: &'a serde_json::Value,
    /// 本节点专用的 mixed 监听端口；并发测速时每个任务一个，互不覆盖。
    pub port: u16,
}

/// 渲染注入多条代理的 sidecar 配置。
///
/// **必须一次带上当前所有在测节点**：mihomo 的 `PUT /configs` 是整份替换，不是
/// 「按名字追加」。并发测速若各自只渲染自己那一条，后到的任务会把先到的节点从
/// 配置里抹掉（实测 A 注入后 B 注入，A 的 delay 直接 404，带宽还会走到 B）。
/// 每个节点配一个 `listeners` 里的 mixed 端口，下载走自己的端口，`mixed-port`
/// 本身不再承担测速流量。
pub fn render_sidecar_config_with_listeners(
    controller_port: u16,
    mixed_port: u16,
    token: &str,
    proxies: &[SidecarProxySpec<'_>],
) -> String {
    let mut yaml = format!(
        "mixed-port: {mixed_port}\nexternal-controller: 127.0.0.1:{controller_port}\nsecret: {token}\nmode: rule\n"
    );
    if proxies.is_empty() {
        yaml.push_str("proxies: []\n");
    } else {
        yaml.push_str("proxies:\n");
        for p in proxies {
            yaml.push_str("  - ");
            yaml.push_str(&serde_json::to_string(p.proxy).expect("JSON Value 必然可序列化"));
            yaml.push('\n');
        }
        yaml.push_str("listeners:\n");
        for (index, p) in proxies.iter().enumerate() {
            let proxy_name = serde_json::to_string(p.name).expect("代理名必然是合法 JSON 字符串");
            yaml.push_str(&format!(
                "  - {{name: \"vpnctl-test-{index}\", type: mixed, listen: 127.0.0.1, port: {}, proxy: {proxy_name}, udp: false}}\n",
                p.port
            ));
        }
    }
    // mixed-port 只保留给兼容/排障；测速流量一律走上面的 per-node listener。
    yaml.push_str("rules:\n  - MATCH,DIRECT\n");
    yaml
}

/// sidecar config.yaml 路径（引擎热重载 payload 与 PUT /configs 的 path 同源）。
pub fn sidecar_config_path(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join(SIDECAR_DIR).join("config.yaml")
}

/// 随机控制器 token：16 个 hex 字符（8 字节随机数）。
pub fn random_token() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// 分配两个本机随机端口（控制器 + 混合代理）：bind 两次取端口后立即释放。
pub fn alloc_local_ports() -> std::io::Result<(u16, u16)> {
    let a = std::net::TcpListener::bind("127.0.0.1:0")?;
    let b = std::net::TcpListener::bind("127.0.0.1:0")?;
    let pa = a.local_addr()?.port();
    let pb = b.local_addr()?.port();
    drop(a);
    drop(b);
    Ok((pa, pb))
}

/// 确保 sidecar 已启动：已运行直接返回既有端口/token；否则解析二进制、渲染配置、
/// 拉起子进程并轮询控制器就绪（≤5s）。任一步失败返回中文可读错误。
/// 入口处一次性获取 state.sidecar 锁并全程持有（tokio Mutex 可跨 await），
/// 消除「检查槽位 → 启动」间隙：并发调用不会互相覆盖槽位导致孤儿进程。
pub async fn ensure_started(state: &AppState) -> Result<(u16, u16, String), String> {
    let mut guard = state.sidecar.lock().await;
    // 崩溃自愈：mihomo 中途退出后槽位仍 Some，复用死端口会全部失败——
    // 检测到子进程已退出则清槽位，走下方正常启动流程重启
    let mut stale = false;
    if let Some(s) = guard.as_mut() {
        if s.child
            .try_wait()
            .map_err(|e| format!("检查 mihomo 进程状态失败: {e}"))?
            .is_some()
        {
            guard.take();
            stale = true;
        } else {
            return Ok((s.controller_port, s.mixed_port, s.token.clone()));
        }
    }

    let sidecar_dir = state.data_dir.join(SIDECAR_DIR);
    // 守卫必须立刻释放（RwLockReadGuard 不 Send，跨 await 直接编译失败）：
    // 读锁只用于取路径字符串，克隆出来即丢
    let resource: Option<std::path::PathBuf> = state
        .sidecar_resource
        .read()
        .map_err(|_| "sidecar 资源锁读取失败".to_string())?
        .clone();
    let exe_path = ensure_binary(&sidecar_dir, resource.as_deref())?;

    let (controller_port, mixed_port) =
        alloc_local_ports().map_err(|e| format!("分配端口失败: {e}"))?;
    let token = random_token();
    std::fs::write(
        sidecar_dir.join("config.yaml"),
        render_sidecar_config(controller_port, mixed_port, &token),
    )
    .map_err(|e| format!("写入 sidecar 配置失败: {e}"))?;
    // -d 指定配置目录：该 mihomo 版本裸 spawn 不读 cwd 的 config.yaml（只按默认
    // 端口监听），-d 后 config.yaml/geodata 均在侧车目录内生效
    let mut child = crate::util::quiet(std::process::Command::new(&exe_path))
        .args(["-d", sidecar_dir.to_str().unwrap_or(".")])
        .current_dir(&sidecar_dir)
        .spawn()
        .map_err(|e| format!("启动 mihomo 失败: {e}"))?;
    if let Err(e) = wait_ready(
        Some(&mut child),
        controller_port,
        &token,
        std::time::Duration::from_secs(5),
    )
    .await
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(if stale {
            format!("上次 sidecar 已退出，重新启动失败: {e}")
        } else {
            e
        });
    }

    let running = RunningSidecar {
        controller_port,
        mixed_port,
        token: token.clone(),
        child,
    };
    *guard = Some(running);
    Ok((controller_port, mixed_port, token))
}

/// 轮询控制器 /version（Bearer token，单请求 500ms 超时，200ms 间隔）直到就绪。
/// 每次轮询间隙检查子进程是否已退出：启动即崩（config 错误/缺依赖）时立即给出
/// 进程退出诊断而非空转满 timeout；timeout 未就绪判定超时。
/// timeout 由调用方决定：测速 sidecar（无 geo 规则）5s 足够；本机上网客户端
/// 带 geoip/geosite 规则启动首帧解析更慢，用 15s。
/// 等控制器就绪。`child` 为 `None` 时跳过「进程早退」诊断——上网客户端经提权
/// 服务起 mihomo 时，子进程归服务持有，主程序拿不到句柄。此时仍保留超时兜底，
/// 只是把「立刻告诉你进程死了」退化成「等到超时」，诊断信息少一档。
pub async fn wait_ready(
    mut child: Option<&mut std::process::Child>,
    controller_port: u16,
    token: &str,
    ready_timeout: std::time::Duration,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{controller_port}/version");
    let deadline = tokio::time::Instant::now() + ready_timeout;
    loop {
        let ready = client
            .get(&url)
            .bearer_auth(token)
            .timeout(std::time::Duration::from_millis(500))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ready {
            return Ok(());
        }
        if let Some(child) = child.as_deref_mut() {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("检查 mihomo 进程状态失败: {e}"))?
            {
                return Err(format!("mihomo 启动失败，退出状态 {status}"));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "mihomo 控制器启动超时（{}s 未就绪）",
                ready_timeout.as_secs()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// 停止 sidecar 并清空槽位（App 退出清理）；未运行时为空操作。
pub async fn stop_all(state: &AppState) {
    let mut guard = state.sidecar.lock().await;
    if let Some(mut s) = guard.take() {
        let _ = s.child.kill();
        let _ = s.child.wait();
    }
}

/// 确保 dir 内存在校验通过的 mihomo 二进制：已匹配直接返回路径；
/// 否则从 candidate_paths 首个存在的源复制；两者皆无返回中文错误。
/// 幂等（sha256 匹配即复用），供 sidecar 与本机上网客户端共用。
pub fn ensure_binary(
    dir: &std::path::Path,
    resource: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let exe_path = dir.join(SIDECAR_EXE);
    if binary_ok(&exe_path) {
        return Ok(exe_path);
    }
    let src = candidate_paths(resource)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "未找到内置 mihomo 二进制，请先运行 npm run fetch:sidecar（dev 模式落盘 src-tauri/binaries/mihomo.exe）".to_string()
        })?;
    std::fs::copy(&src, &exe_path).map_err(|e| format!("复制 mihomo 失败: {e}"))?;
    if !binary_ok(&exe_path) {
        return Err(format!(
            "mihomo 二进制校验失败（sha256 不匹配: {}）",
            exe_path.display()
        ));
    }
    Ok(exe_path)
}

/// 数据目录内的二进制是否已存在且 sha256 匹配（匹配则跳过复制）。
fn binary_ok(path: &std::path::Path) -> bool {
    path.is_file() && sha256_hex(path).as_deref() == Some(SIDECAR_EXE_SHA256)
}

fn sha256_hex(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex::encode(Sha256::digest(&bytes)))
}
