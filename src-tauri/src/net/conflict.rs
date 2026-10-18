//! 冲突检测：**实际接管信号**优先——系统代理已启用（WinINET ProxyEnable=1）或代理客户端
//! TUN 网卡存在才算冲突（进程挂着但不接管流量时不拦，用户实测 Clash Verge 后台挂起
//! 的状态不应阻止本应用起代理）。进程名扫描仅用于错误消息里提示"可能是谁"。
//! 自身进程（测速 sidecar/本客户端 mihomo）按 PID 排除；只检测只提醒，绝不接管/关闭
//! 其它代理软件（设计约束「提醒不抢占」）。

/// tasklist /FO CSV /NH 输出解析：每行 `"image.exe","pid","...`，
/// 只取前两列；无法解析的行（表头/空行）跳过。
pub fn parse_tasklist_csv(csv: &str) -> Vec<(String, u32)> {
    csv.lines()
        .filter_map(|line| {
            let mut f = line.split(',');
            let name = f.next()?.trim_matches('"').to_string();
            let pid: u32 = f.next()?.trim_matches('"').parse().ok()?;
            Some((name, pid))
        })
        .collect()
}

/// 已知代理客户端镜像名（小写后缀匹配——Windows 上同款镜像 + "核心/替换版" 后缀常见）。
/// 未命中 → 不是冲突（系统进程/浏览器/开发工具一律忽略，只提醒真正的代理软件）。
fn is_proxy_image(image: &str) -> bool {
    let lower = image.to_ascii_lowercase();
    [
        "clash-verge.exe",
        "clash-verge-rev.exe",
        "clash.exe",
        "clash-meta.exe",
        "fclash.exe",
        "mihomo.exe",
        "verge-mihomo.exe",
        "v2ray.exe",
        "v2rayn.exe",
        "wingy.exe",
        "hiddify.exe",
        "hiddify-cli.exe",
        "karing.exe",
        "sing-box.exe",
        "trojan.exe",
    ]
    .contains(&lower.as_str())
}

/// 进程镜像名 → 用户可读中文名（代理内核类给中文标注；未知原名返回——调用方已在
/// is_proxy_image 过滤，只可能落在扩展已知名的兜底显示）。
pub fn friendly_name(image: &str) -> String {
    match image.to_ascii_lowercase().as_str() {
        "clash-verge.exe" | "clash-verge-rev.exe" => "Clash Verge".into(),
        "verge-mihomo.exe" => "mihomo(其它实例)".into(),
        "clash.exe" | "clash-meta.exe" | "fclash.exe" => "Clash 内核".into(),
        "v2ray.exe" => "v2ray".into(),
        "v2rayn.exe" => "v2rayN".into(),
        "wingy.exe" => "Wingy".into(),
        "hiddify.exe" | "hiddify-cli.exe" => "Hiddify".into(),
        "karing.exe" => "Karing".into(),
        "sing-box.exe" => "sing-box".into(),
        "trojan.exe" => "trojan".into(),
        "mihomo.exe" => "mihomo(其它实例)".into(),
        _ => image.to_string(),
    }
}

/// 只保留已知代理客户端、过滤排除 PID 后按出现顺序去重（先出现者优先，不重复报）。
pub fn conflicts_from(rows: &[(String, u32)], exclude_pids: &[u32]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    rows.iter()
        .filter(|(_, pid)| !exclude_pids.contains(pid))
        .filter(|(name, _)| is_proxy_image(name))
        .map(|(name, _)| friendly_name(name))
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

/// 扫描当前机器上正在运行的冲突代理**进程**（仅作消息提示，不拦启动）；
/// 非 Windows 平台返回空（无 tasklist 语义）。
pub fn scan_conflicts(exclude_pids: &[u32]) -> Result<Vec<String>, String> {
    #[cfg(windows)]
    {
        let out = crate::util::quiet(std::process::Command::new("tasklist"))
            .args(["/FO", "CSV", "/NH"])
            .output()
            .map_err(|e| format!("扫描进程失败: {e}"))?;
        let csv = String::from_utf8_lossy(&out.stdout);
        Ok(conflicts_from(&parse_tasklist_csv(&csv), exclude_pids))
    }
    #[cfg(not(windows))]
    {
        let _ = exclude_pids;
        Ok(Vec::new())
    }
}

/// 代理客户端 TUN 网卡名特征（小写）：wintun 虚拟网卡带这些名字即拦截全部流量。
///
/// 末项是**自家**网卡名（[`crate::config::TUN_DEVICE`]，同一常量，改名不会漂移）。
/// 为什么自家也要算：这份表原先只有别家的名字，于是「配置里已经没有 tun 块、
/// 而自家网卡仍在且占着默认路由」这种半开状态既不被算作接管信号、也没人核验——
/// 实测（2026-09-19）撞到过。认出自己至少能让残留被闸门看见。
pub const TUN_PATTERNS: [&str; 9] = [
    "clash",
    "mihomo",
    "v2ray",
    "sing-box",
    "hiddify",
    "karing",
    "wingy",
    "tun2socks",
    crate::config::TUN_DEVICE,
];

/// 网卡名 → 接管信号文案：命中 [`TUN_PATTERNS`] 任意特征即计入（保留原顺序、
/// 不额外去重——与旧 PowerShell 实现 `Where-Object { $_.Name -match ... }` 同语义）。
/// 匹配为**大小写不敏感子串**：原实现用 `-match`（正则、忽略大小写），而
/// TUN_PATTERNS 全是无正则元字符的字面量，故子串匹配等价且免引正则依赖。
pub fn tun_signals_from(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| {
            let lower = n.to_ascii_lowercase();
            TUN_PATTERNS.iter().any(|p| lower.contains(p))
        })
        .map(|n| format!("代理 TUN 网卡（{n}）"))
        .collect()
}

/// 实际接管信号：`Some(描述列表)` 表示已有其它代理接管系统流量（非空则拦启动）；
/// 空 = 无接管（后台挂着进程不接管的场景放行）。信号：
/// 1) WinINET 系统代理已启用（ProxyEnable=1 且有服务器值）；
/// 2) 代理客户端 TUN 网卡存在（拦截全部流量，与系统代理无关）。只认**仍在**的网卡：
///    设备已删、只剩注册表残留的幻影条目不算（见 `winprobe::live_adapter_names`），
///    否则会拦停一个用户根本没有对象可关闭的「冲突」。
#[cfg(windows)]
pub fn takeover_signals() -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if let Some(server) = crate::net::sysproxy_set::live_system_proxy()? {
        out.push(format!("系统代理已启用（{server}）"));
    }
    // 进程内枚举网卡（GetAdaptersAddresses）：旧实现 spawn PowerShell 跑
    // Get-NetAdapter -IncludeHidden，实测 4521ms；其中 3.3s 是 NetAdapter
    // 模块首次初始化（同进程第二次仅 93ms），每次点击都新起进程故必然吃满。
    let names = crate::net::winprobe::adapter_names()?;
    out.extend(tun_signals_from(&names));
    Ok(out)
}

/// 非 Windows：无 WinINET/TUN 语义。
#[cfg(not(windows))]
pub fn takeover_signals() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}
