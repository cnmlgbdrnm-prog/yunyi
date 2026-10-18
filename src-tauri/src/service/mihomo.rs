//! 服务侧 mihomo 生命周期，外加会话级防火墙阻断的分发。
//!
//! mihomo 这部分只有「起进程 / 杀进程」在这——端口选择、就绪探测、系统代理
//! （HKCU）、local_proxy 登记全在主程序侧。理由见 service/mod.rs。
//!
//! 阻断命令（`Block*`）放这里而不是另开模块：它与 mihomo 同属**一次网络会话**的
//! 生命周期（开网络时装、关网络时撤），而服务侧只有这一处命令入口。真正的实现
//! 在 `net::firewall`（那里是唯一书写防火墙 COM 的地方）。
//!
//! 运行所需的一切（mihomo 二进制、geo 数据集）都取自**服务自身 exe 同目录**，
//! 绝不接受客户端指定：那等于让任何本地进程指挥 SYSTEM 执行任意程序。
//!
//! 但取来之后**不在原地跑**：mihomo 会被铺进 `%ProgramData%\yunyi\bin`，geo 铺进
//! net_dir，执行/读取的都是服务自有目录里的副本。原地跑有两个问题，且都在 dev 下
//! 真实存在（正式安装时资源在 Program Files，两条都不成立，但统一规则更省事）：
//!
//! 1. dev 下资产目录解析到 `target/debug/binaries`，那是 cargo 的构建产物。
//!    tauri-build 每次重跑 build script 都要覆盖它，而一个**正在执行**的镜像会把
//!    文件锁死，于是 build/clippy 报一句不点名的「另一个程序正在使用此文件」。
//! 2. `target/` 普通用户可写。从那儿跑等于给非提权进程留了个「换掉 SYSTEM 要执行的
//!    程序」的口子，与上面第一条声称的信任级不符。
//!
//! 输入输出：Request → Response。
//! 依赖：std::process（mihomo 是外部进程，不需要 tokio）、config_guard、paths。

#[cfg(windows)]
use super::process::ManagedChild as Child;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::service::config_guard::sanitize;
use crate::service::paths;
use crate::service::protocol::{Request, Response, StaleConn};

/// 当前 mihomo 子进程。服务单实例运行，故用全局 Mutex 即可。
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// 当前实例使用的 mixed 端口；0 = 未运行。回显给 StopNet 的响应，
/// 让「停掉的是哪一个」在日志里可核对。
static MIXED_PORT: AtomicU16 = AtomicU16::new(0);

/// 控制器端口与 token——`SetTun` 要拿它们打 `PUT /configs`。
/// 服务不自己生成：token 由主程序随机生成并经 `StartNet` 送来。
static CTRL_PORT: AtomicU16 = AtomicU16::new(0);
static TOKEN: Mutex<String> = Mutex::new(String::new());

/// 最近一次落盘的配置原文（清洗后的）。热重载要在这份上改 `tun` 块，
/// 而不是重新问主程序要——那样 `SetTun` 就得带上一整份配置，白扩协议面。
static LAST_CONFIG: Mutex<Option<String>> = Mutex::new(None);

/// TUN 当前是否开着。**由服务记账**：它是唯一知道 mihomo 实际配置的一方。
static TUN_ENABLED: AtomicBool = AtomicBool::new(false);

/// dev 构建下 `npm run fetch:sidecar` 的落盘位置。
fn dev_binaries() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries")
}

/// 服务自带的运行时资产目录（含 `mihomo.exe` 与 `geo/`）。
///
/// 正式安装下是 `<服务 exe 同目录>/binaries`——mihomo 与 geo 在 tauri.conf.json
/// 里以 resource 声明为 `binaries/...`，Tauri 打包保留相对路径，正落在这里。
///
/// dev 回退：debug 构建的 exe 在 `target/debug`，资源不在那儿（与 speed/sidecar.rs
/// 的 candidate_paths 同因），回退到 `CARGO_MANIFEST_DIR/binaries`。
fn assets_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("取服务自身路径失败: {e}"))?;
    let beside = exe.parent().map(|d| d.join("binaries"));
    assets_dir_from(beside, cfg!(debug_assertions).then(dev_binaries)).ok_or_else(|| {
        "找不到服务资源目录（安装包应在服务 exe 同目录下有 binaries\\mihomo.exe 与 binaries\\geo\\）"
            .to_string()
    })
}

/// 候选顺序：exe 同目录优先，其次 dev 回退；取第一个真实存在的目录。
/// 抽成纯函数是为了能确定性地测候选顺序，不必依赖本机装没装。
pub(super) fn assets_dir_from(beside: Option<PathBuf>, dev: Option<PathBuf>) -> Option<PathBuf> {
    match beside {
        Some(p) if p.is_dir() => Some(p),
        _ => dev.filter(|d| d.is_dir()),
    }
}

/// 把资产目录里的 mihomo 铺进 `bin_dir`，返回**真正要执行**的那份的路径。
///
/// 见模块头：不直接从资产目录跑，是为了不锁住 cargo 的构建产物，顺带把「SYSTEM
/// 执行的文件必须落在管理员可写处」这条规则坐实。
///
/// 不校验 sha256：能写资产目录的进程本来就能替换服务二进制本身，校验它不增加
/// 任何防护，只增加一次几十 MB 的读。
pub(super) fn provision_mihomo(src: &Path, bin_dir: &Path) -> Result<PathBuf, String> {
    if !src.is_file() {
        return Err(format!("找不到 mihomo.exe：{}", src.display()));
    }
    let dst = bin_dir.join("mihomo.exe");
    std::fs::create_dir_all(bin_dir)
        .map_err(|e| format!("创建 {} 失败: {e}", bin_dir.display()))?;
    // 幂等：与源一致就不重写。31MB 没必要每次开启网络都拷一遍，也顺带躲开
    // 「上一份刚被 kill、镜像句柄还没完全释放」的窄窗口
    if !same_file(src, &dst) {
        std::fs::copy(src, &dst)
            .map_err(|e| format!("铺 mihomo 到 {} 失败: {e}", dst.display()))?;
    }
    Ok(dst)
}

/// 两份文件是否同一内容：长度 + 修改时间。
///
/// 依赖 `fs::copy` 保留 mtime（Windows 走 CopyFileExW）。这条假设有测试钉着：
/// 万一哪天不成立了，表现是「每次都重拷」这种浪费而非错误，但值得知道。
///
/// 只看长度不够——dev 下重新 fetch 的 mihomo 完全可能长度碰巧相同，那时会静默
/// 跑旧的那份，正是最难查的一类不一致。
fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (a.metadata(), b.metadata()) else {
        return false;
    };
    ma.len() == mb.len() && ma.modified().ok() == mb.modified().ok()
}

/// 把 geo 数据集从 `src_geo` 铺进 mihomo 的工作目录，已存在的文件跳过。
///
/// 为什么必须铺：`geodata-dir` 被钉在 net_dir（见 config_guard），mihomo 从那儿读
/// geoip/geosite/country.mmdb。数据集取自**管理员可写**的资产目录，绝不从用户的
/// APPDATA 取——那是非提权可写的位置，换得掉 SYSTEM mihomo 要解析的数据文件。
///
/// 返回未能就位的文件名。**不因缺文件拒绝启动**：配置未必用到 geo 规则（无节点时
/// 是纯 DIRECT），为此把「开启网络」整体打掉是因小失大。
pub(super) fn provision_geo(src_geo: &Path, net_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(src_geo) else {
        return vec![src_geo.display().to_string()];
    };
    let mut missing = Vec::new();
    for entry in entries.flatten() {
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = net_dir.join(name);
        // 幂等：geoip.dat 十几 MB，没必要每次开启网络都重写一遍
        if dst.is_file() {
            continue;
        }
        if std::fs::copy(&src, &dst).is_err() {
            missing.push(name.to_string_lossy().to_string());
        }
    }
    missing
}

/// 分发一条服务请求。
pub fn handle(req: &Request) -> Response {
    match req {
        Request::RecoverNet | Request::Shutdown => Response::Failed {
            message: "会话命令必须经过调用方身份核验".into(),
        },
        Request::StartNet {
            config_yaml,
            mixed_port,
            ctrl_port,
            token,
        } => start(config_yaml, *mixed_port, *ctrl_port, token),
        Request::ReloadNet { config_yaml } => reload(config_yaml),
        Request::StopNet => stop(),
        Request::Detach => detach(),
        Request::SetTun { enable } => set_tun(*enable),
        Request::Status => status(),
        Request::BlockDirect { programs, tag } | Request::BlockPublicDirect { programs, tag } => {
            block_direct(programs, tag)
        }
        Request::UnblockDirect => unblock_direct(),
        Request::BlockStatus => block_status(),
        Request::ResetStale { rows } => reset_stale(rows),
    }
}

/// 掐断一批「代理生效前就已建立」的直连（判据在主程序那边，见 `net::takeover`）。
///
/// **先复查再下手**：表是刚读的，只有「四元组 + 属主 pid + 仍是 ESTABLISHED」三样
/// 都对得上才动。快照到下手之间隔着启动流程（实测最长十几秒），四元组可能已被复用
/// ——那是这条命令唯一有杀伤力的地方，`recheck` 就是防它的。
///
/// 与 `BlockDirect` 同一习惯：不假定请求里的东西还成立、不假定自己一定成功，回报的
/// 是**真掐断了几条**。三条降级都只算「没掐成」，不让整批失败——这是增益功能，
/// 「开启网络」不该因为它而失败（那件事在调用方还有一层兜底）。
fn reset_stale(rows: &[StaleConn]) -> Response {
    if rows.is_empty() {
        return Response::StaleReset {
            killed: 0,
            skipped: 0,
        };
    }
    // 上限是防御性的：正常场景个位数（见 `takeover::MAX_TAKEOVER`）
    let (batch, rest) = rows.split_at(rows.len().min(crate::net::takeover::MAX_TAKEOVER));
    let over = rest.len();
    // 解析不出的（非 IPv4 / 地址畸形）算「没掐成」：它说明主程序与服务对「掐得动
    // 什么」的理解不一致，如实回报，但不该把整批拖下水
    let targets: Vec<crate::net::takeover::Target> = batch
        .iter()
        .filter_map(crate::net::takeover::parse_wire)
        .collect();
    let rejected = batch.len() - targets.len() + over;

    let now = match crate::net::winprobe::socket_snapshot() {
        Ok(rows) => rows,
        // 复查读不到表：**不猜**，一条都不动（宁可这次没接管，不可盲掐）
        Err(e) => {
            return Response::Failed {
                message: format!("复查连接表失败，未掐任何连接: {e}"),
            }
        }
    };
    let (ok, stale) = crate::net::takeover::recheck(&targets, &now);

    let mut killed = 0usize;
    let mut failed = 0usize;
    for t in &ok {
        match crate::net::takeover::reset_one(t) {
            Ok(()) => killed += 1,
            Err(e) => {
                failed += 1;
                // 单条失败只记一行：它不影响整批，也不影响「开启网络」
                eprintln!("[yunyi-service] 掐连接失败: {e}");
            }
        }
    }
    if over > 0 {
        eprintln!(
            "[yunyi-service] 待掐连接超过上限 {}，本次丢弃 {over} 条",
            crate::net::takeover::MAX_TAKEOVER
        );
    }
    Response::StaleReset {
        killed,
        skipped: rejected + stale + failed,
    }
}

/// 装会话级阻断规则（公网 IPv4 + IPv6，程序级出站 BLOCK）。
///
/// **两类失败必须分开**：防火墙拒写（组策略/安全软件）是**降级信号**，主程序据此
/// 给界面说明而「开启网络」照常成功；其余才是服务侧错误。分开靠的是
/// [`crate::net::firewall::is_denied`]，不是让主程序去比对错误措辞。
fn block_direct(programs: &[String], tag: &str) -> Response {
    match crate::net::firewall::apply(programs, tag) {
        Ok(rules) => Response::Blocked { rules },
        Err(e) if crate::net::firewall::is_denied(&e) => Response::BlockDenied { message: e },
        Err(e) => Response::Failed { message: e },
    }
}

/// 撤掉全部自家规则。幂等——四重清扫会让它走很多遍。
fn unblock_direct() -> Response {
    match crate::net::firewall::clear() {
        Ok(removed) => Response::Unblocked { removed },
        Err(e) => Response::Failed { message: e },
    }
}

/// 回读当前在位的自家规则条数。服务不记账：规则是系统持有的事实，记账只会漂移。
fn block_status() -> Response {
    match crate::net::firewall::count() {
        Ok(rules) => Response::BlockStatus { rules },
        Err(e) => Response::Failed { message: e },
    }
}

/// 配置里 TUN 是否开着。启动时据此初始化记账，而不是假定主程序一定送关闭状态——
/// 假定错了，界面会显示「TUN 关着」而实际全局接管，是最难查的那类不一致。
fn tun_in(config_yaml: &str) -> bool {
    use serde_yaml::Value;
    let Ok(v) = serde_yaml::from_str::<Value>(config_yaml) else {
        return false;
    };
    v.get("tun")
        .and_then(|t| t.get("enable"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 主程序的配置更新不得改掉系统代理仍指向的端口或控制器凭据。
/// 服务保留当前 TUN 状态，并继续把 geodata-dir 固定到服务自有目录。
pub(super) fn prepare_reload_config(
    config_yaml: &str,
    net_dir: &Path,
    mixed_port: u16,
    ctrl_port: u16,
    token: &str,
    tun: bool,
) -> Result<String, String> {
    let cleaned = sanitize(config_yaml, net_dir)?;
    let updated = if tun {
        with_tun(&cleaned, true)?
    } else {
        with_tun(&cleaned, false)?
    };
    let v: serde_yaml::Value =
        serde_yaml::from_str(&updated).map_err(|e| format!("配置解析失败: {e}"))?;
    if v["mixed-port"].as_u64() != Some(u64::from(mixed_port))
        || v["external-controller"].as_str() != Some(&format!("127.0.0.1:{ctrl_port}"))
        || v["secret"].as_str() != Some(token)
    {
        return Err("热重载不能改变正在使用的端口或控制器凭据".into());
    }
    Ok(updated)
}

/// 配置热重载保持 mihomo 进程、系统代理端口和已有 TCP 连接不变。
fn reload(config_yaml: &str) -> Response {
    {
        let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
        if !guard
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        {
            return Response::Failed {
                message: "mihomo 未运行，无法热重载".into(),
            };
        }
    }
    let mixed = MIXED_PORT.load(Ordering::Relaxed);
    let ctrl = CTRL_PORT.load(Ordering::Relaxed);
    let token = TOKEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let old = LAST_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(old) = old else {
        return Response::Failed {
            message: "服务没有记住当前配置，无法热重载".into(),
        };
    };
    let updated = match prepare_reload_config(
        config_yaml,
        &paths::net_dir(),
        mixed,
        ctrl,
        &token,
        TUN_ENABLED.load(Ordering::Relaxed),
    ) {
        Ok(value) => value,
        Err(message) => return Response::Failed { message },
    };
    let path = paths::config_path();
    if let Err(e) = std::fs::write(&path, &updated) {
        return Response::Failed {
            message: format!("写入热重载配置失败: {e}"),
        };
    }
    let body = serde_json::json!({
        "path": path.to_string_lossy(),
        "payload": updated,
        "force": true,
    });
    let addr = format!("127.0.0.1:{ctrl}");
    match crate::service::http::put_json(&addr, "/configs", &token, &body) {
        Ok(code) if (200..300).contains(&code) => {
            *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(updated);
            Response::Reloaded
        }
        result => {
            let restore = std::fs::write(&path, &old);
            let detail = match result {
                Ok(code) => format!("mihomo 拒绝热重载: HTTP {code}"),
                Err(e) => format!("热重载通信失败: {e}"),
            };
            Response::Failed {
                message: match restore {
                    Ok(()) => detail,
                    Err(e) => format!("{detail}；恢复旧配置文件失败: {e}"),
                },
            }
        }
    }
}

/// 在既有配置上开/关 TUN：只动 `tun` 这一块。
///
/// 走 serde_yaml 而不是文本拼接：配置结构已知，解析后改再序列化远比正则稳妥
/// （节点名、规则串里都可能出现长得像 YAML 键的文本）。
///
/// dns 块**不归它管**：两种模式都要（见 `config::dns_block` 的说明），所以关 TUN
/// 不摘 dns，原本没有（缺 geo 数据的降级配置）也不凭空造——那种配置配上 dns 反而
/// 会让 mihomo 启动期去下 mmdb。反过来，开 TUN 时若配置里没有 dns 块就照旧补上：
/// TUN + 无 dns 正是实测里国外域名全挂的形态，宁可让 mihomo 当场报错，也别静默降级。
pub(super) fn with_tun(config_yaml: &str, enable: bool) -> Result<String, String> {
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(config_yaml).map_err(|e| format!("配置不是合法 YAML: {e}"))?;
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| "配置顶层必须是映射（key: value），实际不是".to_string())?;
    map.remove(serde_yaml::Value::from("tun"));
    if !enable {
        return serde_yaml::to_string(&root).map_err(|e| format!("序列化配置失败: {e}"));
    }
    // 有分组才敢让 dns fallback 走代理组：零节点降级配置里没有 proxy-groups，
    // 指向不存在的组会让 mihomo 拒绝加载整份配置
    let has_nodes = map
        .get(serde_yaml::Value::from("proxy-groups"))
        .and_then(|v| v.as_sequence())
        .is_some_and(|s| !s.is_empty());
    for (key, value) in crate::config::tun_block(true) {
        map.insert(serde_yaml::Value::from(key), value);
    }
    let (key, value) = crate::config::dns_block(has_nodes);
    map.insert(serde_yaml::Value::from(key), value);
    serde_yaml::to_string(&root).map_err(|e| format!("序列化配置失败: {e}"))
}

/// 生成「保留端口、直连出口」的配置：摘掉 `tun` 块，`mode` 设为 `direct`。
///
/// 为什么不能只 `PATCH /configs {"mode":"direct"}`：TUN 开着时模式切直连并不会
/// 拆掉虚拟网卡，流量仍会被引进一个已不受分流规则管理的设备。整份热重载一次
/// 同时完成「摘 TUN」与「切直连」，只触发一次配置生效。
///
/// 其余字段原样保留：下次 `StartNet` 本来就会重写整份配置，这里不需要也不应该
/// 清理节点/规则——保留它们反而让「切回规则模式」仍然可用。
pub(super) fn direct_config(config_yaml: &str) -> Result<String, String> {
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(config_yaml).map_err(|e| format!("配置不是合法 YAML: {e}"))?;
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| "配置顶层必须是映射（key: value），实际不是".to_string())?;
    map.remove(serde_yaml::Value::from("tun"));
    map.insert(
        serde_yaml::Value::from("mode"),
        serde_yaml::Value::from("direct"),
    );
    serde_yaml::to_string(&root).map_err(|e| format!("序列化配置失败: {e}"))
}

/// 关 TUN 后等待网卡消失的上限。热重载是异步的（CHANGELOG 记实测约 1.4s 生效），
/// 立刻核验会把「还没拆完」误报成残留。
const TUN_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// 轮询间隔：拆网卡通常几十毫秒级，密一点让正常路径尽快放行。
const TUN_TEARDOWN_INTERVAL: Duration = Duration::from_millis(100);

/// 关 TUN 之后仍残留的自家网卡（空 = 拆干净了）。
///
/// 为什么需要核验：关 TUN 只做「改配置 + 热重载」，而 mihomo 到底有没有把网卡拆掉，
/// 此前没有任何一处确认过——TUN 的开关状态只看进程活没活（见 `status`）。
/// 实测（2026-09-19）撞到过这种半开状态：配置里已经没有 `tun` 块，而 `yunyi-tun0`
/// 仍在、仍持有 `198.18.0.1` 与 `fdfe:dcba:9876::1`、并占着默认路由 `0.0.0.0/0`。
/// 流量被引进一个已不受配置管理的东西，而界面显示「TUN 已关闭」。
///
/// 只看网卡在不在：网卡没了，Windows 会连带撤掉它的地址与路由，所以这是
/// 「地址/路由是否残留」的廉价且足够的前哨。
///
/// **前提已验证（2026-09-19）**：`winprobe::adapter_names()`（GetAdaptersAddresses）
/// 确实枚举得出 wintun 网卡——本机活的 Tailscale 网卡（同为 wintun，`OperStatus=Up`）
/// 就在那份列表里，网卡名与设备都能对上。
///
/// 但同一次排查也暴露了反向的坑：枚举当时带着公开的
/// `GAA_FLAG_INCLUDE_ALL_INTERFACES`，会把**设备已不存在、只剩注册表残留**的幻影条目
/// 一并返回（自家 `yunyi-tun0`、别家 `skyline-vpn-ethernet-*`）。那种条目在这儿会被
/// 当成「TUN 没拆掉」，在接管闸门那边则会把「开启网络」拦死。现在枚举只认
/// `NotPresent` 之外的条目（见 `winprobe::live_adapter_names`），两个调用点同时受益。
/// 若将来实测「TUN 明明还在、这里却报干净」，下一步是改查路由表（GetIpForwardTable2）：
/// 默认路由被 TUN 占着，才是当初真正造成伤害的东西。
pub(super) fn tun_teardown_leftovers(adapters: &[String]) -> Vec<String> {
    adapters
        .iter()
        .filter(|n| n.eq_ignore_ascii_case(crate::config::TUN_DEVICE))
        .map(|n| format!("TUN 网卡仍在（{n}）"))
        .collect()
}

/// 轮询到网卡真的消失为止，返回**问题描述**（空 = 已确认拆干净）。
///
/// `probe` 注入当前网卡名列表（生产传 `winprobe::adapter_names`），便于确定性地测
/// 轮询与超时，不必依赖本机真的装没装 TUN。
pub(super) fn wait_tun_gone(
    probe: &dyn Fn() -> Result<Vec<String>, String>,
    timeout: Duration,
    interval: Duration,
) -> Vec<String> {
    let started = Instant::now();
    loop {
        match probe() {
            // 探针坏了 = 压根没核验过。不能当成「已拆干净」放行——那正是本次要消灭的
            // 「没验证就报成功」。重试也没有意义（坏的是探针本身），立即上报。
            Err(e) => return vec![format!("无法核验 TUN 是否已拆除: {e}")],
            Ok(names) => {
                let leftovers = tun_teardown_leftovers(&names);
                if leftovers.is_empty() {
                    return Vec::new();
                }
                if started.elapsed() >= timeout {
                    return leftovers;
                }
            }
        }
        std::thread::sleep(interval);
    }
}

/// 恢复旧版孤儿后也要等自家 TUN 网卡退出，不能让下一步冲突检查抢在异步拆除之前。
pub(super) fn ensure_tun_gone() -> Result<(), String> {
    let problems = wait_tun_gone(
        &crate::net::winprobe::adapter_names,
        TUN_TEARDOWN_TIMEOUT,
        TUN_TEARDOWN_INTERVAL,
    );
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("；"))
    }
}

/// 开/关 TUN：改配置 → 落盘 → `PUT /configs` 热重载。
fn set_tun(enable: bool) -> Response {
    {
        // 及时释放：下面的热重载是网络往返，占着 CHILD 锁会让并发的 Status 查询一起卡住
        let guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            return Response::Failed {
                message: "mihomo 未在运行，TUN 开关无处生效".to_string(),
            };
        }
    }
    let ctrl = CTRL_PORT.load(Ordering::Relaxed);
    let token = TOKEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if ctrl == 0 || token.is_empty() {
        return Response::Failed {
            message: "服务没有记住控制器端口/token，无法热重载".to_string(),
        };
    }
    let Some(current) = LAST_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    else {
        return Response::Failed {
            message: "服务没有记住当前配置，无法热重载".to_string(),
        };
    };

    let updated = match with_tun(&current, enable) {
        Ok(u) => u,
        Err(e) => return Response::Failed { message: e },
    };
    // 先落盘再热重载。顺序反过来的话，热重载成功而落盘失败会让磁盘上的配置与
    // 运行中的状态不一致——直到下次 start 之前都不会有人发现。
    if let Err(e) = std::fs::write(paths::config_path(), &updated) {
        return Response::Failed {
            message: format!("写入配置失败: {e}"),
        };
    }

    let body = serde_json::json!({
        "path": paths::config_path().to_string_lossy(),
        "payload": updated,
        "force": true,
    });
    let addr = format!("127.0.0.1:{ctrl}");
    match crate::service::http::put_json(&addr, "/configs", &token, &body) {
        Ok(code) if (200..300).contains(&code) => {
            *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(updated);
            // 关 TUN 之后必须核验网卡真拆了：热重载只说明 mihomo 收下了配置，不说明
            // 系统那一侧已经还原。不核验的话，「配置里没有 tun 块、网卡却仍在接管」
            // 这种半开状态会一路报到界面上显示成「TUN 已关闭」。
            if !enable {
                let problems = wait_tun_gone(
                    &crate::net::winprobe::adapter_names,
                    TUN_TEARDOWN_TIMEOUT,
                    TUN_TEARDOWN_INTERVAL,
                );
                if !problems.is_empty() {
                    // TUN_ENABLED 有意**不**跟着置 false：网卡可能还在接管，记账留在
                    // 「还开着」这一侧与真实风险同侧——界面据此显示 TUN 仍开启，
                    // 而不是报一个假的「已关闭」。配置记账已更新，下一开关照常收敛。
                    return Response::Failed {
                        message: format!(
                            "配置已改为关闭 TUN，但系统侧没撤干净：{}。\
                             此时流量仍可能被引进虚拟网卡——请「关闭网络」后重试；\
                             若反复如此，多半是安全软件锁住了虚拟网卡。",
                            problems.join("；")
                        ),
                    };
                }
            }
            TUN_ENABLED.store(enable, Ordering::Relaxed);
            Response::TunSet { enabled: enable }
        }
        // 非 2xx 多半是驱动装不上（wintun 被安全软件拦）——原样报出去，
        // 主程序据此让开关回弹并把 mihomo 的话展示给用户
        Ok(code) => Response::Failed {
            message: format!("热重载被 mihomo 拒绝: HTTP {code}"),
        },
        Err(e) => Response::Failed { message: e },
    }
}

/// 保留 mihomo：切直连、摘 TUN，进程与端口都不动。
///
/// **失败也不能改成杀进程**——杀进程正是要修的故障：已经跑着的程序（环境变量里
/// 记着本机代理）会立刻断网。失败时维持现状（通常是 VPN 模式，仍然能上网）并把
/// 原因回报给主程序记日志；下次 `StartNet` 或服务停止会正常收掉它。
fn detach() -> Response {
    {
        // 只核验存活，不在网络往返期间占着 CHILD 锁：并发的 Status 查询会一起卡住
        let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
        if !guard
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        {
            // 没有活着的 mihomo：没有端口要保，顺手把崩溃残留的记账清干净
            let _ = guard.take();
            MIXED_PORT.store(0, Ordering::Relaxed);
            TUN_ENABLED.store(false, Ordering::Relaxed);
            *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Response::Detached;
        }
    }
    let ctrl = CTRL_PORT.load(Ordering::Relaxed);
    let token = TOKEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let current = LAST_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(current) = current else {
        return Response::Failed {
            message: "mihomo 在运行但服务没有记住配置，无法切直连（进程保持不动）".into(),
        };
    };
    if ctrl == 0 || token.is_empty() {
        return Response::Failed {
            message: "服务没有记住控制器端口/token，无法切直连（进程保持不动）".into(),
        };
    }
    let updated = match direct_config(&current) {
        Ok(value) => value,
        Err(message) => {
            return Response::Failed {
                message: format!("{message}（进程保持不动）"),
            }
        }
    };
    let path = paths::config_path();
    if let Err(e) = std::fs::write(&path, &updated) {
        return Response::Failed {
            message: format!("写入直连配置失败: {e}（进程保持不动）"),
        };
    }
    let body = serde_json::json!({
        "path": path.to_string_lossy(),
        "payload": updated,
        "force": true,
    });
    let addr = format!("127.0.0.1:{ctrl}");
    match crate::service::http::put_json(&addr, "/configs", &token, &body) {
        Ok(code) if (200..300).contains(&code) => {
            *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(updated);
            let was_tun = TUN_ENABLED.swap(false, Ordering::Relaxed);
            if was_tun {
                // 与关 TUN 同一条口径：热重载收下配置不等于系统侧已经拆干净。
                // 这里只记日志、仍然报 Detached——直连模式下残留的 TUN 也还能上网，
                // 把它升级成失败反而会诱使调用方去杀进程（那才是断网）。
                let problems = wait_tun_gone(
                    &crate::net::winprobe::adapter_names,
                    TUN_TEARDOWN_TIMEOUT,
                    TUN_TEARDOWN_INTERVAL,
                );
                if !problems.is_empty() {
                    eprintln!(
                        "[yunyi-service] 已切直连，但 TUN 网卡没撤干净: {}",
                        problems.join("；")
                    );
                }
            }
            Response::Detached
        }
        result => {
            // 与 reload 同样的顺序：PUT 失败就把磁盘上的旧配置写回去，
            // 否则下次热重载会从一份与实际运行状态不符的文件起算。
            let restore = std::fs::write(&path, &current);
            let detail = match result {
                Ok(code) => format!("mihomo 拒绝切直连: HTTP {code}"),
                Err(e) => format!("切直连通信失败: {e}"),
            };
            Response::Failed {
                message: match restore {
                    Ok(()) => format!("{detail}（进程保持不动）"),
                    Err(e) => format!("{detail}；恢复旧配置文件失败: {e}（进程保持不动）"),
                },
            }
        }
    }
}

fn start(config_yaml: &str, mixed_port: u16, ctrl_port: u16, token: &str) -> Response {
    // 先确认旧进程、端口与 TUN 已释放，再创建新会话；停止失败不能丢掉句柄继续启动。
    if let Response::Failed { message } = stop() {
        return Response::Failed { message };
    }
    let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());

    let assets = match assets_dir() {
        Ok(a) => a,
        Err(e) => return Response::Failed { message: e },
    };
    // 目录权限先加固再往里写东西。顺序不能反：`%ProgramData%` 的默认继承给
    // BUILTIN\Users 授了 Write，靠继承建出来的目录，非提权用户能抢建、也能换掉
    // 里面要执行的文件（详见 secure_dir 模块头）。父目录在子目录前。
    #[cfg(windows)]
    for dir in [paths::root(), paths::net_dir(), paths::bin_dir()] {
        if let Err(e) = super::secure_dir::ensure(&dir) {
            return Response::Failed { message: e };
        }
    }

    let net_dir = paths::net_dir();
    if let Err(e) = std::fs::create_dir_all(&net_dir) {
        return Response::Failed {
            message: format!("创建 {} 失败: {e}", net_dir.display()),
        };
    }
    // geo 先铺：配置里的 geodata-dir 指向这里，mihomo 一起来就要读
    let missing = provision_geo(&assets.join("geo"), &net_dir);
    if !missing.is_empty() {
        // 只记一行。SCM 下 stderr 不进事件日志，本地手工验证时可见
        eprintln!("[yunyi-service] geo 数据未就位: {}", missing.join("、"));
    }

    let cleaned = match sanitize(config_yaml, &net_dir) {
        Ok(c) => c,
        Err(e) => return Response::Failed { message: e },
    };
    if let Err(e) = std::fs::write(paths::config_path(), &cleaned) {
        return Response::Failed {
            message: format!("写入配置失败: {e}"),
        };
    }
    // 记账：SetTun 要拿这些打热重载。配置原文留一份，免得每次翻开关都重新问主程序要
    TUN_ENABLED.store(tun_in(&cleaned), Ordering::Relaxed);
    CTRL_PORT.store(ctrl_port, Ordering::Relaxed);
    *TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = token.to_string();
    *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(cleaned);

    let exe = match provision_mihomo(&assets.join("mihomo.exe"), &paths::bin_dir()) {
        Ok(p) => p,
        Err(e) => return Response::Failed { message: e },
    };
    // 必须传 -d：该 mihomo 版本不读 cwd 的 config.yaml（与 net/runtime.rs 同因）
    //
    // 必须加 CREATE_NO_WINDOW（走 util::quiet）：服务是 SCM 拉起的**无控制台**进程，
    // 它 spawn 一个 console 子进程时 Windows 会给子进程**新开一个控制台窗口**——
    // 表现就是每次「开启网络」都弹一个空的 cmd 黑框并一直挂着。主程序那条路
    // （net/runtime.rs 的降级 spawn）早就包了 quiet，只有服务这条路漏了。
    #[cfg(windows)]
    let child = Child::spawn(
        &exe,
        &net_dir,
        &[std::ffi::OsStr::new("-d"), net_dir.as_os_str()],
    );
    #[cfg(not(windows))]
    let child = crate::util::quiet(Command::new(&exe))
        .args(["-d", &net_dir.to_string_lossy()])
        .current_dir(&net_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(c) => {
            *guard = Some(c);
            MIXED_PORT.store(mixed_port, Ordering::Relaxed);
            Response::Started
        }
        Err(e) => Response::Failed {
            message: format!("启动 mihomo 失败: {e}"),
        },
    }
}

fn stop() -> Response {
    let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = guard.as_mut() {
        if let Err(e) = c.kill().and_then(|_| c.wait()) {
            return Response::Failed {
                message: format!("停止代理进程失败: {e}"),
            };
        }
    }
    guard.take();
    if TUN_ENABLED.load(Ordering::Relaxed) {
        let problems = wait_tun_gone(
            &crate::net::winprobe::adapter_names,
            TUN_TEARDOWN_TIMEOUT,
            TUN_TEARDOWN_INTERVAL,
        );
        if !problems.is_empty() {
            return Response::Failed {
                message: problems.join("；"),
            };
        }
    }
    let port = MIXED_PORT.swap(0, Ordering::Relaxed);
    // 与 status 的崩溃自愈同因：TUN 随 mihomo 一起没了，记账必须跟着归零，
    // 否则下次起来界面会显示一个并不存在的「TUN 已开启」
    TUN_ENABLED.store(false, Ordering::Relaxed);
    *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Response::Stopped {
        mixed_port: if port == 0 { None } else { Some(port) },
    }
}

fn status() -> Response {
    let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    let alive = match guard.as_mut() {
        Some(c) => matches!(c.try_wait(), Ok(None)),
        None => false,
    };
    if !alive {
        // 崩溃自愈：槽里留着死进程就清掉，否则 Status 会一直报「活着」
        let _ = guard.take();
        MIXED_PORT.store(0, Ordering::Relaxed);
        // TUN 也跟着没了——网卡是进程退出即销毁的临时网卡（spec 0c）。
        // 不清零的话界面会一直显示「TUN 已开启」而实际什么都没接管。
        TUN_ENABLED.store(false, Ordering::Relaxed);
        *LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    Response::Status {
        running: alive,
        tun: alive && TUN_ENABLED.load(Ordering::Relaxed),
    }
}
