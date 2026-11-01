//! fail2ban 封禁名单的读取与增量下发。
//!
//! 分享轮询每台服务器本来就会建一条 SSH 会话；这里顺带读取 `sshd` jail 的
//! 封禁名单，并把本地新增的 IP 推送到还没有它的服务器。只做增量：取游标
//! 之后、目标机尚未封禁的 IP，失败不推进游标，下一轮重试。

use crate::domain::Server;
use crate::servers::deploy::{
    fail2ban_ban_file_cmd, fail2ban_list_bans_cmd, parse_fail2ban_bans, run_cmd, run_cmd_ok,
    ssh_client_ip_cmd, write_line_list, CONFIG_TIMEOUT, FAIL2BAN_SYNC_PATH, INSTALL_TIMEOUT,
    PREBAN_SYNC_LIMIT,
};
use crate::servers::PROBE_TIMEOUT;
use crate::ssh::SshSession;
use crate::state::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::install::connect_server;

/// 读取该服务器的 fail2ban 封禁名单；未安装 fail2ban（命令不存在）时返回空表。
///
/// 这条命令跟着分享轮询一起跑（每台服务器每 2 分钟一次），故意不报错：
/// 老服务器还没部署过 fail2ban 时属于正常状态，不能因此把整轮快照判成失败。
pub(crate) async fn read_fail2ban_bans_on(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Vec<String> {
    match run_cmd(
        sess,
        cancel,
        "封禁名单",
        fail2ban_list_bans_cmd(),
        PROBE_TIMEOUT,
    )
    .await
    {
        Ok(out) => parse_fail2ban_bans(&out.stdout),
        Err(_) => Vec::new(),
    }
}

/// 把增量封禁 IP 下发到一台服务器；返回 Ok 时调用方推进游标。
///
/// 未安装 fail2ban 的服务器直接跳过（部署时会全量补上），不反复重试；jail
/// 未就绪则报错、游标不推进、下一轮再试。下发后回读名单核验——命令里的
/// `|| true` 会把单条失败吞掉，只能靠回读发现。
async fn push_bans_to_server(
    state: &Arc<AppState>,
    server: &Server,
    cancel: &AtomicBool,
    ips: &[String],
) -> Result<(), String> {
    let mut sess = connect_server(state, server).await?;
    let ready = run_cmd(
        &mut sess,
        cancel,
        "封禁下发",
        "command -v fail2ban-client >/dev/null 2>&1 || exit 3; \
         fail2ban-client status sshd >/dev/null 2>&1 || exit 4",
        PROBE_TIMEOUT,
    )
    .await?;
    match ready.exit_code {
        0 => {}
        3 => return Ok(()), // 没装 fail2ban：部署时会全量补，轮询不重试
        _ => return Err("fail2ban 的 sshd jail 未就绪".into()),
    }
    // 跳过应用自身的 SSH 来源 IP：本地名单里若恰好有它，不能把管理通道封死。
    let own = run_cmd(
        &mut sess,
        cancel,
        "封禁下发",
        ssh_client_ip_cmd(),
        PROBE_TIMEOUT,
    )
    .await
    .ok()
    .map(|o| o.stdout.trim().to_string())
    .unwrap_or_default();
    let list: Vec<String> = ips
        .iter()
        .filter(|ip| ip.as_str() != own.as_str())
        .cloned()
        .collect();
    if list.is_empty() {
        return Ok(());
    }
    write_line_list(
        &mut sess,
        cancel,
        "封禁下发",
        FAIL2BAN_SYNC_PATH,
        &list,
        500,
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        cancel,
        "封禁下发",
        &fail2ban_ban_file_cmd(FAIL2BAN_SYNC_PATH),
        INSTALL_TIMEOUT,
    )
    .await?;
    // 回读核验：这一批必须全部出现在 sshd jail 的封禁名单里。
    let out = run_cmd(
        &mut sess,
        cancel,
        "封禁下发",
        fail2ban_list_bans_cmd(),
        PROBE_TIMEOUT,
    )
    .await?;
    let now: HashSet<String> = parse_fail2ban_bans(&out.stdout).into_iter().collect();
    let missing = list.iter().filter(|ip| !now.contains(*ip)).count();
    if missing > 0 {
        return Err(format!("下发后回读核验失败：{missing} 个 IP 未生效"));
    }
    Ok(())
}

/// 把本轮各服务器上报的封禁名单同步回本地，并把增量推送到彼此。
///
/// 1) 首次见到某台服务器时先把游标基线化到当前最大 rowid（历史名单只在部署
///    新机器时全量上传，不在轮询里补推）；
/// 2) 并入本轮各服务器上报的封禁名单（新 IP 的 rowid 落在游标之后）；
/// 3) 对每台服务器取「游标之后、它还没有」的 IP，只有存在增量时才另开 SSH
///    会话下发——A 上新封的 IP 会在一轮内到 B/C，已有的 IP 不会重发。
pub(crate) async fn sync_bans(
    state: &Arc<AppState>,
    server_map: &HashMap<i64, Server>,
    polled: &[(i64, Vec<String>)],
) {
    let mut pending: Vec<(i64, Vec<String>, i64)> = Vec::new();
    {
        let conn = state.db.lock();
        for (id, _) in polled {
            if let Err(e) = crate::db::repos::server_ban_sync::ensure_baseline(&conn, *id) {
                eprintln!("初始化封禁同步游标失败（server {id}）：{e}");
            }
        }
        for (id, bans) in polled {
            if !bans.is_empty() {
                if let Err(e) = crate::db::repos::banned_ips::upsert_many(&conn, bans, *id) {
                    eprintln!("同步 fail2ban 封禁名单失败（server {id}）：{e}");
                }
            }
        }
        // 本轮各服务器已有的封禁：本机已封的 IP 不再重复下发。
        let live: HashMap<i64, HashSet<String>> = polled
            .iter()
            .map(|(id, bans)| (*id, bans.iter().cloned().collect()))
            .collect();
        for (id, _) in polled {
            let cursor = match crate::db::repos::server_ban_sync::get(&conn, *id) {
                Ok(Some(v)) => v,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("读取封禁同步游标失败（server {id}）：{e}");
                    continue;
                }
            };
            let rows =
                match crate::db::repos::banned_ips::list_after(&conn, cursor, PREBAN_SYNC_LIMIT) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("读取增量封禁名单失败（server {id}）：{e}");
                        continue;
                    }
                };
            if rows.is_empty() {
                continue;
            }
            let last_rowid = rows.last().map(|(r, _)| *r).unwrap_or(cursor);
            let have = live.get(id);
            let ips: Vec<String> = rows
                .into_iter()
                .map(|(_, ip)| ip)
                .filter(|ip| !have.is_some_and(|s| s.contains(ip)))
                .collect();
            if ips.is_empty() {
                // 这一页要么是本机自己封的、要么已经封过；推进游标避免每轮重复扫描。
                if let Err(e) = crate::db::repos::server_ban_sync::set(&conn, *id, last_rowid) {
                    eprintln!("推进封禁同步游标失败（server {id}）：{e}");
                }
                continue;
            }
            pending.push((*id, ips, last_rowid));
        }
    }

    // 只有存在增量时才建连下发；失败不推进游标，下一轮重试。
    let mut push_set = tokio::task::JoinSet::new();
    for (id, ips, last_rowid) in pending {
        let Some(server) = server_map.get(&id).cloned() else {
            continue;
        };
        let state = state.clone();
        push_set.spawn(async move {
            let cancel = AtomicBool::new(false);
            (
                id,
                last_rowid,
                push_bans_to_server(&state, &server, &cancel, &ips).await,
            )
        });
    }
    while let Some(joined) = push_set.join_next().await {
        match joined {
            Ok((id, last_rowid, Ok(()))) => {
                let conn = state.db.lock();
                if let Err(e) = crate::db::repos::server_ban_sync::set(&conn, id, last_rowid) {
                    eprintln!("推进封禁同步游标失败（server {id}）：{e}");
                }
            }
            Ok((id, _, Err(e))) => eprintln!("增量下发封禁名单失败（server {id}）：{e}"),
            Err(e) => eprintln!("封禁下发任务异常结束：{e}"),
        }
    }
}
