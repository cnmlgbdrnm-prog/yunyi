//! 系统代理探测：更新检查/下载自动跟随 Windows 系统代理（WinINET 注册表）。
//! 背景：国内网络直连 github.com 常超时（实测 24s 无响应），本机 Clash 等代理
//! 可通（实测 1.6s）；updater 插件的 HTTP 默认不读系统代理，需要显式传入。
//! parse_proxy_server 为纯函数承载全部归一化规则；注册表读取是薄层。

/// 归一化 WinINET ProxyServer 字符串为 updater 可用的 http 代理 URL。
/// 规则（WinINET 两种形态 + 容错）：
/// - `127.0.0.1:7890` → 补 `http://` 前缀
/// - `http://127.0.0.1:7890` → 原样
/// - `https=…;http=…` 多协议列表 → 优先取 `http=` 段（大小写不敏感）
/// - `socks=`、`ftp=` 等非 http 段不能做 updater 的 http 代理 → 跳过
/// - 空串/全空白/无可解析段 → None
pub fn parse_proxy_server(raw: &str) -> Option<String> {
    let segs: Vec<&str> = raw
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segs.is_empty() {
        return None;
    }
    let segment = segs
        .iter()
        .find_map(|s| {
            // get(..5) 而非 s[..5]：段首含多字节字符时字节 5 不在字符边界，
            // 直接切片会 panic；注册表值是可被任意本机软件写入的外部输入
            if s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("http=")) {
                Some(&s[5..])
            } else {
                None
            }
        })
        .or_else(|| segs.iter().copied().find(|s| !s.contains('=')))?;
    let segment = segment.trim();
    if segment.is_empty() {
        return None;
    }
    if segment.contains("://") {
        Some(segment.to_string())
    } else {
        Some(format!("http://{segment}"))
    }
}

/// 读取系统代理 URL。Windows：WinINET 连接级 API（与写入和浏览器采用同一口径），
/// 未启用/读取失败/无可解析段返回 None；非 Windows 平台返回 None。
#[cfg(windows)]
pub fn system_proxy_url() -> Option<String> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let settings = crate::net::sysproxy_windows::read().ok()?;
    let native = parse_proxy_server(settings.manual_server()?)?;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    if key.get_value::<u32, _>("ProxyEnable").ok()? == 0 {
        return None;
    }
    let registry = parse_proxy_server(&key.get_value::<String, _>("ProxyServer").ok()?)?;
    (native == registry).then_some(native)
}

#[cfg(not(windows))]
pub fn system_proxy_url() -> Option<String> {
    None
}

/// IPC 薄层：读取系统代理 URL（更新检查/下载自动跟随系统代理用）。
/// 无代理/未启用/非 Windows 时返回 null；探测型只读命令，永不失败。
#[tauri::command]
pub fn get_system_proxy_cmd() -> Option<String> {
    system_proxy_url()
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/sysproxy/mods.rs"
));
