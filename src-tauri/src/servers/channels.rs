//! 通道服务：每台服务器可登记端口通道，本地 UI 直接控制端口的**防火墙开关**。
//! 输入：AddChannelInput（server_id/name/port/protocol）、通道 id、enabled；
//! 输出：Channel/ChannelStatus。开关状态不入库——SSH iptables INPUT 链规则
//! 存在性是事实源（-C 查询退出码 0=规则存在=已放行）。
//! 开关语义（防火墙放行/封锁，不触碰端口背后的服务）：
//! - 开：规则不存在则 `iptables -I INPUT 1 -p tcp --dport {port} -j ACCEPT`
//! - 关：规则存在则 `iptables -D INPUT -p tcp --dport {port} -j ACCEPT`
//!
//! 安全：端口范围校验；**SSH 端口拒绝封锁**（防自锁，把自己关在服务器外面）；
//! 凭据复用 load_auth 解密即用即弃；错误文本经 ssh 层脱敏（不含凭据）。

use crate::db::repos;
use crate::domain::{Channel, ChannelStatus};
use crate::servers::{load_auth, ServersError, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use chrono::Utc;

/// 通道添加输入（IPC 扁平参数形态）。
#[derive(Debug, Clone)]
pub struct AddChannelInput {
    pub server_id: i64,
    pub name: String,
    pub port: i64,
    pub protocol: String,
}

/// 扫描结果：服务器正在监听的端口及其进程名（进程名可能为空串）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ServerPort {
    pub port: i64,
    pub process: String,
}

/// 允许的通道协议白名单（当前代理场景；扩展时同步前端与测试）。
const PROTOCOLS: &[&str] = &["socks5", "http"];

/// 列表（纯 DB，不触发 SSH；开关状态按需经 channel_status 单查）。
pub fn list_channels(state: &AppState, server_id: i64) -> Result<Vec<Channel>, ServersError> {
    let conn = state.db.lock();
    repos::channels::list_by_server(&conn, server_id).map_err(|e| ServersError::Db(e.to_string()))
}

/// 添加通道：校验 → 确认服务器存在 → 落库（纯本地操作，不触碰服务器）。
pub fn add_channel(state: &AppState, input: AddChannelInput) -> Result<Channel, ServersError> {
    let name = crate::servers::clean_text(&input.name);
    if name.is_empty() {
        return Err(ServersError::Validation("通道名称不能为空".into()));
    }
    if !(1..=65535).contains(&input.port) {
        return Err(ServersError::Validation("端口必须在 1-65535 之间".into()));
    }
    if !PROTOCOLS.contains(&input.protocol.as_str()) {
        return Err(ServersError::Validation(format!(
            "协议只支持 {}",
            PROTOCOLS.join("/")
        )));
    }
    let conn = state.db.lock();
    let server_exists = repos::servers::get(&conn, input.server_id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .is_some();
    if !server_exists {
        return Err(ServersError::NotFound);
    }
    let channel = Channel {
        id: 0,
        server_id: input.server_id,
        name,
        port: input.port,
        protocol: input.protocol,
        created_at: Utc::now().to_rfc3339(),
    };
    let id =
        repos::channels::insert(&conn, &channel).map_err(|e| ServersError::Db(e.to_string()))?;
    Ok(Channel { id, ..channel })
}

/// 扫描服务器正在监听的 TCP 端口（SSH `ss -tlnp`），返回 (端口, 进程名) 列表。
/// 仅解析 LISTEN 行且绑定地址含端口的条目；进程名取 users: 段引号内标识，
/// 解析失败不丢弃整行（进程名回退为空串）。端口去重（IPv4/IPv6 双栈同端口只列一次）。
pub async fn scan_ports(state: &AppState, server_id: i64) -> Result<Vec<ServerPort>, ServersError> {
    let (auth, server) = load_auth(state, server_id)?;
    let mut sess = SshSession::connect(
        crate::domain::effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(ServersError::Ssh)?;
    let out = sess
        .run("端口扫描", "ss -tlnp", PROBE_TIMEOUT)
        .await
        .map_err(ServersError::Ssh)?;
    if out.exit_code != 0 {
        return Err(ServersError::Validation(format!(
            "扫描失败: {}",
            first_nonempty(&out.stderr, &out.stdout)
        )));
    }
    Ok(parse_ss_tlnp(&out.stdout))
}

/// 解析 `ss -tlnp` 输出行：`LISTEN 0 4096 100.113.111.36:1080 0.0.0.0:* users:(("gost",pid=1,fd=3))`
/// → (1080, "gost")。进程名取 users: 段引号内标识，解析失败不丢弃整行（进程名回退为空串）。
pub fn parse_ss_tlnp(output: &str) -> Vec<ServerPort> {
    let mut ports = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with("LISTEN") {
            continue;
        }
        // 端口位于倒数第二列（Local Address:Port）：ss 固定输出「Local Address:Port」列
        let mut cols = line.split_whitespace();
        let _state = cols.next();
        let _recv = cols.next();
        let _send = cols.next();
        let Some(local) = cols.next() else {
            continue;
        };
        // 形如 0.0.0.0:1080、[::]:1080、127.0.0.1%lo:53——取最后一个冒号后的数字
        let Some(port_str) = local.rsplit(':').next() else {
            continue;
        };
        let Ok(port) = port_str.parse::<i64>() else {
            continue;
        };
        if ports.iter().any(|p: &ServerPort| p.port == port) {
            continue;
        }
        let process = extract_process_name(line);
        ports.push(ServerPort { port, process });
    }
    ports.sort_by_key(|p| p.port);
    ports
}

/// 从 users: 段提取进程名（`users:(("gost",pid=123,fd=3))` → "gost"；缺省空串）。
fn extract_process_name(line: &str) -> String {
    let Some(users_idx) = line.find("users:") else {
        return String::new();
    };
    let rest = &line[users_idx + "users:".len()..];
    let Some(quote_start) = rest.find('"') else {
        return String::new();
    };
    let after = &rest[quote_start + 1..];
    after.split('"').next().unwrap_or("").to_string()
}

/// 删除通道（纯 DB，不触碰服务器防火墙——删除与开关分离，已放行的端口保持原状）。
pub fn remove_channel(state: &AppState, id: i64) -> Result<(), ServersError> {
    let conn = state.db.lock();
    let exists = repos::channels::get(&conn, id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .is_some();
    if !exists {
        return Err(ServersError::NotFound);
    }
    repos::channels::delete(&conn, id).map_err(|e| ServersError::Db(e.to_string()))
}

/// 查询通道开关状态：SSH `iptables -C INPUT ...` 退出码 0=已放行，1=已封锁。
pub async fn channel_status(state: &AppState, id: i64) -> Result<ChannelStatus, ServersError> {
    let (channel, mut sess) = connect_channel(state, id).await?;
    match sess
        .run("端口状态", &accept_rule_check(channel.port), PROBE_TIMEOUT)
        .await
    {
        Ok(o) => Ok(status_from_exit(o.exit_code, &o.stderr)),
        Err(e) => Err(ServersError::Ssh(e)),
    }
}

/// 开关通道端口：放行=规则不存在则插到 INPUT 链首；封锁=规则存在则删除。
/// 封锁服务器的 SSH 端口会把自己锁在门外，直接拒绝（防呆）。
pub async fn set_channel_enabled(
    state: &AppState,
    id: i64,
    enabled: bool,
) -> Result<ChannelStatus, ServersError> {
    // 防自锁检查先于 SSH 连接：封锁本机 SSH 端口的操作在本地即拒绝，
    // 不依赖服务器可达性（服务器已断连时同样给出可读原因而非连接错误）
    let channel = {
        let conn = state.db.lock();
        repos::channels::get(&conn, id)
            .map_err(|e| ServersError::Db(e.to_string()))?
            .ok_or(ServersError::NotFound)?
    };
    if !enabled {
        let ssh_port = {
            let conn = state.db.lock();
            repos::servers::get(&conn, channel.server_id)
                .map_err(|e| ServersError::Db(e.to_string()))?
                .map(|s| s.port)
        };
        if ssh_port == Some(channel.port) {
            return Err(ServersError::Validation(format!(
                "端口 {} 是这台服务器的 SSH 端口，封锁后本工具将无法再连上它，已拒绝操作",
                channel.port
            )));
        }
    }
    let (channel, mut sess) = connect_channel(state, channel.id).await?;
    let check = accept_rule_check(channel.port);
    let action = if enabled {
        // 放行：不存在才插入（幂等；与上游部署自动放行同一条命令，只此一处拼）
        ensure_open_cmd(channel.port)
    } else {
        // 封锁：存在才删除（幂等）
        format!(
            "{check} && iptables -D INPUT -p tcp --dport {} -j ACCEPT",
            channel.port
        )
    };
    let out = sess
        .run("端口开关", &action, PROBE_TIMEOUT)
        .await
        .map_err(ServersError::Ssh)?;
    if out.exit_code != 0 {
        return Ok(ChannelStatus {
            running: false,
            detail: format!("操作失败: {}", first_nonempty(&out.stderr, &out.stdout)),
        });
    }
    // 回查规则存在性，返回真实状态
    let status = sess
        .run("开关后状态", &check, PROBE_TIMEOUT)
        .await
        .map_err(ServersError::Ssh)?;
    Ok(status_from_exit(status.exit_code, &status.stderr))
}

/// iptables 规则存在性检查命令（-C 退出码 0=存在、1=不存在，其余为错误）。
fn accept_rule_check(port: i64) -> String {
    format!("iptables -C INPUT -p tcp --dport {port} -j ACCEPT")
}

/// 幂等放行一个入站端口（已放行则不动）。
///
/// 给「自己开的端口自己放行」的流程复用——目前是上游出口部署：它监听的端口必须先
/// 从外面可达，而服务器 INPUT 默认丢弃时不放开，表现是**客户端超时**而不是任何一处
/// 报错（2026-09-21 实测：8864 上 xray 在听、配置进程全对，本机连它超时；手工加上
/// 这条规则后立刻连通）。能自动做的只有机器这一层；云服务商安全组是另一层，
/// 本工具够不着，只能由部署后的可达性核验把话说明白。
pub fn ensure_open_cmd(port: i64) -> String {
    format!(
        "{check} || iptables -I INPUT 1 -p tcp --dport {port} -j ACCEPT",
        check = accept_rule_check(port)
    )
}

/// 读取通道行 + 所在服务器凭据并建立 SSH 会话（凭据即用即弃，不出本函数）。
async fn connect_channel(state: &AppState, id: i64) -> Result<(Channel, SshSession), ServersError> {
    let channel = {
        let conn = state.db.lock();
        repos::channels::get(&conn, id)
            .map_err(|e| ServersError::Db(e.to_string()))?
            .ok_or(ServersError::NotFound)?
    };
    let (auth, server) = load_auth(state, channel.server_id)?;
    let sess = SshSession::connect(
        crate::domain::effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(ServersError::Ssh)?;
    Ok((channel, sess))
}

/// iptables -C 退出码 → ChannelStatus：0=已放行；1=已封锁；其余按失败携带输出。
fn status_from_exit(exit_code: u32, stderr: &str) -> ChannelStatus {
    match exit_code {
        0 => ChannelStatus {
            running: true,
            detail: "已放行".into(),
        },
        1 => ChannelStatus {
            running: false,
            detail: "已封锁".into(),
        },
        _ => ChannelStatus {
            running: false,
            detail: format!("状态查询失败: {}", first_nonempty(stderr, "")),
        },
    }
}

/// stderr 优先、空则 stdout、皆空则占位——错误展示必须携带真实原因（脱敏）。
fn first_nonempty(a: &str, b: &str) -> String {
    let t = if a.trim().is_empty() { b } else { a };
    let t = t.trim();
    if t.is_empty() {
        "(无输出)".to_string()
    } else {
        t.to_string()
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/servers/channels_mods.rs"
));
