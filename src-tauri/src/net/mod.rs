//! 本机上网客户端：独立 mihomo 实例（完整 config）+ 系统代理 + 智能分流。
//! 仪表盘一键开关驱动；与测速 sidecar（proxies 动态注入）互不干扰。
pub mod conflict;
pub mod connlog;
pub mod dnscache;
pub mod firewall;
pub mod geodata;
pub mod local_proxy;
pub mod mmdb;
pub mod orphan;
pub mod private_gate;
pub mod runtime;
pub mod runtime_conns;
pub mod sysproxy_set;
pub mod takeover;
pub mod winprobe;

/// 开发机上本应用正开着网络时，系统代理端口被它占着——任何「真开网络」的用例在
/// 那一刻都**必然失败**，因为修掉端口顺延之后 `start` 撞上占用就如实报错（设计如此）。
/// 这类用例据此跳过，返回 `true` 表示「跳过」。CI 上没人占着 7890，照跑。
///
/// 旧实现在这种情况下会悄悄顺延到 OS 临时端口，于是「开着应用也能跑测试」——代价是
/// 整机代理指针漂到一个随机端口（见 CHANGELOG 2026-09-25）。
#[cfg(all(test, feature = "local-tests"))]
pub(crate) fn proxy_port_busy_skip(reason: &str) -> bool {
    let busy = winprobe::port_is_listening(runtime::NET_DEFAULT_PORT);
    if busy {
        eprintln!(
            "跳过 {reason}：端口 {} 已被占用（开发机上本应用正开着网络）",
            runtime::NET_DEFAULT_PORT
        );
    }
    busy
}

/// 提权服务的安装与运行状态（IPC 线格式：设置页消费）。
///
/// 两个布尔而不是一个枚举：设置页要按四象限给不同入口——都没装给「安装」，
/// 装了没起给「启动/卸载」，装了在跑给「卸载」。压成一个状态反而要在前端重新拆。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct ServiceState {
    pub installed: bool,
    pub running: bool,
}

impl ServiceState {
    /// 查服务状态。非 Windows 上恒为「未安装」——TUN 那条路本就不存在。
    pub fn query() -> Self {
        #[cfg(windows)]
        {
            let (installed, running) = crate::service::install::query_state();
            Self { installed, running }
        }
        #[cfg(not(windows))]
        {
            Self {
                installed: false,
                running: false,
            }
        }
    }
}

/// 上网状态（IPC 线格式：仪表盘网络卡消费）。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct NetStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub conflicts: Vec<String>,
    pub degraded_geo: bool,
    /// mihomo 是否由提权服务托管（false = 主程序直接 spawn 的回退路径）。
    /// 前端据此判断 TUN 开关可不可用——服务没起就没有提权通道，开不了 TUN。
    pub via_service: bool,
    /// TUN 是否开着。**答案来自服务**，主程序不自己记账——服务是唯一知道 mihomo
    /// 实际配置的一方，两处各记一份迟早漂移，而 TUN 状态错了意味着用户以为全局
    /// 接管了、实际没有。
    pub tun: bool,
    /// 会话级阻断（防火墙）的状态，取值见 [`firewall::FirewallState::as_str`]：
    /// `off` / `applied` / `partial` / `denied` / `unavailable` / `unsupported`。
    ///
    /// **这是增益功能的状态，不是网络能不能用的状态**：任何非 `applied` 的取值都
    /// 不表示「开启网络」失败了，它只说明「非 TUN 下推不到的那部分流量没被挡住」。
    pub firewall: String,
    /// 为什么不是 `applied`——给用户看的原话（界面说明直接展示）。
    pub firewall_detail: Option<String>,
    /// 这次「开启网络」接管了几条**开启前就已建立**的直连（见 [`takeover`]）。
    ///
    /// `None` = 这次没做（原因见 [`NetStatus::takeover_detail`]）；`Some(0)` = 做了，
    /// 但没有需要接管的（常态）。与 `firewall` 同一只脚站立：**增益功能的状态，
    /// 不是网络能不能用的状态**——任何取值都不表示「开启网络」失败了。
    pub takeover_killed: Option<usize>,
    /// 为什么没接管/没接管完——给用户看的原话。做成了且一条没漏就不写。
    pub takeover_detail: Option<String>,
    /// 本机 Tor（隐私线入口）的状态，取值见 [`crate::tor::TorState::as_str`]：
    /// `off` / `starting` / `ready` / `failed`。与 `firewall` 同一只脚站立：
    /// **增益功能的状态，不是网络能不能用的状态**。
    ///
    /// **与 `firewall` 的唯一区别**：它会变（bootstrap 有进度），所以由后台任务
    /// [`crate::tor::spawn_bootstrap`] 回写，而不是开启那一刻记一次就完。
    /// 非 `ready` 只表示「隐私线连不上」，普通线照常可用。
    pub tor: String,
    /// 为什么不是 `ready`——给用户看的原话（`starting` 时是「已建立链路 45%」，
    /// `failed` 时是 Tor 日志末行）。例外：`ready` 也可能带 detail
    /// （「使用本机 9050 上已有的 Tor」）——复用了谁的 Tor 是用户该知道的事。
    pub tor_detail: Option<String>,
}

/// 「清除残留阻断规则」的结果（设置页逃生门）。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct FirewallClearResult {
    /// 回读核验后**实际消失**的规则条数（0 = 本来就没有残留）
    pub removed: usize,
}

/// 启动选项：ignore_conflicts 跳过扫描（一次性确认/测试）；
/// conflict_override 驱动冲突列表（生产 None = 真实扫描）。
#[derive(Debug, Clone)]
pub struct StartOptions {
    pub ignore_conflicts: bool,
    pub conflict_override: Option<Vec<String>>,
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/net/mods.rs"
));
