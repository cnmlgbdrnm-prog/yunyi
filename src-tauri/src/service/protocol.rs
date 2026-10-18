//! 主程序 ↔ 服务 的 IPC 载荷。
//!
//! 线格式是 JSON，用 `tag` 做内嵌判别式（`{"cmd":"start_net",…}` /
//! `{"kind":"started"}`），便于人肉排查管道流量。
//!
//! 输入输出：本模块全是纯函数，不碰管道、不碰 SCM，故可跨平台测试。
//! 依赖：serde / serde_json。

use serde::{Deserialize, Serialize};

/// 服务不认识某条命令时的固定前缀（[`decode`] 用）。
///
/// 主程序据此把「**服务太旧**」与「服务坏了」分开：本机装着的服务完全可能是旧构建，
/// 它不认识后来加的命令，而那必须降级（`unavailable`）而不是让「开启网络」整体失败。
/// 用常量而不是各写各的字面量，免得改了措辞就悄悄失配。
pub const ERR_UNKNOWN_COMMAND: &str = "未知命令";

/// 服务命令白名单：网络、会话恢复及会话阻断；会话归属来自管道核验。
///
/// 阻断命令是对这条原则的一次让步：会话级防火墙阻断只有提权进程
/// 做得成（改防火墙要管理员令牌），而主程序全程只有装服务那一次 UAC。
/// 阻断请求只接受**程序绝对路径**，不认配置、不认脚本，
/// 且规则名带固定 UUID 前缀（见 `net::firewall`），服务能做的仍然只是「把这几条
/// 规则装上/撤掉」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// 起 mihomo：配置原文 + 端口 + 控制器 token（服务只清洗与落盘，不改语义）
    StartNet {
        config_yaml: String,
        mixed_port: u16,
        ctrl_port: u16,
        token: String,
    },
    /// 保留现有 mihomo 进程和端口，服务清洗配置后经控制器热重载。
    ReloadNet { config_yaml: String },
    /// 杀 mihomo（幂等：未运行也算成功）
    StopNet,
    /// 开/关 TUN：改配置的 tun 块，再 PUT /configs 热重载（dns 块两种模式都在，不动）
    SetTun { enable: bool },
    /// 查询服务与 mihomo 的存活状态
    Status,
    /// 新界面开启前先核验并回收失主会话；所属 PID 来自命名管道，不接受载荷伪造。
    RecoverNet,
    /// 仅会话所属界面可以请求服务退出；防止另一个空窗口退出时停掉正在使用的服务。
    Shutdown,
    /// 旧版阻断命令，保留给旧主程序；新版主程序只发 `BlockPublicDirect`，以便
    /// 旧网络服务明确返回「未知命令」，避免把旧版 QUIC/v6 规则误报为公网 v4/v6 已保护。
    BlockDirect { programs: Vec<String>, tag: String },
    /// 装会话级公网 IPv4 / IPv6 出站阻断规则。`programs` 是主程序解析好的
    /// **绝对路径**——镜像名 → 路径要枚举本机进程表，那是主程序已有的能力。
    /// `tag` 只进规则名，用于分辨是哪一次会话装的。
    BlockPublicDirect { programs: Vec<String>, tag: String },
    /// 撤掉全部自家阻断规则（幂等：没装过也算成功）
    UnblockDirect,
    /// 查当前在位的自家规则条数（**回读**，不是服务记账）
    BlockStatus,
    /// 掐断一批「代理生效前就已建立」的直连（`net::takeover`）。
    ///
    /// `rows` 由主程序算好——判据（哪条该掐）在主程序那边，服务只做**复查后执行**。
    /// `SetTcpEntry` 要管理员令牌，主程序全程只有装服务那一次 UAC，所以只能在服务里
    /// 做；与 `BlockDirect` 是同一条让步理由（见本模块头的白名单论证）。
    ResetStale { rows: Vec<StaleConn> },
}

/// 一条待掐的连接：IPv4 四元组 + 期望的属主 pid。
///
/// 地址用**点分字符串**（线格式人肉可读是本协议一贯的取舍，见模块头），服务侧解析成
/// `Ipv4Addr`——**解析失败即拒**：`SetTcpEntry` 只有 IPv4，没有 `SetTcp6Entry`，v6
/// 在这儿就被挡在提权边界外。
///
/// `pid` 是复查用的闸而不是装饰：快照到下手之间隔着启动流程（实测最长十几秒），
/// 四元组可能已经被复用——属主对不上就不下手（见 `net::takeover::recheck`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleConn {
    pub local_addr: String,
    pub local_port: u16,
    pub remote_addr: String,
    pub remote_port: u16,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Started,
    Reloaded,
    Recovered,
    ShuttingDown,
    /// mixed_port 回显服务刚才用的端口，供主程序核对
    Stopped {
        mixed_port: Option<u16>,
    },
    /// 服务与 mihomo 的存活状态（`Status` 的响应）
    ///
    /// `tun` 由服务回答而非主程序自己记账：服务是唯一知道 mihomo 实际配置的一方，
    /// 两处各记一份迟早漂移，而 TUN 状态错了意味着用户以为全局接管了、实际没有。
    Status {
        running: bool,
        tun: bool,
    },
    /// `SetTun` 的响应：回显生效后的状态，供主程序核对而不是假定成功
    TunSet {
        enabled: bool,
    },
    /// `BlockPublicDirect` / `BlockDirect` 的响应：装完**回读核验**后仍在位的自家规则条数。
    /// 回读而不是回显请求条数——不假定成功，与「改完 TUN 要核验网卡真的拆了」同因。
    Blocked {
        rules: usize,
    },
    /// 防火墙拒写（`E_ACCESSDENIED`：组策略禁本地规则、或安全软件锁了防火墙）。
    ///
    /// **这是一条降级信号而不是错误**：主程序据此给界面说明，而「开启网络」照常
    /// 成功。单开一个变体是为了让主程序不必去比对错误字符串里的措辞。
    BlockDenied {
        message: String,
    },
    /// `UnblockDirect` 的响应：回读核验后**实际消失**的规则条数（幂等，没装过为 0）
    Unblocked {
        removed: usize,
    },
    /// `BlockStatus` 的响应：当前在位的自家规则条数（回读）
    BlockStatus {
        rules: usize,
    },
    /// `ResetStale` 的响应：真掐断的条数与**没掐成的**条数（连接已经自己关了、
    /// 属主被复用对不上、内核拒绝等）。
    ///
    /// 是两个数而不是一个「成功/失败」：这件事本来就是「尽力而为，如实回报」——
    /// 掐失败不该让「开启网络」失败，但也不能假装掐成了（界面上的计数要真）。
    StaleReset {
        killed: usize,
        skipped: usize,
    },
    Failed {
        message: String,
    },
}

pub fn encode(req: &Request) -> String {
    serde_json::to_string(req).expect("Request 序列化不会失败")
}

pub fn decode(raw: &str) -> Result<Request, String> {
    // serde 的错误对未知 tag 报的是「unknown variant `rm_rf`」，直接透出即可读；
    // 但补一句中文前缀，免得日志里突然冒英文。
    //
    // 前缀是常量：主程序靠它把「**服务是旧构建、不认识这条新命令**」认出来并降级
    // （见 `service::client::is_unknown_command`）——那种情况必须让「开启网络」照常
    // 成功，而不是因为一个增益功能不可用就整体失败。
    serde_json::from_str(raw).map_err(|e| format!("{ERR_UNKNOWN_COMMAND}或载荷格式错误: {e}"))
}

pub fn encode_response(resp: &Response) -> String {
    serde_json::to_string(resp).expect("Response 序列化不会失败")
}

pub fn decode_response(raw: &str) -> Result<Response, String> {
    serde_json::from_str(raw).map_err(|e| format!("响应格式错误: {e}"))
}
