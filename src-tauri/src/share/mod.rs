//! 分享模块：创建、列举、编辑、重置用量与作废。
//!
//! 新分享由「订阅 TLS 服务 + 入口网关 + 分享专属代理实例」组成，全部编排在
//! [`install`]；本文件只保留**旧分享**仍需要的单元模板与升级辅助函数：
//! 旧分享继续以 root 明文 HTTP 运行，不引入 TLS/凭据改造，只补防火墙持久化。
//!
//! 服务器是唯一事实源（本地不建分享表）；分享是快照，主服务后续改动不回写。
//! 旧分享与新版 `ShareInfo` 的差异见 [`install::legacy_info`]。

use crate::servers::deploy::{run_cmd_ok, write_remote_file, CONFIG_TIMEOUT};
use crate::ssh::SshSession;
use std::sync::atomic::AtomicBool;

pub mod clone;
pub mod install;
pub mod model;
pub mod naming;
pub mod plan;
pub mod scripts;
pub mod status_poll;
pub mod tls;

#[allow(unused_imports)] // `update_share` 仍供真机测试与外部调用使用
pub use install::{
    create_share, delete_share, list_shares, reset_usage, update_share, update_share_with_nodes,
    CreateShareInput,
};
pub use model::*;
// 根导出是 T10/T11 的契约；binary crate 里部分路径只在测试中使用，统一放行告警。
#[allow(unused_imports)]
pub use naming::*;
pub use plan::share_candidates;
pub use tls::{prepare_tls, tls_status};

/// 旧分享的静态服务脚本文件名；新脚本同样落在这个路径，旧单元无需改 ExecStart。
pub(crate) const SHARE_SERVER_SCRIPT_NAME: &str = "share_server.py";

/// 从单元内容里取端口。端口的事实源是服务器上的单元文件（本地不存分享记录），
/// 所以旧分享 list 必须能从单元里读回端口。
///
/// **两种形态都要认**：
/// - 现形态 `… python3 <SHARE_ROOT>/share_server.py <port> <dir>`；
/// - 旧形态 `… python3 -m http.server <port> --directory <dir> …`
///   ——旧形态只在**升级既有分享**时用到：升级必须沿用原端口，换端口等于把已经
///   发出去的链接当场作废。
pub(crate) fn parse_port_from_unit(unit: &str) -> Option<u16> {
    let markers = [
        format!("{SHARE_SERVER_SCRIPT_NAME} "),
        "-m http.server ".to_string(),
    ];
    for marker in markers {
        if let Some(rest) = unit.find(&marker).map(|i| &unit[i + marker.len()..]) {
            if let Some(p) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
                return Some(p);
            }
        }
    }
    None
}

/// 单元是不是**旧形态**：还在跑会列目录的 `python3 -m http.server`，
/// 或者缺少防火墙持久化（重启后端口不会自动重新放行）。
///
/// 判据是「ExecStart 里有没有指向我们的脚本」「有没有防火墙助手」而不是逐字比对
/// 模板：模板将来再动（加 Environment、换 Restart 策略）不会把已升级的分享误判成
/// 要升级、反复重启。读不到单元内容（空串）时返回 false——没有单元就没有端口，
/// 无从升级。
pub(crate) fn unit_needs_upgrade(unit: &str) -> bool {
    unit.contains("ExecStart=")
        && (!unit.contains(SHARE_SERVER_SCRIPT_NAME) || !unit.contains("vpnshare-firewall.sh"))
}

/// systemd 单元：在分享目录上起旧版静态服务（HTTP，root）。
///
/// 服务本体是 [`scripts::share_server`]（新脚本向后兼容旧的 `<port> <dir>` 参数，
/// 且同样禁列举）。加 `ExecStartPost`/`ExecStopPost` 是为了**重启后自动恢复端口
/// 放行**：旧实现只在创建时插一条 iptables 规则，机器一重启链接就失联（A5）。
pub(crate) fn unit_content(token: &str, port: u16) -> String {
    let dir = naming::share_dir(token);
    let script = naming::share_server_script_path();
    let firewall = naming::firewall_script_path();
    format!(
        "[Unit]\n\
         Description=vpn-ctl share {token}\n\
         \n\
         [Service]\n\
         ExecStart=/usr/bin/env python3 {script} {port} {dir}\n\
         ExecStartPost=+{firewall} open vpnshare-{token} {port}:tcp\n\
         ExecStopPost=+{firewall} close vpnshare-{token} {port}:tcp\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// 把旧形态的分享就地升级：重写服务脚本与单元 → 删旧注释规则 → daemon-reload → restart。
///
/// **端口与快照一律不动**：已发出的链接必须继续可用；升级只把服务换成禁列举脚本
/// 并为单元补上防火墙持久化。幂等：调用点先用 [`unit_needs_upgrade`] 判定。
pub(crate) async fn upgrade_share_unit(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    token: &str,
    port: u16,
) -> Result<(), String> {
    write_remote_file(
        sess,
        cancel,
        "升级",
        &naming::share_server_script_path(),
        &scripts::share_server_script(),
        CONFIG_TIMEOUT,
    )
    .await?;
    // 新单元用防火墙助手做启动放行/停止撤回，脚本必须先落盘再重启。
    let firewall = naming::firewall_script_path();
    write_remote_file(
        sess,
        cancel,
        "升级",
        &firewall,
        &scripts::firewall_script(),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        sess,
        cancel,
        "升级",
        &format!("chmod 755 {firewall}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        sess,
        cancel,
        "升级",
        &naming::unit_path(&naming::unit_name(token)),
        &unit_content(token, port),
        CONFIG_TIMEOUT,
    )
    .await?;
    // 旧规则用 `vpnshare-<token>` 注释；新单元由 ExecStartPost 用
    // `vpnshare-<token>-<port>-tcp` 重开。先删旧规则，避免同一端口叠两条。
    run_cmd_ok(
        sess,
        cancel,
        "升级",
        &format!(
            "iptables -D INPUT -p tcp --dport {port} -m comment --comment \"vpnshare-{token}\" -j ACCEPT 2>/dev/null || true"
        ),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        sess,
        cancel,
        "升级",
        &format!(
            "systemctl daemon-reload && systemctl restart {}",
            naming::unit_name(token)
        ),
        CONFIG_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// 从快照 YAML 里取 proxies 的名字，用于旧分享展示「这条链接里有哪几条线」。
/// 解析不出来就返回空——列表少一列信息，好过整页报错。
pub(crate) fn proxy_names(yaml: &str) -> Vec<String> {
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return vec![];
    };
    v.get("proxies")
        .and_then(|p| p.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/share/mods.rs"
));
