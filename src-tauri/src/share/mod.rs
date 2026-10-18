//! 分享订阅链接：把选中的节点做成一条公网可达的订阅 URL。
//!
//! app 只做「装」与「拆」：SSH 到目标服务器，写一份**固定快照** YAML 到
//! `/srv/vpnshare/<token>/`，配 systemd 单元起 [`share_server_script`]（禁列举的
//! 静态服务），再把这个端口从外面放行。**分享的真实状态在服务器上**——本地不建表，
//! `list` 每次实读，app 重装或本地库丢失都不影响已发出的分享。
//!
//! 安全前提：服务是纯 HTTP 无加密，**链接本身就是唯一的钥匙**。端口对公网是
//! 敞开的：TCP 上做不到「先验 token 再放行」——握手的 SYN 不带 payload，
//! `-m string` 永远匹配不到它，而服务器 INPUT 默认丢弃，于是对任何客户端都表现为
//! **连接超时**（2026-09-21 实测：链接在服务器自己 curl 是 200，外面一律连不上，
//! 日志里一处报错都没有）。现在放行的就是裸端口，另加一条 `-m comment` 标记归属。
//!
//! 「扫到端口的人什么都取不到」这句话**曾经是假的**：那时单元起的是
//! `python3 -m http.server`，文档根（= `share_dir(token)`）里没有 `index.html`，
//! 于是一个 `GET /` 就把 token 目录列了出来——链接即凭证当场作废（#1）。现在
//! 文档根不再列目录（见 [`share_server_script`]），这句话才成立；既有分享会在
//! `list` 时就地升级（见 [`unit_needs_upgrade`]）。这条已经写在 UI 上
//! （「链接即凭证」），改文案时别把它删了。

use crate::servers::deploy::{
    port_probe_cmd, run_cmd, run_cmd_ok, write_remote_file, CONFIG_TIMEOUT,
};
use crate::servers::{CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 服务器上所有分享的根目录。
pub const SHARE_ROOT: &str = "/srv/vpnshare";
/// systemd 单元名前缀（完整名：`vpnshare-<token>`）。
pub const UNIT_PREFIX: &str = "vpnshare";
/// 端口分配范围（闭区间）。每份分享独占一个端口。
pub const PORT_RANGE_START: u16 = 18080;
pub const PORT_RANGE_END: u16 = 18180;

/// 一份已安装到服务器上的分享。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareInfo {
    pub server_id: i64,
    pub server_name: String,
    pub host: String,
    pub token: String,
    pub port: u16,
    /// `http://<host>:<port>/sub/<token>`
    pub url: String,
    /// 从服务器上的快照里解析出的节点名（解析不出时为空）
    pub node_names: Vec<String>,
    /// `systemctl is-active` 结果
    pub active: bool,
}

/// token 必须是 16 位小写十六进制。它会被拼进 systemd 单元名与 iptables
/// 命令行，限制字符集可省掉一整类转义/注入问题——所以**入口与出口都要校验**，
/// 不能只信前端传回来的那个字符串。
pub fn is_valid_token(token: &str) -> bool {
    token.len() == 16
        && token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn share_dir(token: &str) -> String {
    format!("{SHARE_ROOT}/{token}")
}

/// 快照 YAML 的落盘位置。
///
/// **必须落在 `sub/<token>`，因为这个路径就是对外公布的 URL 路径**：服务单元的
/// 文档根是 `share_dir(token)`（见 `unit_content` 的 `--directory`），而 `url_for`
/// 公布的是 `/sub/<token>`。此前写的是 `config.yaml`，于是**凡是打到服务的请求
/// 必然 404**（python 的 http.server 只会回 "File not found"）。而且创建流程当时
/// 只验了「服务 active」，没验「链接取得到」，死链就这么发出去了
/// （2026-09-21 实测：取回的是 python http.server 的 "File not found"）。
pub fn config_path(token: &str) -> String {
    format!("{SHARE_ROOT}/{token}/sub/{token}")
}

pub fn unit_name(token: &str) -> String {
    format!("{UNIT_PREFIX}-{token}")
}

pub fn unit_path(token: &str) -> String {
    format!("/etc/systemd/system/{}.service", unit_name(token))
}

/// 禁列举服务脚本的文件名。**所有分享共用一份**，落在 [`SHARE_ROOT`] 下：
/// 目录列举关掉是脚本的行为，与具体分享无关，没必要每份分享存一份。
pub const SHARE_SERVER_SCRIPT_NAME: &str = "share_server.py";

pub fn share_server_script_path() -> String {
    format!("{SHARE_ROOT}/{SHARE_SERVER_SCRIPT_NAME}")
}

/// 分享服务脚本：静态文件服务，但**不列目录**。
///
/// 为什么必须自己写一个：`python3 -m http.server` 在文档根没有 `index.html` 时
/// **必然列目录**，而文档根就是 `share_dir(token)`——端口对公网敞开，扫到端口的人
/// 一个 GET 就拿到了 token，「链接即凭证」当场作废（#1）。改名或换成固定文件名都
/// 修不好：目录名本身就是 token。
///
/// 三个刻意的取舍：
/// - `list_directory` 直接 404，服务端不再暴露任何目录结构；
/// - 用 `os.chdir` 而不是 `directory=` 参数，避免依赖 python 3.7+；
/// - 用 `ThreadingMixIn` 而不是单线程 `TCPServer`：端口对公网敞开，一个卡住的连接
///   （端口扫描器就会这么干）就能把单线程服务拖死，之后谁都取不到订阅。
///   `ThreadingMixIn` 自 py2.5 就有，不引入新版本依赖。
///
/// 参数：`argv[1]` = 端口，`argv[2]` = 文档根（见 [`unit_content`] 的 ExecStart）。
pub fn share_server_script() -> String {
    r#"import http.server, socketserver, os, sys
os.chdir(sys.argv[2])
class H(http.server.SimpleHTTPRequestHandler):
    def list_directory(self, path):
        self.send_error(404)
        return None
    def log_message(self, *a):
        pass
class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
Server(("0.0.0.0", int(sys.argv[1])), H).serve_forever()
"#
    .to_string()
}

/// systemd 单元：在分享目录上起静态服务。
///
/// 服务本体是 [`share_server_script`]（禁列举），**不是** `python3 -m http.server`——
/// 后者在文档根没有 `index.html` 时必然列目录，而文档根就是 `share_dir(token)`，
/// 等于把 token 公示给任何扫到端口的人（#1）。
/// 参数顺序与脚本约定一致：`<port> <dir>`（见 `share_server_script` 的 argv 说明）。
/// 用 `/usr/bin/env python3` 而非硬编码路径——python3 装在哪儿由 PATH 说了算。
pub fn unit_content(token: &str, port: u16) -> String {
    let dir = share_dir(token);
    let script = share_server_script_path();
    format!(
        "[Unit]\n\
         Description=vpn-ctl share {token}\n\
         \n\
         [Service]\n\
         ExecStart=/usr/bin/env python3 {script} {port} {dir}\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// 规则主体（`-C` / `-I` / `-D` 三种动作共用）。**只在这里拼一次**：iptables 的
/// `-D` 与 `-C` 都靠完全匹配，多一个空格、少一个字段就查不到也删不掉，而删不掉的
/// 后果是这个端口永远开着。
///
/// `-m comment` 是给规则打归属标记（哪个 token 开的），不是访问控制——放行的就是
/// 裸端口。想看清单：`iptables -S INPUT | grep vpnshare-`。
fn rule_body(token: &str, port: u16) -> String {
    format!("-p tcp --dport {port} -m comment --comment \"vpnshare-{token}\" -j ACCEPT")
}

/// 放行规则：把这个端口对公网打开，`-I` 到链首。
///
/// 访问控制不在这里，在 token 上——服务是文档根 `share_dir(token)` 的静态文件服务，
/// 路径不对就是 404。别指望 iptables 能「先验 token 再放行」：SYN 不带 payload，
/// 任何按内容匹配的规则都拦不到握手包，结果是端口对**所有人**都不可用。
pub fn accept_rule(token: &str, port: u16) -> String {
    format!("iptables -I INPUT 1 {}", rule_body(token, port))
}

/// 查规则在不在链上（退出码 0=在、1=不在）。放行之后必须回查——见 `create_share`。
pub fn check_rule(token: &str, port: u16) -> String {
    format!("iptables -C INPUT {}", rule_body(token, port))
}

/// 删除规则：与 [`accept_rule`] 只差动作词（由 [`rule_body`] 保证逐字一致）。
pub fn drop_rule(token: &str, port: u16) -> String {
    format!("iptables -D INPUT {}", rule_body(token, port))
}

pub fn url_for(host: &str, port: u16, token: &str) -> String {
    format!("http://{host}:{port}/sub/{token}")
}

/// 从单元内容里取端口。端口的事实源是服务器上的单元文件（本地不存分享记录），
/// 所以 list 必须能从单元里读回端口。
///
/// **两种形态都要认**：
/// - 现形态 `… python3 <SHARE_ROOT>/share_server.py <port> <dir>`；
/// - 旧形态 `… python3 -m http.server <port> --directory <dir> …`
///   ——旧形态只在**升级既有分享**时用到：升级必须沿用原端口，换端口等于把已经
///   发出去的链接当场作废。升级动作本身会把旧形态改成新形态（见 `read_server_shares`）。
pub fn parse_port_from_unit(unit: &str) -> Option<u16> {
    let markers = [
        format!("{SHARE_SERVER_SCRIPT_NAME} "),
        "-m http.server ".to_string(),
    ];
    for marker in markers {
        if let Some(rest) = unit.find(&marker).map(|i| &unit[i + marker.len()..]) {
            if let Some(p) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
                return Some(p);
            }
        }
    }
    None
}

/// 单元是不是**旧形态**（还在跑会列目录的 `python3 -m http.server`）。
///
/// 判据是「ExecStart 里有没有指向我们的脚本」而不是逐字比对模板：模板将来再动
/// （加 Environment、换 Restart 策略）不会把已升级的分享误判成要升级、反复重启。
/// 读不到单元内容（空串）时返回 false——没有单元就没有端口，无从升级。
pub fn unit_needs_upgrade(unit: &str) -> bool {
    unit.contains("ExecStart=") && !unit.contains(SHARE_SERVER_SCRIPT_NAME)
}

/// 从服务器本地把链接真取一次，返回 HTTP 状态码。
///
/// **只验「服务 active」不够**：单元在跑、规则也在，链接照样可能取不到
/// （配置没落在 URL 路径上就是 404，2026-09-21 实测；而 `Type=simple` 在 fork 出进程时
/// 就报 active，那时 python 还没 bind——真机复验时第一轮 curl 全 000 就是这个）。
/// create_share 与升级路径都走这里，两处判据必须同源。
async fn probe_share_link(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    port: u16,
    token: &str,
) -> Result<String, String> {
    Ok(run_cmd(
        sess,
        cancel,
        step,
        &format!(
            "curl -s -o /dev/null -w '%{{http_code}}' --max-time 10 http://127.0.0.1:{port}/sub/{token}"
        ),
        PROBE_TIMEOUT,
    )
    .await?
    .stdout
    .trim()
    .to_string())
}

/// 服务是否处于 active。`is-active` 只是第一道筛子——「起来了」不等于「能服务」，
/// 所以调用方在它之后还要过 [`probe_share_link`]（升级路径尤其如此）。
async fn probe_service_active(sess: &mut SshSession, cancel: &AtomicBool, token: &str) -> bool {
    run_cmd(
        sess,
        cancel,
        "状态",
        &format!("systemctl is-active {}", unit_name(token)),
        PROBE_TIMEOUT,
    )
    .await
    .map(|o| o.stdout.trim() == "active")
    .unwrap_or(false)
}

/// 探测服务器有没有 python3——没有它起不了静态服务，**显式报错，不降级**。
///
/// 这里曾经还要探测 `iptables -m string`（那时想在防火墙层认 token）。那个设计在
/// TCP 上不可能成立（见模块头），探测随之一起去掉了。
async fn probe_python3(sess: &mut SshSession, cancel: &AtomicBool) -> Result<(), String> {
    // 用 run_cmd 而非 run_cmd_ok：`command -v` 找不到时退出码即 1，
    // run_cmd_ok 会先吐一句通用的「执行失败」，把「缺 python3」这个真正的原因盖掉。
    let py = run_cmd(sess, cancel, "探测", "command -v python3", PROBE_TIMEOUT).await?;
    if py.exit_code != 0 || py.stdout.trim().is_empty() {
        return Err("服务器上未找到 python3，无法提供分享服务".into());
    }
    Ok(())
}

/// 从 [`PORT_RANGE_START`] 起找第一个空闲端口；`taken` 是本地已登记、必须跳过的端口。
///
/// 注意 `port_probe_cmd` 的退出码方向：**0 = 已被占用**。范围内全占 → 报错，
/// 绝不覆盖别人的服务（包括可能被非本 app 占用的端口）。
async fn allocate_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    taken: &[u16],
) -> Result<u16, String> {
    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if taken.contains(&port) {
            continue;
        }
        let out = run_cmd(sess, cancel, "探测", &port_probe_cmd(port), PROBE_TIMEOUT).await?;
        if out.exit_code != 0 {
            return Ok(port);
        }
    }
    Err(format!(
        "端口 {PORT_RANGE_START}-{PORT_RANGE_END} 全部被占用，无法分配分享端口"
    ))
}

/// 生成并安装一份分享。
///
/// 快照内容走 `build_config_for`（与导出、与「开启网络」同一口径），所以链接里
/// 的节点就是勾的那些，不多不少。生成后内容**不再更新**——这是快照制的明确取舍。
pub async fn create_share(
    state: &Arc<AppState>,
    server_id: i64,
    node_ids: &[String],
) -> Result<ShareInfo, String> {
    // 先把快照生成出来：空配置/全被跳过时直接失败，不白连一次服务器
    let (yaml, _skipped) =
        crate::commands::build_config_for(state, node_ids, crate::commands::ExportFormat::Yaml)
            .await
            .map_err(|e| e.to_string())?;
    crate::commands::ensure_exportable_has_nodes(&yaml).map_err(|e| e.to_string())?;

    let (server, auth) = {
        let conn = state.db.lock();
        let server = crate::db::repos::servers::get(&conn, server_id)
            .map_err(|e| e.to_string())?
            .ok_or("服务器不存在")?;
        drop(conn);
        let (auth, _) = crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
        (server, auth)
    };

    let cancel = AtomicBool::new(false);
    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;

    probe_python3(&mut sess, &cancel).await?;

    // 本地已登记的通道端口也要避开——那些端口上有别的服务，撞上会把别人的东西盖掉
    let taken: Vec<u16> = crate::db::repos::channels::list_by_server(&state.db.lock(), server_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|c| c.port as u16)
        .collect();
    let port = allocate_port(&mut sess, &cancel, &taken).await?;

    let token = new_token();
    // 服务脚本先落盘：单元的 ExecStart 指向它，没有它服务起来就立刻退出
    // （systemd 只报 start request repeated too quickly，看不出是缺文件）。
    // 所有分享共用同一份，内容幂等，重复创建只是重写一次。
    write_remote_file(
        &mut sess,
        &cancel,
        "写入",
        &share_server_script_path(),
        &share_server_script(),
        CONFIG_TIMEOUT,
    )
    .await?;
    // write_remote_file 自带 mkdir -p 父目录：写快照时顺手就把分享目录建了
    write_remote_file(
        &mut sess,
        &cancel,
        "写入",
        &config_path(&token),
        &yaml,
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        &mut sess,
        &cancel,
        "写入",
        &unit_path(&token),
        &unit_content(&token, port),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        &cancel,
        "启动",
        &format!(
            "systemctl daemon-reload && systemctl enable --now {}",
            unit_name(&token)
        ),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        &cancel,
        "放行",
        &accept_rule(&token, port),
        CONFIG_TIMEOUT,
    )
    .await
    .map_err(|e| format!("放行端口 {port} 失败：{e}（服务已装好，但外面连不上这个端口）"))?;

    // 回查规则真的落到链上了。**这一步本地 curl 查不出来**：自测走 127.0.0.1 从 lo
    // 进来，根本不经过 INPUT 链——2026-09-21 就是这么发出去一条死链的：服务器本地
    // 取是 200，外面一律超时。以后规则写法再有差池，这里当场拦下。
    let on_chain = run_cmd(
        &mut sess,
        &cancel,
        "确认",
        &check_rule(&token, port),
        PROBE_TIMEOUT,
    )
    .await?;
    if on_chain.exit_code != 0 {
        return Err(format!(
            "端口 {port} 的放行规则没在 INPUT 链上（iptables -C 退出码 {}）——链接发出去外面连不上，\
             可在「分享」页作废后重试（服务：{}）",
            on_chain.exit_code,
            unit_name(&token)
        ));
    }

    // 回读确认真的起来了——写了单元不等于服务在跑，不回读就会发出一条死链
    let active = run_cmd(
        &mut sess,
        &cancel,
        "确认",
        &format!("systemctl is-active {}", unit_name(&token)),
        PROBE_TIMEOUT,
    )
    .await?
    .stdout
    .trim()
        == "active";
    if !active {
        return Err(format!(
            "分享服务未进入 active 状态（已安装在 {}，可在「分享」页作废后重试）",
            unit_name(&token)
        ));
    }

    // 自测：从服务器本地把刚发的链接真取一次。**只验 active 不够**——单元在跑、
    // 规则也在，链接照样可能取不到（配置没落在 URL 路径上就是 404，2026-09-21 实测），
    // 那样发出去的就是一条死链。取不到就在这一步报错，而不是把链接交给用户。
    let code = probe_share_link(&mut sess, &cancel, "自测", port, &token).await?;
    if code != "200" {
        return Err(format!(
            "分享已建好，但从服务器本地取自己的链接返回 {code}（期望 200）——链接发出去也用不了，\
             可在「分享」页作废后重试（服务：{}）",
            unit_name(&token)
        ));
    }

    Ok(ShareInfo {
        server_id,
        server_name: server.name.clone(),
        host: server.host.clone(),
        token: token.clone(),
        port,
        url: url_for(&server.host, port, &token),
        node_names: proxy_names(&yaml),
        active,
    })
}

/// 分享列表 + 读取失败的服务器。
///
/// 分开返回 `unreachable` 是刻意的：本地不存分享记录，某台连不上时**无法知道
/// 它上面有哪些分享**，列表里就少几条。不告诉用户「哪台没读到」，他会以为
/// 自己发出去的链接凭空消失了。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareList {
    pub shares: Vec<ShareInfo>,
    /// 连不上或读不到的服务器名（UI 需注明「这几台的分享没列出来」）
    pub unreachable: Vec<String>,
}

/// 列出所有服务器上的分享。**逐台读，某台连不上不整体失败**——好那台的分享
/// 照常列出，坏那台进 `unreachable`。
///
/// **并发读，不排队**：每台要建一条 SSH 再跑几个来回，串行时总耗时是各台之和——
/// 实测 4 台服务器要 ~14 秒，而界面在这期间只能说「读取中」（2026-09-21 用户切一下
/// 视图回来，看到空列表以为分享被删了）。并发后等于最慢那一台的耗时。
pub async fn list_shares(state: &Arc<AppState>) -> Result<ShareList, String> {
    let servers = crate::db::repos::servers::list(&state.db.lock()).map_err(|e| e.to_string())?;

    let mut set = tokio::task::JoinSet::new();
    for (idx, server) in servers.into_iter().enumerate() {
        let state = state.clone();
        set.spawn(async move {
            // 取消标志是 per-call 的（`read_server_shares` 只把它透传给 run_cmd）
            let cancel = AtomicBool::new(false);
            let name = server.name.clone();
            (
                idx,
                name,
                read_server_shares(&state, &server, &cancel).await,
            )
        });
    }

    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(t) => results.push(t),
            // 任务自己 panic 了（不是「连不上」——那种是 Ok(Err)）。如实报错，
            // 不要把它悄悄算成某台的分享缺失。
            Err(e) => return Err(format!("读取分享的任务异常结束: {e}")),
        }
    }
    // 按服务器顺序还原：并发完成次序不定，不排的话同一次列表每次刷新都在跳
    results.sort_by_key(|(idx, _, _)| *idx);

    let mut shares = Vec::new();
    let mut unreachable = Vec::new();
    for (_, name, r) in results {
        match r {
            Ok(mut list) => shares.append(&mut list),
            // **原因必须带出来**，不能只报个服务器名：把「升级失败即 unreachable」
            // 这个取舍立起来的前提，是用户知道**要去查什么**。只给名字的话，他分不清
            // 是网络抖动、密码错，还是「有一份分享还在列目录」——而最后那种正是 #1
            // 要消灭的形态，也正是当初宁可让这台「读不到」的全部理由。
            // 名字在前、原因在后：界面上多台是 `、` 串起来的，名字在最前面才认得出是哪台。
            Err(reason) => unreachable.push(format!("{name}：{reason}")),
        }
    }
    Ok(ShareList {
        shares,
        unreachable,
    })
}

/// 读一台服务器上的全部分享。任一环节失败即 Err（由调用方记进 `unreachable`）——
/// 「读不到」与「没有分享」必须区分开，否则失联会被读成「分享没了」。
async fn read_server_shares(
    state: &Arc<AppState>,
    server: &crate::domain::Server,
    cancel: &AtomicBool,
) -> Result<Vec<ShareInfo>, String> {
    let (auth, _) = crate::servers::load_auth(state, server.id).map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 目录不存在时 ls 非 0 退出且 stdout 为空 → 这台就是没有分享，不是读不到
    let listing = run_cmd(
        &mut sess,
        cancel,
        "列举",
        &format!("ls -1 {SHARE_ROOT} 2>/dev/null"),
        PROBE_TIMEOUT,
    )
    .await?;

    let mut out = Vec::new();
    for token in listing
        .stdout
        .lines()
        .map(str::trim)
        // 目录里可能有别人建的东西（lost+found 之类）：token 不合规就不是本模块的分享
        .filter(|t| is_valid_token(t))
    {
        let mut active = probe_service_active(&mut sess, cancel, token).await;

        // 端口的事实源是服务器上的单元文件（本地不存分享记录）
        let unit = run_cmd(
            &mut sess,
            cancel,
            "读取",
            &format!("cat {}", unit_path(token)),
            PROBE_TIMEOUT,
        )
        .await
        .map(|o| o.stdout)
        .unwrap_or_default();
        let port = parse_port_from_unit(&unit).unwrap_or(0);
        // 旧形态（还在跑会列目录的 `-m http.server`）就地升级。用户不会为了一条
        // 已经发出去的链接去手工重建分享，不升级就等于它一直裸着。
        //
        // 升级失败即 `?` → 这台进 unreachable：宁可让用户看到「这台没读出来」去查，
        // 也不要把一条**仍在列目录**的分享静默列成正常——那正是 #1 要消灭的形态。
        if unit_needs_upgrade(&unit) && port != 0 {
            upgrade_share_unit(&mut sess, cancel, token, port).await?;
            // 升级重启过服务，上面那个 active 是**重启前**采的，对这条分享已经不成立——
            // 一次把服务弄死的升级会顶着「重启前的 active」显示正常。重新采。
            active = probe_service_active(&mut sess, cancel, token).await;
            // 而且「active」本身不足以宣布升级成功：升级是**替换一个正在服务的进程**，
            // 新脚本起不来时用户的链接当场作废（比列目录更糟）。按 create_share 的老规矩
            // 验链接真取得到，取不到就 `?` → 这台进 unreachable，让用户看到去查。
            let code = probe_share_link(&mut sess, cancel, "升级", port, token).await?;
            if code != "200" {
                return Err(format!(
                    "分享 {} 已升级到禁列举形态，但从服务器本地取自己的链接返回 {code}（期望 200）\
                     ——升级可能把这条已发出去的链接弄坏了，请检查该服务（unit：{}）",
                    token,
                    unit_name(token)
                ));
            }
        }

        let yaml = run_cmd(
            &mut sess,
            cancel,
            "读取",
            &format!("cat {}", config_path(token)),
            PROBE_TIMEOUT,
        )
        .await
        .map(|o| o.stdout)
        .unwrap_or_default();

        out.push(ShareInfo {
            server_id: server.id,
            server_name: server.name.clone(),
            host: server.host.clone(),
            token: token.to_string(),
            port,
            url: url_for(&server.host, port, token),
            node_names: proxy_names(&yaml),
            active,
        });
    }
    Ok(out)
}

/// 把旧形态的分享就地升到「禁列举」形态：重写服务脚本与单元 → daemon-reload → restart。
///
/// **端口与 iptables 规则一律不动**：已发出去的链接必须继续可用，升级只是把文档根的
/// 服务从会列目录的 `http.server` 换成 [`share_server_script`]。
/// 幂等：调用点先用 [`unit_needs_upgrade`] 判定，已是新形态的分享不会被反复重启。
async fn upgrade_share_unit(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    token: &str,
    port: u16,
) -> Result<(), String> {
    write_remote_file(
        sess,
        cancel,
        "升级",
        &share_server_script_path(),
        &share_server_script(),
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        sess,
        cancel,
        "升级",
        &unit_path(token),
        &unit_content(token, port),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        sess,
        cancel,
        "升级",
        &format!(
            "systemctl daemon-reload && systemctl restart {}",
            unit_name(token)
        ),
        CONFIG_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// 作废一份分享。各步幂等：服务/文件/规则任一不存在都不算错。
pub async fn delete_share(
    state: &Arc<AppState>,
    server_id: i64,
    token: &str,
) -> Result<(), String> {
    // 校验先于连接：token 会拼进命令，非法值绝不能往下走
    if !is_valid_token(token) {
        return Err(format!("非法的分享 token：{token}"));
    }
    let (server, auth) = {
        let conn = state.db.lock();
        let server = crate::db::repos::servers::get(&conn, server_id)
            .map_err(|e| e.to_string())?
            .ok_or("服务器不存在")?;
        drop(conn);
        let (auth, _) = crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
        (server, auth)
    };
    let mut sess = SshSession::connect(
        &server.host,
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;
    let cancel = AtomicBool::new(false);

    // 端口必须在删单元之前读出来——单元一删，规则里的端口就无从得知，
    // 那条 ACCEPT 规则会永远留在链上（端口一直开着，正是本设计要避免的）
    let port = last_known_port(&mut sess, &cancel, token).await;

    // 顺序：先停服务（否则 http.server 还占着目录）→ 删单元 → 删目录
    for (step, cmd) in [
        (
            "停止",
            format!(
                "systemctl disable --now {} 2>/dev/null || true",
                unit_name(token)
            ),
        ),
        (
            "清理",
            format!(
                "rm -f '{}' && systemctl daemon-reload 2>/dev/null || true",
                unit_path(token)
            ),
        ),
        ("清理", format!("rm -rf '{}'", share_dir(token))),
    ] {
        run_cmd_ok(&mut sess, &cancel, step, &cmd, CONFIG_TIMEOUT).await?;
    }

    // 规则删除失败（比如规则本就不在）不影响「作废」这个结果
    if let Some(port) = port {
        run_cmd_ok(
            &mut sess,
            &cancel,
            "封堵",
            &format!("{} 2>/dev/null || true", drop_rule(token, port)),
            CONFIG_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

/// 端口来源：单元文件（权威）→ 退而求其次扫 iptables 里带本 token 注释的规则。
/// 都拿不到就 None，调用方跳过规则删除（此时规则本来也不存在）。
async fn last_known_port(sess: &mut SshSession, cancel: &AtomicBool, token: &str) -> Option<u16> {
    let unit = run_cmd(
        sess,
        cancel,
        "读取",
        &format!("cat {}", unit_path(token)),
        PROBE_TIMEOUT,
    )
    .await
    .ok()?;
    if let Some(p) = parse_port_from_unit(&unit.stdout) {
        return Some(p);
    }
    let rules = run_cmd(
        sess,
        cancel,
        "读取",
        &format!("iptables -S INPUT | grep -- 'vpnshare-{token}'"),
        PROBE_TIMEOUT,
    )
    .await
    .ok()?;
    for line in rules.stdout.lines() {
        // `-A INPUT -p tcp -m tcp --dport 18080 -m comment --comment "vpnshare-<token>" ...`
        let words: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = words.iter().position(|w| *w == "--dport") {
            if let Some(p) = words.get(i + 1).and_then(|p| p.parse().ok()) {
                return Some(p);
            }
        }
    }
    None
}

/// 16 位小写十六进制 token（UUID v4 去横线取前 16 位）。字符集受 [`is_valid_token`] 约束。
fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// 从生成的 YAML 里取 proxies 的名字，用于 UI 展示「这条链接里有哪几条线」。
/// 解析不出来就返回空——列表少一列信息，好过整页报错。
fn proxy_names(yaml: &str) -> Vec<String> {
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return vec![];
    };
    v.get("proxies")
        .and_then(|p| p.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/share/mods.rs"
));
