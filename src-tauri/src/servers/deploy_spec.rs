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
pub fn xray_config_json_multi(entries: &[XrayInboundEntry], egress: EgressFamily) -> String {
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
        // 出口地址族由服务器行的 egress_family 决定（默认 v4）。默认的 AsIs 把域名
        // 交给 Go 的解析器，按 RFC 6724 **优先 IPv6**，而 Google 对机房 IPv6 段的
        // 地理判定是坏的：同一个出口 IP，v6 出去报「Gemini 不支持你所在的地区」，
        // v4 出去正常（2026-09-20 实测，并有多例社区同证）——所以默认钉 IPv4，
        // 只有用户显式选择 v6 才写 UseIPv6。对域名目标是严格的；字面 IP 目标不受影响。
        //
        // domainStrategy 必须写在 settings 里：写出站顶层会被 Xray 忽略而**静默失效**。
        // 非 AsIs 会让下游 sockopt 的 domainStrategy/happyEyeballs 失效——这里要的
        // 正是它，不是副作用。
        "outbounds": [{
            "protocol": "freedom",
            "tag": "direct",
            "settings": { "domainStrategy": egress.xray_domain_strategy() }
        }]
    })
    .to_string()
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
/// 注意：sing-box 两个入站**强制要求显式 tls 块**（无 certificate 直接 FATAL
/// “TLS required”，实测 h2/tuic 部署失败 exit=3）。部署器先幂等生成自签证书
/// （/etc/sing-box/self.crt/key），客户端 skip-cert-verify 跳过校验。
///
/// `inbound_listen` 必须跟着**客户端实际使用的地址族**走：sing-box 的
/// `0.0.0.0` 是纯 IPv4（实测 `ss -uln` 显示 `0.0.0.0:2097`），而香港这类机器
/// 登记的是 IPv6 地址，客户端把 UDP 发到 `[v6]:2097` 永远到不了；Xray 的
/// `0.0.0.0` 实际是双栈（`ss -tln` 显示 `*:443`），所以只有 h2/tuic 会坏。
pub fn singbox_config_json(
    hy2: Option<(u16, &str)>,
    tuic: Option<(u16, &str, &str)>,
    egress: EgressFamily,
    version: Option<(u32, u32)>,
    inbound_listen: &str,
) -> String {
    let mut inbounds = Vec::new();
    if let Some((port, pw)) = hy2 {
        inbounds.push(serde_json::json!({
            "type": "hysteria2", "listen": inbound_listen, "listen_port": port,
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
            "users": [{ "uuid": uuid, "password": pw }]
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
            "domain_resolver": { "server": "egress", "strategy": egress.singbox_strategy() }
        }),
        Some(_) => serde_json::json!({
            "type": "direct",
            "domain_strategy": egress.singbox_strategy()
        }),
        None => serde_json::json!({ "type": "direct" }),
    };
    let mut root = serde_json::json!({
        "inbounds": inbounds,
        "outbounds": [outbound]
    });
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
