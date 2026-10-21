//! 分享的 TLS/ACME 编排：域名解析校验、certbot 签发、证书落盘与续期钩子。
//!
//! 唯一入口是 [`prepare_tls`]（用户点「准备证书」）与 [`tls_status`]（向导打开时
//! 读状态）。所有失败都显式返回——**绝不退回 HTTP**：没有证书就不允许新建分享，
//! 调用方（T7）会先查 [`tls_status`]。
//!
//! 域名来源有三层：优先 `servers.share_domain`；为空时读取服务器上已有
//! `/srv/vpnshare/tls/fullchain.pem` 的 DNS SAN；仍然没有时按服务器 IP 推导
//! `sslip.io` 域名（如 `203.0.113.13` → `203-0-113-13.sslip.io`）。
//! 因此「配置 HTTPS」对新服务器也是一键的，不需要先手工填域名。
//!
//! ACME 邮箱是选填：配置了就传给 certbot 接收到期提醒；没配置则用
//! `--register-unsafely-without-email` 照常签发（证书本身与续期不受影响）。
//!
//! 命令拼装的安全边界：
//! - 域名先过 `servers::normalize_share_domain`，只允许标签字符；
//! - 邮箱先过 [`validate_email`]，拒绝空白/引号/元字符；
//! - 写进 shell 的字符串统一走 [`sh_quote`]（单引号包裹 + 内嵌单引号转义）。

use crate::domain::effective_host;
use crate::servers::deploy::{
    run_cmd, run_cmd_ok, write_remote_file, CONFIG_TIMEOUT, INSTALL_TIMEOUT,
};
use crate::servers::{CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::share::model::ShareTlsStatus;
use crate::share::naming::{firewall_script_path, tls_dir};
use crate::share::scripts;
use crate::settings::SHARE_ACME_EMAIL_KEY;
use crate::ssh::SshSession;
use crate::state::AppState;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Let's Encrypt 为每个域名保存私钥/证书的根目录。
pub const CERT_ROOT: &str = "/etc/letsencrypt/live";
/// certbot 续期成功后自动执行的 deploy hook。
pub const DEPLOY_HOOK_PATH: &str = "/etc/letsencrypt/renewal-hooks/deploy/vpnshare.sh";

/// 从**已在服务器上的分享证书**读取全部 DNS SAN（每行一个）。
///
/// 用途：用户在服务器上已经签过证书（例如上一轮真机验证留下的
/// `203-0-113-11.sslip.io`），但本地 `servers.share_domain` 还是空。此时不必让
/// 用户再手工抄一遍域名——证书本身就是事实源。
///
/// 同时尝试 `-ext subjectAltName` 与完整的 `-text`：前者在旧版本 OpenSSL 上可能
/// 不支持，而后者从 1.0 起就有。命令只含常量路径，不拼接任何用户输入。
pub fn cert_domain_cmd() -> String {
    format!(
        "{{ openssl x509 -noout -ext subjectAltName -in {}/fullchain.pem 2>/dev/null; \
         openssl x509 -noout -text -in {}/fullchain.pem 2>/dev/null \
         | grep -A1 'Subject Alternative Name'; }} \
         | grep -oE 'DNS:[^,[:space:]]+' | cut -c5- | sort -u",
        tls_dir(),
        tls_dir()
    )
}

/// 读取证书里**可用**的域名列表：非法标签与通配符 SAN 会被跳过。
///
/// 这里**不判断证书是否过期**：调用方已经用 `openssl x509 -checkend 0` 判过。
/// 单独拆出来是为了让创建路径在极少数绕过 `tls_status` 的调用里也能兜底。
pub(crate) async fn cert_domains(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<Vec<String>, String> {
    let out = run_cmd(sess, cancel, "证书校验", &cert_domain_cmd(), PROBE_TIMEOUT).await?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| crate::servers::normalize_share_domain(line).ok())
        .collect())
}

/// 自动检测时取证书里第一个合法域名；没有则返回空串。
pub(crate) async fn detect_cert_domain(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<String, String> {
    Ok(cert_domains(sess, cancel)
        .await?
        .into_iter()
        .next()
        .unwrap_or_default())
}

/// 未配置域名、服务器上也没有证书时的兜底域名：把服务器 IP 直接映射到
/// [sslip.io](https://sslip.io)。
///
/// - IPv4：`203.0.113.13` → `203-0-113-13.sslip.io`
/// - IPv6：`2001:db8::1` → `2001-db8--1.sslip.io`（冒号逐个换成短横线）
///
/// 只接受**IP 字面量**。主机名不做猜测：它可能已经是域名，也可能解析到别处，
/// 猜出来的 sslip 域名反而会绕过用户自己的 DNS 规划。sslip.io 是公共通配 DNS，
/// 不需要在服务器上装任何东西；签出的证书仍由本机 certbot 持有。
pub(crate) fn auto_share_domain(server: &crate::domain::Server) -> Option<String> {
    sslip_domain(effective_host(server))
}

/// [`auto_share_domain`] 的纯字符串版本，便于单测。
pub(crate) fn sslip_domain(host: &str) -> Option<String> {
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return Some(format!("{}.sslip.io", ip.to_string().replace('.', "-")));
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        return Some(format!("{}.sslip.io", ip.to_string().replace(':', "-")));
    }
    None
}

/// 单引号安全包装：shell 里唯一不能出现在单引号串中的字符就是 `'`。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 在服务器上用 python3 解析域名的全部地址（IPv4 + IPv6）。
pub fn dns_check_cmd(domain: &str) -> String {
    format!(
        "python3 -c 'import socket,sys;print(\"\\n\".join(sorted({{a[4][0] for a in socket.getaddrinfo(sys.argv[1],None)}})))' {}",
        sh_quote(domain)
    )
}

/// ACME 邮箱会拼进 root shell 命令，必须先拒绝空白/引号/元字符。
pub fn validate_email(email: &str) -> Result<(), String> {
    if email.len() > 254
        || email.contains(|c: char| c.is_whitespace() || "'\"`$;|&<>(){}[]\\".contains(c))
    {
        return Err("ACME 邮箱格式不合法".into());
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("ACME 邮箱格式不合法".into());
    };
    if local.is_empty() || !domain.contains('.') {
        return Err("ACME 邮箱格式不合法".into());
    }
    Ok(())
}

/// 按发行版安装 certbot；已有 certbot 时直接成功。
///
/// 只走系统包管理器，不下载任何来路不明的安装脚本。全部尝试都失败时打印
/// `NO_CERTBOT_PKG_MANAGER` 并以非零退出，调用方据此给出可读错误。
pub fn certbot_install_script() -> String {
    concat!(
        "if command -v certbot >/dev/null 2>&1; then exit 0; fi\n",
        "if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && apt-get install -y certbot;\n",
        "elif command -v dnf >/dev/null 2>&1; then dnf install -y certbot;\n",
        "elif command -v yum >/dev/null 2>&1; then yum install -y certbot;\n",
        "elif command -v pacman >/dev/null 2>&1; then pacman -Sy --noconfirm certbot;\n",
        "elif command -v zypper >/dev/null 2>&1; then zypper --non-interactive install certbot;\n",
        "elif command -v apk >/dev/null 2>&1; then apk add --no-cache certbot;\n",
        "else echo NO_CERTBOT_PKG_MANAGER; exit 1; fi\n"
    )
    .to_string()
}

/// 拼 certbot 签发命令。邮箱为空时显式使用 certbot 的免邮箱注册开关：
/// Let's Encrypt 允许没有联系邮箱的账户，只是收不到证书到期提醒；证书有效期
/// 与自动续期不受影响。
pub fn certbot_issue_cmd(domain: &str, email: &str) -> String {
    let contact = if email.is_empty() {
        "--register-unsafely-without-email".to_string()
    } else {
        format!("-m {}", sh_quote(email))
    };
    format!(
        "certbot certonly --standalone -d {} --non-interactive --agree-tos {} --keep-until-expiring",
        sh_quote(domain),
        contact
    )
}

/// 续期钩子：复制新证书到分享 TLS 目录，并重启所有 `vpnshare-*` 服务。
///
/// 旧分享的单元名不带 TLS，重启它们只是多一次无害的重启；新分享必须重启才能
/// 加载新证书。`|| true` 保证单个单元失败不会让 certbot 续期整体报错。
pub fn renewal_hook_content(domain: &str) -> String {
    let tls = tls_dir();
    format!(
        "#!/bin/sh\nset -e\ncp {CERT_ROOT}/{domain}/fullchain.pem {tls}/fullchain.pem\n\
         cp {CERT_ROOT}/{domain}/privkey.pem {tls}/privkey.pem\n\
         chown vpnshare:vpnshare {tls}/fullchain.pem {tls}/privkey.pem\n\
         chmod 600 {tls}/fullchain.pem {tls}/privkey.pem\n\
         for u in $(systemctl list-units --type=service --all --no-legend 'vpnshare-*.service' | awk '{{print $1}}'); do systemctl restart \"$u\" || true; done\n"
    )
}

/// 订阅服务、网关与分享实例都依赖 python3；缺失时显式失败，不降级。
pub(crate) async fn probe_python3(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let out = run_cmd(sess, cancel, "检查", "command -v python3", PROBE_TIMEOUT).await?;
    if out.exit_code != 0 || out.stdout.trim().is_empty() {
        return Err("服务器缺少 python3（订阅服务与网关依赖它）".into());
    }
    Ok(())
}

async fn install_certbot(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<(), String> {
    match run_cmd_ok(
        sess,
        cancel,
        "安装 certbot",
        &certbot_install_script(),
        INSTALL_TIMEOUT,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.contains("NO_CERTBOT_PKG_MANAGER") => {
            Err("服务器没有可用的包管理器，无法自动安装 certbot".into())
        }
        Err(e) => Err(format!("安装 certbot 失败：{e}")),
    }
}

/// 为服务器的分享域名签发/续期证书，并部署到 `/srv/vpnshare/tls/`。
///
/// 步骤：校验已配置的邮箱（有才校验）→ 连接 → python3 → 域名（配置 → 证书
/// SAN 自动检测 → IP 推导的 sslip.io 域名）→ DNS 交集 → 80 空闲 →
/// 创建 vpnshare 用户与 TLS 目录 → 放行 80 → 安装 certbot → 签发 → 复制证书 →
/// 续期钩子 → 校验到期时间 → **无论成败都撤回 80 放行**。
pub async fn prepare_tls(
    state: &Arc<AppState>,
    server_id: i64,
) -> Result<ShareTlsStatus, String> {
    let (auth, server) =
        crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
    let configured = crate::servers::normalize_share_domain(&server.share_domain)?;
    let email = state
        .settings
        .get(SHARE_ACME_EMAIL_KEY)
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .trim()
        .to_string();
    // 邮箱选填：没配置就交给 certbot 的免邮箱注册开关。配置了才需要校验格式。
    if !email.is_empty() {
        validate_email(&email)?;
    }

    let mut sess = SshSession::connect(
        effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;
    let cancel = AtomicBool::new(false);

    probe_python3(&mut sess, &cancel).await?;

    // 分享域名同样支持自动检测：续期场景下服务器上的证书可能刚好过期，但 SAN
    // 仍是有效信息，可直接用它重新签发，不必让用户手工再抄域名。
    // 最后兜底是按 IP 推导 sslip.io 域名——首次为某台服务器配置 HTTPS 时，
    // 前两层通常都是空的，没有这一层「配置 HTTPS」就还得先手工填域名。
    let domain = if configured.is_empty() {
        let detected = cert_domains(&mut sess, &cancel)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        if detected.is_empty() {
            auto_share_domain(&server).unwrap_or_default()
        } else {
            detected
        }
    } else {
        configured
    };
    if domain.is_empty() {
        return Err(
            "该服务器未配置分享域名，服务器上也没有可自动检测的分享证书；\
             主机名不是 IP，无法自动推导 sslip.io 域名（服务器 → 编辑 → 分享域名）"
                .into(),
        );
    }

    // DNS：域名解析结果必须与本机登记地址有交集，否则 HTTP-01 必然失败。
    let mut server_ips = HashSet::new();
    for h in [&server.host, &server.host_v6] {
        if h.is_empty() {
            continue;
        }
        let out = run_cmd_ok(&mut sess, &cancel, "DNS", &dns_check_cmd(h), PROBE_TIMEOUT).await?;
        server_ips.extend(
            out.stdout
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    let out = run_cmd_ok(
        &mut sess,
        &cancel,
        "DNS",
        &dns_check_cmd(&domain),
        PROBE_TIMEOUT,
    )
    .await?;
    let domain_ips: HashSet<String> = out
        .stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if domain_ips.is_empty() || server_ips.is_empty() || domain_ips.is_disjoint(&server_ips) {
        return Err(format!(
            "域名 {domain} 未解析到本机（域名: {domain_ips:?}，本机: {server_ips:?}）"
        ));
    }

    // HTTP-01 standalone 需要本地 80 空闲。
    let out = run_cmd(
        &mut sess,
        &cancel,
        "探测",
        &crate::share::plan::tcp_port_probe_cmd(80),
        PROBE_TIMEOUT,
    )
    .await?;
    if out.exit_code == 0 {
        return Err("80 端口已被占用，certbot --standalone 无法使用".into());
    }

    // 运行分享进程的系统用户：无 shell、无 home；已存在则跳过。
    run_cmd_ok(
        &mut sess,
        &cancel,
        "准备",
        "id -u vpnshare >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin vpnshare 2>/dev/null || adduser -S -D -H -s /sbin/nologin vpnshare",
        CONFIG_TIMEOUT,
    )
    .await?;
    let tls = tls_dir();
    run_cmd_ok(
        &mut sess,
        &cancel,
        "准备",
        &format!("mkdir -p {tls} && chown vpnshare:vpnshare {tls} && chmod 700 {tls}"),
        CONFIG_TIMEOUT,
    )
    .await?;

    // 防火墙助手以 root 执行；普通用户只能读/执行，不能改写。
    let firewall = firewall_script_path();
    write_remote_file(
        &mut sess,
        &cancel,
        "准备",
        &firewall,
        &scripts::firewall_script(),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        &cancel,
        "准备",
        &format!("chmod 755 {firewall}"),
        CONFIG_TIMEOUT,
    )
    .await?;
    run_cmd_ok(
        &mut sess,
        &cancel,
        "放行",
        &format!("{firewall} open vpnshare-tls 80:tcp"),
        CONFIG_TIMEOUT,
    )
    .await?;

    // 从这里开始无论成败都要撤回 80 放行。不能直接 `?` 返回，否则失败路径会把
    // 80 永久留在 INPUT 链上。
    let result: Result<ShareTlsStatus, String> = async {
        install_certbot(&mut sess, &cancel).await?;
        run_cmd_ok(
            &mut sess,
            &cancel,
            "申请证书",
            &certbot_issue_cmd(&domain, &email),
            INSTALL_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            &mut sess,
            &cancel,
            "部署证书",
            &format!(
                "install -m 600 {CERT_ROOT}/{domain}/fullchain.pem {tls}/fullchain.pem && \
                 install -m 600 {CERT_ROOT}/{domain}/privkey.pem {tls}/privkey.pem && \
                 chown vpnshare:vpnshare {tls}/fullchain.pem {tls}/privkey.pem"
            ),
            CONFIG_TIMEOUT,
        )
        .await?;
        write_remote_file(
            &mut sess,
            &cancel,
            "部署证书",
            DEPLOY_HOOK_PATH,
            &renewal_hook_content(&domain),
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            &mut sess,
            &cancel,
            "部署证书",
            &format!("chmod 755 {DEPLOY_HOOK_PATH}"),
            CONFIG_TIMEOUT,
        )
        .await?;
        run_cmd_ok(
            &mut sess,
            &cancel,
            "证书校验",
            &format!("openssl x509 -checkend 0 -noout -in {tls}/fullchain.pem"),
            PROBE_TIMEOUT,
        )
        .await?;
        let out = run_cmd(
            &mut sess,
            &cancel,
            "证书校验",
            &format!("openssl x509 -enddate -noout -in {tls}/fullchain.pem"),
            PROBE_TIMEOUT,
        )
        .await?;
        Ok(ShareTlsStatus {
            domain: domain.clone(),
            ready: true,
            cert_expires_at: parse_cert_expiry(&out.stdout),
            detail: "证书已就绪".into(),
        })
    }
    .await;

    let _ = run_cmd_ok(
        &mut sess,
        &cancel,
        "清理",
        &format!("{firewall} close vpnshare-tls 80:tcp"),
        CONFIG_TIMEOUT,
    )
    .await;

    result
}

/// 读当前证书状态。
///
/// 域名解析顺序：
/// 1. `servers.share_domain` 显式配置的值；
/// 2. 为空时，从服务器 `/srv/vpnshare/tls/fullchain.pem` 的 DNS SAN 自动检测；
/// 3. 仍为空且主机是 IP 时，推导 `sslip.io` 域名（供「配置 HTTPS」一键签发）。
///
/// 因此 `share_domain` 为空**不再等于「不连服务器、直接失败」**：服务器上已有
/// 有效证书时，向导仍应显示可用的 HTTPS 域名并允许创建；首次配置时也能直接
/// 用推导出的域名签发。ACME 邮箱只用于**签发新证书**（且为选填），不影响已有
/// 证书的使用。
pub async fn tls_status(
    state: &Arc<AppState>,
    server_id: i64,
) -> Result<ShareTlsStatus, String> {
    let (auth, server) =
        crate::servers::load_auth(state, server_id).map_err(|e| e.to_string())?;
    let email = acme_email(state)?;
    let mut sess = SshSession::connect(
        effective_host(&server),
        server.port as u16,
        &server.user,
        &auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("连接服务器失败: {e}"))?;
    read_tls_status(&server, &email, &mut sess).await
}

/// 读分享 ACME 邮箱（未配置 = 空串）。签发与状态读取两处共用。
pub(crate) fn acme_email(state: &Arc<AppState>) -> Result<String, String> {
    Ok(state
        .settings
        .get(SHARE_ACME_EMAIL_KEY)
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .trim()
        .to_string())
}

/// 在**已建立的会话**上读证书状态：分享轮询与手动查询共用一次连接，
/// 避免每台服务器多开一条 SSH。
pub(crate) async fn read_tls_status(
    server: &crate::domain::Server,
    email: &str,
    sess: &mut SshSession,
) -> Result<ShareTlsStatus, String> {
    let configured = crate::servers::normalize_share_domain(&server.share_domain)?;
    // 没有配置、证书里也读不到时，向导要能提前告诉用户「点一下就会用哪个域名」。
    let auto = auto_share_domain(server);
    let cancel = AtomicBool::new(false);
    let cert = format!("{}/fullchain.pem", tls_dir());

    let check = run_cmd(
        sess,
        &cancel,
        "证书校验",
        &format!("openssl x509 -checkend 0 -noout -in {cert}"),
        PROBE_TIMEOUT,
    )
    .await?;
    if check.exit_code != 0 {
        let detail = if !configured.is_empty() {
            if email.is_empty() {
                "证书不存在或已过期；点击「配置 HTTPS」即可签发（未填 ACME 邮箱不影响签发）"
                    .to_string()
            } else {
                "证书不存在或已过期；点击「配置 HTTPS」即可签发".to_string()
            }
        } else if let Some(d) = &auto {
            if email.is_empty() {
                format!(
                    "未配置分享域名与 ACME 邮箱；点击「配置 HTTPS」将自动使用 {d} 签发（无需邮箱）"
                )
            } else {
                format!("未配置分享域名；点击「配置 HTTPS」将自动使用 {d} 签发")
            }
        } else {
            // 主机名既不是 IPv4 也不是 IPv6：没有可推导的 sslip.io 域名。
            "未配置分享域名，且服务器上没有可用的分享证书；该服务器地址不是 IP，\
             无法自动推导域名（服务器 → 编辑 → 分享域名）"
                .to_string()
        };
        return Ok(ShareTlsStatus {
            domain: if configured.is_empty() {
                auto.unwrap_or_default()
            } else {
                configured
            },
            ready: false,
            cert_expires_at: None,
            detail,
        });
    }

    let domains = cert_domains(sess, &cancel).await?;
    let detected = domains.first().cloned().unwrap_or_default();
    let domain = if configured.is_empty() {
        detected.clone()
    } else {
        configured.clone()
    };
    if domain.is_empty() {
        return Ok(ShareTlsStatus {
            domain,
            ready: false,
            cert_expires_at: None,
            detail: "服务器证书有效，但证书里没有可用的 DNS 域名".into(),
        });
    }
    // 配置值必须出现在证书 SAN 中，否则不能假装就绪：链接会用配置域名，而证书
    // 只对 SAN 里的名字有效，客户端必然握手失败。空 SAN（老测试夹具/异常证书）
    // 不触发此判。
    if !configured.is_empty() && !domains.is_empty() && !domains.contains(&configured) {
        return Ok(ShareTlsStatus {
            domain: configured.clone(),
            ready: false,
            cert_expires_at: None,
            detail: format!("服务器证书域名 {detected} 与配置的分享域名 {configured} 不一致"),
        });
    }

    let out = run_cmd(
        sess,
        &cancel,
        "证书校验",
        &format!("openssl x509 -enddate -noout -in {cert}"),
        PROBE_TIMEOUT,
    )
    .await?;
    Ok(ShareTlsStatus {
        domain,
        ready: true,
        cert_expires_at: parse_cert_expiry(&out.stdout),
        detail: if configured.is_empty() {
            "已从服务器证书自动检测到分享域名".into()
        } else {
            "证书已就绪".into()
        },
    })
}

/// `openssl x509 -enddate` 的输出形如 `notAfter=Jan  1 00:00:00 2027 GMT`。
fn parse_cert_expiry(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("notAfter=")
            .map(|s| s.trim().to_string())
    })
}
