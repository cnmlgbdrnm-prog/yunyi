//! 隐私保护：把**某条既有线路**的入口换成 Tor Onion。**是开关，不是新节点。**
//!
//! 链路：客户端 → 本机 Tor(9050) → Tor 网络 → `<onion>:<port>` → 该机器
//! `127.0.0.1:<port>`（**那条线自己已经在监听的端口**）→ 原样走它本来的出站。
//!
//! 因此本模块**只碰 Tor**：
//! - 不新建 Xray 入站、不新建凭据、不新建出站；
//! - Xray 配置一个字节都不动，也就不需要校验/重启/回滚它；
//! - 出口是那条线原本的出口（有 `out-up-*` 就是住宅 IP，没有就是 `freedom`）。
//!
//! 失败面因此小得多：唯一的远端改动是 torrc 里带标记的一段（可精确摘除），
//! 回滚只需恢复 torrc + 重启 Tor。**绝不留下坏 torrc 或打不开的节点。**
//!
//! Onion 私钥纪律：只回读 `hostname` 文件，**永不读取、输出或上传 PrivateKey**；
//! 日志里只出现 `.onion` 地址本身。

use crate::db::repos;
use crate::domain::{JobProgress, Node, Privacy, RelayRule, Server};
use crate::jobs::{EmitFn, PrivacyReq};
use crate::servers::deploy::{
    parse_capability, read_remote_file, run_cmd, run_cmd_ok, wait_service_active,
    write_remote_file, RemoteFile, CAPABILITY_PROBE_SCRIPT, CONFIG_TIMEOUT, INSTALL_TIMEOUT,
};
use crate::servers::{load_auth, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};

/// Tor 主配置（Debian/Ubuntu 默认位置）。
pub const TOR_CONFIG_PATH: &str = "/etc/tor/torrc";
/// systemd 单元名。
pub const TOR_SERVICE: &str = "tor";
/// 本机 Tor 的 SOCKS 地址与端口（客户端侧）。
pub const TOR_SOCKS_HOST: &str = "127.0.0.1";
pub const TOR_SOCKS_PORT: u16 = 9050;
/// 新 Onion 的描述符与引入点建立需要时间；单次 12 秒 curl 会误判正常服务。
pub const ONION_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);
/// 客户端 mihomo 里那个内部依赖代理的名字（不出现在任何分组）。
pub const INTERNAL_TOR_PROXY: &str = "__privacy_tor__";
/// HiddenServiceDir 前缀；完整路径 = 前缀 + 记录 id，**每条独立**。
pub const TOR_DIR_PREFIX: &str = "/var/lib/tor/app-privacy-";
/// v3 onion 地址的固定后缀。
pub const ONION_SUFFIX: &str = ".onion";
/// 开了隐私的那条线在客户端列表里的名字后缀。
pub const NODE_SUFFIX: &str = " [隐私]";

/// 远端 SSH 常以 root 运行，而 Tor 的身份目录由 daemon 用户持有。用 root 跑
/// --verify-config 会把已经正常运行的 HiddenServiceDir 误报为配置错误。
/// 目录属主就是本应用隐藏服务应使用的用户；runuser 不可用时退到 su。
pub(crate) fn tor_verify_command() -> String {
    format!(
        "tor_owner=$(stat -c %U /var/lib/tor) || exit 1; \
         if [ \"$tor_owner\" = root ]; then tor --verify-config -f {config}; \
         elif command -v runuser >/dev/null 2>&1; then \
           runuser -u \"$tor_owner\" -- tor --verify-config -f {config}; \
         else su -s /bin/sh -c 'tor --verify-config -f {config}' \"$tor_owner\"; fi",
        config = TOR_CONFIG_PATH
    )
}

/// 本条独享的 Tor HiddenServiceDir。
pub fn hidden_service_dir(id: i64) -> String {
    format!("{TOR_DIR_PREFIX}{id}")
}

/// torrc 段的起始标记（无空格、无正则元字符，便于 sed 精确圈段）。
pub fn torrc_begin(id: i64) -> String {
    format!("#vpnctl:privacy:{id}:begin")
}

/// torrc 段的结束标记。
pub fn torrc_end(id: i64) -> String {
    format!("#vpnctl:privacy:{id}:end")
}

/// 渲染要追加到 torrc 的一段。
///
/// `listen_port` 是 onion 侧监听的端口——客户端配置里的 `port` 必须是它，而客户端
/// 只换 `server`、不改 `port`，所以它就是 [`crate::config::effective_node`] 算出来的
/// 入口端口；转发目标是那条线**真正的服务地址**，不一定与它同号（中转线的入口端口
/// 与隧道那头的落地端口是两回事）。
///
/// 用标记包起来是为了**删除时只删这一段**：用户自己的 HiddenService（如果有）
/// 一行都不能动，别的线路的隐私段也不能动。
pub fn render_torrc_block(id: i64, listen_port: i64, fwd_host: &str, fwd_port: i64) -> String {
    format!(
        "{begin}\n\
         HiddenServiceDir {dir}/\n\
         HiddenServiceVersion 3\n\
         HiddenServicePort {listen_port} {fwd_host}:{fwd_port}\n\
         {end}\n",
        begin = torrc_begin(id),
        end = torrc_end(id),
        dir = hidden_service_dir(id),
    )
}

/// 去掉 torrc 里本条的段落（部署前与拆除时都用）。纯函数，便于测试。
pub fn strip_torrc_block(torrc: &str, id: i64) -> String {
    let begin = torrc_begin(id);
    let end = torrc_end(id);
    let mut out = String::with_capacity(torrc.len());
    let mut skipping = false;
    for line in torrc.lines() {
        if line.trim() == begin {
            skipping = true;
            continue;
        }
        if line.trim() == end {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 校验 v3 onion 地址：56 个 base32 字符 + `.onion`。
///
/// 只认小写——Tor 生成的 hostname 恒为小写，出现大写说明拿到的不是 hostname
/// 文件的内容（多半读错了东西），此时宁可判非法也不要把脏值写进库。
pub fn is_valid_onion(s: &str) -> bool {
    let Some(body) = s.strip_suffix(ONION_SUFFIX) else {
        return false;
    };
    body.len() == 56
        && body
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

/// 从 `cat hostname` 的输出里取出 onion 地址。取不出 → `None`（调用方按失败处理，
/// 绝不把空串或半截内容写进库）。
pub fn parse_onion_hostname(stdout: &str) -> Option<String> {
    let s = stdout.trim();
    if is_valid_onion(s) {
        Some(s.to_string())
    } else {
        None
    }
}

/// curl 的 time_pretransfer 包括 SOCKS 协商；大于零说明已连到 Onion 虚拟端口。
/// HTTP 退出码不作判据：SS/VLESS/Trojan 可能一直不回复明文 HTTP，curl 随后超时。
pub fn onion_probe_connected(output: &str) -> bool {
    output
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pre="))
        .and_then(|value| value.parse::<f64>().ok())
        .is_some_and(|seconds| seconds.is_finite() && seconds > 0.0)
}

/// SSH 通道可能在远端 Tor 已重载后断开。此时必须重新连接核对远端事实，
/// 不能只凭旧会话的错误把已经运行的 Onion 记为「未生效」。
fn is_ssh_transport_loss(error: &str) -> bool {
    [
        "Disconnected",
        "Channel send error",
        "Connection reset",
        "Broken pipe",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

/// 开了隐私之后，这条线在客户端列表里显示的名字。
pub fn display_name(original: &str) -> String {
    format!("{original}{NODE_SUFFIX}")
}

/// 隐藏服务该开在哪台机器、转发到哪里。
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// 部署隐藏服务的那台机器：直连线＝落地机自己，forward 中转线＝中转机。
    pub hs_server_id: i64,
    /// onion 侧监听的端口（客户端配置里的 `port`）。
    pub listen_port: i64,
    pub fwd_host: String,
    pub fwd_port: i64,
}

/// 由这条线的**生效入口**推出隐藏服务的落点。**与生成侧同源**：入口是什么样，
/// 隐藏服务就开在入口那台机器上、转发到入口本来会送到的那一站。
///
/// - 没挂中转（`node_relay_chain` 返回 None，与 `effective_node` 同一条判据）：
///   入口就是这台机器，服务在 `127.0.0.1:{node.port}`；
/// - forward 模式：入口是**中转机**的转发端口，DNAT 把它送进隧道到
///   `{landing_wg_ip}:{landing_port}`——隐藏服务开在中转机上、直发隧道目标。
///   注意**不能**转发到中转机的 `127.0.0.1:{入口端口}`：DNAT 挂在 PREROUTING，
///   只对从公网口进来的包生效，中转机本机发往回环的流量走 OUTPUT，规则根本不参与，
///   那个端口上也没有任何东西在听；
/// - peer 模式：客户端自己就是隧道里的 peer，入口是落地机的隧道地址，隐藏服务开在
///   落地机上、同样转发到 `{landing_wg_ip}:{landing_port}`。
///
/// 后两种情形共用同一句转发目标并非巧合：收敛
/// （`relay/build.rs` 的 `rewrite_listen`）会把落地机的入站监听地址从 `0.0.0.0`
/// 改写成 `landing_wg_ip`，所以**隧道地址才是那时唯一在听的地方**——落地机上的
/// `127.0.0.1:{port}` 同样连不上。这也是为什么不能按「入口是不是本机」分两种情况写。
pub fn resolve_target(
    node: &Node,
    server: &Server,
    rules: &[RelayRule],
    servers: &[Server],
) -> Target {
    match crate::config::node_relay_chain(node, rules, servers) {
        None => Target {
            hs_server_id: server.id,
            listen_port: node.port,
            fwd_host: "127.0.0.1".into(),
            fwd_port: node.port,
        },
        Some((rule, relay, _)) => Target {
            // 客户端配置里的 port 由 effective_node 算出，隐藏服务要照着它听
            hs_server_id: if rule.client_mode == "forward" {
                relay.id
            } else {
                server.id
            },
            listen_port: crate::config::effective_node(node, rules, servers).port,
            fwd_host: rule.landing_wg_ip.clone(),
            fwd_port: rule.landing_port,
        },
    }
}

/// 生成客户端配置时用：这条线开着隐私吗？开着就返回它的记录（含 onion）。
///
/// `onion_host` 为空表示部署中断（记录已建但地址还没回读到），此时**不生效**——
/// 宁可按普通线路走，也不能拿一个空 server 去生成配置（mihomo 会拒载整份配置）。
pub fn effective_privacy<'a>(node_id: &str, privacies: &'a [Privacy]) -> Option<&'a Privacy> {
    privacies
        .iter()
        .find(|p| p.node_id == node_id && !p.onion_host.is_empty())
}

// ---------------------------------------------------------------- 作业执行体

/// 远端备份路径（每次部署前覆盖写；成功后删除）。
fn bak_of(path: &str, id: i64) -> String {
    format!("{path}.vpnctl-bak-{id}")
}

/// 把 torrc 备份到本条的备份路径。文件**不存在时建一个空文件**：本次操作一定会写
/// torrc，回滚时不能出现「没有备份可还原」而留下半成品。
/// 开启与关闭两条路径共用，避免两处各写一份 shell 而慢慢漂移。
async fn backup_torrc(sess: &mut SshSession, cancel: &AtomicBool, id: i64) -> Result<(), String> {
    run_cmd_ok(
        sess,
        cancel,
        "备份",
        &format!(
            "( [ -f {torrc} ] && cp -a {torrc} {bak} || touch {torrc} ) && true",
            torrc = TOR_CONFIG_PATH,
            bak = bak_of(TOR_CONFIG_PATH, id)
        ),
        CONFIG_TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// 用备份**原样**还原 torrc 并重载 Tor——拆除中途失败的回滚。
///
/// 与 [`rollback`] 的区别是语义方向：`rollback` 是「开启失败」，要顺手把本条段摘掉；
/// 这里是「关闭失败」，必须把含本条段的原样配置放回去，那条线路才能照旧可用。
/// 同样不继承用户的取消标记：取消可能正发生在写盘之后，还原仍必须跑完
/// （故这里**不收** cancel 参数——回滚用恒假 flag，与 [`rollback`] 一致）。
async fn restore_torrc(sess: &mut SshSession, id: i64) -> Result<(), String> {
    let never_cancel = AtomicBool::new(false);
    let bak = bak_of(TOR_CONFIG_PATH, id);
    let mut failures = Vec::new();
    if let Err(e) = run_cmd_ok(
        sess,
        &never_cancel,
        "回滚",
        &format!(
            "if [ -f {bak} ]; then cp -a {bak} {torrc}; else echo 'Tor 配置备份缺失' >&2; exit 1; fi",
            torrc = TOR_CONFIG_PATH
        ),
        CONFIG_TIMEOUT,
    )
    .await
    {
        failures.push(format!("恢复 Tor 配置失败: {e}"));
    }
    if failures.is_empty() {
        // 优先 reload（不断已有连接）；不支持才 restart —— 与开启路径同构
        if let Err(e) = run_cmd_ok(
            sess,
            &never_cancel,
            "回滚",
            &format!(
                "systemctl reload {TOR_SERVICE} 2>/dev/null || systemctl restart {TOR_SERVICE}"
            ),
            CONFIG_TIMEOUT,
        )
        .await
        {
            failures.push(format!("重载 Tor 失败: {e}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

/// 回滚远端配置。校验阶段尚未碰服务时只恢复文件；重载已尝试过才恢复服务状态。
///
/// 不沿用用户的取消标记：取消可能正发生在写入之后，回滚仍必须执行。
/// 恢复失败时保留备份并如实报错，不能宣称「已回滚」。
pub(crate) async fn rollback(
    sess: &mut SshSession,
    id: i64,
    tor_reload_attempted: bool,
) -> Result<(), String> {
    let never_cancel = AtomicBool::new(false);
    let bak = bak_of(TOR_CONFIG_PATH, id);
    let mut failures = Vec::new();
    let restored = match run_cmd_ok(
        sess,
        &never_cancel,
        "回滚",
        &format!(
            "if [ -f {bak} ]; then cp -a {bak} {torrc}; else echo 'Tor 配置备份缺失' >&2; exit 1; fi",
            torrc = TOR_CONFIG_PATH
        ),
        CONFIG_TIMEOUT,
    )
    .await
    {
        Ok(_) => true,
        Err(e) => {
            failures.push(format!("恢复 Tor 配置失败: {e}"));
            false
        }
    };
    // 即便备份里已有本条标记也摘掉它：失败的隐私身份不能继续留在 torrc。
    let stripped = match run_cmd_ok(
        sess,
        &never_cancel,
        "回滚",
        &format!(
            "sed -i '/^{begin}$/,/^{end}$/d' {torrc}",
            begin = torrc_begin(id),
            end = torrc_end(id),
            torrc = TOR_CONFIG_PATH
        ),
        CONFIG_TIMEOUT,
    )
    .await
    {
        Ok(_) => true,
        Err(e) => {
            failures.push(format!("摘除失败的 Onion 配置失败: {e}"));
            false
        }
    };
    // 只清尚未生成 hostname 的身份；已有地址的身份可能已被用户分发，不可删除。
    let dir = hidden_service_dir(id);
    if let Err(e) = run_cmd_ok(
        sess,
        &never_cancel,
        "回滚",
        &format!("if [ -d {dir} ] && [ ! -f {dir}/hostname ]; then rm -rf {dir}; fi"),
        CONFIG_TIMEOUT,
    )
    .await
    {
        failures.push(format!("清理未完成的 Onion 身份失败: {e}"));
    }
    if restored && stripped {
        if tor_reload_attempted {
            // 优先 reload，保留现有电路；只有 reload 不可用才 restart。
            if let Err(e) = run_cmd_ok(
                sess,
                &never_cancel,
                "回滚",
                &format!(
                    "systemctl reload {TOR_SERVICE} 2>/dev/null || systemctl restart {TOR_SERVICE}"
                ),
                CONFIG_TIMEOUT,
            )
            .await
            {
                failures.push(format!("恢复 Tor 服务配置失败: {e}"));
            } else if let Err(e) = wait_service_active(sess, &never_cancel, TOR_SERVICE).await {
                failures.push(format!("Tor 服务未恢复运行: {e}"));
            }
        }
        if failures.is_empty() {
            if let Err(e) = run_cmd_ok(
                sess,
                &never_cancel,
                "回滚",
                &format!("rm -f {bak}"),
                CONFIG_TIMEOUT,
            )
            .await
            {
                failures.push(format!("清理备份失败: {e}"));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

/// 等待 Tor 写出 hostname 文件。Tor 首次建 v3 身份通常数秒内完成。
async fn wait_onion_hostname(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    tor_dir: &str,
) -> Result<String, String> {
    let path = format!("{tor_dir}/hostname");
    // 20 次 × 1.5s = 30s；超时说明 Tor 没把服务接起来，按失败处理（会触发回滚）
    for _ in 0..20 {
        if cancel.load(Ordering::SeqCst) {
            return Err("已取消".into());
        }
        let out = run_cmd(
            sess,
            cancel,
            "等待",
            &format!("cat {path} 2>/dev/null || true"),
            CONFIG_TIMEOUT,
        )
        .await?;
        if let Some(onion) = parse_onion_hostname(&out.stdout) {
            return Ok(onion);
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
    Err(format!(
        "等待 Tor 生成 onion 地址超时（{path} 30 秒内未出现有效地址）"
    ))
}

/// 远端 Onion 建成后还要验证本机实际能经这条代理打开 HTTPS。
/// 配置重载或 Tor 引导失败时保留错误，让界面显示「需处理」，不能只凭端口通就报成功。
async fn sync_local_private(
    state: &AppState,
    target: &Privacy,
    emit: &(dyn Fn(&str, &str, i64) + Send + Sync),
) -> Result<(), String> {
    let result = match state.subs.rebuild_config().await {
        Ok(()) => match crate::net::runtime::apply_config_change(state).await {
            Ok(crate::net::runtime::ConfigApply::NotRunning) => Ok(()),
            Ok(_) => {
                emit("核验", "验证本机隐私线路的 HTTPS 请求", 97);
                crate::net::runtime::verify_private_proxy(state, &display_name(&target.name))
                    .await
                    .map_err(|e| format!("隐私线路 HTTPS 核验失败: {e}"))
            }
            Err(e) => Err(format!("更新运行中客户端配置失败: {e}")),
        },
        Err(e) => Err(format!("重建客户端配置失败: {e}")),
    };
    match result {
        Ok(()) => {
            repos::privacies::set_error(&state.db.lock(), target.id, None)
                .map_err(|e| format!("清除隐私错误失败: {e}"))?;
            emit("完成", "隐私保护已开启且客户端可连接", 100);
            Ok(())
        }
        Err(e) => {
            let detail = format!("远端 Onion 已建立，但本机配置未确认生效: {e}");
            repos::privacies::set_error(&state.db.lock(), target.id, Some(&detail))
                .map_err(|db_err| format!("{detail}；记录错误失败: {db_err}"))?;
            Err(detail)
        }
    }
}

/// 打开某条线的隐私开关（或重建已有的）。
///
/// 幂等：记录由命令层先落库（端口与 tor_dir 此刻已定），本作业只负责把它变成
/// 远端事实——所以首开与重开跑的是同一条路径。
pub async fn run_privacy_job(
    state: &AppState,
    req: &PrivacyReq,
    cancel: &AtomicBool,
    emit_raw: &EmitFn,
) -> Result<(), String> {
    let emit = |step: &str, message: &str, progress: i64| {
        emit_raw(JobProgress {
            job_id: req.job_id,
            kind: "privacy".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    let check_cancel = || -> Result<(), String> {
        if cancel.load(Ordering::SeqCst) {
            Err("已取消".to_string())
        } else {
            Ok(())
        }
    };

    // 1 装载
    emit("探测", "读取服务器与隐私开关", 5);
    let (auth, server, target) = {
        let (auth, server) =
            load_auth(state, req.server_id).map_err(|e| format!("读取服务器失败: {e}"))?;
        let conn = state.db.lock();
        let target = repos::privacies::get(&conn, req.privacy_id)
            .map_err(|e| format!("读取隐私开关失败: {e}"))?
            .ok_or_else(|| "隐私开关不存在".to_string())?;
        (auth, server, target)
    };

    if !target.onion_host.is_empty() && target.last_check_error.is_some() {
        // 上次远端已建成，只是本机同步/验证失败；重试不需要再连 SSH 或重载远端 Tor。
        emit("核验", "重新同步并验证现有 Onion 线路", 80);
        return sync_local_private(state, &target, &emit).await;
    }

    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;

    let caps = run_cmd(
        &mut sess,
        cancel,
        "探测",
        CAPABILITY_PROBE_SCRIPT,
        PROBE_TIMEOUT,
    )
    .await?
    .stdout;
    let (os, has_systemd) = parse_capability(&caps);
    if !has_systemd {
        return Err(format!("该系统未运行 systemd（{os}），暂不支持"));
    }
    check_cancel()?;

    // 2 装 Tor（缺才装，幂等）
    emit("安装", "检查 Tor", 15);
    let installed = run_cmd(&mut sess, cancel, "探测", "command -v tor", PROBE_TIMEOUT)
        .await?
        .exit_code
        == 0;
    if !installed {
        emit("安装", "安装 Tor（可能耗时数分钟）", 20);
        run_cmd_ok(
            &mut sess,
            cancel,
            "安装",
            "if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq tor; \
             elif command -v dnf >/dev/null 2>&1; then dnf install -y tor; \
             elif command -v yum >/dev/null 2>&1; then yum install -y tor; \
             else echo '没有受支持的 Tor 包管理器（apt-get/dnf/yum）' >&2; exit 1; fi",
            INSTALL_TIMEOUT,
        )
        .await
        .map_err(|e| format!("安装 Tor 失败: {e}"))?;
    }
    check_cancel()?;

    let bak = bak_of(TOR_CONFIG_PATH, req.privacy_id);
    // 上次 SSH 断开时远端可能已经建成 Onion，但本机尚未来得及写库。
    // 重试先核对现状；确认可连接就直接补写本机状态，避免再次重载 Tor。
    let recovered = if target.onion_host.is_empty() {
        emit("核对", "检查上次中断后远端是否已经生效", 25);
        match confirm_existing_onion(&mut sess, cancel, &emit, &target).await {
            Ok(onion) => onion,
            Err(e) if is_ssh_transport_loss(&e) => {
                return Err(format!("核对远端 Onion 时 SSH 断开: {e}"));
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let mut tor_reload_attempted = false;
    let mut result = if let Some(onion) = recovered {
        Ok(onion)
    } else {
        // 备份是部署事务的起点；已有 Onion 已验证成功时无需改写远端。
        emit("备份", "备份 Tor 配置", 30);
        backup_torrc(&mut sess, cancel, req.privacy_id)
            .await
            .map_err(|e| format!("备份 torrc 失败: {e}"))?;
        deploy_inner(&mut sess, cancel, &emit, &target, &mut tor_reload_attempted).await
    };

    if tor_reload_attempted && !cancel.load(Ordering::SeqCst) {
        if let Err(e) = &result {
            if is_ssh_transport_loss(e) {
                emit("重连", "SSH 中断，重新核对远端 Onion", 92);
                if let Ok(mut fresh) = SshSession::connect(
                    &server.host,
                    server.port as u16,
                    &server.user,
                    &auth,
                    CONNECT_TIMEOUT,
                )
                .await
                {
                    if let Ok(Some(onion)) =
                        confirm_existing_onion(&mut fresh, cancel, &emit, &target).await
                    {
                        result = Ok(onion);
                    }
                    sess = fresh;
                }
            }
        }
    }

    match result {
        Ok(onion) => {
            emit("写回", "记录 onion 地址", 95);
            {
                let conn = state.db.lock();
                repos::privacies::set_onion(&conn, target.id, &onion).map_err(|e| e.to_string())?;
            }
            let _ = run_cmd(
                &mut sess,
                cancel,
                "收尾",
                &format!("rm -f {bak}"),
                CONFIG_TIMEOUT,
            )
            .await;
            sync_local_private(state, &target, &emit).await
        }
        Err(e) => {
            emit("回滚", "开启失败，正在回滚", 90);
            // 原会话可能正是失败原因。回滚先建立新连接，才有机会恢复远端配置。
            let rollback_result = match SshSession::connect(
                &server.host,
                server.port as u16,
                &server.user,
                &auth,
                CONNECT_TIMEOUT,
            )
            .await
            {
                Ok(mut fresh) => rollback(&mut fresh, req.privacy_id, tor_reload_attempted).await,
                Err(_) => rollback(&mut sess, req.privacy_id, tor_reload_attempted).await,
            };
            let detail = match rollback_result {
                Ok(()) => format!("开启隐私保护失败（已回滚）: {e}"),
                Err(rollback_error) => {
                    format!("开启隐私保护失败，且回滚未完成: {e}；{rollback_error}")
                }
            };
            {
                let conn = state.db.lock();
                let _ = repos::privacies::set_error(&conn, target.id, Some(&detail));
            }
            Err(detail)
        }
    }
}

/// 部署主体：写 torrc → 校验 → 重载 Tor → 等 onion → 自验。
/// 任何失败返回 Err 交给调用方回滚。
///
/// **不碰 Xray**：这条线的入站照旧在监听，我们只是给它前面加了一层洋葱门。
/// pub(crate)：测试直接驱动它来验「读失败不得覆盖 torrc」（起整个 job 要连 DB/网络）。
pub(crate) async fn deploy_inner(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    emit: &(dyn Fn(&str, &str, i64) + Send + Sync),
    target: &Privacy,
    tor_reload_attempted: &mut bool,
) -> Result<String, String> {
    emit("配置", "写入 Tor 隐藏服务", 45);
    // 读**必须**用严格原语：原先的 `cat … 2>/dev/null || true` 把读失败变成「空文件」，
    // 于是下面 strip 出空串、整份 torrc 只剩本次这一段——用户手工配的 bridges/ExitNodes
    // 与别的线路的 HiddenService 全被冲掉，且远端原因被 `2>/dev/null` 吞掉。
    let existing =
        match read_remote_file(sess, cancel, "配置", TOR_CONFIG_PATH, CONFIG_TIMEOUT).await? {
            RemoteFile::Content(c) => c,
            RemoteFile::Absent => String::new(), // 真·首次：没有任何既有内容要保留
        };
    // 先摘掉本条旧段再追加：重复开启不会把同一段堆两遍
    let mut torrc = strip_torrc_block(&existing, target.id);
    if !torrc.is_empty() && !torrc.ends_with('\n') {
        torrc.push('\n');
    }
    torrc.push_str(&render_torrc_block(
        target.id,
        target.port,
        &target.fwd_host,
        target.fwd_port,
    ));
    write_remote_file(
        sess,
        cancel,
        "配置",
        TOR_CONFIG_PATH,
        &torrc,
        CONFIG_TIMEOUT,
    )
    .await?;

    // 校验不过不许重启（需求 §16.12）
    emit("校验", "校验 Tor 配置", 55);
    run_cmd_ok(sess, cancel, "校验", &tor_verify_command(), CONFIG_TIMEOUT)
        .await
        .map_err(|e| format!("Tor 配置校验未通过，已放弃本次开启（{e}）"))?;

    emit("启动", "重载 Tor", 65);
    // 从此可能已改变运行中的服务；失败时回滚必须恢复服务配置。
    *tor_reload_attempted = true;
    // 优先 reload（不断已有连接）；不支持才 restart
    run_cmd_ok(
        sess,
        cancel,
        "启动",
        &format!("systemctl reload {TOR_SERVICE} 2>/dev/null || systemctl restart {TOR_SERVICE}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    wait_service_active(sess, cancel, TOR_SERVICE)
        .await
        .map_err(|e| format!("Tor 没起来（{e}）"))?;

    emit("等待", "等待 Tor 生成 onion 地址", 80);
    let onion = wait_onion_hostname(sess, cancel, &hidden_service_dir(target.id)).await?;
    // 只输出地址本身，**绝不**读取或输出 HiddenServiceDir 里的私钥
    emit("完成", &format!("onion 地址已生成：{onion}"), 88);

    probe_onion(sess, cancel, emit, target, &onion, ONION_PROBE_BUDGET).await?;
    Ok(onion)
}

/// 对账只接受与本条记录完全相符、服务在运行且 Onion 可回连的远端事实。
/// torrc 中没有本条段落时直接返回 None，不触碰现有服务。
pub(crate) async fn confirm_existing_onion(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    emit: &(dyn Fn(&str, &str, i64) + Send + Sync),
    target: &Privacy,
) -> Result<Option<String>, String> {
    let torrc = run_cmd_ok(
        sess,
        cancel,
        "核对",
        &format!("cat {TOR_CONFIG_PATH}"),
        CONFIG_TIMEOUT,
    )
    .await?
    .stdout;
    let expected = render_torrc_block(target.id, target.port, &target.fwd_host, target.fwd_port);
    if !torrc.contains(&expected) {
        return Ok(None);
    }
    let active = run_cmd(
        sess,
        cancel,
        "核对",
        "systemctl is-active tor",
        CONFIG_TIMEOUT,
    )
    .await?;
    if active.exit_code != 0 || active.stdout.trim() != "active" {
        return Ok(None);
    }
    let hostname = run_cmd(
        sess,
        cancel,
        "核对",
        &format!("cat {}/hostname 2>/dev/null", hidden_service_dir(target.id)),
        CONFIG_TIMEOUT,
    )
    .await?;
    let Some(onion) = parse_onion_hostname(&hostname.stdout) else {
        return Ok(None);
    };
    match probe_onion(
        sess,
        cancel,
        emit,
        target,
        &onion,
        std::time::Duration::from_secs(45),
    )
    .await
    {
        Ok(()) => Ok(Some(onion)),
        Err(e) if is_ssh_transport_loss(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

async fn probe_onion(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    emit: &(dyn Fn(&str, &str, i64) + Send + Sync),
    target: &Privacy,
    onion: &str,
    budget: std::time::Duration,
) -> Result<(), String> {
    // 自验只看 SOCKS 是否完成到 Onion 虚拟端口的连接，不等待非 HTTP 后端回复。
    // 新服务的描述符可能尚未发布；给它限定总预算并重试，仍不放宽真实失败。
    let deadline = tokio::time::Instant::now() + budget;
    let mut attempts = 0;
    let mut last_code = "?".to_string();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("已取消".into());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        attempts += 1;
        emit(
            "核验",
            &format!("等待 Onion 可连接（第 {attempts} 次）"),
            92,
        );
        let seconds = remaining.as_secs().clamp(1, 45);
        // 已连上但后端保持静默时不用等满 45 秒；低速超时只作用于传输阶段，
        // 不截断仍在建立中的 Onion/SOCKS 连接。
        let probe = format!(
            "curl --noproxy '' --socks5-hostname 127.0.0.1:{socks} --max-time {seconds} \
             --speed-limit 1 --speed-time 5 -s -o /dev/null -w 'pre=%{{time_pretransfer}}' \
             http://{onion}:{port}/ ; echo -n \" exit=$?\"",
            socks = TOR_SOCKS_PORT,
            port = target.port
        );
        let out = run_cmd(sess, cancel, "核验", &probe, CONFIG_TIMEOUT).await?;
        if onion_probe_connected(&out.stdout) {
            return Ok(());
        }
        last_code = out
            .stdout
            .split_whitespace()
            .find_map(|field| field.strip_prefix("exit="))
            .unwrap_or("?")
            .to_string();
        if last_code == "127" {
            return Err("远端缺少 curl，无法核验 Onion 入口".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(format!(
        "经 Tor 回连 {onion}:{port} 在 {} 秒内未完成 SOCKS 连接（{attempts} 次，最后 curl exit={last_code}）；检查 Tor 描述符发布、网络及转发目标 {fwd_host}:{fwd_port}",
        budget.as_secs(),
        port = target.port,
        fwd_host = target.fwd_host,
        fwd_port = target.fwd_port
    ))
}

/// 拆除主体：备份 torrc → 摘掉本条段 → 写回 → 重载 Tor → 删身份目录。
///
/// 与 [`deploy_inner`] 对称：只做远端动作，本机状态（DB 行、本机配置重建）留给
/// 调用方——这样「远端已经拆干净但本机没同步」的重试语义与开启路径一致。
/// pub(crate)：测试直接驱动它来验读失败与回滚（起整个 job 要连 DB 与运行中的客户端）。
pub(crate) async fn remove_inner(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    emit: &(dyn Fn(&str, &str, i64) + Send + Sync),
    target: &Privacy,
) -> Result<(), String> {
    emit("关闭", "移除 Tor 隐藏服务", 40);
    // 备份是拆除事务的起点：下面紧跟一次写盘，中途失败必须能把原样配置放回去
    // （开启路径一直有备份，拆除路径此前**没有**——失败了连手工救的余地都没有）。
    let bak = bak_of(TOR_CONFIG_PATH, target.id);
    backup_torrc(sess, cancel, target.id)
        .await
        .map_err(|e| format!("备份 torrc 失败，未改动远端：{e}"))?;

    // 读**必须**用严格原语：原先的 `cat … 2>/dev/null || true` 把读失败变成空串，
    // 于是下面 strip 出空串并把**整份 torrc 写空**——用户其它 HiddenService、bridges、
    // ExitNodes 一次性清零，且远端原因被 `2>/dev/null` 吞掉。
    let existing = match read_remote_file(sess, cancel, "关闭", TOR_CONFIG_PATH, CONFIG_TIMEOUT)
        .await
        .map_err(|e| {
            // 读失败时远端一个字都没改（只有备份文件），直接中止即可，不需要还原
            format!("读取 Tor 配置失败，未改动远端（{e}）")
        })? {
        RemoteFile::Content(c) => c,
        RemoteFile::Absent => String::new(), // 没有 torrc＝没有要摘的段
    };
    // 只删本条标记之间的行——别的线路、用户自己的 HiddenService 一行不动
    let stripped = strip_torrc_block(&existing, target.id);
    write_remote_file(
        sess,
        cancel,
        "关闭",
        TOR_CONFIG_PATH,
        &stripped,
        CONFIG_TIMEOUT,
    )
    .await?;

    // 从这里开始远端已经变过了：任何失败都要还原配置，否则「配置里已无此隐藏服务、
    // 本机记录仍在」——界面显示开关还开着，实际早就不生效。
    emit("关闭", "重载 Tor", 70);
    if let Err(e) = run_cmd_ok(
        sess,
        cancel,
        "关闭",
        &format!("systemctl reload {TOR_SERVICE} 2>/dev/null || systemctl restart {TOR_SERVICE}"),
        CONFIG_TIMEOUT,
    )
    .await
    {
        return Err(restore_then_report(sess, target.id, format!("重载 Tor 失败：{e}")).await);
    }

    // Onion 身份目录精确删除：只删本条那个。这里是用户显式关闭，
    // 地址本就作废，所以不像回滚那样保留 hostname 判断。
    emit("关闭", "清理 Onion 身份目录", 85);
    let dir = if target.tor_dir.is_empty() {
        hidden_service_dir(target.id)
    } else {
        target.tor_dir.clone()
    };
    if let Err(e) = run_cmd(
        sess,
        cancel,
        "关闭",
        &format!("rm -rf {dir}"),
        CONFIG_TIMEOUT,
    )
    .await
    {
        // 身份目录没删掉：把含本条段的配置放回去，与目录保持自洽
        return Err(
            restore_then_report(sess, target.id, format!("删除 Onion 身份目录失败：{e}")).await,
        );
    }
    // 拆除成功：备份没有保留价值（本机记录随后就删了）
    let _ = run_cmd(
        sess,
        cancel,
        "关闭",
        &format!("rm -f {bak}"),
        CONFIG_TIMEOUT,
    )
    .await;
    Ok(())
}

/// 还原 torrc 与 Tor 服务，并把还原结果拼进错误文本。
///
/// 还原**失败**时必须说出来：不能只报原始原因，让用户以为已经回到原状
/// （与 [`rollback`] 的「不能宣称已回滚」同一条原则）。
async fn restore_then_report(sess: &mut SshSession, id: i64, cause: String) -> String {
    match restore_torrc(sess, id).await {
        Ok(()) => cause,
        Err(e) => format!("{cause}；且还原 Tor 配置未完成：{e}"),
    }
}

/// 关掉某条线的隐私开关（或清理失败留下的半成品）。
pub async fn run_privacy_remove_job(
    state: &AppState,
    req: &crate::jobs::PrivacyRemoveReq,
    cancel: &AtomicBool,
    emit_raw: &EmitFn,
) -> Result<(), String> {
    let emit = |step: &str, message: &str, progress: i64| {
        emit_raw(JobProgress {
            job_id: req.job_id,
            kind: "privacy".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };

    let (auth, server, target) = {
        let (auth, server) =
            load_auth(state, req.server_id).map_err(|e| format!("读取服务器失败: {e}"))?;
        let conn = state.db.lock();
        let target = repos::privacies::get(&conn, req.privacy_id)
            .map_err(|e| format!("读取隐私开关失败: {e}"))?
            .ok_or_else(|| "隐私开关不存在".to_string())?;
        (auth, server, target)
    };

    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;

    remove_inner(&mut sess, cancel, &emit, &target).await?;

    {
        let conn = state.db.lock();
        repos::privacies::delete(&conn, target.id).map_err(|e| e.to_string())?;
    }
    state
        .subs
        .rebuild_config()
        .await
        .map_err(|e| format!("远端隐私入口已关闭，但重建本机配置失败：{e}"))?;
    crate::net::runtime::apply_config_change(state)
        .await
        .map_err(|e| format!("远端隐私入口已关闭，但更新运行中客户端失败：{e}"))?;

    emit("完成", "隐私保护已关闭", 100);
    Ok(())
}
