//! 落地机一键部署管道：探测→安装→配置→启动→写回。
//! 输入：DeployRequest（job_id/server_id/kind/port/reality_dest）；输出：进度事件与落库结果。
//! 命令序列白名单化：本文件是唯一构造远端命令的位置，可变参数经校验后拼接；
//! 远端文件写入走「echo base64 | base64 -d > 白名单路径」管道。
//! 取消为协作式：步骤边界 check_cancel + 单命令内 select 轮询（50ms）。
//! 依赖：ssh 适配器、servers::load_auth_parts、repos、config 缓存重建、注入式 SS 版本查询。

use crate::db::repos;
use crate::domain::{JobProgress, Node};
use crate::jobs::{DeployRequest, EmitFn, JobRequest, JobRunner};
use crate::servers::{load_auth_parts, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use uuid::Uuid;

pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
pub const CONFIG_TIMEOUT: Duration = Duration::from_secs(60);
/// 启动后的状态验证（is-active / 端口监听）：失败重试等场景下 30s 足够，
/// 与启动命令的 60s 配置超时区分开，避免验证阶段长时间卡死。
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// 远端写文件白名单路径（常量，禁止动态构造）；pub(crate)：relay::build 收敛复用。
pub(crate) const XRAY_CONFIG_PATH: &str = "/usr/local/etc/xray/config.json";
pub(crate) const SS_CONFIG_PATH: &str = "/etc/shadowsocks-rust/config.json";
const SS_UNIT_PATH: &str = "/etc/systemd/system/ssserver.service";
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

/// 查看某台服务器已部署协议：依据最新一次部署摘要（无记录 → 空列表）。
pub fn deployed_protocols(state: &Arc<AppState>, server_id: i64) -> Result<Vec<String>, String> {
    let conn = state.db.lock();
    match repos::deploys::latest_summary(&conn, server_id) {
        Ok(Some(s)) => Ok(parse_deployed_protocols(&s)),
        Ok(None) => Ok(vec![]),
        Err(e) => Err(e.to_string()),
    }
}

/// 端口占用检查：true=端口已被监听（占用），false=空闲可部署。
async fn probe_port(sess: &mut SshSession, cancel: &AtomicBool, port: u16) -> Result<bool, String> {
    Ok(
        run_cmd(sess, cancel, "探测", &port_probe_cmd(port), PROBE_TIMEOUT)
            .await?
            .exit_code
            == 0,
    )
}

/// 从 base 起探测空闲端口(逐 +1),最多 20 个连续候选;全占报错。
async fn allocate_port(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    base: u16,
) -> Result<u16, String> {
    for n in 0..21u16 {
        let cand = base.saturating_add(n);
        if !probe_port(sess, cancel, cand).await? {
            return Ok(cand);
        }
    }
    Err(format!("端口 {base} 起 20 个连续端口全被占用"))
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
fn public_key_of(params_json: &str) -> Option<String> {
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

/// 读取远端已部署配置中的端口表（重部署复用：云侧安全组/防火墙通常只放行首次部署
/// 端口，漂移会直接打不通——实测 trojan 4443→4444 被云商策略性封禁后客户端 SYN 超时）。
/// 文件不存在/退出码非 0 → 无既有端口（首次部署）。
async fn read_existing_ports(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    need_xray: bool,
    need_ss: bool,
    need_singbox: bool,
) -> Result<ExistingRemote, String> {
    let mut ports: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    let mut xray_inbounds: std::collections::HashMap<u16, XrayInboundEntry> =
        std::collections::HashMap::new();
    let mut ss_password: Option<String> = None;
    let mut singbox_hy2_pw: Option<String> = None;
    let mut singbox_tuic_uuid: Option<String> = None;
    let mut singbox_tuic_pw: Option<String> = None;
    if need_xray {
        // 读失败即中止（`?`），理由同下面的 sing-box 分支：退化成「没有既有部署」
        // 就会重新分配端口、重新生成 reality 私钥并整份重渲染 config.json，
        // 而客户端手里还是旧参数；远端失败还会被 `2>/dev/null` 吞掉。
        let existing =
            read_remote_file(sess, cancel, "探测", XRAY_CONFIG_PATH, PROBE_TIMEOUT).await?;
        if let RemoteFile::Content(c) = &existing {
            for (proto, port) in parse_xray_inbound_ports(c) {
                ports.insert(proto, port);
            }
            // 同一份配置里顺带还原凭据：重复部署靠它复用而不是重新生成（见 run_deploy）
            xray_inbounds = parse_xray_inbounds(c);
        }
    }
    if need_ss {
        let existing =
            read_remote_file(sess, cancel, "探测", SS_CONFIG_PATH, PROBE_TIMEOUT).await?;
        if let RemoteFile::Content(c) = &existing {
            if let Some(p) = parse_ss_port(c) {
                ports.insert("ss".into(), p);
            }
            ss_password = parse_ss_password(c);
        }
    }
    if need_singbox {
        // 读失败即中止（`?`）：读不回来就会按「首次部署」重新生成凭据与端口，
        // 而客户端手里还是旧凭据——服务端新密码 + 客户端旧密码 = 静默变砖。
        let existing =
            read_remote_file(sess, cancel, "探测", SINGBOX_CONFIG_PATH, PROBE_TIMEOUT).await?;
        if let RemoteFile::Content(c) = &existing {
            for (proto, port) in parse_singbox_ports(c) {
                ports.insert(proto, port);
            }
            let (hy2, tuic) = parse_singbox_creds(c);
            singbox_hy2_pw = hy2;
            if let Some((uuid, pw)) = tuic {
                singbox_tuic_uuid = Some(uuid);
                singbox_tuic_pw = Some(pw);
            }
        }
    }
    Ok(ExistingRemote {
        ports,
        xray_inbounds,
        ss_password,
        singbox_hy2_pw,
        singbox_tuic_uuid,
        singbox_tuic_pw,
    })
}

/// 远端既有部署的明细：端口表（重部署复用端口）+ 凭据（重部署复用凭据，不轮换）。
///
/// 两件事都要读同一批文件，所以一次读完一起带回来——分两次读会让「探测」阶段
/// 多跑 2~3 条 SSH 命令，也会让两份数据有可能来自不同时刻的配置。
struct ExistingRemote {
    ports: std::collections::HashMap<String, u16>,
    /// 按端口索引的 xray 入站（含 reality 私钥，见 [`parse_xray_inbounds`]）
    xray_inbounds: std::collections::HashMap<u16, XrayInboundEntry>,
    /// shadowsocks 既有密码（复用避免轮换）
    ss_password: Option<String>,
    /// sing-box hysteria2 既有密码（同上，缺失才新生成）
    singbox_hy2_pw: Option<String>,
    /// sing-box tuic 既有 uuid / 密码（同上）
    singbox_tuic_uuid: Option<String>,
    singbox_tuic_pw: Option<String>,
}

/// 生产 runner：按 JobRequest 分发给部署/建链/拆链/设备 peer 执行器。
/// state 经 OnceLock 注入：AppState 构造存在循环依赖（runner 需要 state，
/// state 需要 runner），worker 惰性启动保证首次 submit 前必已注入。
pub fn prod_runner(
    state: Arc<OnceLock<Arc<AppState>>>,
    http: reqwest::Client,
    ss_api_base: String,
    singbox_api_base: String,
) -> Arc<dyn JobRunner> {
    Arc::new(ProdRunner {
        state,
        http,
        ss_api_base,
        singbox_api_base,
    })
}

struct ProdRunner {
    state: Arc<OnceLock<Arc<AppState>>>,
    http: reqwest::Client,
    ss_api_base: String,
    singbox_api_base: String,
}

impl ProdRunner {
    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .get()
            .cloned()
            .ok_or_else(|| "应用状态未就绪".to_string())
    }
}

#[async_trait::async_trait]
impl JobRunner for ProdRunner {
    async fn run(
        &self,
        req: JobRequest,
        cancel: Arc<AtomicBool>,
        emit: EmitFn,
    ) -> Result<(), String> {
        let state = self.state()?;
        match req {
            JobRequest::Deploy(r) => self.run_deploy(&state, r, &cancel, &emit).await,
            JobRequest::Upstream(r) => {
                crate::servers::upstream::run_upstream_job(&state, &r, &cancel, &emit).await
            }
            JobRequest::Privacy(r) => {
                crate::servers::privacy::run_privacy_job(&state, &r, &cancel, &emit).await
            }
            JobRequest::PrivacyRemove(r) => {
                crate::servers::privacy::run_privacy_remove_job(&state, &r, &cancel, &emit).await
            }
            JobRequest::LinkBuild(r) => {
                crate::relay::build::run_build(&state, r, &cancel, &emit).await
            }
            JobRequest::LinkTeardown(r) => {
                crate::relay::build::run_teardown(&state, r, &cancel, &emit).await
            }
            JobRequest::PeerAdd(r) => {
                crate::relay::peers::run_peer_add(&state, r, &cancel, &emit).await
            }
            JobRequest::PeerRemove(r) => {
                crate::relay::peers::run_peer_remove(&state, r, &cancel, &emit).await
            }
            JobRequest::SpeedTest(r) => {
                crate::speed::run_speed_job(&state, r, cancel.clone(), emit.clone()).await
            }
            JobRequest::ServerSpeed(r) => {
                crate::server_speed::run_server_speed_job(state, r, cancel.clone(), emit.clone())
                    .await
            }
            JobRequest::Throughput(r) => {
                crate::server_speed::iperf::run_throughput_job(
                    &state,
                    r,
                    cancel.clone(),
                    emit.clone(),
                )
                .await
            }
        }
    }
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

/// 一次发布查询的结果：版本、文件名、以及**上游给的** sha256。
///
/// `sha256` 为 `None` 表示上游没给 digest（老发布或换了算法）：此时**不校验**，
/// 改为把实测哈希打出来让漂移可见——绝不塞一个恒真的校验冒充「钉住了」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub tag: String,
    pub asset: String,
    pub sha256: Option<String>,
}

/// 从发布资产的 `digest` 字段取 sha256 十六进制值。
/// GitHub 的形状是 `"digest": "sha256:<hex>"`；其它算法或缺失一律 None
/// （`sha256sum -c` 只认 sha256，拿别的算法去校验只会误报）。
fn parse_asset_digest(asset: &serde_json::Value) -> Option<String> {
    asset
        .get("digest")?
        .as_str()?
        .strip_prefix("sha256:")
        .map(|s| s.to_string())
}

/// 上游是否为发布资产提供构建来源证明（GitHub Artifact Attestation）。
///
/// 这是**实测结论**，不是推断（2026-09-27 实测）：
/// - sing-box：`{api}/attestations/sha256:<digest>` 返回 bundle 数组；
///   `gh attestation verify` 对真归档退出 0，对「篡改 1 字节 / 换个仓库 / 随机文件」
///   三种阴性样本一律退出 1——该端点确实在判别，不是恒真。
/// - shadowsocks-rust：同一端点对其真实资产 digest 返回 404，上游不发布。
///
/// 上游若变更发布流程（不再签 attestation），这条策略要跟着改；改错的方向是
/// 「本该中止却放过」，所以宁可让它响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// 上游发布来源证明：digest 上查不到即视为篡改信号，中止部署。
    Attested,
    /// 上游不发布来源证明：无从核验，如实记录，不假装已核验。
    NotPublished,
}

/// 构建来源核验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceOutcome {
    /// 查到 `count` 份 attestation：该 digest 由该仓库的 CI 产出。
    Confirmed { count: usize },
    /// 上游不发布来源证明，未做核验。
    NotPublished,
}

/// digest 查不到 attestation 时的统一错误：404 与 200+空数组是同一个结论。
fn missing_attestation_err(label: &str, sha: &str) -> String {
    format!(
        "{label} 的 digest {sha} 查不到构建来源证明（attestation），已中止部署：\
         该资产不是上游 CI 产出的，可能已被替换。\
         若上游已改为不发布 attestation，请更新 Provenance 策略。"
    )
}

/// 核验发布资产的构建来源，确认该 digest 是否有上游 CI 的 attestation。
///
/// **能挡什么**：偷到 GitHub token 的人能把木马换进 release——他改得了字节，
/// 也改得了 API 报的 digest，但**签不出 attestation**（attestation 由仓库 workflow
/// 的 OIDC 身份签出，PAT 伪造不了）。此时本函数对那个 digest 查不到 attestation，
/// 部署中止。这是现有链路唯一没覆盖的一段：digest 校验挡得住服务端网络上的 MITM
/// （预期值由本机经 SSH 送达），却挡不住「上传资产的人自己就是攻击者」。
///
/// **边界（不假装比实际更强）**：这不是完整 sigstore 验签——不验证 Fulcio 证书链、
/// 不验证 Rekor 包含证明、不校验签名，只确认 GitHub 的 attestations 端点对该
/// repo+digest 返回了非空 bundle 列表。完整验签需要 `sigstore` crate，而它硬依赖
/// `aws-lc-rs`（Windows 上要 NASM）——正是本项目为 russh 刻意移除的依赖
/// （见 Cargo.toml 里 `default-features = false, features = ["ring"]` 的说明）。
/// 因此本核验的信任锚是 GitHub API 本身，与「digest 也取自同一个 API」一致：
/// 没有引入新的信任假设，也没有比它更强。
pub async fn verify_provenance(
    http: &reqwest::Client,
    api_base: &str,
    label: &str,
    policy: Provenance,
    sha256: Option<&str>,
) -> Result<ProvenanceOutcome, String> {
    if policy == Provenance::NotPublished {
        return Ok(ProvenanceOutcome::NotPublished);
    }
    // 没有 digest 就无从核验。上游若一边给 attestation 一边不给 digest，
    // 不能默认「没事」——那正是「假装钉住了」。
    let Some(sha) = sha256 else {
        return Err(format!(
            "{label} 的发布资产没有 digest，无法核验构建来源，已中止部署"
        ));
    };
    let url = format!("{api_base}/attestations/sha256:{sha}");
    let resp = http
        .get(url)
        .header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("核验 {label} 构建来源失败（网络）: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(missing_attestation_err(label, sha));
    }
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("核验 {label} 构建来源失败（GitHub API 错误）: {e}"))?;
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("核验 {label} 构建来源失败（响应解析失败）: {e}"))?;
    // 200 + 空数组是「查得到但为空」的形状，与 404 是同一个结论：
    // 只判 status 会把这种情况误放行。
    let count = j["attestations"].as_array().map_or(0, |a| a.len());
    if count == 0 {
        return Err(missing_attestation_err(label, sha));
    }
    Ok(ProvenanceOutcome::Confirmed { count })
}

/// 查询 sing-box 最新发布资产名（api_base 注入：生产为 GitHub API，测试为 wiremock）。
/// 下载 URL 由调用方以固定前缀拼接；tag 与资产名拼入 URL 前必须过字符集白名单。
pub async fn latest_singbox_asset(
    http: &reqwest::Client,
    api_base: &str,
    target: &str,
) -> Result<ReleaseAsset, String> {
    let url = format!("{api_base}/releases/latest");
    let resp = http
        .get(url)
        .header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("查询 sing-box 最新版本失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API 错误: {e}"))?;
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API 响应解析失败: {e}"))?;
    let tag = j["tag_name"]
        .as_str()
        .ok_or("GitHub API 响应缺少 tag_name")?
        .to_string();
    // sing-box 发布资产名按平台而非 Rust 三元组命名（如 sing-box-1.11.11-linux-amd64.tar.gz）；
    // 把三原组映射到其平台后缀再匹配。
    let suffix = match target {
        "x86_64-unknown-linux-gnu" => "linux-amd64.tar.gz",
        "aarch64-unknown-linux-gnu" => "linux-arm64.tar.gz",
        t => return Err(format!("暂不支持的 sing-box 目标架构: {t}")),
    };
    let assets = j["assets"].as_array().ok_or("GitHub API 响应缺少 assets")?;
    let picked = assets
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
        .ok_or_else(|| format!("未找到 {suffix} 的发布资产"))?;
    let asset = picked["name"]
        .as_str()
        .ok_or("发布资产缺少 name")?
        .to_string();
    if !is_safe_url_fragment(&tag) || !is_safe_url_fragment(&asset) {
        return Err("资产名不合法".into());
    }
    Ok(ReleaseAsset {
        tag,
        asset,
        sha256: parse_asset_digest(picked),
    })
}

/// 查询 shadowsocks-rust 最新发布资产名（api_base 注入：生产为 GitHub API，测试为 wiremock）。
/// 下载 URL 由调用方以固定前缀拼接，本函数只解析 tag 与匹配 target 的资产名；
/// tag 与资产名拼入 URL 前必须过字符集白名单（字母数字点下划线连字符）。
pub async fn latest_ss_asset(
    http: &reqwest::Client,
    api_base: &str,
    target: &str,
) -> Result<ReleaseAsset, String> {
    let url = format!("{api_base}/releases/latest");
    let resp = http
        .get(url)
        .header("User-Agent", "vpn-ctl")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("查询 SS 最新版本失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API 错误: {e}"))?;
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API 响应解析失败: {e}"))?;
    let tag = j["tag_name"]
        .as_str()
        .ok_or("GitHub API 响应缺少 tag_name")?
        .to_string();
    let assets = j["assets"].as_array().ok_or("GitHub API 响应缺少 assets")?;
    let suffix = format!("{target}.tar.xz");
    let picked = assets
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(&suffix)))
        .ok_or_else(|| format!("未找到 {target} 的发布资产"))?;
    let asset = picked["name"]
        .as_str()
        .ok_or("发布资产缺少 name")?
        .to_string();
    // 拼 URL 前校验：非法字符（引号、空白等）会被注入到远端下载命令
    if !is_safe_url_fragment(&tag) || !is_safe_url_fragment(&asset) {
        return Err("资产名不合法".into());
    }
    Ok(ReleaseAsset {
        tag,
        asset,
        sha256: parse_asset_digest(picked),
    })
}

/// URL 片段字符集白名单：字母数字点下划线连字符，且非空。
fn is_safe_url_fragment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// 上游没给校验和时，把 `sha256sum` 实测出来的那一行记进部署进度。
///
/// 这是「不假装钉住了」的落地：`ReleaseAsset::sha256` 为 `None` 时下载命令里没有
/// 校验（见 [`binary_install_cmd`]），只能把实测哈希**如实记下来**——版本漂移
/// 至少留在部署记录里，而不是无声无息。已经校验过的情形什么都不做（校验失败时
/// 命令本身就失败了，根本走不到这里）。
fn emit_measured_hash(
    emit: &impl Fn(&str, &str, i64),
    archive: &str,
    rel: &ReleaseAsset,
    out: &crate::ssh::CommandOutput,
) {
    if rel.sha256.is_some() {
        return;
    }
    let measured = out
        .stdout
        .lines()
        .find(|l| l.contains(archive))
        .unwrap_or("（远端未回显 sha256sum 输出）")
        .trim();
    emit(
        "安装",
        &format!(
            "上游 {} 未提供校验和，实测 {}: {measured}",
            rel.tag, rel.asset
        ),
        46,
    );
}

/// 把**实际装上去的那个可执行文件**的实测 sha256 记进部署进度。
///
/// 这是 `scripts/fetch-tor.mjs` 那条「归档 + 解出来的 exe」双哈希链在这一侧的
/// 对应物：tor 那边的 exe 哈希是常量（版本写死），而这里的版本是运行时才定的
/// （`releases/latest`），钉不住预期值——那就把实测值留下来，至少让「服务器上装的
/// 到底是哪一份」查得到，而不是只有一个「装好了」。
/// 远端没回显时也要出声：静默通过等于这条留痕根本不存在。
fn emit_installed_bin_hash(
    emit: &impl Fn(&str, &str, i64),
    bin: &str,
    out: &crate::ssh::CommandOutput,
) {
    let measured = out
        .stdout
        .lines()
        .find(|l| l.contains(bin))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("（远端未回显 sha256sum 输出）");
    emit("安装", &format!("已安装 {bin} sha256: {measured}"), 48);
}

/// 远端文件读取结果：区分「文件不存在」（正常）与「读取失败」（异常）。
///
/// 为什么需要这个类型：本仓多处把「读失败」与「文件不存在」当成同一件事
/// （`relay/build.rs` 的 `read_conf` 把非零退出归成空串、`servers/privacy.rs` 的
/// `cat … 2>/dev/null || true`），后果是读失败时按「首次部署」处理——覆盖用户
/// 既有配置、轮换本不该动的密钥，且表面上什么都看不出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteFile {
    /// 文件不存在（`[ -e ]` 为假）。
    Absent,
    /// 文件存在，内容逐字带回（**空文件是 `Content("")`**，与 Absent 不同：
    /// 前者要保留其余段合并，后者才整份渲染）。
    Content(String),
}

impl RemoteFile {
    /// 内容视图：`Absent` 视作空串。
    ///
    /// 只用在「按内容决定取舍」的**下游**函数（渲染/合并/解析私钥——它们本来就把
    /// 空配置当首建处理）。**判断「文件在不在」必须显式 `matches!(.., Absent)`**：
    /// 这个访问器存在的意义只是别让下游把两个变体各写一遍，不是抹平二者差异。
    pub(crate) fn content(&self) -> &str {
        match self {
            RemoteFile::Absent => "",
            RemoteFile::Content(c) => c,
        }
    }
}

/// 读远端文件：存在则 cat（exit 0），不存在则 exit 42，其它非零 = 读失败。
///
/// **单次往返**同时取回内容与存在性，不必「先 test 再 cat」跑两条命令（也就没有
/// 两次读取之间文件被改动的窗口）。`[ -e ]` 为真但 cat 失败（权限/IO）→ cat 非零
/// → 落 Err，正是我们要的语义。
pub(crate) async fn read_remote_file(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    path: &str,
    timeout: Duration,
) -> Result<RemoteFile, String> {
    let script = format!("if [ -e '{path}' ]; then cat '{path}'; else exit 42; fi");
    let out = run_cmd(sess, cancel, step, &script, timeout).await?;
    match out.exit_code {
        0 => Ok(RemoteFile::Content(out.stdout)),
        42 => Ok(RemoteFile::Absent),
        code => {
            let tail = crate::util::stderr_tail(&out.stderr);
            Err(format!(
                "读取 {path} 失败(exit={code}): {}",
                if tail.trim().is_empty() {
                    "远端无错误输出"
                } else {
                    tail.trim_end()
                }
            ))
        }
    }
}

/// 远端写文件：mkdir -p 父目录 + 临时文件 + chmod + `mv -f` 原子替换。
/// 路径为常量白名单，故无注入面（内容整体 base64 后单引号包裹）。
/// pub(crate)：relay::build 收敛写回与 peers 下发复用。
///
/// **原子性**：原实现 `> '{path}'` 是 truncate-then-stream，取消/超时/断连
/// （都是常态）会留下截断的配置，服务起不来而不报原因。
///
/// **权限**：按路径判定，**不加参数**——本批缺陷的形态就是「某条路径上忘了应用」，
/// 可选参数等于给未来的调用点留同一个坑。`wg0.conf` 内含 PrivateKey，首次写入按
/// umask 会落 644（`mkdir -p` 还会把 /etc/wireguard 建成 755），而 `wg-quick`
/// 自己都会警告 world accessible。
///
/// 顺带一提：`chmod` 不能省。改成 `> tmp && mv` 之后，tmp 的模式会**替换**掉原文件
/// 的模式，省掉 chmod 反而会把本来就是 600 的 wg0.conf 降级成 644。
pub(crate) async fn write_remote_file(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    remote_path: &str,
    content: &str,
    timeout: Duration,
) -> Result<(), String> {
    let b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        content.as_bytes(),
    );
    let parent = remote_path.rsplit_once('/').map(|(p, _)| p).unwrap_or(".");
    let (mode, dir_mode) = remote_perms(remote_path);
    let mut cmd = format!("mkdir -p '{parent}' && ");
    if let Some(dm) = dir_mode {
        // 显式 chmod 而非 `mkdir -p -m`：后者只对**新建**目录生效，已存在却是 755 的
        // /etc/wireguard 它修不了——而恰恰是这种情况最需要修。
        // 顺序必须在 `mkdir -p` **之后**：目录还不存在时 chmod 会失败，`&&` 链直接
        // 短路，首次写 wg0.conf 一个字节都写不进去。
        cmd.push_str(&format!("chmod {dm:o} '{parent}' && "));
    }
    cmd.push_str(&format!(
        "echo '{b64}' | base64 -d > '{remote_path}.new' \
         && chmod {mode:o} '{remote_path}.new' \
         && mv -f '{remote_path}.new' '{remote_path}'"
    ));
    run_cmd_ok(sess, cancel, step, &cmd, timeout).await?;
    Ok(())
}

/// 远端文件权限策略：路径 → (文件 mode, 父目录 mode)。
///
/// 新增敏感文件时**必须**在这里登记，否则落 umask 默认（通常 644）。
fn remote_perms(path: &str) -> (u32, Option<u32>) {
    if path == crate::relay::WG_CONF_PATH || path == crate::relay::WG_BAK_PATH {
        (0o600, Some(0o700))
    } else {
        (0o644, None)
    }
}

/// 取消感知的单命令执行：取消标志置位即中断（不等命令超时）。
/// pub(crate)：relay::build/peers 建链拆链与设备下发复用。
pub(crate) async fn run_cmd(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    cmd: &str,
    timeout: Duration,
) -> Result<crate::ssh::CommandOutput, String> {
    tokio::select! {
        out = sess.run(step, cmd, timeout) => out.map_err(|e| e.to_string()),
        _ = async {
            loop {
                if cancel.load(Ordering::SeqCst) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => Err("已取消".into()),
    }
}

/// 等服务进入 active 的等待预算：服务实际启动在毫秒级，这里给足余量只为扛住
/// 重启时的过渡态；超时再报错。
pub const ACTIVATE_BUDGET: Duration = Duration::from_secs(10);

/// 等服务进入 active，按**状态**判成败。
///
/// 为什么不能直接 `run_cmd_ok("systemctl is-active …")`：`is-active` 在服务没起来时
/// **本身就返回非零**（未激活 = 退出码 3），`run_cmd_ok` 会先一步把它当成「命令执行
/// 失败」抛出去——错误只剩一句「启动 执行失败(exit=3): 」加一片空白，而紧接着那句
/// 精心写的「xxx 服务未进入 active 状态: …」永远执行不到（2026-09-18 实测踩过）。
///
/// 分状态处理：`active` 成功；`failed` 是终态，再等也不会自己起来，立刻把状态报出去；
/// 其余（activating / inactive / 空）算过渡态，轮询到预算耗尽再报最后一次读到的状态。
pub(crate) async fn wait_service_active(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    unit: &str,
) -> Result<(), String> {
    let cmd = format!("systemctl is-active {unit}");
    let deadline = std::time::Instant::now() + ACTIVATE_BUDGET;
    let mut last = String::new();
    loop {
        // 用 run_cmd 而非 run_cmd_ok：未激活时非零退出是**预期内**的探测结果
        let out = run_cmd(sess, cancel, "启动", &cmd, VERIFY_TIMEOUT).await?;
        let state = out.stdout.trim().to_string();
        if state == "active" {
            return Ok(());
        }
        if state == "failed" {
            return Err(format!("{unit} 服务启动失败（systemctl：failed）"));
        }
        if !state.is_empty() {
            last = state;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{unit} 服务未进入 active 状态（systemctl：{}）",
                if last.is_empty() { "无输出" } else { &last }
            ));
        }
        // 取消感知的 200ms 间隔
        for _ in 0..4 {
            if cancel.load(Ordering::SeqCst) {
                return Err("已取消".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// 取消感知 + 要求退出码为 0。
/// pub(crate)：relay::build/peers 建链拆链与设备下发复用。
pub(crate) async fn run_cmd_ok(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    step: &str,
    cmd: &str,
    timeout: Duration,
) -> Result<crate::ssh::CommandOutput, String> {
    tokio::select! {
        out = sess.run_ok(step, cmd, timeout) => out.map_err(|e| e.to_string()),
        _ = async {
            loop {
                if cancel.load(Ordering::SeqCst) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => Err("已取消".into()),
    }
}

impl ProdRunner {
    /// 部署主线。步骤：探测(5→20)→安装(30→45)→配置(55)→启动(80)→优化(88→90，仅勾选 BBR)
    /// →写回(95→100)。进度单调递增；全协议验证失败在「优化」之前中止，不发 95/100。
    /// 多协议：一次会话内按选中协议并行完成安装/配置/启动/验证；
    /// 单协议失败仅标记 failed_protocols，其余协议继续验证与写回（失败隔离）。
    async fn run_deploy(
        &self,
        state: &Arc<AppState>,
        req: DeployRequest,
        cancel: &Arc<AtomicBool>,
        emit: &EmitFn,
    ) -> Result<(), String> {
        let server = {
            let conn = state.db.lock();
            repos::servers::get(&conn, req.server_id)
                .map_err(|e| e.to_string())?
                .ok_or("服务器不存在")?
        };
        let need_xray = ["vless", "trojan", "vmess"]
            .iter()
            .any(|p| req.protocols.iter().any(|x| x == p));
        let need_ss = req.protocols.iter().any(|x| x == "ss");
        let need_singbox = ["hysteria2", "tuic"]
            .iter()
            .any(|p| req.protocols.iter().any(|x| x == p));
        let emit_progress = |step: &str, message: &str, progress: i64| {
            emit(JobProgress {
                job_id: req.job_id,
                kind: "deploy".into(),
                step: step.to_string(),
                message: message.to_string(),
                progress,
                error: None,
            });
        };
        emit_progress("探测", "连接服务器", 5);
        let (auth, _) =
            load_auth_parts(&state.db, &state.creds, req.server_id).map_err(|e| e.to_string())?;
        let mut sess = SshSession::connect(
            &server.host,
            server.port as u16,
            &server.user,
            &auth,
            CONNECT_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;

        // 1 探测（dry-run，不落配置）
        check_cancel(cancel)?;
        let arch = run_cmd_ok(&mut sess, cancel, "探测", "uname -m", PROBE_TIMEOUT)
            .await?
            .stdout;
        let arch = arch.trim().to_string();
        let target = match arch.as_str() {
            "x86_64" => "x86_64-unknown-linux-gnu",
            "aarch64" => "aarch64-unknown-linux-gnu",
            other => return Err(format!("暂不支持的架构: {other}")),
        };
        check_cancel(cancel)?;
        let caps = run_cmd(
            &mut sess,
            cancel,
            "探测",
            CAPABILITY_PROBE_SCRIPT,
            PROBE_TIMEOUT,
        )
        .await?
        .stdout;
        let (os, has_systemd) = parse_capability(&caps);
        if !has_systemd {
            return Err(format!("该系统未运行 systemd（{os}），暂不支持自动部署"));
        }
        check_cancel(cancel)?;
        // 组件安装状态：只探测选中协议需要的组件（对未选组件不产生命令）
        let xray_installed = if need_xray {
            run_cmd(&mut sess, cancel, "探测", "command -v xray", PROBE_TIMEOUT)
                .await?
                .exit_code
                == 0
        } else {
            true
        };
        let ss_installed = if need_ss {
            run_cmd(
                &mut sess,
                cancel,
                "探测",
                "command -v ssserver",
                PROBE_TIMEOUT,
            )
            .await?
            .exit_code
                == 0
        } else {
            true
        };
        let singbox_installed = if need_singbox {
            run_cmd(
                &mut sess,
                cancel,
                "探测",
                "command -v sing-box",
                PROBE_TIMEOUT,
            )
            .await?
            .exit_code
                == 0
        } else {
            true
        };
        // 端口分配：已部署协议复用远端既有端口（不重新探测——防漂移撞防火墙/安全组）；
        // 新协议才探测分配（占用偏移逐 +1）。任一失败即中止（不落配置）。
        let existing =
            read_existing_ports(&mut sess, cancel, need_xray, need_ss, need_singbox).await?;
        let mut ports: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
        for proto in &req.protocols {
            let base = default_port(proto);
            if base == 0 {
                return Err(format!("未知协议: {proto}"));
            }
            let assigned = match existing.ports.get(proto.as_str()) {
                Some(p) => *p,
                None => allocate_port(&mut sess, cancel, base).await?,
            };
            ports.insert(proto.as_str(), assigned);
        }
        emit_progress("探测", "探测完成", 20);
        check_cancel(cancel)?;

        // 2 安装（缺才装）
        if need_xray && !xray_installed {
            emit_progress("安装", "安装 Xray（可能耗时数分钟）", 30);
            run_cmd_ok(
                &mut sess,
                cancel,
                "安装",
                &xray_install_cmd(),
                INSTALL_TIMEOUT,
            )
            .await?;
        }
        if need_ss && !ss_installed {
            emit_progress("安装", "安装 shadowsocks-rust（可能耗时数分钟）", 30);
            let rel = latest_ss_asset(&self.http, &self.ss_api_base, target).await?;
            // 实测：shadowsocks-rust 不发布 attestation（该端点对其真实 digest 返回 404），
            // 所以这里必然走到 NotPublished——如实说「无从核验」，不冒充「已核验」。
            // 上游哪天开始签 attestation，把这条策略改成 Attested 即可启用整条核验路径。
            match verify_provenance(
                &self.http,
                &self.ss_api_base,
                "shadowsocks-rust",
                Provenance::NotPublished,
                rel.sha256.as_deref(),
            )
            .await?
            {
                ProvenanceOutcome::NotPublished => emit_progress(
                    "安装",
                    "上游 shadowsocks-rust 不发布构建来源证明，本步无从核验",
                    32,
                ),
                ProvenanceOutcome::Confirmed { count } => emit_progress(
                    "安装",
                    &format!("构建来源已证实（{count} 份 attestation，digest 匹配）"),
                    32,
                ),
            }
            let url = format!(
                "https://github.com/shadowsocks/shadowsocks-rust/releases/download/{}/{}",
                rel.tag, rel.asset
            );
            let cmd = ss_install_cmd(&url, rel.sha256.as_deref());
            let out = run_cmd_ok(&mut sess, cancel, "安装", &cmd, INSTALL_TIMEOUT).await?;
            emit_measured_hash(&emit_progress, "ss.tar.xz", &rel, &out);
            emit_installed_bin_hash(&emit_progress, "ssserver", &out);
        }
        if need_singbox && !singbox_installed {
            emit_progress("安装", "安装 sing-box（可能耗时数分钟）", 30);
            let rel = latest_singbox_asset(&self.http, &self.singbox_api_base, target).await?;
            // 实测：sing-box 发布 attestation。查不到就中止——偷 token 换包的人
            // 改得了字节与 digest，但签不出 attestation（见 verify_provenance 说明）。
            match verify_provenance(
                &self.http,
                &self.singbox_api_base,
                "sing-box",
                Provenance::Attested,
                rel.sha256.as_deref(),
            )
            .await?
            {
                ProvenanceOutcome::Confirmed { count } => emit_progress(
                    "安装",
                    &format!("构建来源已证实（{count} 份 attestation，digest 匹配）"),
                    32,
                ),
                ProvenanceOutcome::NotPublished => {
                    emit_progress("安装", "上游 sing-box 未发布构建来源证明，本步无从核验", 32)
                }
            }
            let url = format!(
                "https://github.com/SagerNet/sing-box/releases/download/{}/{}",
                rel.tag, rel.asset
            );
            let cmd = singbox_install_cmd(&url, rel.sha256.as_deref());
            let out = run_cmd_ok(&mut sess, cancel, "安装", &cmd, INSTALL_TIMEOUT).await?;
            emit_measured_hash(&emit_progress, "singbox.tar.gz", &rel, &out);
            emit_installed_bin_hash(&emit_progress, "sing-box", &out);
        }
        check_cancel(cancel)?;
        // 已装/新装统一文案（双分支相同，clippy if_same_then_else）
        emit_progress("安装", "组件就绪", 45);
        check_cancel(cancel)?;

        // 3 配置：按协议组生成配置并写入白名单路径
        emit_progress("配置", "生成配置", 55);
        // 本机库里已有的节点（按协议索引）：复用入站时要拿它补 reality 公钥——公钥只在
        // 客户端这一侧（服务器配置只存私钥）。读库失败按空表处理：复用是增益，读不回来
        // 就退化成新生成，不能让一次部署因为「读不到旧节点」而失败。
        let existing_nodes: std::collections::HashMap<String, crate::domain::Node> = {
            let conn = state.db.lock();
            repos::nodes::list(&conn, &state.creds)
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.source == "self-built" && n.addr == server.host)
                .map(|n| (n.protocol.clone(), n))
                .collect()
        };
        // 每协议记录 (protocol, port, params_json) 供写回；失败收集 failed_protocols
        let mut success_protocols: Vec<(String, u16, String)> = vec![];
        let mut failed_protocols: Vec<String> = vec![];

        if need_xray {
            // 每个 Reality 入站独立生成 x25519 密钥对（vless 与 trojan 不得共用：
            // 实测同私有密钥多入站并存时 Xray v26 reality 握手全线失败，客户端一律
            // EOF/503）；vmess(WS) 不涉 Reality，空密钥即可。
            // 兜底域名与上游出口共用同一个常量，避免两条链路各写一份而漂移——
            // 漂移的表现是「界面显示 A、服务器写 B」，且失败只在客户端侧可见。
            let reality_dest = req
                .reality_dest
                .clone()
                .unwrap_or_else(|| crate::servers::upstream::DEFAULT_REALITY_DEST.into());
            let mut entries: Vec<XrayInboundEntry> = Vec::new();
            for p in ["vless", "trojan", "vmess"] {
                if !req.protocols.iter().any(|x| x == p) {
                    continue;
                }
                let port = ports[p];
                // **幂等**：这个端口上已有入站 → 原样复用它的凭据与 reality 私钥，只按
                // 本次请求更新伪装域名。
                //
                // 为什么非复用不可：凭据一改就是「两侧短暂不一致」——新凭据先落到服务器
                // 配置，要等到写回阶段才进客户端节点行，中间任何失败（重启 xray 掐断
                // 承载这次部署的隧道、SSH 抖动、端口验证失败、用户取消）都会让客户端
                // 攥着旧凭据去连一个只认新凭据的入站：**节点变砖，界面上还看不出来**
                // （行还在、还启用在用）。2026-09-20 真机：job 356 停在 80%，ashura 三个
                // 协议全废，只能重部署一次才恢复。复用之后，一次失败的重复部署只是
                // 「没改成」，不会再毁掉已经能用的节点。
                //
                // 公钥不在服务器配置里（Xray 只存私钥），只能从既有节点参数取；取不到就
                // 不复用，退化成新生成——宁可多写一次新凭据，也不要复用出一个公钥为空的
                // reality 入站（那同样是全废，且更难查）。
                if let Some(old) = existing.xray_inbounds.get(&port) {
                    let old_pk = existing_nodes
                        .get(p)
                        .and_then(|n| public_key_of(&n.params_json));
                    if p == "vmess" || old_pk.is_some() {
                        let mut e = old.clone();
                        e.reality_dest = reality_dest.clone();
                        e.public_key = old_pk.unwrap_or_default();
                        entries.push(e);
                        continue;
                    }
                }
                let (priv_k, pub_k) = if p == "vmess" {
                    (String::new(), String::new())
                } else {
                    let keys_out =
                        run_cmd_ok(&mut sess, cancel, "配置", "xray x25519", CONFIG_TIMEOUT)
                            .await?
                            .stdout;
                    parse_x25519_keys(&keys_out)?
                };
                let short_id: String = (0..8)
                    .map(|_| format!("{:x}", rand::random::<u8>() % 16))
                    .collect();
                entries.push(XrayInboundEntry {
                    proto: p,
                    port: ports[p],
                    uuid: Uuid::new_v4().to_string(),
                    password: Uuid::new_v4().to_string(),
                    short_id,
                    private_key: priv_k,
                    public_key: pub_k,
                    reality_dest: reality_dest.clone(),
                    ws_path: "/ray".into(),
                });
            }
            let cfg = xray_config_json_multi(&entries);
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                XRAY_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            for e in &entries {
                let params = match e.proto {
                    "vless" => serde_json::json!({
                        "uuid": e.uuid, "security": "reality", "type": "tcp",
                        "flow": "xtls-rprx-vision", "sni": e.reality_dest,
                        "public-key": e.public_key, "short-id": e.short_id, "fingerprint": "chrome"
                    })
                    .to_string(),
                    "trojan" => serde_json::json!({
                        "password": e.password, "security": "reality", "type": "tcp",
                        "sni": e.reality_dest, "public-key": e.public_key,
                        "short-id": e.short_id, "fingerprint": "chrome"
                    })
                    .to_string(),
                    _ => serde_json::json!({
                        "uuid": e.uuid, "alterId": "0", "cipher": "auto",
                        "network": "ws", "path": "/ray"
                    })
                    .to_string(),
                };
                success_protocols.push((e.proto.to_string(), e.port, params));
            }
        }
        if need_ss {
            // 同 xray 那段：既有密码原样复用，重复部署不轮换（轮换 = 两侧短暂不一致，
            // 中途失败就把节点变砖）。只有首次部署或读不回配置时才新生成。
            let key: [u8; 32] = rand::random();
            let pw = existing.ss_password.clone().unwrap_or_else(|| {
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key)
            });
            let ss_port = ports["ss"];
            let cfg = ss_config_json(ss_port, &pw);
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SS_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SS_UNIT_PATH,
                &ss_systemd_unit(),
                CONFIG_TIMEOUT,
            )
            .await?;
            let params = serde_json::json!({ "cipher": "2022-blake3-aes-256-gcm", "password": pw })
                .to_string();
            success_protocols.push(("ss".into(), ss_port, params));
        }
        if need_singbox {
            // 同 xray/ss 两段：既有凭据原样复用，重复部署不轮换（轮换 = 服务端新凭据 +
            // 客户端旧凭据 = 静默变砖，界面还显示「部署成功」）。只有首次部署、
            // 或既有配置里读不回该项时才新生成。
            let hy2_pw = existing
                .singbox_hy2_pw
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let tuic_uuid = existing
                .singbox_tuic_uuid
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let tuic_pw = existing
                .singbox_tuic_pw
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            // 自签证书（幂等：已存在跳过）。缺 openssl 时按包管理器安装；
            // 全无包管理器 → 脚本回 NO_OPENSSL，映射成可读错误（而非裸 exit code）。
            // 超时用 INSTALL_TIMEOUT(600s) 而非 CONFIG_TIMEOUT(60s)：该命令内含
            // `apt-get update -qq && apt-get install -y openssl`，慢镜像下单是索引
            // 刷新就可能超 60s，会把一条罕见但合法的路径变成「配置阶段的不明超时」。
            let out = run_cmd(
                &mut sess,
                cancel,
                "配置",
                &singbox_cert_cmd(),
                INSTALL_TIMEOUT,
            )
            .await?;
            if out.stdout.contains("NO_OPENSSL") {
                return Err(
                    "该系统找不到 openssl，也没有可识别的包管理器（apt-get/dnf/yum/pacman/zypper/apk），无法生成 sing-box 自签证书"
                        .into(),
                );
            }
            if out.exit_code != 0 {
                return Err(format!(
                    "生成自签证书失败(exit={}): {}",
                    out.exit_code,
                    out.stderr.trim_end()
                ));
            }
            // 按选中集生成 inbounds：`ports` 只装选中协议，未选中的键不存在。
            // 必须用 then(|| …) 惰性闭包——then_some 的参数是急切求值，
            // ports["hysteria2"] 会在调用前就索引并 panic（单 L2 即触发）。
            let hy2_selected = req.protocols.iter().any(|p| p == "hysteria2");
            let tuic_selected = req.protocols.iter().any(|p| p == "tuic");
            let cfg = singbox_config_json(
                hy2_selected.then(|| (ports["hysteria2"], hy2_pw.as_str())),
                tuic_selected.then(|| (ports["tuic"], tuic_uuid.as_str(), tuic_pw.as_str())),
            );
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SINGBOX_CONFIG_PATH,
                &cfg,
                CONFIG_TIMEOUT,
            )
            .await?;
            write_remote_file(
                &mut sess,
                cancel,
                "配置",
                SINGBOX_UNIT_PATH,
                &singbox_systemd_unit(),
                CONFIG_TIMEOUT,
            )
            .await?;
            if req.protocols.contains(&"hysteria2".to_string()) {
                success_protocols.push((
                    "hysteria2".into(),
                    ports["hysteria2"],
                    serde_json::json!({
                        "password": hy2_pw, "skip-cert-verify": "1"
                    })
                    .to_string(),
                ));
            }
            if req.protocols.contains(&"tuic".to_string()) {
                success_protocols.push((
                    "tuic".into(),
                    ports["tuic"],
                    serde_json::json!({
                        "uuid": tuic_uuid, "password": tuic_pw, "skip-cert-verify": "1"
                    })
                    .to_string(),
                ));
            }
        }
        if success_protocols.is_empty() {
            return Err("没有生成任何协议配置".into());
        }
        check_cancel(cancel)?;

        // 4 启动：逐 unit 激活并验证；端口验证失败 → 该协议标记失败（不中断其余）
        emit_progress("启动", "启动服务", 80);
        if need_xray {
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                "systemctl enable --now xray",
                CONFIG_TIMEOUT,
            )
            .await?;
            // restart 的退出码不作判据：它偶发抖一下（实测 exit=3、stderr 空）不代表
            // 服务没起来，真正的判据是下面等出来的状态。
            run_cmd(
                &mut sess,
                cancel,
                "启动",
                "systemctl restart xray",
                CONFIG_TIMEOUT,
            )
            .await?;
            wait_service_active(&mut sess, cancel, "xray").await?;
        }
        if need_ss {
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                "systemctl daemon-reload && systemctl enable --now ssserver",
                CONFIG_TIMEOUT,
            )
            .await?;
            wait_service_active(&mut sess, cancel, "ssserver").await?;
        }
        if need_singbox {
            run_cmd_ok(
                &mut sess,
                cancel,
                "启动",
                "systemctl daemon-reload && systemctl enable --now singbox",
                CONFIG_TIMEOUT,
            )
            .await?;
            wait_service_active(&mut sess, cancel, "singbox").await?;
        }
        // 逐协议端口验证：失败隔离（其余协议继续,仅标记）
        for (proto, port, _params) in &success_protocols {
            let listening = run_cmd(
                &mut sess,
                cancel,
                "启动",
                &port_probe_cmd(*port),
                VERIFY_TIMEOUT,
            )
            .await?
            .exit_code
                == 0;
            if !listening {
                failed_protocols.push(proto.clone());
            }
        }
        check_cancel(cancel)?;

        // 4.4 全协议失败守卫：必须在 BBR 之前。BBR 会真实改写内核参数（sysctl -w +
        // 追加 /etc/sysctl.conf），若先开 BBR 再因「全部协议验证失败」返回 Err，
        // 内核已被动过而 repos::deploys::insert 从未执行，用户拿到一个 failed 部署
        // 且不知道内核被改过。部分失败不走这里：那条路径的摘要（含 bbr 字段）在
        // Err 之前已落库，不受影响。
        let ok_protocols: Vec<&(String, u16, String)> = success_protocols
            .iter()
            .filter(|(p, _, _)| !failed_protocols.contains(p))
            .collect();
        if ok_protocols.is_empty() {
            return Err(format!("所有协议验证失败: {}", failed_protocols.join(", ")));
        }

        // 4.5 BBR（可选）：插在端口验证之后、写回之前——协议服务确认起来了才动内核参数，
        // BBR 出问题不可能被误判成部署失败。任何失败都不上抛：协议已部署成功，
        // BBR 属优化项，加 `?` 会让一次成功的部署在最后一步变成 failed，所以要收集成字符串。
        //
        // 本阶段的**命令内部**不感知取消：server_bbr::check_bbr / enable_bbr 走裸
        // SshSession::run，脚本没有取消标志，只能靠命令自身超时兜底（检测 15s + 开启 30s
        // ⇒ 最坏窗口约 45s）。这里不打断执行中的命令（SSH 粒度决定，见模块头）。
        //
        // 但 BBR 段**之后**必须重新看一眼取消位（下面 5 之前那句）：否则用户在这 45 秒里
        // 点的取消就是空操作——部署照样写回节点并报成功，取消等于没点（#14）。
        // 代价是可能停在「远端服务已起、BBR 已生效，而本地没有节点行」：这是**有意选的**。
        // 用户显式点了取消，应用不该替他决定「其实挺好的，就当没取消吧」；重部署一次即可
        // 收敛，而用户被无视是无从挽回的。同理，取消落在更早的步骤时也早就这样了
        // （配置已写、服务已起，只是不写回），这里保持一致而不是破例。
        let mut bbr_note: Option<String> = None;
        if req.enable_bbr {
            emit_progress("优化", "检测 BBR", 88);
            match crate::server_bbr::check_bbr(&mut sess).await {
                // 已是 bbr：不下发开启脚本（脚本虽幂等，但没必要多跑一次 sysctl 写）
                Ok(algo) if algo == "bbr" => bbr_note = Some("BBR 已启用".to_string()),
                Ok(algo) => {
                    emit_progress("优化", &format!("当前 {algo}，开启 BBR"), 90);
                    bbr_note = Some(match crate::server_bbr::enable_bbr(&mut sess).await {
                        Ok(m) => m,
                        Err(e) => format!("开启失败：{e}"),
                    });
                }
                Err(e) => bbr_note = Some(format!("检测失败：{e}")),
            }
        }

        // 步骤边界：BBR 段可能刚跑了几十秒，这期间用户点的取消必须在这里被看见（#14）。
        // 写回是「应用认为这次部署成立」的那一刻，取消要在它之前生效。
        check_cancel(cancel)?;

        // 5 写回：仅写成功协议节点(含端口验证通过);failed 在成功节点写回后随 error 上报
        // （ok_protocols 已在上方 4.4 守卫处算出，非空已保证）
        emit_progress("写回", "写入节点与部署记录", 95);
        {
            let conn = state.db.lock();
            for (proto, port, params) in &ok_protocols {
                let node = Node {
                    id: Uuid::new_v4().simple().to_string(),
                    name: format!("{}-{}", server.name, proto),
                    source: "self-built".into(),
                    protocol: proto.clone(),
                    addr: server.host.clone(),
                    port: *port as i64,
                    params_json: params.clone(),
                    group_name: "自建".into(),
                    sub_id: None,
                    relay_rule_id: None,
                    enabled: true,
                    latency: None,
                    bandwidth: None,
                    last_checked_at: None,
                    last_check_error: None,
                    direct_latency: None,
                    hop_relay_ms: None,
                    hop_landing_ms: None,
                    hop_tunnel_ms: None,
                };
                // 幂等：重复部署先清同一地址同协议的旧自建行，不留参数失效的副本
                repos::nodes::delete_self_built(&conn, &node.protocol, &node.addr)
                    .map_err(|e| e.to_string())?;
                repos::nodes::upsert(&conn, &node, &state.creds).map_err(|e| e.to_string())?;
            }
            // 自动补挂中转：建链只改指「当时已存在」的节点，之后部署上来的节点
            // relay_rule_id 恒为 None，客户端就按直连拨——落地机直连不通时表现为
            // 测速 504，节点行上却毫无线索。这里补上，并把挂不上的原因写进摘要。
            let deployed: Vec<(String, i64)> = ok_protocols
                .iter()
                .map(|(p, port, _)| (p.clone(), *port as i64))
                .collect();
            let attaches =
                crate::relay::attach_deployed_nodes(&conn, &server, &deployed, &state.creds)
                    .unwrap_or_else(|e| {
                        // 补挂失败不该让整次部署失败：节点已在库，链路问题照实报出来
                        vec![crate::relay::AttachOutcome {
                            node: server.name.clone(),
                            attached: false,
                            detail: format!("自动补挂中转时出错：{e}"),
                        }]
                    });
            let ports_map: serde_json::Map<String, serde_json::Value> = ok_protocols
                .iter()
                .map(|(p, port, _)| (p.clone(), serde_json::Value::from(*port)))
                .collect();
            let mut summary = serde_json::json!({
                "protocols": ok_protocols.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(),
                "ports": serde_json::Value::Object(ports_map),
                "relay": attaches.iter().map(|a| serde_json::json!({
                    "node": a.node, "attached": a.attached, "detail": a.detail
                })).collect::<Vec<_>>()
            });
            // BBR 结果（有才写）：parse_deployed_protocols 只读 protocols、容忍额外字段，
            // 不影响既有摘要解析
            if let Some(note) = &bbr_note {
                summary["bbr"] = serde_json::Value::from(note.clone());
            }
            let summary = summary.to_string();
            repos::deploys::insert(&conn, req.server_id, "deploy", &summary, "ok")
                .map_err(|e| e.to_string())?;
        }
        state
            .subs
            .rebuild_config()
            .await
            .map_err(|e| e.to_string())?;
        if failed_protocols.is_empty() {
            // BBR 结果并进 100% 文案：用户不点开摘要也能看到
            let done = match &bbr_note {
                Some(n) => format!("部署完成（{n}）"),
                None => "部署完成".to_string(),
            };
            emit_progress("写回", done.as_str(), 100);
            Ok(())
        } else {
            let msg = format!("部分协议失败: {}", failed_protocols.join(", "));
            emit(JobProgress {
                job_id: req.job_id,
                kind: "deploy".into(),
                step: "写回".into(),
                message: msg.clone(),
                progress: 100,
                error: Some(msg),
            });
            Err("部分协议部署失败".into())
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("已取消".into())
    } else {
        Ok(())
    }
}
