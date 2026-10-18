//! 本机上网运行时：独立 mihomo 实例（完整节点+分组+智能规则 config）。
//! start 流程：冲突 → 二进制 → 端口 → config → spawn → 就绪 → 系统代理；
//! stop 反向：杀进程 + 还原代理。槽位锁全程持有（仿 sidecar::ensure_started 模式），
//! 子进程已退出视为崩溃自愈（清槽重启）；App 退出经 stop_all 切直连并保留端口
//! （不能杀：已运行程序的环境变量里还记着这个端口，见 [`stop_for_exit`]）。

use crate::config::AUTO_GROUP;
use crate::domain::{Node, RelayRule, Server};
use crate::net::connlog::parse_connections;
use crate::net::firewall::{self, BlockReply, FirewallState};
use crate::net::{geodata, local_proxy, sysproxy_set, NetStatus, StartOptions};
use crate::service::client as svc;
use crate::service::protocol::{Request as SvcRequest, Response as SvcResponse};
use crate::speed::sidecar;
use crate::state::AppState;
use std::collections::{HashMap, HashSet};

/// 客户端数据目录名（与 geodata::GEO_DIR 一致：data_dir/net）。
pub const NET_DIR: &str = "net";
/// 系统代理 mixed 端口：**恒为它**，占用即报错，绝不顺延（见 [`pinned_mixed_port`]）。
pub const NET_DEFAULT_PORT: u16 = 7890;

fn alloc_tor_upstream_port(mixed_port: u16, ctrl_port: u16) -> Result<u16, String> {
    for _ in 0..8 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("分配 Tor 上游端口失败: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        if port != mixed_port && port != ctrl_port {
            return Ok(port);
        }
    }
    Err("无法分配独立的 Tor 上游端口".into())
}

/// 运行中的客户端实例：child 供退出清理/崩溃检测，ctrl_port+token 供
/// 手动切换出口节点（控制器 API），snapshot 供还原系统代理。
///
/// `child` 为 `None` 表示进程归**提权服务**持有——此时存活探测与停止都改走
/// IPC（见 stop/status），主程序这边没有任何句柄可用。
pub struct RunningNet {
    pub child: Option<std::process::Child>,
    pub mixed_port: u16,
    pub ctrl_port: u16,
    pub tor_upstream_port: u16,
    pub token: String,
    pub snapshot: Option<sysproxy_set::ProxySnapshot>,
    /// 本次会话的阻断状态（见 [`NetStatus::firewall`]）。
    ///
    /// 记在实例上而不是每次问服务：状态轮询很频繁，而防火墙规则只在「开启网络」
    /// 与「开关 TUN」两个时刻变。**不能**把它塞进 `Response::Status`——那会给旧
    /// 服务加上一个它没有的字段，旧服务回的话解析不出 `running`，界面就会把正在
    /// 跑的网络报成「未开启」（实测过同款形态：管道 DACL 不对时整条链路都是这个
    /// 表现）。详情也一并存着，否则说明只在开启那一瞬间出现、下一次轮询就没了。
    pub firewall: String,
    pub firewall_detail: Option<String>,
    /// 本次会话接管了几条「开启前就已建立」的直连（见 [`NetStatus::takeover_killed`]）。
    /// 与阻断状态同一理由记在实例上：它只在开启那一刻产生，轮询时得答得出来。
    pub takeover_killed: Option<usize>,
    pub takeover_detail: Option<String>,
    /// 本机 Tor（隐私线入口）的状态，见 [`NetStatus::tor`]。
    ///
    /// 与 `firewall` **不完全一样**：它由后台看门狗（[`crate::tor::spawn_bootstrap`]）
    /// 在 bootstrap 过程中反复回写，而不是开启那一刻记一次。所以读写都要经过
    /// [`publish_tor`]，别在本文件别处直接改。
    pub tor: String,
    pub tor_detail: Option<String>,
}

/// 手动选择分组名的 URL 安全编码（PUT/GET /proxies/{group} 路径用）。
/// 分组名固定为 config::MANUAL_GROUP 的「手动选择」：手=E6%89%8B 动=E5%8A%A8
/// 选=E9%80%89 择=E6%8B%A9。
pub const MANUAL_GROUP_PATH: &str = "%E6%89%8B%E5%8A%A8%E9%80%89%E6%8B%A9";

/// 起连接日志采样器（幂等，已在采样直接返回）。
///
/// 放在实例真正起来之后：采样器要拿 ctrl_port/token 去查 mihomo 的 `/connections`，
/// 这两个值只有进程就绪了才有意义。**采样器是探测器不是依赖**——它挂了不影响上网，
/// 故这里的失败路径没有错误可报（起不来就是没数据，面板会说「未采样」）。
fn spawn_conn_sampler(state: &AppState, n: &RunningNet) {
    state.conn_sampler.start(
        n.ctrl_port,
        n.token.clone(),
        state.db.clone(),
        state.creds.clone(),
        // 国家库：data_dir/net/country.mmdb——**mihomo 自己读的那一份**
        // （`mihomo -d <dir>` 只认目录根下这个文件名），故两边判「是不是国内」
        // 用的是同一份数据，构造上不可能打架
        crate::net::geodata::mmdb_path(state),
    );
}

/// 本次会话的短标记（进规则名，用于在 `wf.msc` 里分辨是哪一次会话装的）。
/// 删除只认前缀不认它，故它纯粹是排障线索。
fn session_tag() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// 按泄漏关注名单装会话级阻断规则，返回 `(状态, 原因)`。
///
/// **任何失败都不返回 Err**：阻断是增益，它失败绝不能反过来把「开启网络」打掉。
/// 所有失败都落成状态 + 一句给用户看的原因，由界面出说明。
///
/// 注意调用点：必须在**系统代理写成功之后**调。顺序反过来的话，代理还没生效而
/// 阻断先生效，那段时间里关注程序连公网都出不去（系统代理尚未接入）。
fn block_session(state: &AppState) -> (String, Option<String>) {
    // 测试构建下**一律不碰真实防火墙**。三条理由，都是硬约束或实测：
    //
    // 1. **不许在开发机留下规则**（本仓库的硬规矩，与「真装真拆的用例必须非管理员
    //    跳过」同源）。这里解析出来的是**用户真实在跑的**程序路径——实测本机解析到
    //    `C:\Program Files\Google\Chrome\Application\chrome.exe` 与
    //    `C:\Program Files(x86)\Microsoft\Edge\Application\msedge.exe`。服务若已经是
    //    新构建，`cargo test` 就会真的给用户的浏览器装上 BLOCK 规则；而任何在
    //    start 与 stop 之间 panic 的用例都会把它们留下来。
    // 2. **交叉干扰已实测**：开关这套接线后，并发的 SSH 假服务器用例（deploy/iperf）
    //    从 0/6 轮失败变成 3/6 轮失败（交错 A/B，每轮 657 个用例）。`local_proxy` 是
    //    **进程级全局**，网络用例的窗口一被拉长就会接管同一进程里其它用例的连接——
    //    这正是本仓库为系统代理立 `global_state_lock` 时踩过的同款形态。
    // 3. 服务侧**读响应没有超时**，服务一旦卡住这条调用会永久阻塞（实测撞到过整轮
    //    `cargo test` 卡死 6 分 40 秒）。生产侧同理，只是概率低得多。
    //
    // 真装真拆的行为由 `net::firewall_tests` 里那两条专用用例覆盖（非管理员自动跳过，
    // 且 Drop 里撤干净）。用 `cfg!` 而不是 `#[cfg]`：后者会让后面整段成为不可达代码。
    if cfg!(test) {
        return (FirewallState::Off.as_str().to_string(), None);
    }
    // 名单与泄漏检测**同源**（同一个设置键、同一个解析函数），见 firewall::watch_programs
    let watch = firewall::watch_programs(&state.db);
    // 只认正在跑的进程：防火墙的程序规则要绝对路径，而这是唯一的可靠来源。
    // 探测失败当成「一个路径都没拿到」——下面会如实报 partial，不会假装挡住了
    let running = crate::net::winprobe::process_paths(&watch).unwrap_or_default();
    let (programs, unresolved) = firewall::resolve_programs(&watch, &running);
    if programs.is_empty() {
        // 一条规则都凑不出来。两种成因要分开说，不能含糊：
        // - 名单本来就是空的：用户没打算挡谁，等同「关着」，不是故障；
        // - 名单里的程序一个都没在跑：**如实说没挡住谁**，绝不报「已阻断」
        return if watch.is_empty() {
            (FirewallState::Off.as_str().to_string(), None)
        } else {
            (
                FirewallState::Partial.as_str().to_string(),
                Some(firewall::unresolved_note(&unresolved)),
            )
        };
    }
    // 一个程序两条规则（公网 v4 + v6），期望条数按此算，用于回读核验
    let expected = programs.len() * 2;
    let reply = match svc::call(&SvcRequest::BlockPublicDirect {
        programs,
        tag: session_tag(),
    }) {
        Ok(SvcResponse::Blocked { rules }) => BlockReply::Applied { rules },
        Ok(SvcResponse::BlockDenied { message }) => BlockReply::Denied { message },
        // **本机装着的服务是旧构建**时走这条：它不认识这条命令，回的是
        // 「未知命令…」。那要降级成 unavailable 并说清「得重装服务」，
        // 而不是让「开启网络」整体失败——服务是增益不是依赖。
        Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {
            BlockReply::Unreachable {
                message: format!(
                    "网络服务是旧构建，不认识阻断命令——要到「设置」里重新安装网络服务后\
                     阻断才会生效（{message}）"
                ),
            }
        }
        Ok(SvcResponse::Failed { message }) if firewall::is_unsupported(&message) => {
            BlockReply::Unsupported { message }
        }
        Ok(SvcResponse::Failed { message }) => BlockReply::Failed { message },
        Ok(other) => BlockReply::Failed {
            message: format!("服务返回了非预期响应: {other:?}"),
        },
        Err(e) if svc::is_unavailable(&e) => BlockReply::Unreachable { message: e },
        Err(e) => BlockReply::Failed { message: e },
    };
    let (state, detail) = firewall::block_state(expected, &unresolved, reply);
    (state.as_str().to_string(), detail)
}

/// 掐掉「开启网络**之前**就已建立」的直连（判据见 [`crate::net::takeover`]）。
/// 返回 `(真掐断的条数, 说明)`；`None` = 这次没做（服务不可达/太旧/空快照）。
///
/// 契约与 [`block_session`] 同款：**任何失败都不返回 Err**——这是增益功能，它失败
/// 绝不能反过来把「开启网络」打掉。
///
/// `snapshot` 由调用方在**开启动作最开头**取（那一刻系统代理还没被任何副作用动过）。
/// 判据在这里才算、而不是取快照时算：判据要查国家库，而国家库由 `ensure_geodata`
/// 保证——先算会把国内目标全认成「查不到」，那一边的取舍是「查不到就不掐」。
fn take_over_pre_net(
    state: &AppState,
    snapshot: &[crate::net::winprobe::SockRow],
    nodes: &[Node],
    rules: &[RelayRule],
    servers: &[Server],
) -> (Option<usize>, Option<String>) {
    // 测试构建下**一律不碰**。本机装着的服务可能已经是新构建，而 `cargo test` 跑的
    // 就是开发机——真掐会去断**用户此刻正在用**的连接（与 `block_session` 同一条硬
    // 规矩，那段注释里的三条理由这里都成立）。真机行为由 `takeover_tests` 里那条
    // 需要管理员的用例与手工验收覆盖。
    if cfg!(test) {
        return (None, None);
    }
    if snapshot.is_empty() {
        // 表都读不到（非 Windows / 权限异常）：与「一条都不需要掐」区分开
        return (Some(0), None);
    }
    let images: HashMap<u32, String> = match crate::net::winprobe::process_images() {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            return (
                None,
                Some(format!("读进程表失败，这次没有接管开启前的连接: {e}")),
            )
        }
    };
    // 豁免名单刻意只有两个：**我们自己的** mihomo（隧道出站）与云驿自己（读分享、
    // 部署服务器都走 SSH）。别的代理客户端不在名单里——它们的既有连接该不该被打断
    // 是用户的选择（见 takeover 模块头的「范围=所有程序」）。
    let mut exempt: HashSet<String> = HashSet::from(["mihomo.exe".to_string()]);
    if let Some(me) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
    {
        exempt.insert(crate::net::connlog::image_key(&me));
    }
    let own = crate::net::connlog::own_endpoints(&crate::net::connlog::endpoint_map(
        nodes, rules, servers,
    ));
    // 国家库：`data_dir/net/country.mmdb`——**mihomo 自己读的那一份**，与采样器同源
    let geo = crate::net::mmdb::CountryLookup::new(crate::net::geodata::mmdb_path(state));
    let pre = crate::net::takeover::PreNet {
        images: &images,
        exempt: &exempt,
        own: &own,
        country: &geo,
    };
    let (rows, dropped) = crate::net::takeover::plan(snapshot, &pre);
    if dropped > 0 {
        eprintln!(
            "[vpn-ctl] 待接管连接超过上限 {}，本次丢弃 {dropped} 条",
            crate::net::takeover::MAX_TAKEOVER
        );
    }
    if rows.is_empty() {
        return (Some(0), None);
    }
    let count = rows.len();
    match svc::call(&SvcRequest::ResetStale { rows }) {
        Ok(SvcResponse::StaleReset { killed, skipped }) => {
            eprintln!(
                "[vpn-ctl] 接管开启前的直连：选中 {count} 条，掐断 {killed} 条，跳过 {skipped} 条"
            );
            // 跳过的缘由要说清：多半是服务侧复查时它们已经不在了（自己关掉/属主变了）。
            // 一条都没掐成时这个说明就是唯一线索，不能省。
            let detail = (skipped > 0).then(|| {
                format!("另有 {skipped} 条没掐成（连接已自己关了，或属主已变）")
            });
            (Some(killed), detail)
        }
        // **本机装着的服务是旧构建**：它不认识这条命令。降级 — 并说清得重装服务，
        // 而不是让「开启网络」整体失败（服务是增益不是依赖）。
        Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => (
            None,
            Some(format!(
                "网络服务是旧构建，不认识接管命令——要到「设置」里重新安装网络服务后才会生效（{message}）"
            )),
        ),
        Ok(SvcResponse::Failed { message }) => (
            None,
            Some(format!("接管开启前的连接失败（不影响上网）: {message}")),
        ),
        Ok(other) => (
            None,
            Some(format!("服务返回了非预期响应: {other:?}")),
        ),
        Err(e) if svc::is_unavailable(&e) => (
            None,
            Some(format!("网络服务不可用，这次没有接管开启前的连接: {e}")),
        ),
        Err(e) => (
            None,
            Some(format!("与服务通信失败，这次没有接管开启前的连接: {e}")),
        ),
    }
}

/// 撤掉全部自家阻断规则（历次会话的都撤）。
fn clear_session_rules() -> Result<usize, String> {
    match svc::call(&SvcRequest::UnblockDirect) {
        Ok(SvcResponse::Unblocked { removed }) => Ok(removed),
        Ok(SvcResponse::Failed { message }) => Err(message),
        Ok(other) => Err(format!("服务返回了非预期响应: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 尽力撤规则并只记一行日志。**用在不能被阻断的路径上**（关闭网络、开启网络的自愈、
/// TUN 切换）：那里出错只能咽下去，界面另有「清除残留规则」的逃生门。
fn clear_session_rules_quietly(what: &str) {
    // 测试构建下不碰真实防火墙，理由与 [`block_session`] 完全相同（同一段注释，
    // 三条理由都成立：不许留规则、已实测的交叉干扰、读响应没有超时）
    if cfg!(test) {
        return;
    }
    if let Err(e) = clear_session_rules() {
        eprintln!("[vpn-ctl] {what}失败（可在设置页手工清除残留阻断规则）: {e}");
    }
}

/// 自身进程 PID（测速 sidecar + 本客户端），冲突扫描时排除。
fn exclude_own_pids(state: &AppState) -> Vec<u32> {
    let mut pids = Vec::new();
    if let Ok(guard) = state.sidecar.try_lock() {
        if let Some(s) = guard.as_ref() {
            pids.push(s.child.id());
        }
    }
    if let Ok(guard) = state.net.try_lock() {
        if let Some(n) = guard.as_ref() {
            // 服务托管时主程序没有句柄，它的 pid 也扫不到（服务是 SYSTEM，
            // 列不出别人的命令行）——孤儿清理那步本来也只针对主程序直接起的
            if let Some(c) = n.child.as_ref() {
                pids.push(c.id());
            }
        }
    }
    pids
}

/// 本应用实例目录（data_dir/net、data_dir/sidecar）只允许本实例的子进程持有：
/// 上一实例异常退出（窗口关闭/强杀，exit 清理未成功）遗留的孤儿 mihomo（-d 指向
/// 这两个目录）一律结束——否则它继续监听并被系统代理指向，重开网络必被接管闸门
/// 拦下（重启 app 场景实测复现两次：父进程死亡、孤儿监听 61415 存活）。
/// 测试用目录判定；生产清理还会核验真实镜像和父进程身份，见 orphan::cleanup。
#[cfg(all(test, feature = "local-tests"))]
pub fn orphan_mihomo_pids(
    cmdlines: &[(u32, String)],
    own_pids: &[u32],
    data_dir: &str,
) -> Vec<u32> {
    cmdlines
        .iter()
        .filter(|(pid, cmd)| {
            !own_pids.contains(pid)
                && ["net", "sidecar"].iter().any(|name| {
                    crate::net::orphan::command_matches(cmd, &format!("{data_dir}\\{name}"))
                })
        })
        .map(|(pid, _)| *pid)
        .collect()
}

fn kill_own_orphans(data_dir: &std::path::Path, own_pids: &[u32]) -> Result<(), String> {
    let targets: Vec<_> = ["net", "sidecar"]
        .iter()
        .map(|name| {
            let dir = data_dir.join(name);
            (dir.join("mihomo.exe"), dir)
        })
        .collect();
    crate::net::orphan::cleanup(&targets, own_pids).map(|_| ())
}

fn recover_service_session() -> Result<(), String> {
    recovery_result(svc::call(&SvcRequest::RecoverNet))
}

pub(super) fn recovery_result(result: Result<SvcResponse, String>) -> Result<(), String> {
    match result {
        Ok(SvcResponse::Recovered) => Ok(()),
        Err(e) if svc::is_unavailable(&e) => Ok(()),
        Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {
            Err("网络服务仍是旧版本，请先重启云驿及网络服务，再开启网络".into())
        }
        Ok(SvcResponse::Failed { message }) => Err(message),
        Ok(other) => Err(format!("恢复网络会话时收到未知响应: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 本应用的系统代理端口：**恒定** [`NET_DEFAULT_PORT`]，占用即报错，绝不顺延。
///
/// 抽成注入式纯函数只为给「不许顺延」这条口径留一处可测的判据——它曾经是个端口候选
/// 循环，第二候选起走 OS 临时端口，而那正是整机断网的根因（见 `start` 的调用点注释）。
pub(crate) fn pinned_mixed_port(
    is_listening: &dyn Fn(u16) -> bool,
    owner_image: &dyn Fn(u16) -> Option<String>,
) -> Result<u16, String> {
    if !is_listening(NET_DEFAULT_PORT) {
        return Ok(NET_DEFAULT_PORT);
    }
    let who = owner_image(NET_DEFAULT_PORT)
        .filter(|n| !n.is_empty())
        .map(|n| format!("（{n}）"))
        .unwrap_or_default();
    Err(format!(
        "端口 {NET_DEFAULT_PORT} 已被占用{who}：系统代理固定用这个端口，换端口会让已经\
         启动的程序指向一个没人听的代理（它们把代理地址记在环境变量里，运行中改不掉），\
         整机所有程序会报连接被拒。请先关掉占用它的程序，再开启网络。"
    ))
}

fn degraded(state: &AppState) -> bool {
    !geodata::geo_present(state)
}

/// 刷新「自我托管」主机集合：所有节点地址（落地机）+ 各中转规则的入口机。
///
/// 判据是「这台机器上跑着我们自己会重启的东西」，**不是**「这是我们管理的服务器」——
/// 后者会把 112 那种只有经代理才可达的机器也拉进直连优先，白白多等一次连接超时
/// 才回退（实测 112 直连建不上）。所以只收节点宿主与中转入口。
fn refresh_self_hosted(nodes: &[Node], rules: &[RelayRule], servers: &[Server]) {
    let mut hosts: std::collections::HashSet<String> =
        nodes.iter().map(|n| n.addr.clone()).collect();
    for r in rules {
        if let Some(s) = servers.iter().find(|s| s.id == r.relay_server_id) {
            hosts.insert(s.host.clone());
        }
    }
    crate::net::local_proxy::set_self_hosted(hosts);
}

/// 开启：冲突即拒（提醒不抢占）→ 二进制确保 → 端口分配 → 客户端 config → spawn →
/// 控制器就绪（≤5s）→ 系统代理快照+写入。重复调用幂等（已在运行直接回状态）。
pub async fn start(state: &AppState, opts: StartOptions) -> Result<NetStatus, String> {
    let mut guard = state.net.lock().await;
    // 已在运行：直接返回（防双进程）；子进程已退出则清槽走重新启动（崩溃自愈）
    if let Some(n) = guard.as_mut() {
        if process_alive(n) {
            // 幂等重入：登记表万一被清过（如并发 stop），这里补回，保证「在跑就登记」
            local_proxy::set(Some(n.mixed_port));
            // 采样器同理补回：它可能因连续查询失败自杀过（如 mihomo 一时间不响应），
            // 而「再点一次开启网络」是用户唯一能做的重试动作，不能点了还是没数据
            spawn_conn_sampler(state, n);
            let via_service = n.child.is_none();
            // 幂等重入可能发生在 TUN 已经开着的时候，这里必须问服务要真实状态，
            // 不能想当然填 false——那会让界面把开着的 TUN 显示成关着
            let tun = via_service && service_state().is_some_and(|(_, t)| t);
            return Ok(NetStatus {
                running: true,
                port: Some(n.mixed_port),
                conflicts: vec![],
                degraded_geo: degraded(state),
                via_service,
                tun,
                // 幂等重入不算新会话：阻断状态沿用实例上记着的那份，不重装规则
                // （重装会有一个「先撤后装」的空窗，而那期间用户的直连流量是放行的）
                firewall: n.firewall.clone(),
                firewall_detail: n.firewall_detail.clone(),
                takeover_killed: n.takeover_killed,
                takeover_detail: n.takeover_detail.clone(),
                // 幂等重入同样沿用实例上的 Tor 状态：它可能正由上一次开启起的看门狗
                // 往回写（bootstrap 有进度），这里如实回报当前值，别写死 off——
                // 那会让界面把「正在建立」显示成「没这个功能」。
                tor: n.tor.clone(),
                tor_detail: n.tor_detail.clone(),
            });
        }
        // 子进程已经没了：**换进程就是换会话**，旧进程的累计不属于新会话。
        // 清零原本只挂在 `stop()` 上，而「崩溃自愈」这条路径不经过它——于是旧进程的
        // 总量会顶着「本次会话（经 mihomo）」的名义显示到第一轮采样成功为止。用户刚点完
        // 开启网络就去看日志，正好命中那个窗口。
        // 也**不能**改成在 `ConnSampler::start()` 里清零：上面那条幂等重入是同一个
        // mihomo，那时保留累计才是对的。要清的只有「进程被换掉」这一条。
        state.conn_sampler.stop();
        guard.take();
    }
    // 接管判据的输入：**在代理生效之前**取一次连接表（此刻还没有任何副作用动过
    // 系统代理）。再往后就会跨过服务会话回收、孤儿清理、冲突扫描、geo 下载——那些
    // 加起来实测可达十几秒，「开启前」就被定义错了。判据本身在这一步不急着算：
    // 它要查国家库，而国家库由下面的 `ensure_geodata` 保证（那时才算得出国内目标）。
    let pre_net_snapshot: Vec<crate::net::winprobe::SockRow> =
        crate::net::winprobe::socket_snapshot().unwrap_or_default();
    // 同类孤儿清理：上一实例未杀干净的 mihomo 先结束（此时本实例尚未持有 net 子
    // 进程，排除 pid 仅可能来自测速 sidecar——它正在为列出的节点提供注入服务,保它）。
    // 必须在接管信号扫描之前：孤儿还监听着上次写进系统代理的端口,不先清掉就会被
    // 判为「其它代理接管」（就是用户此前反复遇到的 61415 拦截）。
    // 服务先核验会话所属进程，再回收失主会话；必须早于系统代理/TUN 冲突检查。
    recover_service_session()?;
    // 上一轮退出可能留下一个直连 mihomo 占着固定端口（退出保护，见
    // `stop_for_exit`）。用户这次明确要开网络：先让服务收掉它，避免把自家端点
    // 误判成「端口被占用」。失败无所谓——服务不可达时没有服务托管的端点；
    // 另一个活窗口持有时后面的 StartNet 也会因归属冲突被拒。
    let _ = svc::call(&SvcRequest::StopNet);
    kill_own_orphans(&state.data_dir, &exclude_own_pids(state))?;
    // 强杀自愈（四重清扫之二）：走到这里必然意味着**本应用没有会话在跑**（上面
    // 要么已 return，要么把死实例清了槽），所以此刻还挂着的自家阻断规则，必然是
    // 上一次会话被强杀留下的——主进程没了，`stop()` 那段清扫根本没机会跑。
    // 这个位置的自愈是免费且完备的：不用等用户想起来去设置页点「清除残留」。
    //
    // 放在冲突扫描**之前**：万一残留规则正是「其它代理看起来连不上」的原因，
    // 先撤掉再判定，免得把自家残留误判成别人的问题。
    clear_session_rules_quietly("清扫上次会话残留的阻断规则");
    if !opts.ignore_conflicts {
        // 自愈：强杀/崩溃后本应用遗留的「指向本机死端口」系统代理先清掉（否则每次
        // 开启都被自己的僵尸指针拦下）。真接管（本机端口有监听/非 loopback）绝不动——
        // 那是别的客户端的流量,交给下方接管信号闸门拦停提醒。
        if let Err(e) = crate::net::sysproxy_set::heal_dead_system_proxy() {
            eprintln!("[vpn-ctl] 残留系统代理自愈失败: {e}");
        }
        let conflicts = match &opts.conflict_override {
            Some(c) => c.clone(),
            None => {
                // 只看实际接管信号：系统代理已启用或代理 TUN 网卡。进程挂着但未接管
                // 流量（如 Clash Verge 仅后台挂起）不拦——扫描进程仅提示"可能是谁"
                let sigs = crate::net::conflict::takeover_signals()?;
                if sigs.is_empty() {
                    vec![]
                } else {
                    let procs = crate::net::conflict::scan_conflicts(&exclude_own_pids(state))?;
                    let hint = if procs.is_empty() {
                        String::new()
                    } else {
                        format!("；相关进程: {}", procs.join("、"))
                    };
                    return Err(format!(
                        "检测到其它代理正在接管系统流量: {}{hint}。请先关闭其系统代理/TUN 再开启",
                        sigs.join("、")
                    ));
                }
            }
        };
        if !conflicts.is_empty() {
            return Err(format!(
                "检测到正在运行的代理软件: {}。请先在其设置中关闭系统代理/TUN 再开启",
                conflicts.join("、")
            ));
        }
    }
    let net_dir = state.data_dir.join(NET_DIR);
    let resource = state
        .sidecar_resource
        .read()
        .map_err(|_| "资源锁读取失败".to_string())?
        .clone();
    let exe = sidecar::ensure_binary(&net_dir, resource.as_deref())?;
    // 先确保分流数据：geoip/geosite 存在时 mihomo 无需启动期联网下载
    // （缺失会触发 country.mmdb 下载，每次候选端口多 5s）；失败降级不阻断启动
    let geo_client = crate::subs::service::http_client(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(30),
    );
    let _ = geodata::ensure_geodata(state, geo_client).await;
    let nodes =
        crate::db::repos::nodes::list(&state.db.lock(), &state.creds).map_err(|e| e.to_string())?;
    // 上游闸门：没有可用节点就别开。否则会生成一份兜底为 MATCH,REJECT 的配置——
    // 那份配置**是对的**（fail-closed，绝不肯静默直连），但对用户毫无用处，且表现
    // 是「国内能上、墙外全被拒」，很容易被当成「代理坏了」而不是「没节点」。
    // 判据与 client_yaml 同源（has_usable_nodes），免得出现「闸门放行、配置却降级」。
    if !crate::config::has_usable_nodes(&nodes) {
        return Err(
            "还没有可用节点：请先导入订阅、或在「节点」页启用至少一个节点再开启网络。\
             现在开启只会得到一份没有出口的配置——墙外流量会被全部拒绝。"
                .into(),
        );
    }
    let geo = geodata::geo_present(state);
    // 隐私开关：与 client_yaml_with_privacy 内部同源的判据（config::privacy_in_use）。
    // 一次算好放在循环外——端口候选循环里每轮都要用同一份，而「要不要起 Tor」
    // 也只该判定一次。
    let privacies =
        crate::db::repos::privacies::list_all(&state.db.lock()).map_err(|e| e.to_string())?;
    let needs_tor = crate::config::privacy_in_use(&nodes, &privacies);
    let tor_candidates = crate::config::tor_upstream_candidates(&nodes, &privacies);
    // 混合端口**恒定**为 NET_DEFAULT_PORT，占用即报错——绝不顺延到别的端口。
    //
    // 2026-09-25 实测：这个端口会被写进系统代理（WinINET）与**用户级环境变量**，而
    // 环境变量是进程启动那一刻继承的死快照，运行中改不了。本机当时有 71 个进程
    // （浏览器、QQ、终端、Claude Code…）的环境块里记着 `HTTP_PROXY=http://127.0.0.1:7890`。
    // 这里一旦顺延（旧实现：第二候选起走 OS 临时端口），那些进程就永久指向一个没人听的
    // 端口，整机所有程序报 Connection refused；而临时端口用完即还，之后还可能被系统
    // 分给无关程序，连「至少连到本机」都做不到。
    //
    // 判据查**真实监听表**而不是 bind 试探：Windows 上带 SO_REUSEADDR 的监听者（Go 系
    // 代理）会对第二次 bind 放行，试探会说「空闲」，然后两个监听者抢同一个端口
    // （见 `winprobe::port_is_listening` 的同一条实测）。
    let mixed_port = pinned_mixed_port(&crate::net::winprobe::port_is_listening, &|p| {
        crate::net::winprobe::listener_image(p).ok().flatten()
    })?;
    let (_, ctrl_port) = sidecar::alloc_local_ports().map_err(|e| e.to_string())?;
    let tor_upstream_port = alloc_tor_upstream_port(mixed_port, ctrl_port)?;
    let token = sidecar::random_token();
    // 中转规则/服务器表：节点挂规则时生成经中转机转发的代理（effective_node 一侧收敛）
    let rules = crate::relay::list_relay_rules(state).map_err(|e| e.to_string())?;
    let servers = crate::db::repos::servers::list(&state.db.lock()).map_err(|e| e.to_string())?;
    let yaml = crate::config::client_yaml_with_tor_upstream(
        &nodes,
        mixed_port,
        ctrl_port,
        &token,
        geo,
        net_dir.to_str().unwrap_or("."),
        &rules,
        &servers,
        // 一律以系统代理模式起来，TUN 由用户在界面上按需打开（spec §1 的形态）。
        // 不持久化 TUN 状态：重启后回到可预期的系统代理模式，比悄悄接管全局好排查。
        false,
        &privacies,
        Some(tor_upstream_port),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(net_dir.join("config.yaml"), &yaml)
        .map_err(|e| format!("写入客户端配置失败: {e}"))?;
    // 必须传 -d：该 mihomo 版本不读取 cwd 的 config.yaml（实测裸 spawn 只按
    // 默认值监听 7890/9090），-d 指定配置目录后才加载 config.yaml
    // 优先经提权服务起进程（后续 TUN 全局模式需要它持有管理员令牌）；服务
    // 未装/未启动则回退直接 spawn——服务是**增益不是依赖**，不能让「开启
    // 网络」因为服务缺失而从可用变成不可用。
    //
    // 注意区分两种失败：服务**不可达**是预期情况（没装），静默降级；服务
    // **活着却起不来**是真错误，如实上报不降级——否则会把「服务有 bug」
    // 伪装成「没装服务」，永远查不出来。
    let mut child: Option<std::process::Child> = None;
    let via_service = match svc::call(&SvcRequest::StartNet {
        config_yaml: yaml.clone(),
        mixed_port,
        ctrl_port,
        token: token.clone(),
    }) {
        Ok(SvcResponse::Started) => true,
        Ok(SvcResponse::Failed { message }) => {
            return Err(format!("服务启动 mihomo 失败: {message}"));
        }
        Ok(other) => {
            return Err(format!("服务返回了非预期响应: {other:?}"));
        }
        // 只有「服务不可达」（未装/未启动）才降级。服务**明明连着却通信
        // 失败**是服务端故障，降级会把它伪装成「没装服务」，永远查不出来。
        Err(e) if svc::is_unavailable(&e) => {
            child = Some(
                crate::util::quiet(std::process::Command::new(&exe))
                    .args(["-d", net_dir.to_str().unwrap_or(".")])
                    .current_dir(&net_dir)
                    .spawn()
                    .map_err(|e| format!("启动 mihomo 失败: {e}"))?,
            );
            false
        }
        Err(e) => return Err(format!("与服务通信失败: {e}")),
    };
    // 带 geo 规则首帧解析更慢：15s 就绪窗口（测速 sidecar 无 geo 仍用 5s）
    match sidecar::wait_ready(
        child.as_mut(),
        ctrl_port,
        &token,
        std::time::Duration::from_secs(15),
    )
    .await
    {
        Ok(()) => {
            let snapshot = sysproxy_set::apply_net_proxy(mixed_port)?;
            // 登记本地代理：此后进程内所有出站裸 TCP（SSH/部署/服务器测速控制
            // 通道）经 mixed-port 拨号，白拿 mihomo 的规则路由。放在系统代理写
            // 成功之后，与 RunningNet 入槽同步——启动失败就不登记，行为同以前。
            local_proxy::set(Some(mixed_port));
            // 自我托管主机（节点宿主）直连优先：否则对落地机的部署会经装在它
            // 自己身上的节点，80% 那步重启 Xray 就把承载部署的隧道掐断
            refresh_self_hosted(&nodes, &rules, &servers);
            // 会话级阻断：**必须在系统代理写成功之后**。反过来的话有一段
            // 「公网直连已挡、代理还没人接」的空窗，那期间关注程序会断网。
            // 失败不影响本次开启——它只是增益，状态与原因照实报给界面。
            let (firewall, firewall_detail) = block_session(state);
            // 接管「开启前就已建立的直连」：**必须在阻断装好之后**。反过来的话
            // 公网直连仍通时，程序会立刻连回来，等于白掐。
            //
            // 「每次开启最多掐一次」由结构保证：走到这一段的分支只会 return，
            // 端口候选循环不会带着同一份旧快照再跑一遍（见上面那个 `Ok(()) => {
            // … return Ok(status) }`）。
            let (takeover_killed, takeover_detail) =
                take_over_pre_net(state, &pre_net_snapshot, &nodes, &rules, &servers);
            let running = RunningNet {
                child,
                mixed_port,
                ctrl_port,
                tor_upstream_port,
                token: token.clone(),
                snapshot: Some(snapshot),
                firewall,
                firewall_detail,
                takeover_killed,
                takeover_detail,
                // 起步先记「还没起」，下一行 spawn 的看门狗会在几百毫秒内改写它。
                // 不在这里直接记 Starting：那需要先判断 needs_tor，而这份实例
                // 构造在端口候选循环里、判据在循环外——两处各写一遍迟早漂移。
                tor: crate::tor::TorState::Off.as_str().to_string(),
                tor_detail: None,
            };
            spawn_conn_sampler(state, &running);
            let status = NetStatus {
                running: true,
                port: Some(mixed_port),
                conflicts: vec![],
                degraded_geo: !geo,
                via_service,
                // 一律以系统代理模式起来（client_yaml 收的是 tun_enabled=false），
                // 所以这里是事实而非假设
                tun: false,
                firewall: running.firewall.clone(),
                firewall_detail: running.firewall_detail.clone(),
                takeover_killed: running.takeover_killed,
                takeover_detail: running.takeover_detail.clone(),
                tor: running.tor.clone(),
                tor_detail: running.tor_detail.clone(),
            };
            *guard = Some(running);
            // 先放锁再挑出口：`select` 内部要拿同一把 `state.net`，握着不放就是死锁。
            drop(guard);
            // 开网络就按已有读数把出口落到实测最快。不这么做的话，开着「自动选择」
            // 起来的这段会先由 url-test 按延迟挑一个——正是那个选项要避免的事——
            // 直到用户下次测速才纠正。读数为空/过期时它自己会放弃，停在 url-test。
            let _ = apply_fastest_measured(state).await;
            // 隐私线真的进了这份配置才懒启动 Tor（用户拍板：真要用才起）。
            // **必须在 drop(guard) 之后**：看门狗回写状态要拿 `net` 锁。
            // 放在 apply_fastest_measured 之后而不是紧随 drop：让它以**已经落定的
            // 出口**为上游，免得刚建的 Tor 电路走在一个马上要被换掉的节点上。
            // 返回 `()`、不上抛——Tor 起不来绝不能把「开启网络」打掉。
            if needs_tor {
                crate::tor::spawn_bootstrap(
                    state,
                    crate::tor::upstream_for(tor_upstream_port),
                    ctrl_port,
                    token.clone(),
                    tor_candidates.clone(),
                );
            }
            Ok(status)
        }
        Err(e) => {
            // 回退路径自己起的进程自己收；服务托管的杀不掉，让它自己停
            if let Some(mut c) = child {
                let _ = c.kill();
                let _ = c.wait();
            } else {
                let _ = svc::call(&SvcRequest::StopNet);
            }
            Err(e)
        }
    }
}

/// 配置变更后客户端的去向（[`apply_config_change`] 的返回值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigApply {
    /// 客户端没开：无需处理，下次开启自然用新配置
    NotRunning,
    /// 配置已热重载，mihomo 进程与系统代理端口保持不变
    Reloaded,
}

/// 配置变更后让**正在运行**的客户端跟上新配置（改伪装域名/改名/开关节点/挂中转后调用）。
///
/// 为什么必须有这一步：客户端 config 是**启动那一刻**渲出来的，而 `start` 是幂等的
/// （已在跑直接返回），所以此前任何配置变更都到不了正在跑的 mihomo——库里是新的、
/// 客户端还是旧的。改 REALITY 伪装域名时这最致命：服务器立刻要求新 SNI，客户端还在
/// 发旧的，那条节点直接连不上，而界面上一切正常（2026-09-18 实测踩过）。
///
/// 两条路径都热重载：主程序直接起的进程由主程序调用控制器，服务托管的进程由服务
/// 清洗、落盘并调用控制器。服务同时更新 LAST_CONFIG，后续 SetTun 不会把配置改回旧版。
///
/// 端口/token 一律沿用运行中实例的：系统代理指着 mixed_port，端口一变就得连带改系统
/// 代理，那是另一个动作，不该在这里顺手做。同理不碰系统代理快照与 local_proxy 登记。
pub async fn apply_config_change(state: &AppState) -> Result<ConfigApply, String> {
    let guard = state.net.lock().await;
    let Some(n) = guard.as_ref() else {
        return Ok(ConfigApply::NotRunning);
    };
    let (mixed_port, ctrl_port, tor_upstream_port, token) = (
        n.mixed_port,
        n.ctrl_port,
        n.tor_upstream_port,
        n.token.clone(),
    );
    let direct = n.child.is_some();

    let nodes =
        crate::db::repos::nodes::list(&state.db.lock(), &state.creds).map_err(|e| e.to_string())?;
    let rules = crate::relay::list_relay_rules(state).map_err(|e| e.to_string())?;
    let servers = crate::db::repos::servers::list(&state.db.lock()).map_err(|e| e.to_string())?;
    let privacies =
        crate::db::repos::privacies::list_all(&state.db.lock()).map_err(|e| e.to_string())?;
    // 隐私线在不在配置里。开关隐私的作业最后就是调到这里，所以这里是
    // 「刚开了隐私 → 配置立刻跟上 → 顺手把 Tor 拉起来」的合流点。
    let needs_tor = crate::config::privacy_in_use(&nodes, &privacies);
    let tor_candidates = crate::config::tor_upstream_candidates(&nodes, &privacies);
    // 节点/链接变了，自我托管集合跟着变（新部署的落地机马上也要能直连 SSH）
    refresh_self_hosted(&nodes, &rules, &servers);
    let net_dir = state.data_dir.join(NET_DIR);
    // tun 一律先渲成 false：直连模式本就开不了 TUN；服务模式由下面的 SetTun 补回，
    // 一次只做一件事——把 tun 塞进初始配置再加 SetTun 只会让两块 dns/tun 打架
    let yaml = crate::config::client_yaml_with_tor_upstream(
        &nodes,
        mixed_port,
        ctrl_port,
        &token,
        geodata::geo_present(state),
        net_dir.to_str().unwrap_or("."),
        &rules,
        &servers,
        false,
        &privacies,
        Some(tor_upstream_port),
    )
    .map_err(|e| e.to_string())?;

    if direct {
        std::fs::write(net_dir.join("config.yaml"), &yaml)
            .map_err(|e| format!("写入客户端配置失败: {e}"))?;
        let resp = reqwest::Client::new()
            .put(format!("http://127.0.0.1:{ctrl_port}/configs"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "path": net_dir.join("config.yaml").to_string_lossy(),
                "payload": yaml,
                "force": true,
            }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("热重载客户端配置失败: {e}"))?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let trimmed: String = body.chars().take(200).collect();
            return Err(format!("热重载客户端配置被拒: HTTP {code} {trimmed}"));
        }
        // 隐私线进了这份配置 → 顺手把本机 Tor 拉起来（起不来只是那条线连不上，
        // 别的线照常，且绝不把这次成功的热重载报成失败）。
        // **先放锁再 spawn**：看门狗要回写状态、会拿 `net` 锁。
        drop(guard);
        if needs_tor {
            crate::tor::spawn_bootstrap(
                state,
                crate::tor::upstream_for(tor_upstream_port),
                ctrl_port,
                token.clone(),
                tor_candidates.clone(),
            );
        }
        return Ok(ConfigApply::Reloaded);
    }

    // 服务托管：必须让服务更新它记账的 LAST_CONFIG；绕过服务直接调控制器会让
    // 下次 SetTun 把隐私配置改回旧版。旧服务不认识 ReloadNet 时保持现有网络运行。
    match svc::call(&SvcRequest::ReloadNet { config_yaml: yaml }) {
        Ok(SvcResponse::Reloaded) => {}
        Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {
            return Err(
                "当前网络服务版本过旧，不支持无断线更新配置；请在方便时更新服务，现有连接保持不变"
                    .into(),
            );
        }
        Ok(SvcResponse::Failed { message }) => {
            return Err(format!("热重载客户端配置失败: {message}"));
        }
        Ok(other) => return Err(format!("服务热重载返回了非预期响应: {other:?}")),
        Err(e) => return Err(format!("请求服务热重载失败: {e}")),
    }
    // 同直连分支：先放锁再 spawn（看门狗回写状态要拿 `net` 锁）
    drop(guard);
    if needs_tor {
        crate::tor::spawn_bootstrap(
            state,
            crate::tor::upstream_for(tor_upstream_port),
            ctrl_port,
            token.clone(),
            tor_candidates.clone(),
        );
    }
    Ok(ConfigApply::Reloaded)
}

/// 把主程序自己起的 mihomo 切到直连（保留进程与端口）。
///
/// 只改 `mode`，不重载整份配置：本地子进程没有 TUN（TUN 只能由服务打开），
/// 而 `PATCH /configs` 是 mihomo 支持的运行时开关（实测 204，端口不断）。
async fn switch_direct(n: &RunningNet) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .patch(format!("http://127.0.0.1:{}/configs", n.ctrl_port))
        .bearer_auth(&n.token)
        .json(&serde_json::json!({ "mode": "direct" }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("请求控制器切换直连失败: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let trimmed: String = body.chars().take(200).collect();
        return Err(format!("控制器拒绝切换直连: HTTP {code} {trimmed}"));
    }
    Ok(())
}

/// 关闭网络时的收尾策略。
#[derive(Clone, Copy, PartialEq, Eq)]
enum StopPolicy {
    /// 用户点「关闭网络」：服务托管的 mihomo 切直连保留；主程序自己起的子进程
    /// 仍然杀掉（无服务时没有 TUN，测试与收尾也需要它退出）。
    KeepService,
    /// 程序退出：两种托管方式都保留一个直连端点。
    KeepAll,
}

/// 关闭：服务托管的切直连、本地子进程杀进程 → 还原系统代理快照 → 清槽。
/// 未运行时空操作（幂等）。
pub async fn stop(state: &AppState) -> Result<NetStatus, String> {
    stop_with_policy(state, StopPolicy::KeepService).await
}

/// 程序退出专用：不杀本机代理，只把它切到直连。
///
/// 根因见 [`pinned_mixed_port`] 的注释：用户级 `HTTP_PROXY` 是进程启动那一刻的
/// 死快照。关闭程序时如果杀掉 7890，浏览器/终端/Claude Code 之类已经跑着的
/// 程序会整片 `ECONNREFUSED`。切直连后它们仍有一个活着的本地代理，新进程按
/// 已还原的系统代理设置走直连。
pub async fn stop_for_exit(state: &AppState) -> Result<NetStatus, String> {
    stop_with_policy(state, StopPolicy::KeepAll).await
}

async fn stop_with_policy(state: &AppState, policy: StopPolicy) -> Result<NetStatus, String> {
    let mut guard = state.net.lock().await;
    if let Some(mut n) = guard.take() {
        match n.child.take() {
            Some(mut c) => {
                if policy == StopPolicy::KeepAll {
                    if let Err(e) = switch_direct(&n).await {
                        // 切不过去也**不杀**：留在原模式（通常是 VPN）至少还能上网。
                        eprintln!("[vpn-ctl] 退出时切换直连失败，保留代理进程: {e}");
                    }
                    // 丢掉句柄不会杀进程（std::process::Child 的 Drop 只关句柄）；
                    // 下次 start 的 kill_own_orphans 会按镜像与 -d 收掉它。
                    drop(c);
                } else {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
            // 进程归服务管，主程序没有句柄——只能请服务切直连。服务此刻不可达
            // 也不报错：系统代理与登记表照样要还原，留着才是真断网。
            None => match svc::call(&SvcRequest::Detach) {
                Ok(SvcResponse::Detached) => {}
                Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {
                    // 旧服务不认识 Detach，它的 reap_owner 仍会杀掉 mihomo；
                    // 如实留一条线索，别把「没修上」伪装成成功。
                    eprintln!(
                        "[vpn-ctl] 网络服务版本过旧，无法保留直连代理；更新网络服务后，\
                         关闭程序才不会再让已运行的程序断网: {message}"
                    );
                }
                Ok(SvcResponse::Failed { message }) => {
                    eprintln!("[vpn-ctl] 保留直连代理失败（进程不杀）: {message}");
                }
                Ok(other) => eprintln!("[vpn-ctl] 保留直连代理收到非预期响应: {other:?}"),
                Err(e) => eprintln!("[vpn-ctl] 请求网络服务保留直连代理失败: {e}"),
            },
        }
        sysproxy_set::restore_proxy(n.snapshot.as_ref())?;
    } else {
        // 界面刚启动、还没开网络也要清掉服务归属：服务可能在上一轮退出时留了
        // 一个直连 mihomo，归属不清会让下一次开启/退出都被「另一个窗口持有」拦住。
        match svc::call(&SvcRequest::Detach) {
            Ok(SvcResponse::Detached) => {}
            Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {}
            Ok(SvcResponse::Failed { message }) => {
                eprintln!("[vpn-ctl] 释放网络服务归属失败: {message}");
            }
            Ok(other) => eprintln!("[vpn-ctl] 释放网络服务归属收到非预期响应: {other:?}"),
            // 服务未安装/未启动：没有归属可释放
            Err(_) => {}
        }
    }
    // 无条件注销：幂等 stop（本就未运行）也不该留下可疑登记。自我托管集合一起清
    // ——留着它会让下次「没开网络」时的拨号白试一次直连（那时本来就直连，无害但
    // 没道理），而它只在开启时才有意义。
    local_proxy::set(None);
    local_proxy::set_self_hosted(std::iter::empty());
    // 阻断规则**同样无条件撤**，且刻意放在 `if let Some(n)` 之外（四重清扫之一）：
    // 与上面那句 `local_proxy::set(None)` 同一个理由——幂等 stop（本就未运行）也不该
    // 留下可疑状态。而这里还有更强的理由：残留的阻断规则是**真会挡住用户网络的**，
    // 撤不掉比留一个陈旧登记严重得多。
    clear_session_rules_quietly("撤销会话阻断规则");
    // 采样器同样无条件停：网络关了它只会每秒失败一次，最后留下一屏过期数据
    state.conn_sampler.stop();
    drop(guard);
    // 关网络同时收掉本机 Tor。三条理由，缺一条都不该留它热乎：
    // 1. Tor 的出站**借道 mihomo 的混合端口**（见 tor::upstream_for）——mihomo 一死，
    //    它的电路全断、bootstrap 回不去，留着只是每几秒失败一次、刷一屏日志；
    // 2. 它**占着 9050**——用户回头开自己的 Tor 会发现端口被一个没人管的 tor.exe 占着；
    // 3. 与阻断规则、采样器同属「本次会话的增益」，会话结束就撤，是既有口径。
    //
    // **必须放在 `drop(guard)` 之后**：`stop_all` 拿 `tor` 锁，而并发的看门狗是
    // 「持 tor 锁 → 等 net 锁」（回写状态）。两边同时发生就是 AB-BA 死锁——
    // `state.rs` 那条 tor→net 的规矩对**停止路径同样成立**，不只是启动路径。
    // 顺带：停掉 Tor 也正好让看门狗的下一次 publish 拿到 None 而安静退出。
    crate::tor::stop_all(state).await;
    Ok(NetStatus {
        running: false,
        port: None,
        conflicts: vec![],
        degraded_geo: degraded(state),
        via_service: false,
        // 没在跑就没有 TUN——网卡是进程退出即销毁的临时网卡（spec 0c）
        tun: false,
        // 没在跑就没有阻断：规则刚被无条件撤掉了
        firewall: FirewallState::Off.as_str().to_string(),
        firewall_detail: None,
        // 接管同理：那是「开启那一刻」的事，关了网络就没有可说的了（下次开启会重新算）
        takeover_killed: None,
        takeover_detail: None,
        // Tor 刚被 stop_all 收掉，如实归零
        tor: crate::tor::TorState::Off.as_str().to_string(),
        tor_detail: None,
    })
}

/// 状态：只读内存+子进程存活（try_wait 非阻塞），不触网不扫描。
/// 只恢复 WinINET/用户环境变量到当前代理端口，不重启 mihomo、Tor 或服务。
/// 供界面发现系统代理被外部关闭时由用户主动修复。
pub async fn repair_system_proxy(state: &AppState) -> Result<(), String> {
    // 一直持锁到注册表写完：否则 stop 可在就绪探测后停掉进程，留下死端口代理。
    let guard = state.net.lock().await;
    let n = guard.as_ref().ok_or("客户端未运行")?;
    let (mixed, ctrl, token) = (n.mixed_port, n.ctrl_port, n.token.clone());
    sidecar::wait_ready(
        None::<&mut std::process::Child>,
        ctrl,
        &token,
        std::time::Duration::from_secs(5),
    )
    .await
    .map_err(|e| format!("代理进程未就绪，未修改系统代理: {e}"))?;
    let _ = sysproxy_set::apply_net_proxy(mixed)?;
    let expected = format!("http://127.0.0.1:{mixed}");
    if crate::sysproxy::system_proxy_url().as_deref() != Some(expected.as_str()) {
        return Err("已写入系统代理，但回读未确认生效".into());
    }
    Ok(())
}

pub async fn status(state: &AppState) -> Result<NetStatus, String> {
    let mut guard = state.net.lock().await;
    let (running, via_service, tun) = match guard.as_mut() {
        // 回退路径（主程序直接 spawn）：能跑，但没有管理员令牌，TUN 无从谈起
        Some(n) if n.child.is_some() => (process_alive(n), false, false),
        // 服务托管：一次调用同时拿到存活与 TUN。管道调用是阻塞的、状态轮询又频繁，
        // 分两次问等于把开销翻倍。
        Some(_) => match service_state() {
            Some((r, t)) => (r, true, t),
            // 服务托管却问不到：**不知道**，不是「没在跑」。如实报错，让界面走
            // `net === null` 的「状态未知」分支（按钮一并禁用），好过给它一个
            // 会把用户引去点「开启网络」的 false。
            None => return Err("网络服务不可达，无法确认当前状态".into()),
        },
        // 槽位为空＝本进程从未启动过网络，这是确定的「没在跑」（服务侧是否还留着
        // 上一轮的实例由启动对账处理），如实回答。
        None => (false, false, false),
    };
    // 未运行（含子进程已死但槽位还在的崩溃态）就注销本地代理登记。不注销也不会
    // 错——拨号撞上死端口会立刻 ECONNREFUSED 并回退直连，代价约 0ms——但状态探测
    // 是唯一能在无用户操作时发现崩溃的时机，在这里自愈比留着陈旧登记干净。
    // 与 start 互斥于同一把槽位锁，故「status 看到未运行」不会与刚起的实例打架。
    if !running {
        local_proxy::set(None);
        local_proxy::set_self_hosted(std::iter::empty());
    }
    let port = if running {
        guard.as_ref().map(|n| n.mixed_port)
    } else {
        None
    };
    // 阻断状态取自实例记账（开启/关 TUN 时写入），**不问服务**：状态轮询很频繁，
    // 而规则只在两个时刻变。没在跑就一定没有阻断——stop 是无条件撤的。
    let (firewall, firewall_detail) = match guard.as_ref().filter(|_| running) {
        Some(n) => (n.firewall.clone(), n.firewall_detail.clone()),
        None => (FirewallState::Off.as_str().to_string(), None),
    };
    // 接管状态同理取实例记账：它只在开启那一刻产生，轮询时还是那几个数
    let (takeover_killed, takeover_detail) = match guard.as_ref().filter(|_| running) {
        Some(n) => (n.takeover_killed, n.takeover_detail.clone()),
        None => (None, None),
    };
    // Tor 状态也取自实例记账——**但读的就是最新值**：看门狗在后台改它，
    // 所以界面每次轮询都能看到 bootstrap 的真实进度（这正是它记在实例上的理由）。
    let (tor, tor_detail) = match guard.as_ref().filter(|_| running) {
        Some(n) => (n.tor.clone(), n.tor_detail.clone()),
        None => (crate::tor::TorState::Off.as_str().to_string(), None),
    };
    Ok(NetStatus {
        running,
        port,
        conflicts: vec![],
        degraded_geo: degraded(state),
        via_service,
        // 没在跑就谈不上 TUN：服务那边可能还留着上次的记账，这里兜一道
        tun: running && tun,
        firewall,
        firewall_detail,
        takeover_killed,
        takeover_detail,
        tor,
        tor_detail,
    })
}

/// 向服务要 (是否存活, TUN 是否开着)；**问不到时返回 `None`（未知），不返回「没在跑」**。
///
/// 早先这里把 `_` 一律降级成 `(false, false)`，注释写的是「状态查询不必把错误抛给界面」。
/// 但那个降级把「问不到服务」说成了「网络没开」——界面据此渲染「未开启」**并给出一个
/// 绿色的「开启网络」按钮**，而网络其实跑得好好的。2026-09-28 实测：应用挂久了仪表盘
/// 就这么显示，切页重挂载才恢复（轮询前端也有闩锁，两处一起才构成那个 bug）。
/// **未知与否定是两件事，不能共用一个返回值。**
fn service_state() -> Option<(bool, bool)> {
    match svc::call(&SvcRequest::Status) {
        Ok(SvcResponse::Status { running, tun }) => Some((running, tun)),
        _ => None,
    }
}

/// 开关 TUN：交由服务改配置并热重载。
///
/// **没有降级路径**——建 TUN 网卡要装内核驱动、必须管理员令牌，非提权的 mihomo
/// 永远建不了。所以这里不学 `start` 那样「服务不可用就退回直接 spawn」，而是
/// 如实报错：静默成功会让界面显示已开启，而实际什么都没接管。
pub async fn set_tun(state: &AppState, enable: bool) -> Result<(), String> {
    let mut guard = state.net.lock().await;
    // 只看「是不是主程序直接起的」：后面还要改实例上的阻断记账，故这里不能把
    // 不可变借用一直挂着
    if guard.as_ref().ok_or("客户端未运行")?.child.is_some() {
        return Err("当前 mihomo 是主程序直接启动的（无提权通道），开不了 TUN。\
                    请先安装网络服务后重新开启网络"
            .to_string());
    }
    // **开 TUN 之前**先取一次表：TUN 是在 IP 层接管**新**流量，开关那一刻已经建立的
    // 连接会留在隧道外继续直连——2026-09-22 实测：WpnService 的两条（4.145.79.81/82）
    // 在系统表里活着、在 mihomo 的连接表里一条都没有，面板如实报 suspect。
    //
    // 取表必须早于 `SetTun`：晚了会把已经进隧道的连接也当成「要掐的」，那不但多余，
    // 还会平白打断几条刚连上的。关 TUN 时不需要（隧道拆了连接本就重建）。
    let pre_tun_snapshot: Vec<crate::net::winprobe::SockRow> = if enable {
        crate::net::winprobe::socket_snapshot().unwrap_or_default()
    } else {
        Vec::new()
    };
    match svc::call(&SvcRequest::SetTun { enable }) {
        Ok(SvcResponse::TunSet { enabled }) if enabled == enable => {}
        Ok(SvcResponse::TunSet { enabled }) => {
            return Err(format!(
                "服务报告的状态与请求不符：请求 {enable}，实际 {enabled}"
            ))
        }
        // 服务**答话**说不成功是真错误（多半是 wintun 驱动装不上），原样透出
        Ok(SvcResponse::Failed { message }) => return Err(message),
        Ok(other) => return Err(format!("服务返回了非预期响应: {other:?}")),
        Err(e) if svc::is_unavailable(&e) => {
            return Err(format!("网络服务未就绪，TUN 不可用（{e}）"))
        }
        Err(e) => return Err(e),
    }
    // TUN 与阻断规则**互斥**，必须跟着切换：
    //
    // TUN 会把 UDP 与 IPv6 一起接管（config 的 tun_block 是 `stack: mixed` +
    // `dns-hijack: any:53`），此时再挡公网 v4/v6 就是纯粹的损失——挡掉的正是
    // TUN 本来能转发的那部分流量。阻断规则的**目的**只是补非 TUN 模式推不到的
    // 那一段，TUN 开着时它没有存在理由。
    //
    // 失败一律咽下去：TUN 那边已经成功了，这里是增益功能的补偿动作，不能反过来
    // 把一次成功的 TUN 切换变成失败。
    if enable {
        clear_session_rules_quietly("关闭 TUN 后撤销阻断规则");
    }
    // 无论哪条分支都要回写记账，否则界面显示的阻断状态与真实规则不符
    let (firewall, firewall_detail) = if enable {
        (FirewallState::Off.as_str().to_string(), None)
    } else {
        block_session(state)
    };
    if let Some(n) = guard.as_mut() {
        n.firewall = firewall;
        n.firewall_detail = firewall_detail;
    }
    // 掐掉留在隧道外的既有直连：复用「开启网络」那条链路（快照 → 判据 → 服务侧复查
    // → `SetTcpEntry`）。**必须在 SetTun 之后**——隧道起来了才知道哪些还在外面。
    //
    // 失败一律咽下去：TUN 已经切成功了，这是增益动作，不能反过来把一次成功的切换
    // 变成失败（与上面清阻断规则同一条规矩）。
    //
    // **IPv6 掐不掉**：Windows 没有 `SetTcp6Entry`，判据本身只选 IPv4
    // （`takeover::should_take_over`）。那部分只能等它空闲超时自然消失——如实计入
    // 「没掐成」，不假装。
    if enable {
        // 读节点/链路/服务器只为算「自家端点」豁免集合；读库失败不该打掉这次切换
        let ctx = (|| -> Result<_, String> {
            let nodes = crate::db::repos::nodes::list(&state.db.lock(), &state.creds)
                .map_err(|e| e.to_string())?;
            let rules = crate::relay::list_relay_rules(state).map_err(|e| e.to_string())?;
            let servers =
                crate::db::repos::servers::list(&state.db.lock()).map_err(|e| e.to_string())?;
            Ok((nodes, rules, servers))
        })();
        match ctx {
            Ok((nodes, rules, servers)) => {
                let (killed, detail) =
                    take_over_pre_net(state, &pre_tun_snapshot, &nodes, &rules, &servers);
                if let Some(n) = guard.as_mut() {
                    n.takeover_killed = killed;
                    n.takeover_detail = detail;
                }
            }
            Err(e) => eprintln!("[vpn-ctl] 开 TUN 后接管既有直连：读库失败，本次跳过（{e}）"),
        }
    }
    Ok(())
}

/// 清除残留的自家阻断规则（设置页逃生门）。
///
/// **会话在跑时拒绝**：那时规则是正在生效的阻断，清掉会让界面显示「已阻断」而实际
/// 一条不剩。要清就先关网络——而 `stop()` 本来就会无条件撤干净，所以这个按钮真正
/// 服务的场景是「上次被强杀、现在还没开网络、但想立刻把残留清掉」。
pub async fn clear_firewall(state: &AppState) -> Result<crate::net::FirewallClearResult, String> {
    let mut guard = state.net.lock().await;
    if let Some(n) = guard.as_mut() {
        if process_alive(n) {
            return Err("网络正在运行，此刻清除会让阻断失效。要清就先「关闭网络」——\
                        关闭时本来就会自动撤掉全部规则"
                .to_string());
        }
    }
    let removed = clear_session_rules()?;
    // 再**独立回读一次**条数：删除路径报的是「我删掉了几条」，而用户按下这个按钮要的
    // 是「现在真的没有残留了」。两者在正常情况下相等，不等就说明有规则删不掉
    // （多半被组策略或安全软件锁住）——那时必须如实报错，而不是回一句「已清除」。
    match svc::call(&SvcRequest::BlockStatus) {
        Ok(SvcResponse::BlockStatus { rules: 0 }) => {}
        Ok(SvcResponse::BlockStatus { rules }) => {
            return Err(format!(
                "清掉了 {removed} 条，但回读发现还有 {rules} 条删不掉——\
                 多半是规则被组策略或安全软件锁住了，可到 wf.msc 里按「yunyi-vpn-」\
                 开头的名字手工删除"
            ))
        }
        // 回读不了就不敢说「清干净了」：把已删条数如实报出来，并说明没能核验
        Ok(other) => {
            return Err(format!(
                "已清掉 {removed} 条，但没能回读核验（服务返回 {other:?}）"
            ))
        }
        Err(e) => return Err(format!("已清掉 {removed} 条，但没能回读核验：{e}")),
    }
    Ok(crate::net::FirewallClearResult { removed })
}

/// 实例是否存活。`child` 为 `None` = 进程归提权服务持有，改问服务。
///
/// 这是阻塞式管道调用（内含最多 3×100ms 的重试），跑在 async 上下文里会占住
/// 一个 worker 线程。可接受的理由：本地管道、服务是单线程循环，正常路径亚毫秒；
/// 本函数所在的两处本来就在做同步 IO（fs::write、Command::spawn）。
fn process_alive(n: &mut RunningNet) -> bool {
    match n.child.as_mut() {
        Some(c) => c.try_wait().map(|w| w.is_none()).unwrap_or(false),
        // tun 字段这里不关心（另由 tun_status 查询），用 .. 免得每加一个状态字段
        // 都要回来改这行
        None => matches!(
            svc::call(&SvcRequest::Status),
            Ok(SvcResponse::Status { running: true, .. })
        ),
    }
}

/// 单次切换最多断开多少条旧连接。兜底，防止连接数异常时把「切换节点」卡住；
/// 触顶不静默，见 [`close_stale_conns`]。
const MAX_CLOSE_PER_SWITCH: usize = 200;
/// 断开旧连接的单请求超时（本机回环，正常亚毫秒）。
const CONN_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

// 判据已拆到 net/runtime_conns.rs（控制单文件行数）；此处 re-export 保持既有路径。
pub use crate::net::runtime_conns::conns_to_close;

/// 切到隐私线前先让 Mihomo **经这条指定代理**完成 HTTPS 请求。Tor 的 100% 引导
/// 与远端 Onion 端口可连，都不能证明 VMess/WS/TLS 等完整协议已可用。
pub(crate) async fn probe_private_proxy(
    controller: &str,
    token: &str,
    proxy_name: &str,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("建立本地核验客户端失败: {e}"))?;
    let mut last_error = String::new();
    for target in [
        "https://www.google.com/generate_204",
        "https://example.com/",
    ] {
        let mut url =
            reqwest::Url::parse(controller).map_err(|e| format!("控制器地址无效: {e}"))?;
        url.path_segments_mut()
            .map_err(|_| "控制器地址不能追加路径".to_string())?
            .extend(["proxies", proxy_name, "delay"]);
        url.query_pairs_mut()
            .append_pair("timeout", "15000")
            .append_pair("url", target);
        match client
            .get(url)
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(19))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("delay").and_then(|d| d.as_u64()))
                    .is_some()
                {
                    return Ok(());
                }
                last_error = "控制器未返回有效延迟".into();
            }
            Ok(resp) => last_error = format!("{} 返回 HTTP {}", target, resp.status()),
            Err(e) => last_error = format!("{} 请求失败: {e}", target),
        }
    }
    Err(last_error)
}

pub async fn verify_private_proxy(state: &AppState, proxy_name: &str) -> Result<(), String> {
    let (ctrl, token) = {
        let guard = state.net.lock().await;
        let n = guard.as_ref().ok_or("客户端未运行")?;
        (n.ctrl_port, n.token.clone())
    };
    probe_private_proxy(&format!("http://127.0.0.1:{ctrl}/"), &token, proxy_name).await
}

/// 清理旧连接失败时的留痕。抽出来是因为几条失败路径要说同一句话——分头写迟早漂移。
fn warn_cleanup(what: &str) {
    eprintln!("[vpn-ctl] {what}；旧连接可能仍走旧出口");
}

/// 断开仍钉在旧出口上的连接（判据见 [`conns_to_close`]）。
///
/// 调用点必须在 PUT **之后**：反过来会有一瞬间「组还没改、连接已被踢」，客户端立刻用
/// 旧节点重连，等于没断。
///
/// 失败**不上抛**：切换本身已经生效（组确实改了），把它报成「切换失败」是假报。但也
/// 不能静默——清理失败的表现是用户看到旧症状重现、却无从查起，故几条失败路径一律留痕。
async fn close_stale_conns(client: &reqwest::Client, n: &RunningNet, new_selection: &str) {
    let base = format!("http://127.0.0.1:{}/connections", n.ctrl_port);
    let list: serde_json::Value = match client
        .get(&base)
        .bearer_auth(&n.token)
        .timeout(CONN_CLEANUP_TIMEOUT)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => return warn_cleanup(&format!("解析连接列表失败: {e}")),
        },
        Err(e) => return warn_cleanup(&format!("读取连接列表失败: {e}")),
    };

    let Ok(base_url) = reqwest::Url::parse(&base) else {
        return warn_cleanup("控制器地址无法解析");
    };
    let ids = conns_to_close(&parse_connections(&list), new_selection);
    let total = ids.len();
    let mut failed = 0usize;
    for id in ids.iter().take(MAX_CLOSE_PER_SWITCH) {
        let mut url = base_url.clone();
        {
            // 连接 id 由 mihomo 生成、形状不归我们管，走 path_segments 转义而不是手工
            // 拼串——id 里出现 `/` 时手拼会打到别的路径上。
            let Ok(mut segs) = url.path_segments_mut() else {
                continue;
            };
            segs.push(id);
        }
        let ok = client
            .delete(url)
            .bearer_auth(&n.token)
            .timeout(CONN_CLEANUP_TIMEOUT)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !ok {
            failed += 1;
        }
    }
    if total > MAX_CLOSE_PER_SWITCH {
        eprintln!(
            "[vpn-ctl] 旧连接过多：本次只断开前 {MAX_CLOSE_PER_SWITCH} 条（共 {total} 条），其余仍走旧出口"
        );
    }
    if failed > 0 {
        eprintln!("[vpn-ctl] {failed}/{total} 条旧连接未能断开，它们仍走旧出口");
    }
}

/// 手动切换出口节点：PUT /proxies/手动选择 {"name": proxy_name}
/// （proxy_name = config 中节点名；「自动选择」为分组默认成员，可改回）。
///
/// 改完分组**必须再断开仍钉在旧节点上的连接**，否则用户看到的还是旧出口（见
/// [`conns_to_close`]）。
pub async fn select(state: &AppState, proxy_name: &str) -> Result<(), String> {
    let private = crate::db::repos::privacies::list_all(&state.db.lock())
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|p| {
            !p.onion_host.is_empty() && crate::servers::privacy::display_name(&p.name) == proxy_name
        });
    if private {
        verify_private_proxy(state, proxy_name)
            .await
            .map_err(|e| format!("隐私出口尚未通过 HTTPS 连通核验，已保留原出口: {e}"))?;
    }
    let guard = state.net.lock().await;
    let n = guard.as_ref().ok_or("客户端未运行")?;
    let url = format!(
        "http://127.0.0.1:{}/proxies/{}",
        n.ctrl_port, MANUAL_GROUP_PATH
    );
    let client = reqwest::Client::new();
    let resp = client
        .put(&url)
        .bearer_auth(&n.token)
        .json(&serde_json::json!({ "name": proxy_name }))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("切换出口节点失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("切换出口节点失败（HTTP {status}）: {body}"));
    }
    // 顺序不能反：先改了组再断连接。反过来会有一瞬间「组还没改、连接已被踢」，客户端
    // 立刻用旧节点重连，等于没断。失败只留痕、不改变返回值——组确实已经改了。
    close_stale_conns(&client, n, proxy_name).await;
    Ok(())
}

/// 当前手动选择的分组出口名（仪表盘下拉回显）；未运行或获取失败给错误。
pub async fn current(state: &AppState) -> Result<String, String> {
    let guard = state.net.lock().await;
    let n = guard.as_ref().ok_or("客户端未运行")?;
    let url = format!(
        "http://127.0.0.1:{}/proxies/{}",
        n.ctrl_port, MANUAL_GROUP_PATH
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&n.token)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("查询出口节点失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("查询出口节点失败（HTTP {}）", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析出口节点失败: {e}"))?;
    Ok(v.get("now")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string())
}

/// 「出口模式」的持久化键，只在**用户明确选过**时写：
///   - `"auto"` = 跟着测速最快走（[`apply_fastest_measured`] 可以在测速后改出口）；
///   - `"manual"` = 用户钉了具体节点，自动逻辑不许动。
///
/// 为什么必须单独存：`select()` 是把「手动选择」组改成**节点名**，而自动挑选本身也走
/// 这条路，于是「自动挑过一次」和「用户手动钉住」在组状态上长得一模一样。下游一旦用
/// `current() == AUTO_GROUP` 反推模式，自动挑选**第一次就会被当成用户的手动选择**，
/// 之后测速再出结果也不会改出口——「自动选择」实际成了一次性的（2026-09-20 实测：
/// 首测选中香港-trojan 后，40 分钟后另一节点快 3 倍，出口纹丝不动）。
/// 模式只能由用户动作决定，不能由「组里现在是谁」反推。
pub const EXIT_MODE_KEY: &str = "exit_mode";

/// 当前是不是「自动（测速最快）」模式。**缺省算自动**：新生成的配置里「手动选择」
/// 默认就指向「自动选择」，下拉框里那个选项也写着「测速最快」。
pub fn exit_mode_is_auto(state: &AppState) -> bool {
    !matches!(state.settings.get(EXIT_MODE_KEY), Ok(Some(v)) if v == "manual")
}

/// 记下用户明确选的出口模式（下拉框改一次记一次）。节点名 = 钉死，自动选择组名 = 自动。
///
/// 写不进去不算错：下次测速至多按缺省（自动）处理，为这个拦下一次节点切换不值当。
pub fn remember_exit_mode(state: &AppState, proxy_name: &str) {
    let mode = if proxy_name == AUTO_GROUP {
        "auto"
    } else {
        "manual"
    };
    let _ = state.settings.set(EXIT_MODE_KEY, mode);
}

/// 测速之后把出口切到**实测最快**的节点。每次测速完调用一次。
///
/// 仅当出口处于自动模式才动——那是「我不挑，你替我挑最快」的意思；钉了具体节点是明确
/// 选择，不碰（见 [`EXIT_MODE_KEY`]：模式取自用户动作，不取自组里当前是谁）。
///
/// 为什么非它不可：`自动选择` 是 mihomo 的 url-test 组，**按延迟挑**，而延迟最低的
/// 未必最快。2026-09-20 实测：香港-vless 延迟 200ms 却只有 0.29 MB/s，大美 510ms 有
/// 1.42 MB/s——url-test 挑了前者，出口慢了近 5 倍，而界面上那个选项写着「测速最快」，
/// 两边说的根本不是一回事。测速数据是应用自己量出来的，出口选择就该用它。
///
/// 切不动不算错（没开网络、没有带宽读数都属正常）：调用方按尽力而为处理。
pub async fn apply_fastest_measured(state: &AppState) -> Result<(), String> {
    if !exit_mode_is_auto(state) {
        return Ok(());
    }
    let (best, has_any_reading) = {
        let conn = state.db.lock();
        let nodes =
            crate::db::repos::nodes::list(&conn, &state.creds).map_err(|e| e.to_string())?;
        let privacies = crate::db::repos::privacies::list_all(&conn).map_err(|e| e.to_string())?;
        let nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|n| crate::config::auto_eligible(n, &privacies))
            .collect();
        (
            crate::config::fastest_measured(&nodes, chrono::Utc::now()).map(|n| n.name.clone()),
            nodes
                .iter()
                .any(|n| n.enabled && n.protocol != "ssr" && n.bandwidth.is_some()),
        )
    };
    match best {
        Some(name) => select(state, &name).await,
        // 有读数但全过期与压根没测过是两回事：前者重测一下就能选，后者得先测速。
        None if has_any_reading => Err(format!(
            "带宽读数已超过 {} 分钟，重新测速后再自动选最快",
            crate::config::READING_TTL.num_minutes()
        )),
        None => Err("还没有任何带宽读数，先测速再自动选最快".into()),
    }
}

/// 写回 Tor 状态。**只拿 `net` 一把锁**——调用方（Tor 看门狗）手里一把锁都不许有，
/// 于是除 `tor → net` 这条允许方向之外不会再出现任何反向持锁。
///
/// 返回 `false` = **网络已经关了**（`stop()` 把实例清掉了）。这是「用户关了网络」
/// 与「Tor 真失败」的分水岭：前者写不进去、也不该报失败（Tor 正是 stop 杀的），
/// 看门狗据此安静退出。**不需要额外的竞态标志位**。
pub async fn publish_tor(
    state: &AppState,
    st: crate::tor::TorState,
    detail: Option<String>,
) -> bool {
    let mut guard = state.net.lock().await;
    match guard.as_mut() {
        Some(n) => {
            n.tor = st.as_str().to_string();
            n.tor_detail = detail;
            true
        }
        None => false,
    }
}

/// App 退出清理（main ExitRequested 调用）。
pub async fn stop_all(state: &AppState) {
    // 程序退出的唯一收尾入口：保留一个直连端点，见 [`stop_for_exit`]。
    let _ = stop_for_exit(state).await;
}
