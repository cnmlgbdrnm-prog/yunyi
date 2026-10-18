//! WG 配置渲染与段级管理（纯函数）：中转/落地 wg0.conf、设备 .conf、
//! DNAT PostUp/PostDown 命令组、peer 段按公钥定位的替换/追加/删除、
//! 多链路共享 Interface 的段级合并、网段与设备 IP 分配。
//! 输入：RelayRule/DevicePeerRow/密钥文本；输出：配置文本。
//! 无 IO/DB 依赖，供 build.rs/peers.rs 复用，金样由 config_tests 锁定。

use crate::domain::{DevicePeerRow, RelayRule};

/// 回程伪装（PostUp/PostDown 一对）：DNAT 只改目的地址，**源地址仍是客户端公网 IP**，
/// 落地机回包会按自己的默认路由出去（源是 WG 内网口），客户端永远等不到响应——
/// 表现为「隧道握手正常、端口也通了，但连上去就超时」。
/// 手工搭的中转链路都带这一条（实测 203.0.113.127 上 `-A POSTROUTING -o wg1 -j MASQUERADE`），
/// 本工具此前整份配置里没有任何 MASQUERADE，于是自己建的转发链路一条都通不了。
/// 按接口收口（不是按端口逐条）：一条链路只写一次，多协议也不会重复。
pub fn masquerade_pair(wg_iface: &str) -> (String, String) {
    let rule = format!("iptables -t nat -A POSTROUTING -o {wg_iface} -j MASQUERADE");
    (rule.clone(), rule.replace("-A", "-D"))
}

/// 是不是本链写下的回程伪装行（重建时替换而不是堆积）。
pub fn is_masquerade_line(body: &str, wg_iface: &str) -> bool {
    body.contains("POSTROUTING")
        && body.contains(&format!("-o {wg_iface} "))
        && body.contains("-j MASQUERADE")
}

/// 单端口链路的转发计划：(入口端口, 落地端口)。整机转发不用它——那条链路的端口表
/// 派生自落地机的自建节点（见 `relay::same_port_plan`）。
pub fn dnat_plan(rule: &RelayRule) -> Vec<(u16, u16)> {
    vec![(
        rule.forward_port.unwrap_or(443) as u16,
        rule.landing_port as u16,
    )]
}

/// 中转机 wg0.conf：模式 A（forward）挂 DNAT PostUp/PostDown；
/// Peer(落地) AllowedIPs 恒为落地机 /32（两种模式统一），设备段为各自 wg_ip/32。
///
/// `ports` 是本次链路的完整转发计划：整机转发下有多条，**必须全部写进 PostUp**——
/// 只有 PostUp 里的那份会在 wg-quick 重启后自动恢复，光靠运维时补的 iptables 命令
/// 重启一次就没了。
pub fn render_relay_conf(
    rule: &RelayRule,
    relay_privkey: &str,
    landing_pubkey: &str,
    egress_if: &str,
    ports: &[(u16, u16)],
) -> String {
    let (post_up, post_down) = if rule.client_mode == "forward" {
        let mut ups: Vec<String> = Vec::new();
        let mut downs: Vec<String> = Vec::new();
        for (fwd_port, tgt_port) in ports {
            let (u, d) = render_dnat_pair(egress_if, *fwd_port, &rule.landing_wg_ip, *tgt_port);
            ups.extend(u);
            downs.extend(d);
        }
        // 回程伪装一条足矣（按隧道接口收口，不随端口数增加）
        let (m_up, m_down) = masquerade_pair(crate::relay::WG_IFACE);
        ups.push(m_up);
        downs.push(m_down);
        (ups.join("\n"), downs.join("\n"))
    } else {
        (String::new(), String::new())
    };
    // 落地段恒为落地机自身 /32（两种模式统一）：WG cryptokey 路由同一接口上
    // AllowedIPs 前缀不得重叠——若落地段覆盖 subnet/24 会与设备段（各自身 /32）
    // 同前缀冲突，后添加者抹除先者（加设备后落地机不可达）；落地机回程由
    // 落地侧 conf AllowedIPs=subnet/24 承担，中转侧无需覆盖
    let landing_allowed = format!("{}/32", rule.landing_wg_ip);
    let mut out = format!(
        "[Interface]\nPrivateKey = {relay_privkey}\nListenPort = {}\nAddress = {}/24\n",
        rule.wg_port, rule.relay_wg_ip
    );
    if !post_up.is_empty() {
        out.push_str(&format!(
            "PostUp = {}\n",
            post_up.replace('\n', "\nPostUp = ")
        ));
        out.push_str(&format!(
            "PostDown = {}\n",
            post_down.replace('\n', "\nPostDown = ")
        ));
    }
    out.push_str(&format!(
        "\n[Peer]\nPublicKey = {landing_pubkey}\nAllowedIPs = {landing_allowed}\nPersistentKeepalive = 25\n"
    ));
    out
}

/// 落地机 wg0.conf：Peer 指向中转公网地址，回程网段覆盖全隧道（含设备）。
pub fn render_landing_conf(
    rule: &RelayRule,
    landing_privkey: &str,
    relay_pubkey: &str,
    relay_public_host: &str,
) -> String {
    format!(
        "[Interface]\nPrivateKey = {landing_privkey}\nAddress = {}/24\n\n[Peer]\nPublicKey = {relay_pubkey}\nEndpoint = {relay_public_host}:{}\nAllowedIPs = {}\nPersistentKeepalive = 25\n",
        rule.landing_wg_ip, rule.wg_port, rule.subnet
    )
}

/// 设备 WG 客户端 .conf：私钥/PSK 由调用方解密后传入，即用即弃。
pub fn render_device_conf(
    peer: &DevicePeerRow,
    privkey: &str,
    psk: &str,
    rule: &RelayRule,
    relay_pubkey: &str,
    relay_public_host: &str,
) -> String {
    format!(
        "[Interface]\nPrivateKey = {privkey}\nAddress = {}/32\nDNS = 1.1.1.1\n\n[Peer]\nPublicKey = {relay_pubkey}\nPresharedKey = {psk}\nEndpoint = {relay_public_host}:{}\nAllowedIPs = {}\nPersistentKeepalive = 25\n",
        peer.wg_ip, rule.wg_port, rule.subnet
    )
}

/// 模式 A DNAT 命令对：(PostUp 组, PostDown 组)。iptables 幂等语义由调用方处理。
pub fn render_dnat_pair(
    egress_if: &str,
    forward_port: u16,
    target_ip: &str,
    target_port: u16,
) -> (Vec<String>, Vec<String>) {
    // 值一律单引号包裹：这一行会进 wg0.conf 的 PostUp，由 wg-quick 以 root 执行。
    // 调用方（build.rs）已用 valid_iface_name 过了白名单，这里是第二道。
    let iface = crate::util::shell_quote(egress_if);
    let target = crate::util::shell_quote(&format!("{target_ip}:{target_port}"));
    let daddr = crate::util::shell_quote(target_ip);
    let nat = format!(
        "iptables -t nat -A PREROUTING -i {iface} -p tcp --dport {forward_port} -j DNAT --to-destination {target}"
    );
    let fwd = format!("iptables -A FORWARD -p tcp -d {daddr} --dport {target_port} -j ACCEPT");
    (
        vec![nat.clone(), fwd.clone()],
        vec![nat.replace("-A", "-D"), fwd.replace("-A", "-D")],
    )
}

/// peer 段级替换/追加：按段内 `PublicKey = ` 行定位；未匹配则整段追加到末尾。
pub fn upsert_peer_section(conf: &str, peer_section: &str) -> String {
    let key = peer_section
        .lines()
        .find(|l| l.starts_with("PublicKey = "))
        .and_then(|l| l.strip_prefix("PublicKey = "));
    let Some(key) = key else {
        return format!("{conf}\n{peer_section}\n");
    };
    if let Some(replaced) = replace_section_by_key(conf, key, peer_section.trim()) {
        return replaced;
    }
    format!("{}\n{}\n", conf.trim_end(), peer_section.trim())
}

/// 删除指定公钥的 peer 段；无匹配原样返回。
pub fn remove_peer_section(conf: &str, public_key: &str) -> String {
    replace_section_by_key(conf, public_key, "").unwrap_or_else(|| conf.to_string())
}

/// 中转机 conf 段级合并（多链路共享 Interface）：既有 conf 非空时，[Interface] 的
/// PrivateKey/ListenPort/Address 与他链 PostUp/PostDown 原样保留（缺项补默认），
/// 仅按本链 forward 规则行（目标落地机 IP:端口）去重合并 PostUp/PostDown
/// （他链行保留、本链行替换）；[Peer] 段仅移除本链旧落地段（AllowedIPs 匹配且
/// 无 PresharedKey——设备段带 PresharedKey 不受影响），其余链路与设备段原样
/// 保留，再追加新落地段。既有 conf 为空（首建）→ 整份渲染。
pub fn merge_relay_conf(
    rule: &RelayRule,
    old_conf: &str,
    relay_privkey: &str,
    landing_pubkey: &str,
    egress_if: &str,
    ports: &[(u16, u16)],
) -> String {
    if old_conf.trim().is_empty() {
        return render_relay_conf(rule, relay_privkey, landing_pubkey, egress_if, ports);
    }
    // 旧落地段判别：新格式 landing_wg_ip/32 与旧格式 subnet/24（本工具前版所写）
    // 双匹配；设备段带 PresharedKey 不受影响
    let old_landing_allowed = if rule.client_mode == "peer" {
        rule.subnet.clone()
    } else {
        format!("{}/32", rule.landing_wg_ip)
    };
    let landing_32 = format!("{}/32", rule.landing_wg_ip);
    // 「本链行」按整份端口表判：整机转发下本链有多个端口，只看 landing_port 会把
    // 自己其余的 DNAT 行当成别条链路保留下来（重建时旧端口清不掉）
    // 我们自己写的行现在带单引号（见 render_dnat_pair），**老版本写的不带**——匹配前先
    // 把引号抹掉，两代格式都认。少了这一步，升级后第一次重建会把旧 DNAT 行当成别条
    // 链路的保留下来：端口关不掉、PostUp 越堆越多（正是本函数注释里那个 bug 的回归）。
    let is_own_nat = |body: &str| {
        let body = body.replace('\'', "");
        is_masquerade_line(&body, crate::relay::WG_IFACE)
            || ports.iter().any(|(_, tgt)| {
                body.contains(&format!("--to-destination {}:{}", rule.landing_wg_ip, tgt))
                    || body.contains(&format!("-d {} --dport {}", rule.landing_wg_ip, tgt))
            })
    };
    let lines = old_conf.lines().collect::<Vec<_>>();
    let mut out: Vec<String> = Vec::new(); // 段外行（文件头注释等）
    let mut interface_lines: Vec<String> = Vec::new(); // Interface 保留行（不含 Post 行）
    let mut peer_sections: Vec<String> = Vec::new();
    let mut in_interface = false;
    let mut has_privkey = false;
    let mut has_listen = false;
    let mut has_address = false;
    let mut kept_up: Vec<&str> = Vec::new();
    let mut kept_down: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t == "[Interface]" {
            in_interface = true;
            i += 1;
        } else if t == "[Peer]" {
            let mut j = i + 1;
            let mut allowed: Option<&str> = None;
            let mut has_psk = false;
            while j < lines.len() && lines[j].trim() != "[Peer]" && lines[j].trim() != "[Interface]"
            {
                let lt = lines[j].trim();
                if let Some(a) = lt.strip_prefix("AllowedIPs = ") {
                    allowed = Some(a);
                }
                if lt.starts_with("PresharedKey = ") {
                    has_psk = true;
                }
                j += 1;
            }
            if (allowed == Some(old_landing_allowed.as_str())
                || allowed == Some(landing_32.as_str()))
                && !has_psk
            {
                // 本链旧落地段：整段丢弃，新段末尾追加
                i = j;
                continue;
            }
            peer_sections.push(lines[i..j].join("\n"));
            i = j;
        } else if in_interface {
            let lt = lines[i].trim();
            if let Some(body) = lt.strip_prefix("PostUp = ") {
                if !is_own_nat(body) {
                    kept_up.push(body);
                }
            } else if let Some(body) = lt.strip_prefix("PostDown = ") {
                if !is_own_nat(body) {
                    kept_down.push(body);
                }
            } else if lt.starts_with("PrivateKey = ") {
                has_privkey = true;
                interface_lines.push(lines[i].to_string());
            } else if lt.starts_with("ListenPort = ") {
                has_listen = true;
                interface_lines.push(lines[i].to_string());
            } else if lt.starts_with("Address = ") {
                has_address = true;
                interface_lines.push(lines[i].to_string());
            } else {
                interface_lines.push(lines[i].to_string());
            }
            i += 1;
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }
    // 组装：Interface（缺项补默认）→ 保留行 + 本链 DNAT 行 → 其余段 + 新落地段
    out.push("[Interface]".into());
    if !has_privkey {
        out.push(format!("PrivateKey = {relay_privkey}"));
    }
    if !has_listen {
        out.push(format!("ListenPort = {}", rule.wg_port));
    }
    if !has_address {
        out.push(format!("Address = {}/24", rule.relay_wg_ip));
    }
    out.extend(interface_lines);
    for body in kept_up {
        out.push(format!("PostUp = {body}"));
    }
    for body in kept_down {
        out.push(format!("PostDown = {body}"));
    }
    if rule.client_mode == "forward" {
        for (fwd_port, tgt_port) in ports {
            let (ups, downs) =
                render_dnat_pair(egress_if, *fwd_port, &rule.landing_wg_ip, *tgt_port);
            for body in ups {
                out.push(format!("PostUp = {body}"));
            }
            for body in downs {
                out.push(format!("PostDown = {body}"));
            }
        }
        let (m_up, m_down) = masquerade_pair(crate::relay::WG_IFACE);
        out.push(format!("PostUp = {m_up}"));
        out.push(format!("PostDown = {m_down}"));
    }
    out.push(String::new());
    for sec in peer_sections {
        out.push(sec);
        out.push(String::new());
    }
    // 新落地段同样为落地机 /32（与 render_relay_conf 一致，避免同前缀覆盖）
    out.push(format!(
        "[Peer]\nPublicKey = {landing_pubkey}\nAllowedIPs = {landing_32}\nPersistentKeepalive = 25"
    ));
    out.join("\n").trim_end().to_string() + "\n"
}

/// 从 conf 的 [Interface] 段提取 PrivateKey（无则 None）——rebuild 复用之，不轮换。
pub fn interface_private_key(conf: &str) -> Option<String> {
    let lines = conf.lines().collect::<Vec<_>>();
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == "[Interface]" {
            for line in lines.iter().skip(i + 1) {
                let t = line.trim();
                if t == "[Peer]" || t == "[Interface]" {
                    break;
                }
                if let Some(k) = t.strip_prefix("PrivateKey = ") {
                    return Some(k.to_string());
                }
            }
            return None;
        }
    }
    None
}

/// 按段内 AllowedIPs 定位 peer 段并返回其 PublicKey（拆链定位旧落地段用）；未匹配返回 None。
pub fn peer_section_pubkey(conf: &str, allowed: &str) -> Option<String> {
    let lines = conf.lines().collect::<Vec<_>>();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[Peer]" {
            let mut j = i + 1;
            let mut pubkey: Option<String> = None;
            let mut sec_allowed: Option<&str> = None;
            while j < lines.len() && lines[j].trim() != "[Peer]" && lines[j].trim() != "[Interface]"
            {
                let t = lines[j].trim();
                if let Some(k) = t.strip_prefix("PublicKey = ") {
                    pubkey = Some(k.to_string());
                }
                if let Some(a) = t.strip_prefix("AllowedIPs = ") {
                    sec_allowed = Some(a);
                }
                j += 1;
            }
            if sec_allowed == Some(allowed) {
                return pubkey;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// 按公钥定位 [Peer] 段并整体替换（new 为空串即删除）；未匹配返回 None。
fn replace_section_by_key(conf: &str, public_key: &str, new: &str) -> Option<String> {
    let lines = conf.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .enumerate()
        .find(|(i, l)| {
            l.trim() == "[Peer]"
                && lines
                    .get(i + 1)
                    .map(|n| n.trim() == format!("PublicKey = {public_key}"))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)?;
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if l.trim() == "[Peer]" || l.trim() == "[Interface]" {
            end = i;
            break;
        }
    }
    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 2);
    out.extend(&lines[..start]);
    if !new.is_empty() {
        out.push(new);
        out.push("");
    }
    out.extend(&lines[end..]);
    Some(join_lines(&out))
}

/// 归一化拼接：trim 尾部空白后补一个换行，删除段不留空行成对。
fn join_lines(lines: &[&str]) -> String {
    lines.join("\n").trim_end().to_string() + "\n"
}

/// 网段分配：10.9.x.0/24 从 0 起返回第一个未占用段。
pub fn alloc_subnet(taken: &[String]) -> String {
    let mut i = 0u32;
    loop {
        let candidate = format!("10.9.{i}.0/24");
        if !taken.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// 设备 IP 分配：.3 起；有既有时末段 +1（删除不复用）。
pub fn alloc_peer_ip(rule: &RelayRule, existing_last: Option<&str>) -> String {
    let prefix = rule.subnet.trim_end_matches(".0/24");
    match existing_last {
        None => format!("{prefix}.3"),
        Some(last) => {
            let n: u32 = last
                .rsplit('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2);
            format!("{prefix}.{}", n + 1)
        }
    }
}
