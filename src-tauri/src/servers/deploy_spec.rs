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
         && bash -c \"$(cat /tmp/xray-install.sh)\" @ install \
         && rm -f /tmp/xray-install.sh"
    )
}

/// 二进制安装命令（ss 与 sing-box 共用，只有归档类型与可执行文件名不同）。
///
/// 三道闸，顺序不能换：
/// 1. `sha256` 非空时按上游给的哈希校验**归档**（TLS 只保证「从哪来」，不保证
///    「内容是什么」）。`None` 时**不假装校验**，改为把实测哈希打出来，让版本漂移
///    至少看得见（调用方再把这一行记进部署进度）。
/// 2. 解到**临时目录**再点名确认 `bin` 真的在里面——归档哈希对得上只说明拿到的是
///    上游发布的那一份，不说明 `--strip-components=1` 解出来的布局与预期一致。
///    布局一变，光看归档是看不出来的，表现是「安装成功」而服务起不来，报错落在几步
///    之后的 systemd 上。本仓为这个布局已经栽过一次（无 strip-components 时 binary
///    不在 /usr/local/bin）。
/// 3. 只有前两步都过了才 `cp` 进 `/usr/local/bin`：失败时那里不会留下半装状态。
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
        "if ! test -x {dir}/{bin}; then echo '解包后没有 {bin}：归档布局与预期不符' >&2; exit 1; fi"
    );
    format!(
        "curl -fL -o {tmp} '{url}' && {verify} \
         && rm -rf {dir} && mkdir -p {dir} && tar {tar_flag} {tmp} -C {dir} --strip-components=1 \
         && {guard} && sha256sum {dir}/{bin} \
         && cp -a {dir}/. /usr/local/bin/ && rm -rf {dir} {tmp}"
    )
}

/// shadowsocks-rust 安装命令。`tar -xJf` 的 `f` 紧跟归档名；无 `--strip-components`
/// 时解出的是首层目录（binary 不在 /usr/local/bin，systemd 起不来）。
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

/// 部署协议白名单。字符串标识同时作 nodes.protocol 与 params 来源。
pub const PROTOCOLS: [&str; 6] = ["vless", "trojan", "vmess", "ss", "hysteria2", "tuic"];

/// 协议是否在部署白名单内。
pub fn is_supported_protocol(proto: &str) -> bool {
    PROTOCOLS.contains(&proto)
}

/// 协议默认端口表(被占时 allocate_port 逐 +1 偏移)。
pub fn default_port(proto: &str) -> u16 {
    match proto {
        "vless" => 443,
        "trojan" => 4443,
        "vmess" => 8443,
        "ss" => 8388,
        "hysteria2" => 2097,
        "tuic" => 2053,
        _ => 0,
    }
}

/// 解析部署摘要（deploys.latest_summary 的 config_json）中的已部署协议列表；
/// 摘要形状 {"protocols":[...],"ports":{...}}，缺字段/损坏记录容忍为空。
pub fn parse_deployed_protocols(config_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .and_then(|v| {
            v.get("protocols")?.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
        })
        .unwrap_or_default()
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
        let client = ib.pointer("/settings/clients/0");
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
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .pointer("/servers/0/server_port")?
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
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .pointer("/servers/0/password")?
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

/// 解析 sing-box 配置里 hysteria2/tuic 的凭据（重部署复用凭据用）。
///
/// 形状见 [`singbox_config_json`]：`inbounds[]` 各带 `users[0]`——hysteria2 是
/// `password`，tuic 是 `uuid` + `password`。返回 `(hy2 密码, tuic (uuid,密码))`，
/// 缺失或内容损坏一律 `None`（调用方退回新生成）。
pub fn parse_singbox_creds(json: &str) -> (Option<String>, Option<(String, String)>) {
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
            "hysteria2" if hy2.is_none() => hy2 = field("password"),
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

/// Xray 单进程多入站配置:vless/trojan 走 Reality(tcp),vmess 走 WS(明文 /ray,
/// 证书体系属后续范围)。金样见 deploy_tests::xray_multi_inbounds_includes_three_protocols。
pub fn xray_config_json_multi(entries: &[XrayInboundEntry]) -> String {
    let inbounds: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
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
                    serde_json::json!({
                        "clients": [{ "id": e.uuid, "flow": "xtls-rprx-vision" }],
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
                    serde_json::json!({ "clients": [{ "password": e.password }], "fallbacks": [] })
                }
                _ => {
                    let path = if e.ws_path.is_empty() {
                        "/ray"
                    } else {
                        e.ws_path.as_str()
                    };
                    stream.insert("network".into(), "ws".into());
                    stream.insert("wsSettings".into(), serde_json::json!({ "path": path }));
                    serde_json::json!({ "clients": [{ "id": e.uuid, "alterId": 0 }] })
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
    serde_json::json!({
        "inbounds": inbounds,
        // 出口钉死 IPv4。默认的 AsIs 把域名交给 Go 的解析器，按 RFC 6724
        // **优先 IPv6**，而 Google 对机房 IPv6 段的地理判定是坏的：同一个出口 IP，
        // v6 出去报「Gemini 不支持你所在的地区」，v4 出去正常（2026-09-20 实测，
        // 并有多例社区同证）。
        //
        // domainStrategy 必须写在 settings 里：写出站顶层会被 Xray 忽略而**静默失效**。
        // 非 AsIs 会让下游 sockopt 的 domainStrategy/happyEyeballs 失效——这里要的
        // 正是它，不是副作用。
        "outbounds": [{
            "protocol": "freedom",
            "tag": "direct",
            "settings": { "domainStrategy": "UseIPv4" }
        }]
    })
    .to_string()
}

/// SS 服务端配置（2022-blake3-aes-256-gcm）。
pub fn ss_config_json(port: u16, password_b64: &str) -> String {
    serde_json::json!({
        "servers": [{
            "server": "0.0.0.0", "server_port": port,
            "method": "2022-blake3-aes-256-gcm", "password": password_b64, "mode": "tcp_and_udp"
        }],
        "locals": []
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
/// 注意：sing-box 两个入站**强制要求显式 tls 块**（无 certificate 直接 FATAL
/// “TLS required”，实测 h2/tuic 部署失败 exit=3）。部署器先幂等生成自签证书
/// （/etc/sing-box/self.crt/key），客户端 skip-cert-verify 跳过校验。
pub fn singbox_config_json(hy2: Option<(u16, &str)>, tuic: Option<(u16, &str, &str)>) -> String {
    let mut inbounds = Vec::new();
    if let Some((port, pw)) = hy2 {
        inbounds.push(serde_json::json!({
            "type": "hysteria2", "listen": "0.0.0.0", "listen_port": port,
            "tls": {
                "enabled": true,
                "certificate_path": "/etc/sing-box/self.crt",
                "key_path": "/etc/sing-box/self.key"
            },
            "users": [{ "password": pw }]
        }));
    }
    if let Some((port, uuid, pw)) = tuic {
        inbounds.push(serde_json::json!({
            "type": "tuic", "listen": "0.0.0.0", "listen_port": port,
            "tls": {
                "enabled": true,
                "certificate_path": "/etc/sing-box/self.crt",
                "key_path": "/etc/sing-box/self.key"
            },
            "users": [{ "uuid": uuid, "password": pw }]
        }));
    }
    serde_json::json!({
        "inbounds": inbounds,
        "outbounds": [{ "type": "direct" }]
    })
    .to_string()
}

/// sing-box 的 systemd unit（工具自写）。
pub fn singbox_systemd_unit() -> String {
    "[Unit]\nDescription=sing-box server\nAfter=network.target\n\n[Service]\nExecStart=/usr/local/bin/sing-box run -c /etc/sing-box/config.json\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}
