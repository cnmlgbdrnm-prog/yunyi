//! 系统代理设置（WinINET HKCU）：开启前快照、关闭时还原。
//! 与 src/sysproxy.rs 的读取职责互补（那份只读，供更新下载跟随现有代理）；
//! 本模块只写不读——快照语义「开前存原值，关时原样写回」保证绝不残留。
//! 非 Windows 平台为 no-op（跨平台测试与打包一致不炸）。

#[cfg(windows)]
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
#[cfg(windows)]
use winreg::RegKey;

#[cfg(windows)]
const INTERNET_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// 快照：开启前注册表现值（还原用）。enable 0/1，server 可能为空串。
#[derive(Clone, Debug, PartialEq)]
pub struct ProxySnapshot {
    pub enable: u32,
    pub server: String,
    /// 开网络前的用户级环境变量原值（None = 原本就没设，还原时要删掉）。
    /// 与 WinINET 那两个值同生共死，一起快照、一起还原。
    pub env: Vec<(&'static str, Option<String>)>,
}

/// 所有 URL 共用一个 HTTP 代理；HTTPS / WSS 通过 CONNECT 把域名交给 mihomo。
///
/// 不写 `socks=host:port`：Chrome 会优先把 WebSocket 分给它，并按 SOCKS4 处理。
/// SOCKS4 在浏览器端解析，只传 IPv4，可能绕过 mihomo 的域名规则。TUN 的 fake-ip
/// 能恢复域名，但普通模式不应依赖它。裸 host:port 是 WinINET 的统一代理格式，
/// Chrome 将其作为 HTTP 代理用于 HTTP / HTTPS / WS / WSS。
/// https://chromium.googlesource.com/chromium/src/+/HEAD/net/docs/proxy.md
pub fn proxy_server_value(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// 标准代理环境变量名：**大小写各一套**——不同客户端读的名字不一样（reqwest / curl /
/// Go 都同时认两套），值相同故不冲突。
///
/// 为什么必须写它们：本模块原先只写 WinINET，而**只有读 WinINET 的程序**才看得见。
/// 实测（2026-09-19，本机）：`netsh winhttp show proxy` 是 Direct、Process/User/Machine
/// 三个作用域都没有任何代理环境变量。于是 Codex（Rust/reqwest 二进制，内含
/// `reqwest`×66 与 `HTTPS_PROXY`/`ALL_PROXY` 符号）这类不读 WinINET 的客户端
/// **完全没有进入代理的通道**，只能直连——而直连路上 `auth.openai.com` 的 IPv6 是死的
/// （`curl -4` → 403/1.5s，`curl -6` → 12s 超时，而本机 IPv6 本身是通的）。
/// 同一请求加不加变量的对照：无 → `peer=172.64.146.15`（直连），有 → `peer=127.0.0.1`。
///
/// 只写 `http://` 一种 scheme：mixed-port 一个端口同时讲 HTTP 与 SOCKS5，混着塞会让
/// 各客户端按各自的理解选路，故一律 http://。
pub const ENV_PROXY_KEYS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// NO_PROXY 的键名（同样大小写各一份）。
pub const ENV_NO_PROXY_KEYS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// 环回与私网直连。CIDR 不是所有客户端都认——认不出的只是这条不命中，退回经 mihomo，
/// 而 mihomo 有同义的 `GEOIP,LAN`/`GEOIP,PRIVATE`/`GEOIP,CN` 直连规则兜底，所以写了不亏。
pub const ENV_NO_PROXY_VALUE: &str =
    "localhost,127.0.0.1,::1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12";

/// 要写入**用户作用域**环境变量的 (键, 值)：6 个代理变量 + 2 个 NO_PROXY。
///
/// 纯函数，故可金样测试；真正的注册表写入在 [`write_user_env`]。
pub fn env_proxy_writes(port: u16) -> Vec<(&'static str, String)> {
    let url = format!("http://127.0.0.1:{port}");
    let mut out: Vec<(&'static str, String)> =
        ENV_PROXY_KEYS.iter().map(|k| (*k, url.clone())).collect();
    out.extend(
        ENV_NO_PROXY_KEYS
            .iter()
            .map(|k| (*k, ENV_NO_PROXY_VALUE.to_string())),
    );
    out
}

#[cfg(windows)]
fn open() -> Result<RegKey, String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(INTERNET_SETTINGS, KEY_READ | KEY_SET_VALUE)
        .map_err(|e| format!("打开系统代理设置失败: {e}"))
}

/// 用户级环境变量的注册表位置（`setx` 写的就是这里）。
#[cfg(windows)]
const ENVIRONMENT: &str = r"Environment";

/// 读本次会写到的全部变量在用户作用域的**原值**（None = 原本没设）。
///
/// 读注册表而不是 `std::env::var`：要还原的是「新进程会继承到的那个值」，而本进程的
/// 环境块是父进程给的快照，不代表用户级设置。
#[cfg(windows)]
fn env_snapshot() -> Result<Vec<(&'static str, Option<String>)>, String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(ENVIRONMENT, KEY_READ)
        .map_err(|e| format!("打开用户环境变量键失败: {e}"))?;
    Ok(ENV_PROXY_KEYS
        .iter()
        .chain(ENV_NO_PROXY_KEYS.iter())
        .map(|name| (*name, key.get_value::<String, _>(*name).ok()))
        .collect())
}

/// 读用户作用域当前的代理环境变量值（None = 没设）。自愈用：判断这批指针是不是
/// 指向本机死端口的残留。
#[cfg(windows)]
fn read_user_env_proxies() -> Result<Vec<(&'static str, Option<String>)>, String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(ENVIRONMENT, KEY_READ)
        .map_err(|e| format!("打开用户环境变量键失败: {e}"))?;
    Ok(ENV_PROXY_KEYS
        .iter()
        .map(|name| (*name, key.get_value::<String, _>(*name).ok()))
        .collect())
}

/// 写（或删）一个用户级环境变量。**不广播**——广播由调用方在整批写完后做一次。
///
/// 曾经每个变量各广播一次（8 个变量 = 8 次 `WM_SETTINGCHANGE`，每次最长等 1s）。
/// 两个坏处：一是慢（开/关网络的动作被拖长），二是 explorer 可能在半写完的那一刻
/// 就读走（看到「只有 HTTP_PROXY、还没有 ALL_PROXY」的中间态）。一批一次才是对的语义。
#[cfg(windows)]
fn write_user_env(name: &str, value: Option<&str>) -> Result<(), String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(ENVIRONMENT, KEY_READ | KEY_SET_VALUE)
        .map_err(|e| format!("打开用户环境变量键失败: {e}"))?;
    match value {
        Some(v) => key
            .set_value(name, &v)
            .map_err(|e| format!("写环境变量 {name} 失败: {e}"))?,
        None => match key.delete_value(name) {
            Ok(()) => {}
            // 本来就没有 → 删除是幂等的成功
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("删环境变量 {name} 失败: {e}")),
        },
    }
    Ok(())
}

/// 通知系统「环境变量变了」，让 explorer 重读并把新值带给之后启动的进程。
///
/// **不能省**：新进程的环境块是从父进程（通常是 explorer）继承的，而 explorer 只在收到
/// `WM_SETTINGCHANGE`（lparam = "Environment"）时才重读注册表。不广播的话，这次写入
/// 要等下次登录才生效——那就等于没修。
///
/// 失败不阻断：「开网络」不该因为一次通知送不到就整体失败；最坏是生效推迟到下次登录。
#[cfg(windows)]
fn broadcast_environment_change() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };
    let param: Vec<u16> = "Environment\0".encode_utf16().collect();
    // SAFETY: 纯通知，无返回值语义；param 是 NUL 结尾的宽字符串且活到调用结束
    unsafe {
        let mut result: usize = 0;
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            param.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            &mut result,
        );
    }
}

/// 只读：系统代理当前是否已启用并配置服务器（其它软件真正接管流量的信号）。
/// ProxyEnable=1 且 ProxyServer 非空 → Some(server 值)，否则 None。
#[cfg(windows)]
pub fn live_system_proxy() -> Result<Option<String>, String> {
    let key = open()?;
    let enable: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    let server: String = key.get_value("ProxyServer").unwrap_or_default();
    if enable != 0 && !server.trim().is_empty() {
        Ok(Some(server))
    } else {
        Ok(None)
    }
}

/// 非 Windows：无系统代理语义。
#[cfg(not(windows))]
pub fn live_system_proxy() -> Result<Option<String>, String> {
    Ok(None)
}

/// 开启：快照当前值 → 写 ProxyEnable=1 + ProxyServer=127.0.0.1:port（统一 HTTP 代理）。
#[cfg(windows)]
pub fn apply_net_proxy(port: u16) -> Result<ProxySnapshot, String> {
    let key = open()?;
    let enable: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    let server: String = key.get_value("ProxyServer").unwrap_or_default();
    // 先快照再写：这样即使后面某一步失败，快照里已经是原值，restore 能原样还原
    let env = env_snapshot()?;
    let snap = ProxySnapshot {
        enable,
        server,
        env,
    };
    key.set_value("ProxyServer", &proxy_server_value(port))
        .map_err(|e| format!("写 ProxyServer 失败: {e}"))?;
    key.set_value("ProxyEnable", &1_u32)
        .map_err(|e| format!("写 ProxyEnable 失败: {e}"))?;
    // WinINET 与环境变量是**两套互不相干的机制**，缺一套就有一整类客户端收不到：
    // WinINET 只覆盖读它的程序（浏览器），环境变量覆盖 reqwest/Go/Node/Python 那类。
    for (name, value) in env_proxy_writes(port) {
        write_user_env(name, Some(&value))?;
    }
    // 整批写完再广播一次（不是每个变量一次：见 write_user_env 的说明）
    broadcast_environment_change();
    Ok(snap)
}

/// 关闭：还原快照；无快照（异常路径）安全置 0/清空，绝不残留指向死端口的代理。
#[cfg(windows)]
pub fn restore_proxy(snap: Option<&ProxySnapshot>) -> Result<(), String> {
    let key = open()?;
    match snap {
        Some(s) => {
            key.set_value("ProxyEnable", &s.enable)
                .map_err(|e| format!("还原 ProxyEnable 失败: {e}"))?;
            key.set_value("ProxyServer", &s.server)
                .map_err(|e| format!("还原 ProxyServer 失败: {e}"))?;
            for (name, value) in &s.env {
                write_user_env(name, value.as_deref())?;
            }
        }
        None => {
            let _ = key.set_value("ProxyEnable", &0_u32);
            let _ = key.set_value("ProxyServer", &"");
            // 异常路径（强杀后自愈、快照缺失）：没有原值可依，只能把本次可能写过的
            // 键一律清掉。宁可清掉用户手工设的值，也不留一个指向死端口的代理指针
            // ——与上面 WinINET 分支同样的取舍。
            for name in ENV_PROXY_KEYS.iter().chain(ENV_NO_PROXY_KEYS.iter()) {
                let _ = write_user_env(name, None);
            }
        }
    }
    // 与 apply 对称：整批改完广播一次
    broadcast_environment_change();
    Ok(())
}

#[cfg(not(windows))]
pub fn apply_net_proxy(_port: u16) -> Result<ProxySnapshot, String> {
    Ok(ProxySnapshot {
        enable: 0,
        server: String::new(),
        env: Vec::new(),
    })
}

#[cfg(not(windows))]
pub fn restore_proxy(_snap: Option<&ProxySnapshot>) -> Result<(), String> {
    Ok(())
}

/// 解析 ProxyServer 值为 (host, port) 端点列表。支持两种写法：
/// - 分段式：`http=127.0.0.1:51248;https=127.0.0.1:51248;socks=127.0.0.1:51248`
/// - 裸式：`127.0.0.1:51248`
///   解析失败（无端口）的段忽略；非分段写法整体作裸式尝试。
pub fn parse_proxy_endpoints(server: &str) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    for seg in server
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let pair = match seg.split_once('=') {
            Some((_scheme, host_port)) => host_port,
            None => seg,
        };
        if let Some((host, port)) = pair.rsplit_once(':') {
            if let Ok(port) = port.parse::<u16>() {
                out.push((host.to_string(), port));
            }
        }
    }
    out
}

/// 判定「指向本机死端口的自残留代理」：所有端点都在 loopback 上且**无任何本地监听**
/// （强杀/崩溃后应用程序没法还原的僵尸指针）。任一端点非 loopback 或存在监听
/// （别的客户端真接管）→ false，不得清理。listener 由调用方注入（测试用假实现）。
pub fn dead_self_proxy(server: &str, listener: &dyn Fn(u16) -> bool) -> bool {
    let endpoints = parse_proxy_endpoints(server);
    if endpoints.is_empty() {
        return false;
    }
    for (host, port) in &endpoints {
        let loopback =
            host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost");
        if !loopback || listener(*port) {
            return false;
        }
    }
    true
}

/// 解析**环境变量式**的代理值（`scheme://host:port`，也容忍裸 `host:port`）成
/// (host, port)。认不出返回 None。
///
/// 与 [`parse_proxy_endpoints`] 是两套语法，不能互相顶替：注册表那边是
/// `http=127.0.0.1:7890;https=...` 的分段写法，环境变量这边是 URL。
pub fn parse_env_proxy_url(value: &str) -> Option<(String, u16)> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    // 去 scheme：http:// / socks5:// ……（裸 host:port 没有 scheme，原样往下走）
    let rest = match v.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => v,
    };
    let (host, port) = rest.rsplit_once(':')?;
    // IPv6 字面量写作 [::1]:7890，方括号不是主机名的一部分
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse::<u16>().ok()?))
}

/// 判定「用户级环境变量里的代理指针全都指向本机死端口」——强杀/崩溃后本应用留下的
/// 僵尸指针，和 [`dead_self_proxy`] 是同一件事，只是值长在环境变量里。
///
/// 判定保守到苛刻：**只要有一个变量指向非 loopback、端口有人听、或压根认不出来**，
/// 就返回 false 不动手。宁可留一次残留（下次开网络会被正确值覆盖掉），也绝不能误删
/// 用户自己配的公司代理——那是删了就找不回来的设置。
/// 全都没设同样返回 false：没有残留可清。
pub fn dead_self_env_proxy(
    values: &[(&str, Option<String>)],
    listener: &dyn Fn(u16) -> bool,
) -> bool {
    let mut checked = 0usize;
    for name in ENV_PROXY_KEYS {
        let Some(value) = values
            .iter()
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| v.as_deref())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some((host, port)) = parse_env_proxy_url(value) else {
            return false; // 认不出 → 说不清是不是我们的残留，不动
        };
        let loopback =
            host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost");
        if !loopback || listener(port) {
            return false;
        }
        checked += 1;
    }
    checked > 0
}

#[cfg(windows)]
fn local_listener_exists(port: u16) -> bool {
    // Go 系代理（SO_REUSEADDR，Windows 假空闲）吃 bind 探测——必须查真实监听表。
    // 走进程内 GetExtendedTcpTable：旧的 PowerShell `Get-NetTCPConnection -State Listen`
    // 冷启动实测 3510ms（NetTCPIP 模块首次初始化），而本函数位于「开启网络」的
    // 系统代理自愈分支上，崩溃残留后再点开启必被它拖住数秒。
    crate::net::winprobe::port_is_listening(port)
}

/// 自愈入口（start 前调用）：系统代理指向本机死端口 → 清除；
/// 活监听/非 loopback → 不动（后者是其它客户端的真接管，交给冲突闸门拦停）。
/// 清除失败不阻断（读取失败才报错；写失败容忍为「下次再清」）。
#[cfg(windows)]
pub fn heal_dead_system_proxy() -> Result<(), String> {
    // 两套机制各查各的，**不能因为一套干净就跳过另一套**：强杀可能只写完一套就断了。
    // （曾经这里是 `let Some(server) = live_system_proxy()? else { return Ok(()) }`，
    // 于是 WinINET 干净、环境变量留着死指针时，自愈直接放行——那正是本函数要消灭的
    // 「只看一处就下结论」。）
    if let Some(server) = live_system_proxy()? {
        if dead_self_proxy(&server, &local_listener_exists) {
            // 快照都不用留：残留本身即无效态，直清零（等价 restore_proxy(None)）。
            // 该分支会**连环境变量一起清**，所以下面那次 env 检查多半就是空跑。
            restore_proxy(None)?;
        }
    }
    if dead_self_env_proxy(&read_user_env_proxies()?, &local_listener_exists) {
        for name in ENV_PROXY_KEYS.iter().chain(ENV_NO_PROXY_KEYS.iter()) {
            let _ = write_user_env(name, None);
        }
        broadcast_environment_change();
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn heal_dead_system_proxy() -> Result<(), String> {
    Ok(())
}
