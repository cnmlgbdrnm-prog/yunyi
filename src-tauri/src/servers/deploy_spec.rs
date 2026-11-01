//! 部署规格：协议白名单、安装与配置生成命令，以及远端既有配置的解析。
//!
//! 从 `servers::deploy` 拆出以控制单文件规模（≤1500 行）。本模块保持**纯函数**：
//! 不持有 SSH 会话、不做远端 I/O——执行编排仍在 `deploy.rs`。`deploy.rs` 以
//! `pub use crate::servers::deploy_spec::*;` 重导出，原 `crate::servers::deploy::X`
//! 路径保持不变。

/// Xray 官方安装脚本的固定 commit（仓库 `XTLS/Xray-install`）。
///
/// **该仓库没有任何 tag**（2026-09-27 实测 `GET /repos/XTLS/Xray-install/tags` → `[]`），
/// 所以「固定版本」只能落到 commit SHA 上。原先用的是 `raw/main/…`：`main` 是可变引用，
/// 上游（或任何能推 `main` 的人）随时能换掉我们在 root 下执行的脚本内容——这是供应链
/// 攻击最省事的一条路，而本模块**就是**在别人的服务器上以 root 执行它。
pub const XRAY_INSTALL_SHA: &str = "e741a4f56d368afbb9e5be3361b40c4552d3710d";
/// 上面那个 commit 上 `install-release.sh` 的 sha256（2026-09-27 实测：
/// `curl -sL <固定 commit 的 raw 地址> | sha256sum`）。
///
/// 与 [`XRAY_INSTALL_SHA`] **必须成对更新**：换 commit 时重新算一遍这个值。
/// 两层钉法各管一件事——commit 钉住「上游改了 main」，哈希钉住「上游改写了这个
/// commit」（force-push 只有哈希拦得住）。
///
/// 注意钉的是**安装脚本**，不是脚本再去下载的 Xray 二进制：那一步仍只有 TLS 保护。
pub const XRAY_INSTALL_SCRIPT_SHA256: &str =
    "7f70c95f6b418da8b4f4883343d602964915e28748993870fd554383afdbe555";

/// 部署钉住的 Xray 版本。
///
/// **不要用 latest**：2026-10-14 Xray 把 REALITY 依赖切到 Go 1.27 同步后的实现，
/// 26.10.21 与内置的 mihomo 1.19.10 握手不兼容——同一份节点参数（公钥/shortId/
/// SNI 全对），Xray 26.10.21 客户端能过，mihomo 报
/// `REALITY: authentication failed`；把服务端换成 26.3.27 后同一 mihomo 节点
/// 602ms 通过（2026-10-28 圣克拉拉真机，复制配置在备用端口复现）。
/// 这就是「新部署的服务器 REALITY 全 503、老服务器正常」的分界。
///
/// 长期应升级内置 mihomo；在那之前 Xray 只能跟着客户端的兼容面走。
pub const XRAY_PIN_VERSION: &str = "v26.3.27";

/// 探测远端是否已是钉住的 Xray 版本。
///
/// 不能用 `command -v xray`：SC 上已经装了 26.10.21，重部署时会被当成「已安装」
/// 而跳过安装，版本永远不会回到兼容面。这里比对 `xray version` 第一行；只要不是
/// 26.3.27（没装、版本漂移、输出异常）就返回非零，让部署重新执行安装命令。
pub fn xray_installed_probe_cmd() -> String {
    format!(
        "xray version 2>/dev/null | head -n1 | grep -Fq 'Xray {} ('",
        XRAY_PIN_VERSION.trim_start_matches('v')
    )
}

/// Xray 安装命令：取固定 commit 的脚本 → **核对内容哈希** → 执行。
///
/// 三处刻意的形状：
/// - `curl -fL`：不带 `-f` 时 404 页面会被当成脚本内容；而 `bash -c "$(curl …)"`
///   拿到空串会**退出 0**，部署于是报「装好了」，系统上却什么都没有（静默失败）。
/// - 先落盘再校验，最后仍用 `bash -c "$(cat …)" @ install` 执行：`@` 是 `bash -c` 的
///   `$0`、`install` 是 `$1`——保持与官方用法完全一致的调用契约，只把「取内容」从
///   curl 换成读一个已校验过的文件。
/// - `sha256sum -c` 失败即整条命令失败，脚本根本不会被执行。
pub fn xray_install_cmd() -> String {
    let url =
        format!("https://github.com/XTLS/Xray-install/raw/{XRAY_INSTALL_SHA}/install-release.sh");
    format!(
        "curl -fL -o /tmp/xray-install.sh '{url}' \
         && echo '{XRAY_INSTALL_SCRIPT_SHA256}  /tmp/xray-install.sh' | sha256sum -c - \
         && bash -c \"$(cat /tmp/xray-install.sh)\" @ install --version {XRAY_PIN_VERSION} \
         && rm -f /tmp/xray-install.sh"
    )
}

/// 二进制安装命令（ss 与 sing-box 共用，只有归档类型与可执行文件名不同）。
///
/// 三道闸，顺序不能换：
/// 1. `sha256` 非空时按上游给的哈希校验**归档**（TLS 只保证「从哪来」，不保证
///    「内容是什么」）。`None` 时**不假装校验**，改为把实测哈希打出来，让版本漂移
///    至少看得见（调用方再把这一行记进部署进度）。
/// 2. 整包解到**临时目录**，再按名字 `find` 出 `bin`——**不能假定归档只有一层目录**。
///    2026-10-23 实测两种布局并存：shadowsocks-rust 的 tar.xz 是平铺的（`ssserver`
///    等二进制直接在根），sing-box 的 tar.gz 带一层版本目录。对平铺归档用
///    `--strip-components=1` 时 GNU tar 会把每个成员剥成空名并**静默跳过**（退出码
///    仍是 0），解出空目录——表现正是「解包后没有 ssserver：归档布局与预期不符」。
///    按名字找则不依赖布局；真找不到时报错仍然说人话，不会「装好了却起不来」。
/// 3. 只有前两步都过了才把**二进制所在目录**整份 `cp` 进 `/usr/local/bin`：失败时
///    那里不会留下半装状态。搬整个目录而不是单个文件，因为 sing-box 归档里还有
///    运行时要按同目录查找的 `libcronet.so`。
///
/// 顺带把装上的那个文件的实测 sha256 留在 stdout（调用方记进部署进度）——tor 那条链
/// 的 exe 哈希是常量（版本写死），我们这里的版本是运行时才定的，钉不住预期值，
/// 那就把实测值留下来，至少查得到「服务器上装的到底是哪一份」。
fn binary_install_cmd(
    url: &str,
    sha256: Option<&str>,
    tmp: &str,
    dir: &str,
    bin: &str,
    tar_flag: &str,
) -> String {
    let verify = match sha256 {
        Some(sha) => format!("echo '{sha}  {tmp}' | sha256sum -c -"),
        None => format!("sha256sum {tmp}"),
    };
    let guard = format!(
        "if [ -z \"$binpath\" ] || [ ! -x \"$binpath\" ]; then \
         echo '解包后没有 {bin}：归档布局与预期不符' >&2; exit 1; fi"
    );
    format!(
        "curl -fL -o {tmp} '{url}' && {verify} \
         && rm -rf {dir} && mkdir -p {dir} && tar {tar_flag} {tmp} -C {dir} \
         && binpath=$(find {dir} -type f -name '{bin}' | head -n 1) && {guard} \
         && bindir=$(dirname \"$binpath\") && cp -a \"$bindir\"/. /usr/local/bin/ \
         && sha256sum /usr/local/bin/{bin} \
         && rm -rf {dir} {tmp}"
    )
}

/// shadowsocks-rust 安装命令。`tar -xJf` 的 `f` 紧跟归档名；归档是**平铺**的
/// （`ssserver` 等二进制直接在根，2026-10-23 实测 v1.18.4 与 v1.25.0 都是），
/// 不能加 `--strip-components`。
pub fn ss_install_cmd(url: &str, sha256: Option<&str>) -> String {
    binary_install_cmd(
        url,
        sha256,
        "/tmp/ss.tar.xz",
        "/tmp/ss.x",
        "ssserver",
        "-xJf",
    )
}

/// sing-box 安装命令。注意 `-xzf` 里已带 `f`，必须紧跟归档名；写成
/// `-xzf -C … -f file` 会把 `-C` 吞成归档名并再出现一个 `-f`（GNU tar 报
/// Multiple archive files）。
pub fn singbox_install_cmd(url: &str, sha256: Option<&str>) -> String {
    binary_install_cmd(
        url,
        sha256,
        "/tmp/singbox.tar.gz",
        "/tmp/singbox.x",
        "sing-box",
        "-xzf",
    )
}
/// shadowsocks-rust GitHub API 基址；latest_ss_asset 按 {api_base}/releases/latest 拼接。
/// 测试注入 wiremock 地址，生产由 state 装配传入本常量。
pub const SS_GH_API_BASE: &str = "https://api.github.com/repos/shadowsocks/shadowsocks-rust";

/// 端口监听探测命令。**必须只匹配 LISTEN 数据行**：`ss -tln 'sport = :N'` 在无
/// 匹配时仍打印表头（`State Recv-Q Send-Q ...`），`grep -q .` 命中表头恒真——
/// 任何空闲端口全会误报「已被占用」（部署与建链的前置探测均受影响）。
pub fn port_probe_cmd(port: u16) -> String {
    format!("ss -tln 'sport = :{port}' | grep -q '^LISTEN'")
}

/// UDP 端口监听探测。Hysteria2 / TUIC 是 UDP 协议，`ss -tln` 永远看不到它们的
/// 套接字；`ss -uln` 里已绑定 UDP 套接字的状态列是 `UNCONN`。
pub fn udp_port_probe_cmd(port: u16) -> String {
    format!("ss -uln 'sport = :{port}' | grep -q '^UNCONN'")
}

/// 按协议选择监听探测：UDP 协议查 UDP 套接字，其余查 TCP。
///
/// 2026-10-23 实测：验证循环对 Hysteria2/TUIC 也发 `ss -tln`，端口永远「没监听」
/// → 两个协议被判失败、节点不写回，用户看到的是「勾了 Hysteria2 却没部署上」。
pub fn listen_probe_cmd(protocol: &str, port: u16) -> String {
    match protocol {
        "hysteria2" | "tuic" => udp_port_probe_cmd(port),
        _ => port_probe_cmd(port),
    }
}

/// 持久放行已部署协议的入站端口。
///
/// 为什么不能只靠运行时规则：2026-10-23 ashura 重启后，UFW 的持久规则里没有
/// 8443/2053/2083/2096/8002/8989（这些端口此前只由运行时 iptables 放行），
/// 默认 DROP 立即把它们挡掉——节点行还在、界面上一切正常，客户端全部 0ms。
/// 部署成功意味着「客户端应该能连上」，端口放行就不能依赖任何开机后才补的规则。
///
/// 优先级：
/// 1. `ufw allow`（规则写入 /etc/ufw，同时维护 IPv4/IPv6，重启后仍生效）；
/// 2. 没有 ufw 时退回 `iptables` + `ip6tables`（幂等 `-C || -I`）；若装了
///    `netfilter-persistent` 再落盘，否则只能是运行时规则。
///
/// 套接字类型随协议：`ss` 的配置是 `tcp_and_udp`，Hysteria2/TUIC 只绑 UDP，
/// vless/vmess/trojan 只用 TCP。
pub fn firewall_allow_cmd(protocols: &[(&str, u16)]) -> String {
    let mut rules: Vec<(u16, &'static str)> = Vec::new();
    for (protocol, port) in protocols {
        let sockets: &[&'static str] = match *protocol {
            "hysteria2" | "tuic" => &["udp"],
            "ss" => &["tcp", "udp"],
            _ => &["tcp"],
        };
        for socket in sockets {
            if !rules.contains(&(*port, *socket)) {
                rules.push((*port, *socket));
            }
        }
    }
    if rules.is_empty() {
        // 空 if 分支在 shell 里是语法错误（`then` 后直接 `elif`）。
        return String::new();
    }
    rules.sort_unstable();

    let mut ufw = String::new();
    let mut iptables = String::new();
    let mut ip6tables = String::new();
    for (port, socket) in &rules {
        ufw.push_str(&format!(
            "ufw allow {port}/{socket} >/dev/null 2>&1 || exit 1\n"
        ));
        // 幂等：先 -C 查，缺了再 -I 1 插到最前（与 channels.rs 的放行写法一致）。
        iptables.push_str(&format!(
            "( iptables -C INPUT -p {socket} --dport {port} -j ACCEPT 2>/dev/null \
             || iptables -I INPUT 1 -p {socket} --dport {port} -j ACCEPT ) || exit 1\n"
        ));
        ip6tables.push_str(&format!(
            "( ip6tables -C INPUT -p {socket} --dport {port} -j ACCEPT 2>/dev/null \
             || ip6tables -I INPUT 1 -p {socket} --dport {port} -j ACCEPT ) || exit 1\n"
        ));
    }
    format!(
        "if command -v ufw >/dev/null 2>&1; then\n\
         {ufw}\
         elif command -v iptables >/dev/null 2>&1; then\n\
         {iptables}\
         if command -v ip6tables >/dev/null 2>&1; then\n\
         {ip6tables}\
         fi\n\
         if command -v netfilter-persistent >/dev/null 2>&1; then \
         netfilter-persistent save >/dev/null 2>&1 || true; fi\n\
         fi"
    )
}

/// 部署协议白名单。字符串标识同时作 nodes.protocol 与 params 来源。
pub const PROTOCOLS: [&str; 6] = ["vless", "trojan", "vmess", "ss", "hysteria2", "tuic"];

// ---------- fail2ban（SSH 爆破防护） ----------

/// fail2ban 的 drop-in 配置路径。
///
/// 用 `jail.d/` 下的独立文件而不是改 `jail.local`：fail2ban 的加载顺序是
/// `jail.conf → jail.d/*.conf → jail.local → jail.d/*.local`，这个文件最后生效，
/// 不会覆盖机器上已有的其它 jail，同时保证每次部署都收敛到同一份 sshd 策略。
pub const FAIL2BAN_JAIL_PATH: &str = "/etc/fail2ban/jail.d/vpn-ctl.local";

/// 部署管理的 fail2ban 规范（2026-10-29 定）：
///
/// - **只管 sshd**：部署不知道其它服务的日志格式，乱配 jail 只会误封；
/// - `backend = systemd`：Ubuntu 22.04+/Debian 12 默认没有 `/var/log/auth.log`，
///   auto 后端会找不到日志；systemd 后端只看 journal。sshd 过滤器自带
///   `_COMM=sshd` 的 journalmatch，Debian 的单元名是 `ssh.service` 也能命中；
/// - 1 分钟内 5 次失败**永久封禁**（`bantime = -1`）：封禁记录持久化在
///   fail2ban 自己的库/规则里，服务重启、机器重启都不解除，只能人工 unban；
/// - 只封管理 SSH 端口（应用连哪个端口就用哪个），不封业务端口——代理端口被
///   滥用的形态与 SSH 爆破不同，没有通用过滤器；
/// - `ignoreip` 只放行回环：管理 IP 是动态的，硬编码白名单等于没有保护。
///
/// **不覆盖 `banaction`**：沿用系统 fail2ban 的配置。不同发行版/版本的后端不同
/// （Ubuntu 26.04 的 fail2ban 1.1 默认 `nftables`，Debian 12 是
/// `iptables-multiport`，管理员也可能已经选了 `ufw`）。中途切换动作会让
/// fail2ban 用新动作恢复旧封禁、旧动作的规则没人清理；让系统自己决定最稳。
///
/// 永久封禁令的代价：管理 IP 连续失败 5 次就再也连不上，必须从服务商控制台
/// 解封（`fail2ban-client set sshd unbanip <IP>`）。这是用户明确选择的策略。
pub fn fail2ban_jail_config(ssh_port: u16) -> String {
    format!(
        "# 由 vpn-ctl 部署生成，每次部署幂等覆盖，不要手改。\n\
         [sshd]\n\
         enabled = true\n\
         backend = systemd\n\
         port = {ssh_port}\n\
         ignoreip = 127.0.0.1/8 ::1\n\
         findtime = 1m\n\
         maxretry = 5\n\
         bantime = -1\n"
    )
}

/// 幂等安装：已装 fail2ban 且 systemd 后端可导入时直接成功。
///
/// `python3-systemd` 是 systemd 后端的依赖，apt 只把它列为 Recommends；
/// 漏装时 fail2ban 能启动但 sshd jail 起不来，必须显式装上。
/// 兼容 apt / dnf / yum（RHEL 系 fail2ban 在 EPEL）。
pub fn fail2ban_install_cmd() -> String {
    "need=0; \
     command -v fail2ban-client >/dev/null 2>&1 || need=1; \
     python3 -c 'import systemd.journal' >/dev/null 2>&1 || need=1; \
     if [ \"$need\" = 0 ]; then echo FAIL2BAN_ALREADY_INSTALLED; exit 0; fi; \
     if command -v apt-get >/dev/null 2>&1; then \
     DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
     DEBIAN_FRONTEND=noninteractive apt-get install -y -qq fail2ban python3-systemd; \
     elif command -v dnf >/dev/null 2>&1; then \
     dnf install -y -q fail2ban python3-systemd || \
     { dnf install -y -q epel-release && \
     dnf install -y -q fail2ban python3-systemd; }; \
     elif command -v yum >/dev/null 2>&1; then \
     yum install -y -q epel-release && \
     yum install -y -q fail2ban python3-systemd; \
     else echo NO_PACKAGE_MANAGER >&2; exit 3; fi"
        .to_string()
}

/// 应用配置并验证 sshd jail 真的起来了。
///
/// `systemctl restart` 返回 0 只代表服务进程起来了，jail 加载失败（配置语法、
/// 后端缺模块、日志不存在）不会让它非零；必须 `fail2ban-client status sshd`
/// 才算验证。重启后 socket 可能还没就绪，重试 10 秒。
pub fn fail2ban_apply_cmd() -> &'static str {
    "systemctl enable fail2ban >/dev/null 2>&1 || true; \
     systemctl restart fail2ban || exit 1; \
     i=0; \
     until fail2ban-client status sshd >/dev/null 2>&1; do \
     i=$((i+1)); \
     if [ \"$i\" -ge 10 ]; then fail2ban-client status sshd >&2; exit 1; fi; \
     sleep 1; \
     done"
}

/// 读取 sshd jail 当前封禁的 IP；未安装/没有 jail 时输出空串而不是报错。
pub fn fail2ban_list_bans_cmd() -> &'static str {
    "if command -v fail2ban-client >/dev/null 2>&1; then \
     fail2ban-client get sshd banip 2>/dev/null || true; fi"
}

/// 解析 `fail2ban-client get sshd banip` 的输出。
///
/// 输出是空格分隔的 IP 列表（无封禁时为空串）；只接受能解析为 IP 的词，
/// 防止把错误信息、时间戳或占位符写进本地名单。结果排序去重，保证落库可复现。
pub fn parse_fail2ban_bans(out: &str) -> Vec<String> {
    let mut ips: Vec<String> = out
        .split_whitespace()
        .filter(|s| s.parse::<std::net::IpAddr>().is_ok())
        .map(|s| s.to_string())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// 本次 SSH 会话的来源 IP（服务器视角，`SSH_CLIENT` 的第一段）。
///
/// 预封禁时必须跳过操作者自己：本地名单是永久累积的，一旦某个历史 IP 恰好是
/// 现在的管理 IP（例如以前在另一台机器上输错过密码），新机器刚部署完就会把
/// 自己关在门外。当前会话的 IP 用 sshd 自己注入的变量取，不依赖应用侧探测。
pub fn ssh_client_ip_cmd() -> &'static str {
    "echo \"${SSH_CLIENT%% *}\""
}

/// 单次同步下发/单轮增量下发的数量上限。
///
/// 部署时：本地名单里缺的 IP 不超过这个数量就同步批量下发（1000 条约 25 秒）；
/// 超过则只上传文件、交给服务器后台 worker 慢慢封，部署不再等待（初次部署一万条
/// 就是这条路径）。
/// 轮询时：每台服务器每轮最多取这么多条增量，避免一台离线很久的服务器把一轮
/// 轮询拖成几分钟。
pub const PREBAN_SYNC_LIMIT: usize = 1000;

/// 预封禁临时文件（一行一个 IP）。用文件而不是把上千个 IP 拼进命令行：
/// 避免超长命令与引号/转义问题，内容分块 base64 追加写入。
pub const FAIL2BAN_PREBAN_PATH: &str = "/tmp/vpn-ctl-preban-ips";

/// 轮询增量下发的临时文件（与部署预封禁分开，避免两者同时写同一路径）。
pub const FAIL2BAN_SYNC_PATH: &str = "/tmp/vpn-ctl-ban-sync-ips";

/// 后台预封禁 worker 的脚本 / pid / 日志路径。
pub const FAIL2BAN_PREBAN_SCRIPT: &str = "/tmp/vpn-ctl-preban.sh";
pub const FAIL2BAN_PREBAN_PID: &str = "/tmp/vpn-ctl-preban.pid";
pub const FAIL2BAN_PREBAN_LOG: &str = "/tmp/vpn-ctl-preban.log";

/// 把指定文件里的 IP 分批交给 `set sshd banip`，完成后删除文件。
///
/// fail2ban-client 一次调用可以传多个 IP（ashura 1.1.0 实测一次 1000 个，24.8s
/// 全部生效）。逐条调用时每条都要起一个 Python 客户端（实测 ~257ms/条，1000 条
/// 要 4 分多钟，会把部署拖到超时）；每批 200 条时固定开销几乎可以忽略，总耗时
/// 由 fail2ban 内部逐条执行封禁动作主导（~25s/1000 条）。`|| true` 包在 `sh -c`
/// 里：已封禁的 IP 可能返回非零，不能让 xargs 因某一批失败提前退出。
pub fn fail2ban_ban_file_cmd(path: &str) -> String {
    format!(
        "if [ -s {p} ]; then \
         xargs -r -n 200 sh -c 'fail2ban-client set sshd banip \"$@\" >/dev/null 2>&1 || true' sh < {p}; \
         rm -f {p}; \
         fi",
        p = path
    )
}

/// 部署里的同步预封禁：读取固定路径 [`FAIL2BAN_PREBAN_PATH`]。
pub fn fail2ban_preban_cmd() -> String {
    fail2ban_ban_file_cmd(FAIL2BAN_PREBAN_PATH)
}

/// 后台预封禁脚本：每批 200 个 IP、批间 sleep 2 秒慢慢封，避免大批量下发在
/// 服务器上形成 CPU 尖峰。由部署以 setsid 分离启动，SSH 断开/部署结束都不影响
/// 它；结束时删除 IP 文件与 pid 文件。
pub fn fail2ban_preban_worker_script() -> String {
    format!(
        "#!/bin/sh\n\
         # 由 vpn-ctl 部署生成，每次部署幂等覆盖。\n\
         f={preban}\n\
         log={log}\n\
         pid={pid}\n\
         : > \"$log\"\n\
         echo \"start $(date -Is) total=$(grep -c . \"$f\")\" >> \"$log\"\n\
         xargs -r -n 200 sh -c 'fail2ban-client set sshd banip \"$@\" >/dev/null 2>&1 || true; echo \"batch $(date -Is) $#\" >> {log}; sleep 2' sh < \"$f\"\n\
         echo \"done $(date -Is)\" >> \"$log\"\n\
         rm -f \"$f\" \"$pid\"\n",
        preban = FAIL2BAN_PREBAN_PATH,
        log = FAIL2BAN_PREBAN_LOG,
        pid = FAIL2BAN_PREBAN_PID,
    )
}

/// 停掉上一轮的后台 worker（如果还在跑）。用 pid 文件 + `/proc/<pid>/cmdline`
/// 校验，避免 pid 复用后误杀别的进程。
pub fn fail2ban_stop_worker_cmd() -> String {
    format!(
        "p={pid}; if [ -f \"$p\" ]; then pid=$(cat \"$p\"); \
         if [ -n \"$pid\" ] && grep -q vpn-ctl-preban.sh \"/proc/$pid/cmdline\" 2>/dev/null; then \
         kill \"$pid\" 2>/dev/null || true; fi; rm -f \"$p\"; fi",
        pid = FAIL2BAN_PREBAN_PID
    )
}

/// 分离启动后台 worker：setsid + 重定向全部标准流，SSH 通道关闭后继续跑。
pub fn fail2ban_start_worker_cmd() -> String {
    format!(
        "command -v setsid >/dev/null 2>&1 || exit 1; \
         setsid sh {script} </dev/null >/dev/null 2>&1 & echo $! > {pid}; cat {pid}",
        script = FAIL2BAN_PREBAN_SCRIPT,
        pid = FAIL2BAN_PREBAN_PID,
    )
}

/// 协议是否在部署白名单内。
pub fn is_supported_protocol(proto: &str) -> bool {
    PROTOCOLS.contains(&proto)
}

/// 补装合并：现有协议在前（部署顺序稳定），新勾选协议只补不在列表中的项。
pub fn merge_deploy_protocols(existing: &[String], chosen: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = existing.to_vec();
    for p in chosen {
        if !merged.contains(p) {
            merged.push(p.clone());
        }
    }
    merged
}

/// 已部署协议的展示详情：`label` 是完整的协议变体名（传输层/安全层/流控），
/// 而不是白名单里的裸协议名。只显示 `vless` 无法区分 TCP+REALITY+Vision 与
/// WebSocket+TLS，用户也就无从判断“部署的到底是哪种”。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DeployedProtocolDetail {
    pub protocol: String,
    pub port: u16,
    pub label: String,
    pub transport: String,
    pub security: String,
    pub flow: Option<String>,
}

/// 协议在 UI/部署白名单中的固定顺序（未知协议排最后）。
pub fn protocol_order(protocol: &str) -> usize {
    PROTOCOLS
        .iter()
        .position(|p| *p == protocol)
        .unwrap_or(PROTOCOLS.len())
}

/// 安全层显示名：只做大小写归一，不替缺失字段编造标签。
fn security_name(raw: &str) -> String {
    if raw.eq_ignore_ascii_case("reality") {
        "REALITY".into()
    } else if raw.eq_ignore_ascii_case("tls") {
        "TLS".into()
    } else if raw.is_empty() {
        String::new()
    } else {
        raw.to_uppercase()
    }
}

/// 从节点参数还原协议变体。参数是自建节点写回时的明文 JSON；
/// 缺失字段按空处理——标签必须与实际握手参数一致，不能“猜一个更好看的”。
pub fn describe_deployed_protocol(
    protocol: &str,
    port: u16,
    params_json: &str,
) -> DeployedProtocolDetail {
    let params: serde_json::Value =
        serde_json::from_str(params_json).unwrap_or_else(|_| serde_json::json!({}));
    let s = |key: &str| params.get(key).and_then(|v| v.as_str()).unwrap_or("");

    let (label, transport, security, flow) = match protocol {
        "vless" => {
            // 节点参数里写的是小写 "tcp"，大小写都当 TCP，不能把 "tcp" 当传输层
            // 拼进标签（会出现「VLESS + tcp + REALITY」这种半成品）。
            let raw_type = s("type");
            let transport = match raw_type.to_ascii_lowercase().as_str() {
                "ws" => "WebSocket",
                "" | "tcp" => "TCP",
                _ => raw_type,
            };
            let security = security_name(s("security"));
            let raw_flow = s("flow");
            let mut parts = vec!["VLESS"];
            if transport != "TCP" {
                parts.push(transport);
            }
            if !security.is_empty() {
                parts.push(&security);
            }
            if raw_flow == "xtls-rprx-vision" {
                parts.push("Vision");
            } else if !raw_flow.is_empty() {
                parts.push(raw_flow);
            }
            (
                parts.join(" + "),
                transport.to_string(),
                security,
                (!raw_flow.is_empty()).then(|| raw_flow.to_string()),
            )
        }
        "trojan" => {
            let transport = if s("type").eq_ignore_ascii_case("ws") {
                "WebSocket"
            } else {
                "TCP"
            };
            let security = security_name(s("security"));
            let mut parts = vec!["Trojan"];
            if transport != "TCP" {
                parts.push(transport);
            }
            if !security.is_empty() {
                parts.push(&security);
            }
            (parts.join(" + "), transport.to_string(), security, None)
        }
        "vmess" => {
            let raw_network = s("network");
            let transport = match raw_network.to_ascii_lowercase().as_str() {
                "" | "ws" => "WebSocket",
                "tcp" => "TCP",
                _ => raw_network,
            };
            (
                format!("VMess + {transport}（无 TLS）"),
                transport.to_string(),
                "无".to_string(),
                None,
            )
        }
        "ss" => {
            let cipher = match s("cipher") {
                "" => "2022-blake3-aes-256-gcm",
                raw => raw,
            };
            (
                format!("SS-2022（{cipher}）"),
                "TCP+UDP".to_string(),
                "无".to_string(),
                None,
            )
        }
        "hysteria2" => (
            "Hysteria2（QUIC + 自签 TLS）".to_string(),
            "QUIC".to_string(),
            "自签 TLS".to_string(),
            None,
        ),
        "tuic" => (
            "TUIC（QUIC + 自签 TLS）".to_string(),
            "QUIC".to_string(),
            "自签 TLS".to_string(),
            None,
        ),
        other => (other.to_string(), String::new(), String::new(), None),
    };

    DeployedProtocolDetail {
        protocol: protocol.to_string(),
        port,
        label,
        transport,
        security,
        flow,
    }
}

/// 解析远端 xray 单进程多入站配置的 (protocol, port) 列表（重部署复用端口用）。
/// 形状见 xray_config_json_multi：inbounds[] 每项含 protocol 与 port。
pub fn parse_xray_inbound_ports(json: &str) -> Vec<(String, u16)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec![];
    };
    v.get("inbounds")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ib| {
                    let proto = ib.get("protocol")?.as_str()?.to_string();
                    let port = ib.get("port")?.as_u64()? as u16;
                    Some((proto, port))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 从远端既有 xray 配置还原**每个端口上的入站凭据**，形状与 [`xray_config_json_multi`]
/// 对偶。重复部署据此复用凭据而不是重新生成（见 run_deploy 里那段「幂等」注释）。
///
/// reality 私钥只存在于服务器配置里（客户端 params 只有公钥），所以复用必须从这份
/// 配置读回——光靠库里的节点数据重建不出同一份配置。`public_key` 这里**读不到**
/// （Xray 端不存公钥），由调用方从既有节点参数补上；漏补会让写回的节点参数里
/// public-key 为空、reality 握手全废，故返回结构上把它留空并要求调用方填。
///
/// 损坏 JSON / 未知协议 / 缺端口一律跳过该入站——跳过的等价于「没有既有入站」，
/// 走新生成那条老路。**不报错**：还原是尽力而为，读不回来不该让部署失败。
pub fn parse_xray_inbounds(json: &str) -> std::collections::HashMap<u16, XrayInboundEntry> {
    let mut out = std::collections::HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return out;
    };
    let Some(arr) = v.get("inbounds").and_then(|a| a.as_array()) else {
        return out;
    };
    for ib in arr {
        let proto: &'static str = match ib.get("protocol").and_then(|p| p.as_str()) {
            Some("vless") => "vless",
            Some("trojan") => "trojan",
            Some("vmess") => "vmess",
            _ => continue,
        };
        let Some(port) = ib.get("port").and_then(|p| p.as_u64()).map(|p| p as u16) else {
            continue;
        };
        // 主配置里可能同时有主用户与复用入口的上游用户（up-<id>）。必须取主用户：
        // 拿成上游用户会把它的 uuid/口令当成主凭据写回主节点，重部署后主节点立刻变砖。
        // 老配置没有 email 字段 → find 落空，回落到第一个（仍是主用户）。
        let client = ib
            .pointer("/settings/clients")
            .and_then(|v| v.as_array())
            .and_then(|clients| {
                clients
                    .iter()
                    .find(|c| {
                        !c.get("email")
                            .and_then(|e| e.as_str())
                            .unwrap_or("")
                            .starts_with("up-")
                    })
                    .or_else(|| clients.first())
            });
        let reality = ib.pointer("/streamSettings/realitySettings");
        let s = |v: Option<&serde_json::Value>, k: &str| -> String {
            v.and_then(|x| x.get(k))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        out.insert(
            port,
            XrayInboundEntry {
                proto,
                port,
                uuid: s(client, "id"),
                password: s(client, "password"),
                short_id: reality
                    .and_then(|r| r.get("shortIds"))
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                private_key: s(reality, "privateKey"),
                public_key: String::new(), // 见函数注释：调用方补
                reality_dest: reality
                    .and_then(|r| r.get("dest"))
                    .and_then(|d| d.as_str())
                    .map(|d| d.trim_end_matches(":443").to_string())
                    .unwrap_or_default(),
                ws_path: ib
                    .pointer("/streamSettings/wsSettings/path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("/ray")
                    .to_string(),
            },
        );
    }
    out
}

/// 解析 shadowsocks-rust 服务端配置的监听端口：**`servers[0].server_port`**。
///
/// 形状以 [`ss_config_json`] 为准——本工具写出去的永远是 `servers[]` 数组形态。
/// 旧实现读的是**顶层** `server_port`，而那个键在真实配置里根本不存在：它只匹配得上
/// 手写的扁平夹具（那条夹具因此是装饰性的），于是 `read_existing_ports` 永远读不到
/// ss 端口，**每次重部署都重新分配一个端口**——云侧安全组通常只放行首次部署的端口，
/// 漂移的表现就是客户端 SYN 超时。2026-09-20 用「拿生成器输出反过来喂解析器」的用例
/// 抓到的（见 deploy_tests::parse_ss_port_extracts_server_port）。
pub fn parse_ss_port(json: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    // 新版是 servers[0]；旧版 shadowsocks-rust 配置是顶层 server/server_port。
    // 只认新形状会让老机器重部署时读不到端口 → 拿正在跑的 ssserver 自己当「端口被占」
    // → 端口漂移（实测 8388→8389），旧节点与中转规则一起失效。
    v.pointer("/servers/0/server_port")
        .or_else(|| v.get("server_port"))?
        .as_u64()
        .map(|p| p as u16)
}

/// 从客户端节点参数里取 reality 公钥。键名与 [`run_deploy`] 写回时用的 `public-key`
/// 一致（`params_json` 是 kebab-case 的 clash 参数形态）。vmess 没有这个键 → None。
pub fn public_key_of(params_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(params_json)
        .ok()?
        .get("public-key")?
        .as_str()
        .map(|s| s.to_string())
}

/// 解析 shadowsocks-rust 服务端配置里的密码（重部署复用凭据用）；缺失/损坏 → None。
/// 形状见 [`ss_config_json`]：servers[0].password。
pub fn parse_ss_password(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    // 同 parse_ss_port：兼容旧版顶层形状；读不到密码会重新生成，客户端旧参数
    // 立刻变砖（服务端新密码 + 客户端旧密码）。
    v.pointer("/servers/0/password")
        .or_else(|| v.get("password"))?
        .as_str()
        .map(|s| s.to_string())
}

/// 解析 sing-box 配置中 hysteria2/tuic 入站端口（重部署复用端口用）。
pub fn parse_singbox_ports(json: &str) -> Vec<(String, u16)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec![];
    };
    v.get("inbounds")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|ib| {
                    ib.get("type")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t == "hysteria2" || t == "tuic")
                })
                .filter_map(|ib| {
                    let t = ib.get("type")?.as_str()?.to_string();
                    let port = ib.get("listen_port")?.as_u64()? as u16;
                    Some((t, port))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// hysteria2 凭据：(密码, obfs 密码)。旧配置没有 obfs 块时第二项为 None。
pub type Hy2Creds = (String, Option<String>);
/// tuic 凭据：(uuid, 密码)。
pub type TuicCreds = (String, String);

/// 解析 sing-box 配置里 hysteria2/tuic 的凭据（重部署复用凭据用）。
///
/// 形状见 [`singbox_config_json`]：`inbounds[]` 各带 `users[0]`——hysteria2 是
/// `password`，tuic 是 `uuid` + `password`。返回
/// `(hy2 (密码, obfs 密码), tuic (uuid, 密码))`；旧配置没有 obfs 块时第二项为
/// `None`（部署器会复用密码、另生成 obfs 密码）。缺失或内容损坏一律 `None`
/// （调用方退回新生成）。
pub fn parse_singbox_creds(json: &str) -> (Option<Hy2Creds>, Option<TuicCreds>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return (None, None);
    };
    let mut hy2 = None;
    let mut tuic = None;
    for ib in v
        .get("inbounds")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
    {
        let user = ib
            .get("users")
            .and_then(|u| u.as_array())
            .and_then(|a| a.first());
        let field = |name: &str| {
            user.and_then(|u| u.get(name))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        };
        match ib.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            // 同一个配置里出现多个同类入站时取**第一个**能解析出凭据的：后一个覆盖
            // 前一个会让「复用」复用到一个与客户端不一致的值
            "hysteria2" if hy2.is_none() => {
                if let Some(pw) = field("password") {
                    let obfs = ib
                        .get("obfs")
                        .and_then(|o| o.get("password"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                    hy2 = Some((pw, obfs));
                }
            }
            "tuic" if tuic.is_none() => {
                if let (Some(u), Some(p)) = (field("uuid"), field("password")) {
                    tuic = Some((u, p));
                }
            }
            _ => {}
        }
    }
    (hy2, tuic)
}

/// 部署前置能力探测：一次 SSH 同时取系统标识与 systemd 判定。
/// 判据是 /run/systemd/system 目录（systemd 作为 PID 1 时创建）**加** systemctl 可执行，
/// 而不是发行版名字——覆盖所有 systemd 系及其衍生版（含国内云厂商发行版）。
/// 只看 command -v 不够：容器/WSL 里 systemctl 存在但 systemd 未运行，
/// 会走到「unit 写完 → enable 静默不生效 → 端口验证失败」的半截状态，失败点离原因太远。
/// `. ` 用 POSIX 点号而非 source（busybox ash 不认 source）。
pub const CAPABILITY_PROBE_SCRIPT: &str = concat!(
    ". /etc/os-release 2>/dev/null; echo \"OS=${ID:-unknown}${ID_LIKE:+ ${ID_LIKE}}\"\n",
    "if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then echo SYSTEMD=yes; else echo SYSTEMD=no; fi",
);

/// 解析 CAPABILITY_PROBE_SCRIPT 输出 → (OS 描述, 是否运行 systemd)。
/// OS 行缺失/为空 → 「未知」；SYSTEMD 行缺失或取值非 `yes` → false。
pub fn parse_capability(stdout: &str) -> (String, bool) {
    let os = stdout
        .lines()
        .find_map(|l| l.strip_prefix("OS="))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("未知")
        .to_string();
    let systemd = stdout.lines().any(|l| l.trim() == "SYSTEMD=yes");
    (os, systemd)
}

/// 解析 `xray x25519` 输出。兼容两种版本标签（install-release.sh 装的最新版
/// Xray 26.3.27 实测：`PrivateKey: …` / `Password (PublicKey): …` / `Hash32: …`；
/// 旧版为 `Private key: …` / `Public key: …`）——只认旧标签会误报「缺少密钥」。
pub fn parse_x25519_keys(stdout: &str) -> Result<(String, String), String> {
    let mut priv_k = None;
    let mut pub_k = None;
    for line in stdout.lines() {
        if let Some(v) = line
            .strip_prefix("Private key: ")
            .or(line.strip_prefix("PrivateKey: "))
        {
            priv_k = Some(v.trim().to_string());
        }
        if let Some(v) = line
            .strip_prefix("Public key: ")
            .or(line.strip_prefix("Password (PublicKey): "))
        {
            pub_k = Some(v.trim().to_string());
        }
    }
    match (priv_k, pub_k) {
        (Some(p), Some(q)) => Ok((p, q)),
        _ => Err("x25519 输出缺少密钥".into()),
    }
}

/// 一条「复用主入口」的上游出口在服务端配置里的投影。
///
/// 主配置按 `entry_proto` 找到对应入站，给该出口追加一个独立用户（`up-<id>`），
/// 再用路由规则把这个用户送到它的住宅代理出站。凭据（`credential`/`credential_password`）
/// 是该出口自己的，绝不能抄主入口用户——否则同一入口上的多个出口会变成同一用户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRoute {
    pub id: i64,
    pub entry_proto: String,
    pub entry_port: u16,
    /// vless/vmess: uuid；trojan/hysteria2: 密码；tuic: uuid
    pub credential: String,
    /// tuic 的密码（其它协议空）
    pub credential_password: String,
    /// 住宅代理协议：socks5 | http
    pub outbound_proto: String,
    pub outbound_host: String,
    pub outbound_port: u16,
    pub outbound_username: String,
    pub outbound_password: String,
}

/// 主配置里给上游出口使用的用户名/email（路由规则按它匹配）。
pub fn upstream_user_tag(id: i64) -> String {
    format!("up-{id}")
}

/// 主配置里住宅代理出站的 tag。
pub fn upstream_outbound_tag(id: i64) -> String {
    format!("out-up-{id}")
}

/// 多入站单条:一个协议一个列表项,由 xray_config_json_multi 组装。
#[derive(Clone)]
pub struct XrayInboundEntry {
    pub proto: &'static str, // "vless"|"trojan"|"vmess"
    pub port: u16,
    pub uuid: String,
    pub password: String,
    pub short_id: String,
    pub private_key: String,
    /// Reality 公钥（客户端参数用；每入站独立密钥对，不与同服务器其它入站共享）。
    pub public_key: String,
    pub reality_dest: String,
    pub ws_path: String,
}

/// 服务器出站地址族。集中映射三种协议各自控制“解析/建连优先哪一族”的写法，
/// 避免在三个生成函数里散落字符串字面量。
///
/// 语义差异（必须如实反映到 UI/文档）：
/// - Xray：`UseIPv4`/`UseIPv6` 对**域名目标**是严格的；
/// - shadowsocks：只能“优先”，无 AAAA/A 时仍会回落另一族；
/// - sing-box：1.12 起用出站 `domain_resolver`，更早版本用 `domain_strategy`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressFamily {
    V4,
    V6,
}

impl EgressFamily {
    /// 从数据库值解析：只认 `"v6"`，其余（含历史空串/脏值）一律按默认 v4，
    /// 保证坏数据不会让已部署服务在重部署时突然换族。
    pub fn from_db(s: &str) -> Self {
        if s == "v6" {
            Self::V6
        } else {
            Self::V4
        }
    }

    pub fn xray_domain_strategy(self) -> &'static str {
        match self {
            Self::V4 => "UseIPv4",
            Self::V6 => "UseIPv6",
        }
    }

    pub fn singbox_strategy(self) -> &'static str {
        match self {
            Self::V4 => "ipv4_only",
            Self::V6 => "ipv6_only",
        }
    }

    pub fn ss_ipv6_first(self) -> bool {
        matches!(self, Self::V6)
    }
}

/// 从 `sing-box version 1.x.y...` 解析主/次版本；无法识别返回 None。
/// None 在生成配置时按 <1.12 的旧写法处理，避免写出旧版不认识的字段把服务打挂。
pub fn parse_singbox_version(stdout: &str) -> Option<(u32, u32)> {
    let line = stdout.lines().next()?.trim();
    let rest = line.strip_prefix("sing-box version ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    // 次版本可能是 "14" 或 "14-rc1"/"14.0-beta"，取前导数字
    let minor_raw = parts.next()?;
    let minor_digits: String = minor_raw
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let minor: u32 = minor_digits.parse().ok()?;
    Some((major, minor))
}

/// Xray 单进程多入站配置:vless/trojan 走 Reality(tcp),vmess 走 WS(明文 /ray,
/// 证书体系属后续范围)。金样见 deploy_tests::xray_multi_inbounds_includes_three_protocols。
///
/// `egress` 决定 freedom 出站解析域名时限定哪个地址族。只影响 **VPS → 目标域名**；
/// 客户端发来的字面 IP、客户端侧 DNS/分流都不受它控制。
///
/// 生产部署走 [`xray_config_json_multi_routed`]；这个薄封装只给测试与分享克隆用
/// （无复用出口 = 空路由），`allow(dead_code)` 是因为非测试构建里没有调用方。
#[allow(dead_code)]
pub fn xray_config_json_multi(entries: &[XrayInboundEntry], egress: EgressFamily) -> String {
    xray_config_json_multi_routed(entries, egress, &[])
}

/// 带复用入口的主配置渲染。`routes` 里的每个出口都会在**同协议**入站里追加一个
/// `up-<id>` 用户，并配一个住宅代理出站 + `user=up-<id>` 路由。
///
/// 主用户带上 `email: "main"`：路由规则只匹配 `up-*`，主用户走默认的第一条出站
/// （freedom），行为与没有路由块时逐字节等价；这个 email 也让解析器能把主用户与
/// 上游用户区分开（见 `parse_xray_inbounds`）。
pub fn xray_config_json_multi_routed(
    entries: &[XrayInboundEntry],
    egress: EgressFamily,
    routes: &[UpstreamRoute],
) -> String {
    let xray_routes: Vec<&UpstreamRoute> = routes
        .iter()
        .filter(|r| matches!(r.entry_proto.as_str(), "vless" | "trojan" | "vmess"))
        .collect();
    let inbounds: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let mine: Vec<&&UpstreamRoute> = xray_routes
                .iter()
                .filter(|r| r.entry_proto == e.proto)
                .collect();
            let mut stream = serde_json::Map::new();
            let settings = match e.proto {
                "vless" => {
                    stream.insert("network".into(), "tcp".into());
                    stream.insert("security".into(), "reality".into());
                    stream.insert(
                        "realitySettings".into(),
                        serde_json::json!({
                            "dest": format!("{}:443", e.reality_dest),
                            "serverNames": [e.reality_dest],
                            "privateKey": e.private_key,
                            "shortIds": [e.short_id]
                        }),
                    );
                    let mut clients = vec![serde_json::json!({
                        "id": e.uuid, "flow": "xtls-rprx-vision", "email": "main"
                    })];
                    for r in &mine {
                        clients.push(serde_json::json!({
                            "id": r.credential,
                            "flow": "xtls-rprx-vision",
                            "email": upstream_user_tag(r.id)
                        }));
                    }
                    serde_json::json!({
                        "clients": clients,
                        "decryption": "none"
                    })
                }
                "trojan" => {
                    stream.insert("network".into(), "tcp".into());
                    stream.insert("security".into(), "reality".into());
                    stream.insert(
                        "realitySettings".into(),
                        serde_json::json!({
                            "dest": format!("{}:443", e.reality_dest),
                            "serverNames": [e.reality_dest],
                            "privateKey": e.private_key,
                            "shortIds": [e.short_id]
                        }),
                    );
                    let mut clients = vec![serde_json::json!({
                        "password": e.password, "email": "main"
                    })];
                    for r in &mine {
                        clients.push(serde_json::json!({
                            "password": r.credential,
                            "email": upstream_user_tag(r.id)
                        }));
                    }
                    serde_json::json!({ "clients": clients, "fallbacks": [] })
                }
                _ => {
                    let path = if e.ws_path.is_empty() {
                        "/ray"
                    } else {
                        e.ws_path.as_str()
                    };
                    stream.insert("network".into(), "ws".into());
                    stream.insert("wsSettings".into(), serde_json::json!({ "path": path }));
                    let mut clients = vec![serde_json::json!({
                        "id": e.uuid, "alterId": 0, "email": "main"
                    })];
                    for r in &mine {
                        clients.push(serde_json::json!({
                            "id": r.credential, "alterId": 0,
                            "email": upstream_user_tag(r.id)
                        }));
                    }
                    serde_json::json!({ "clients": clients })
                }
            };
            serde_json::json!({
                "listen": "0.0.0.0",
                "port": e.port,
                "protocol": e.proto,
                "settings": settings,
                "streamSettings": serde_json::Value::Object(stream),
                // routeOnly 的理由与 upstream.rs 同一处（那里有完整实测记录）：不加它则
                // 目标被改写成探测出的域名，下游解析不出随机 SNI 就在 ~2.3 秒后静默关断。
                // 落地入站的出口是 freedom（由它自己解析），后果同样是连不上，只是死法不同。
                "sniffing": { "enabled": true, "destOverride": ["http", "tls", "quic"], "routeOnly": true }
            })
        })
        .collect();
    let mut outbounds = vec![serde_json::json!({
        "protocol": "freedom",
        "tag": "direct",
        // 出口地址族由服务器行的 egress_family 决定（默认 v4）。默认的 AsIs 把域名
        // 交给 Go 的解析器，按 RFC 6724 **优先 IPv6**，而 Google 对机房 IPv6 段的
        // 地理判定是坏的：同一个出口 IP，v6 出去报「Gemini 不支持你所在的地区」，
        // v4 出去正常（2026-09-20 实测，并有多例社区同证）——所以默认钉 IPv4，
        // 只有用户显式选择 v6 才写 UseIPv6。对域名目标是严格的；字面 IP 目标不受影响。
        //
        // domainStrategy 必须写在 settings 里：写出站顶层会被 Xray 忽略而**静默失效**。
        // 非 AsIs 会让下游 sockopt 的 domainStrategy/happyEyeballs 失效——这里要的
        // 正是它，不是副作用。
        "settings": { "domainStrategy": egress.xray_domain_strategy() }
    })];
    for r in &xray_routes {
        outbounds.push(serde_json::json!({
            // socks5 在 Xray 里叫 "socks"；http 就是 "http"。
            // settings 是扁平结构（address/port/user/pass），与入站的 servers[] 不同。
            "protocol": if r.outbound_proto == "http" { "http" } else { "socks" },
            "tag": upstream_outbound_tag(r.id),
            "settings": {
                "address": r.outbound_host,
                "port": r.outbound_port,
                "user": r.outbound_username,
                "pass": r.outbound_password
            }
        }));
    }
    let rules: Vec<serde_json::Value> = xray_routes
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "field",
                "user": [upstream_user_tag(r.id)],
                "outboundTag": upstream_outbound_tag(r.id)
            })
        })
        .collect();
    let mut root = serde_json::json!({
        "inbounds": inbounds,
        // 显式给 Xray 内部解析器配上游。新版 freedom 在 UseIPv4/UseIPv6 下会用它；
        // 没有 dns.servers 时会打出 `features/dns: empty response`（2026-10-28
        // 圣克拉拉真机日志）。注意：同一条日志更可能是 REALITY 握手失败后回落伪装
        // 站时的次生现象，**不是**那次 503 的根因（根因见 XRAY_PIN_VERSION）；但让
        // 内部解析器有确定的上游仍然必要，否则域名解析取决于本机 resolver 实现。
        //
        // queryStrategy 必须跟着出口族走：出口钉了 IPv4 却用 UseIP 查询，解析出的
        // AAAA 目标仍会被出口丢掉，等于换一种方式失败。
        "dns": {
            "servers": ["8.8.8.8", "1.1.1.1"],
            "queryStrategy": egress.xray_domain_strategy()
        },
        "outbounds": outbounds
    });
    if !rules.is_empty() {
        // AsIs：域名保持未解析交给住宅代理远端解析（避开 DNS 污染/泄漏）。
        // 主用户不命中任何规则，仍走首条 freedom 出站；freedom 自己的 domainStrategy
        // 仍按出口族解析，与没有 routing 块时一致。
        root["routing"] = serde_json::json!({
            "domainStrategy": "AsIs",
            "rules": rules
        });
    }
    root.to_string()
}

/// SS 服务端配置（2022-blake3-aes-256-gcm）。
///
/// `inbound_listen` 与 sing-box 同理：shadowsocks-rust 的 `server: "0.0.0.0"`
/// **也是纯 IPv4**（2026-10-23 香港实测 `ss -tln` 只有 `0.0.0.0:8388`，客户端连
/// `[v6]:8388` 直接 503）；入口是 IPv6 时必须写 `::`。Xray 的 `0.0.0.0` 是双栈，
/// 所以这个坑只在 ss / h2 / tuic 上出现。
///
/// 出口族只能**尽力而为**：`ipv6_first` 仅把 IPv6 排在 DNS 结果的 IPv4 前面，
/// DNS 没有 AAAA 时仍会回落 IPv4；shadowsocks-rust 没有「只允许某族」的开关。
pub fn ss_config_json(
    port: u16,
    password_b64: &str,
    egress: EgressFamily,
    inbound_listen: &str,
) -> String {
    serde_json::json!({
        "servers": [{
            "server": inbound_listen, "server_port": port,
            "method": "2022-blake3-aes-256-gcm", "password": password_b64, "mode": "tcp_and_udp"
        }],
        "locals": [],
        "ipv6_first": egress.ss_ipv6_first()
    })
    .to_string()
}

/// SS 的 systemd unit（工具自写，不依赖发布包内容）。
pub fn ss_systemd_unit() -> String {
    "[Unit]\nDescription=Shadowsocks-rust server\nAfter=network.target\n\n[Service]\nExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}

/// sing-box 远端路径(白名单常量)。
pub const SINGBOX_CONFIG_PATH: &str = "/etc/sing-box/config.json";
/// sing-box systemd unit 路径(白名单常量)。
pub const SINGBOX_UNIT_PATH: &str = "/etc/systemd/system/singbox.service";
/// sing-box GitHub 仓库 API 基址(注入式,prod=GitHub)。
pub const SINGBOX_GH_API_BASE: &str = "https://api.github.com/repos/SagerNet/sing-box";

/// openssl 缺失时的安装命令：按包管理器探测，顺序与写法对齐
/// server_speed/iperf.rs 的 iperf_install_cmd。全无 → NO_OPENSSL + exit 1，
/// 调用方识别成可读错误。实际很少触发：RHEL 系 / Arch 上 openssl 是 curl 的依赖。
pub const OPENSSL_INSTALL_SCRIPT: &str = concat!(
    "if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && apt-get install -y openssl;\n",
    "elif command -v dnf >/dev/null 2>&1; then dnf install -y openssl;\n",
    "elif command -v yum >/dev/null 2>&1; then yum install -y openssl;\n",
    "elif command -v pacman >/dev/null 2>&1; then pacman -Sy --noconfirm openssl;\n",
    "elif command -v zypper >/dev/null 2>&1; then zypper -n install openssl;\n",
    "elif command -v apk >/dev/null 2>&1; then apk add openssl;\n",
    "else echo NO_OPENSSL; exit 1; fi",
);

/// sing-box 自签证书生成命令（幂等）：已有证书跳过；缺 openssl 时按包管理器安装。
/// sing-box 两个入站强制要求显式 tls 块，无证书直接 FATAL。
pub fn singbox_cert_cmd() -> String {
    format!(
        "mkdir -p /etc/sing-box; \
         if ! command -v openssl >/dev/null 2>&1; then {OPENSSL_INSTALL_SCRIPT}; fi; \
         if [ ! -f /etc/sing-box/self.crt ]; then \
         openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
         -keyout /etc/sing-box/self.key -out /etc/sing-box/self.crt -days 3650 \
         -subj /CN=vpn-ctl.singbox; fi"
    )
}

/// L2 sing-box 服务端配置：按选中协议生成 inbounds（hysteria2/tuic 同进程，
/// 各自可缺省——只勾一个 L2 是合法部署形态，未选中的 inbound 不得出现）。
/// `hy2` 的三元组是 `(端口, 密码, obfs 密码)`；obfs 密码由部署器生成，必须非空
/// （客户端要带同一个值才能完成 Salamander 握手）。
/// 注意：sing-box 两个入站**强制要求显式 tls 块**（无 certificate 直接 FATAL
/// “TLS required”，实测 h2/tuic 部署失败 exit=3）。部署器先幂等生成自签证书
/// （/etc/sing-box/self.crt/key），客户端 skip-cert-verify 跳过校验。
///
/// `inbound_listen` 必须跟着**客户端实际使用的地址族**走：sing-box 的
/// `0.0.0.0` 是纯 IPv4（实测 `ss -uln` 显示 `0.0.0.0:2097`），而香港这类机器
/// 登记的是 IPv6 地址，客户端把 UDP 发到 `[v6]:2097` 永远到不了；Xray 的
/// `0.0.0.0` 实际是双栈（`ss -tln` 显示 `*:443`），所以只有 h2/tuic 会坏。
///
/// 生产部署走 [`singbox_config_json_routed`]；这个薄封装只给测试与分享克隆用
/// （无复用出口 = 空路由），`allow(dead_code)` 是因为非测试构建里没有调用方。
#[allow(dead_code)]
pub fn singbox_config_json(
    hy2: Option<(u16, &str, &str)>,
    tuic: Option<(u16, &str, &str)>,
    egress: EgressFamily,
    version: Option<(u32, u32)>,
    inbound_listen: &str,
) -> String {
    singbox_config_json_routed(hy2, tuic, egress, version, inbound_listen, &[])
}

/// 带复用入口的 sing-box 配置。`routes` 里 hysteria2/tuic 的出口会在同协议入站里
/// 追加一个带 `name=up-<id>` 的用户，并用 `auth_user` 路由把它送到住宅代理出站。
/// 主用户也显式命名为 `main`，路由规则只匹配 `up-*`，主用户仍走首条 direct 出站。
pub fn singbox_config_json_routed(
    hy2: Option<(u16, &str, &str)>,
    tuic: Option<(u16, &str, &str)>,
    egress: EgressFamily,
    version: Option<(u32, u32)>,
    inbound_listen: &str,
    routes: &[UpstreamRoute],
) -> String {
    let sb_routes: Vec<&UpstreamRoute> = routes
        .iter()
        .filter(|r| matches!(r.entry_proto.as_str(), "hysteria2" | "tuic"))
        .collect();
    let mut inbounds = Vec::new();
    if let Some((port, pw, obfs_pw)) = hy2 {
        let mut users = vec![serde_json::json!({ "name": "main", "password": pw })];
        for r in &sb_routes {
            if r.entry_proto == "hysteria2" {
                users.push(serde_json::json!({
                    "name": upstream_user_tag(r.id),
                    "password": r.credential
                }));
            }
        }
        inbounds.push(serde_json::json!({
            "type": "hysteria2", "listen": inbound_listen, "listen_port": port,
            // Salamander 混淆：裸 QUIC 的包特征会被 DPI 识别后限速。
            "obfs": { "type": "salamander", "password": obfs_pw },
            "tls": {
                "enabled": true,
                "certificate_path": "/etc/sing-box/self.crt",
                "key_path": "/etc/sing-box/self.key"
            },
            "users": users
        }));
    }
    if let Some((port, uuid, pw)) = tuic {
        let mut users = vec![serde_json::json!({
            "name": "main", "uuid": uuid, "password": pw
        })];
        for r in &sb_routes {
            if r.entry_proto == "tuic" {
                users.push(serde_json::json!({
                    "name": upstream_user_tag(r.id),
                    "uuid": r.credential,
                    "password": r.credential_password
                }));
            }
        }
        inbounds.push(serde_json::json!({
            "type": "tuic", "listen": inbound_listen, "listen_port": port,
            "tls": {
                "enabled": true,
                "certificate_path": "/etc/sing-box/self.crt",
                "key_path": "/etc/sing-box/self.key",
                // 必须显式声明 h3：不写时 sing-box 的 TLS 服务端不选择 ALPN，
                // mihomo 客户端直接失败 `CRYPTO_ERROR ... server did not select
                // an ALPN protocol`（2026-10-23 真机：服务端在跑、UDP 也到了，
                // 客户端仍 503；补上该行后 gstatic 204，约 0.4s）。
                "alpn": ["h3"]
            },
            "users": users
        }));
    }
    // 出口族写法按版本分支：
    // - >=1.12：出站 domain_resolver 指向 dns.servers 里的 local 解析器；
    // - <1.12：出站 domain_strategy 旧写法；
    // - 探测失败（None）：**两个都不写**，保持 sing-box 默认行为。
    //
    // 最后一条是 2026-10-18 在真实 1.13.20 上用 `sing-box check` 实测修正的：
    // 新版对 domain_strategy 不是「警告」而是 **FATAL**（要求设置
    // ENABLE_DEPRECATED_LEGACY_DOMAIN_STRATEGY_OPTIONS 才继续）。原先「探测失败
    // 按旧版处理」会把正在运行的服务打挂——比出口不生效严重得多。探测失败时省略
    // 整个出口字段，旧版与新版都能加载；本次切换不生效，由调用方在进度里说明。
    let outbound = match version {
        Some((major, minor)) if (major, minor) >= (1, 12) => serde_json::json!({
            "type": "direct",
            "tag": "direct",
            "domain_resolver": { "server": "egress", "strategy": egress.singbox_strategy() }
        }),
        Some(_) => serde_json::json!({
            "type": "direct",
            "tag": "direct",
            "domain_strategy": egress.singbox_strategy()
        }),
        None => serde_json::json!({ "type": "direct", "tag": "direct" }),
    };
    let mut outbounds = vec![outbound];
    for r in &sb_routes {
        outbounds.push(serde_json::json!({
            "type": if r.outbound_proto == "http" { "http" } else { "socks" },
            "tag": upstream_outbound_tag(r.id),
            "server": r.outbound_host,
            "server_port": r.outbound_port,
            "username": r.outbound_username,
            "password": r.outbound_password
        }));
    }
    let mut root = serde_json::json!({
        "inbounds": inbounds,
        "outbounds": outbounds
    });
    if !sb_routes.is_empty() {
        // auth_user 匹配入站用户的 name（hysteria2/tuic 均支持）。主用户叫 main，
        // 不命中这些规则，继续走首条 direct 出站。
        root["route"] = serde_json::json!({
            "rules": sb_routes
                .iter()
                .map(|r| serde_json::json!({
                    "auth_user": [upstream_user_tag(r.id)],
                    "outbound": upstream_outbound_tag(r.id)
                }))
                .collect::<Vec<_>>()
        });
    }
    if version.is_some_and(|(major, minor)| (major, minor) >= (1, 12)) {
        // 新版 DNS 格式：local 类型解析器按出站策略选族
        root["dns"] = serde_json::json!({
            "servers": [{ "type": "local", "tag": "egress" }]
        });
    }
    root.to_string()
}

/// sing-box 的 systemd unit（工具自写）。
pub fn singbox_systemd_unit() -> String {
    "[Unit]\nDescription=sing-box server\nAfter=network.target\n\n[Service]\nExecStart=/usr/local/bin/sing-box run -c /etc/sing-box/config.json\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}

#[cfg(test)]
mod firewall_tests {
    use super::*;

    /// 部署成功的判据必须包括「端口已持久放行」：运行时 iptables 规则活不过重启
    /// （2026-10-23 ashura 重启后广播 IP 节点端口全被 UFW 默认策略挡掉）。
    #[test]
    fn firewall_allow_cmd_uses_persistent_ufw_first() {
        let cmd = firewall_allow_cmd(&[("vless", 443), ("vmess", 8443)]);
        assert!(cmd.contains("command -v ufw"), "必须优先探测 ufw：{cmd}");
        assert!(
            cmd.contains("ufw allow 443/tcp"),
            "缺少 443/tcp 放行：{cmd}"
        );
        assert!(
            cmd.contains("ufw allow 8443/tcp"),
            "缺少 8443/tcp 放行：{cmd}"
        );
        let ufw_at = cmd.find("ufw allow 443/tcp").unwrap();
        let fallback_at = cmd.find("iptables -C INPUT").unwrap();
        assert!(
            ufw_at < fallback_at,
            "ufw 分支必须排在 iptables 回退之前：{cmd}"
        );
    }

    /// 套接字类型随协议：ss 是 tcp_and_udp，Hysteria2/TUIC 只绑 UDP，其余 TCP。
    #[test]
    fn firewall_allow_cmd_maps_protocol_sockets() {
        let cmd = firewall_allow_cmd(&[
            ("ss", 8388),
            ("hysteria2", 2097),
            ("tuic", 2053),
            ("trojan", 4443),
        ]);
        assert!(cmd.contains("ufw allow 8388/tcp"));
        assert!(cmd.contains("ufw allow 8388/udp"));
        assert!(cmd.contains("ufw allow 2097/udp"));
        assert!(!cmd.contains("ufw allow 2097/tcp"));
        assert!(cmd.contains("ufw allow 2053/udp"));
        assert!(!cmd.contains("ufw allow 2053/tcp"));
        assert!(cmd.contains("ufw allow 4443/tcp"));
    }

    /// 无 ufw 时退回 iptables，且 IPv6 入口也必须放行（节点地址可能是 v6）。
    #[test]
    fn firewall_allow_cmd_falls_back_to_iptables_and_ipv6() {
        let cmd = firewall_allow_cmd(&[("vless", 443)]);
        assert!(cmd.contains("iptables -C INPUT -p tcp --dport 443 -j ACCEPT"));
        assert!(cmd.contains("iptables -I INPUT 1 -p tcp --dport 443 -j ACCEPT"));
        assert!(cmd.contains("ip6tables -C INPUT -p tcp --dport 443 -j ACCEPT"));
        assert!(cmd.contains("netfilter-persistent save"));
    }

    /// 同一端口被多个协议选中时只写一次（vless/vmess 共用端口是允许的）。
    #[test]
    fn firewall_allow_cmd_dedups_same_port_socket() {
        let cmd = firewall_allow_cmd(&[("vless", 443), ("vless", 443), ("vmess", 443)]);
        assert_eq!(cmd.matches("ufw allow 443/tcp").count(), 1);
    }

    /// 空协议列表不能生成语法残缺的 shell（if 分支为空会让整条命令失败）。
    #[test]
    fn firewall_allow_cmd_is_noop_for_empty_ports() {
        assert!(firewall_allow_cmd(&[]).trim().is_empty());
    }
}
