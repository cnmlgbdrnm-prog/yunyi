//! 分享节点规格与 systemd 单元生成。
//!
//! 这里只做纯数据结构/字符串拼装，不建连、不落盘；调用方负责把结果写到
//! 服务器。拆出来是为了让 `install` 只保留编排流程。

use crate::domain::Node;
use crate::share::clone::{ShareNodeSpec, ShareSource};
use crate::share::naming::{
    firewall_script_path, share_gateway_script_path, share_server_script_path, tls_dir,
};
use std::collections::{HashMap, HashSet};

/// 由节点与端口对生成克隆规格；凭据在这里一次性生成。
///
/// `upstream_node_ids` 是这台服务器上由上游出口写回的节点 id。它们虽然也表现为
/// `self-built` 的 VLESS 节点，入站却在专用 `xray-upstream` 配置里，克隆来源不同。
pub fn build_specs(
    nodes: &[Node],
    pairs: &[(u16, u16)],
    upstream_node_ids: &HashSet<String>,
    reused_upstreams: &HashMap<String, i64>,
) -> Result<Vec<ShareNodeSpec>, String> {
    if nodes.len() != pairs.len() {
        return Err("端口数量与节点数量不一致".into());
    }
    nodes
        .iter()
        .zip(pairs)
        .map(|(n, (public, internal))| {
            let transport = match n.protocol.as_str() {
                "hysteria2" | "tuic" => "udp",
                "ss" => "tcp_and_udp",
                "vless" | "trojan" | "vmess" => "tcp",
                other => return Err(format!("不支持分享的协议: {other}")),
            };
            let original_port = u16::try_from(n.port)
                .map_err(|_| format!("节点 {} 端口非法: {}", n.name, n.port))?;
            let original_params = serde_json::from_str(&n.params_json)
                .map_err(|e| format!("节点 {} 参数解析失败: {e}", n.name))?;
            let (new_credential, new_password) = if n.protocol == "tuic" {
                (
                    uuid::Uuid::new_v4().to_string(),
                    Some(uuid::Uuid::new_v4().to_string()),
                )
            } else if n.protocol == "ss" {
                let key: [u8; 32] = rand::random();
                (
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key),
                    None,
                )
            } else {
                (uuid::Uuid::new_v4().to_string(), None)
            };
            let source = if let Some(upstream_id) = reused_upstreams.get(&n.id) {
                // 复用主入口的出口：入站在主配置里，克隆时给分享入站生成唯一 tag，
                // 只带被选中的 out-up-<id> 出站和路由。复制整份主配置会把同机其它
                // 上游的 SOCKS/HTTP 凭据带进分享实例，所以这里仍按出口逐条复制。
                if !matches!(n.protocol.as_str(), "vless" | "trojan" | "vmess") {
                    return Err(format!(
                        "复用主入口的上游出口 {} 的协议为 {}，当前只支持分享 vless/trojan/vmess",
                        n.name, n.protocol
                    ));
                }
                ShareSource::ReusedUpstream(*upstream_id)
            } else if upstream_node_ids.contains(&n.id) {
                // 旧专用实例的出口只有 VLESS+REALITY 一种客户端形态；真出现别的协议
                // 说明节点被别处改过，宁可显式拒绝，也不要拿错来源的配置去猜。
                if n.protocol != "vless" {
                    return Err(format!(
                        "上游出口节点 {} 的协议为 {}，当前只支持分享 VLESS 上游",
                        n.name, n.protocol
                    ));
                }
                ShareSource::Upstream
            } else {
                ShareSource::Main
            };
            Ok(ShareNodeSpec {
                name: n.name.clone(),
                protocol: n.protocol.clone(),
                original_port,
                public_port: *public,
                internal_port: *internal,
                transport,
                original_params,
                new_credential,
                new_password,
                source,
            })
        })
        .collect()
}

// ---------- 单元与防火墙 ----------

pub(crate) fn firewall_specs(pairs: &[(u16, &str)]) -> String {
    pairs
        .iter()
        .map(|(p, proto)| format!("{p}:{proto}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn hardening(dir: &str) -> String {
    format!(
        "User=vpnshare\nGroup=vpnshare\nNoNewPrivileges=yes\nProtectSystem=strict\n\
         ProtectHome=yes\nPrivateTmp=yes\nReadWritePaths={dir}\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK\n"
    )
}

pub struct ShareUnits {
    pub http: String,
    pub gateway: String,
}

pub fn unit_contents(
    id: &str,
    port: u16,
    bind: &str,
    dir: &str,
    pairs: &[(u16, &str)],
) -> ShareUnits {
    let http_script = share_server_script_path();
    let gateway_script = share_gateway_script_path();
    let firewall = firewall_script_path();
    let tls = tls_dir();
    let h = hardening(dir);
    let http = format!(
        "[Unit]\nDescription=vpn-ctl share {id} subscription\nAfter=network.target\n\n[Service]\n{h}\
         ExecStart=/usr/bin/env python3 {http_script} {port} {dir} --bind {bind} \
--cert {tls}/fullchain.pem --key {tls}/privkey.pem\n\
         ExecStartPost=+{firewall} open vpnshare-{id} {port}:tcp\n\
         ExecStopPost=+{firewall} close vpnshare-{id} {port}:tcp\n\
         Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n"
    );
    let specs = firewall_specs(pairs);
    let gateway = format!(
        "[Unit]\nDescription=vpn-ctl share {id} gateway\nAfter=network.target\n\n[Service]\n{h}\
         MemoryHigh=256M\nMemoryMax=384M\nLimitNOFILE=2048\n\
         ExecStart=/usr/bin/env python3 {gateway_script} --config {dir}/gateway.json \
--usage {dir}/usage.json --bind {bind}\n\
         ExecStartPost=+{firewall} open vpnshare-{id} {specs}\n\
         ExecStopPost=+{firewall} close vpnshare-{id} {specs}\n\
         Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n"
    );
    ShareUnits { http, gateway }
}

pub(crate) fn instance_unit(id: &str, unit: &str, kind: &str, dir: &str, config: &str) -> String {
    let exec = match kind {
        "xray" => format!("/usr/local/bin/xray run -config {config}"),
        "singbox" => format!("/usr/local/bin/sing-box run -c {config}"),
        _ => format!("/usr/local/bin/ssserver -c {config}"),
    };
    let h = hardening(dir);
    format!(
        "[Unit]\nDescription=vpn-ctl share {id} {kind} ({unit})\nAfter=network.target\n\n[Service]\n{h}\
         ExecStart={exec}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}
