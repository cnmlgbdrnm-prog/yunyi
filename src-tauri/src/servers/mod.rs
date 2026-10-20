//! 服务器服务：CRUD（凭据 seal 入库）、连通测试、一键免密（ed25519 生成+下发+切换）；
//! deploy 子模块提供部署执行器（探测→安装→配置→启动→写回全序列）。
//! 输入：CreateServerInput/UpdateServerInput/服务器 id；输出：Server/ConnectionTest。
//! 凭据经 CredentialService seal 存储，读取即解密即用即弃；auth_ref 不出本层。

pub mod channels;
pub mod deploy;
pub mod deploy_spec;
pub mod ipv6_detect;
pub mod privacy;
pub mod reality_dest;
pub mod remote_file;
pub mod upstream;

use crate::crypto::CredentialService;
use crate::db::repos;
use crate::domain::{ConnectionTest, CreateServerInput, Server, UpdateServerInput};
use crate::ssh::{AuthMaterial, SshError, SshSession};
use crate::state::AppState;
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum ServersError {
    #[error("{0}")]
    Validation(String),
    #[error("服务器不存在")]
    NotFound,
    #[error("数据库错误: {0}")]
    Db(String),
    #[error("凭据加密错误: {0}")]
    Crypto(String),
    #[error("存在关联记录，不能删除（有部署、中转规则、通道或上游出口引用该服务器）")]
    HasDependents,
    #[error("本机免密准备失败: {0}")]
    LocalKey(String),
    #[error(transparent)]
    Ssh(#[from] SshError),
}

/// 输入净化：去除粘贴常见的零宽字符（U+200B 等，不属于 is_whitespace，
/// 残留会导致 DNS 解析失败 os error 11001），再去除首尾空白。
pub fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
            )
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// IPv6 输入净化：去零宽字符/首尾空白，并容忍用户粘贴 `[addr]` 形式。
/// 数据库只存裸字面量；显示与 URL 拼接时再按需加方括号。
fn clean_host_v6(s: &str) -> String {
    clean_text(s)
        .trim_matches(|c| c == '[' || c == ']')
        .to_string()
}

/// 分享域名归一化：只接受裸域名（可空）。空串 = 未配置。
/// 统一小写；拒绝 scheme、路径、端口、空格与非法标签，避免把 URL 当域名拼进
/// certbot/iptables 命令。
pub fn normalize_share_domain(raw: &str) -> Result<String, String> {
    let d = clean_text(raw).to_ascii_lowercase();
    if d.is_empty() {
        return Ok(d);
    }
    if d.len() > 253 || d.contains("://") || d.contains('/') || d.contains(':') || d.contains(' ') {
        return Err("分享域名只能是裸域名，例如 share.example.com".into());
    }
    let ok = d.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    if !ok {
        return Err("分享域名格式不合法".into());
    }
    Ok(d)
}

/// 校验输入并返回规整后的 Server 行（不含 id/created_at 的派生字段）。
fn validate_fields(
    name: &str,
    host: &str,
    host_v6: &str,
    port: i64,
    user: &str,
    auth_kind: &str,
    kind: &str,
) -> Result<(), ServersError> {
    if name.trim().is_empty() {
        return Err(ServersError::Validation("服务器名不能为空".into()));
    }
    if host.trim().is_empty() {
        return Err(ServersError::Validation("主机地址不能为空".into()));
    }
    if !host_v6.is_empty() && host_v6.parse::<std::net::Ipv6Addr>().is_err() {
        return Err(ServersError::Validation(
            "IPv6 地址格式不正确（只填字面量，不要带方括号）".into(),
        ));
    }
    if !(1..=65535).contains(&port) {
        return Err(ServersError::Validation("端口必须在 1-65535".into()));
    }
    if user.trim().is_empty() {
        return Err(ServersError::Validation("登录用户不能为空".into()));
    }
    if !matches!(auth_kind, "password" | "key") {
        return Err(ServersError::Validation(
            "认证方式只能是 password 或 key".into(),
        ));
    }
    if !matches!(kind, "relay" | "landing") {
        return Err(ServersError::Validation(
            "服务器类型只能是 relay 或 landing".into(),
        ));
    }
    Ok(())
}

/// seal 凭据并返回 (凭据行 id, base64 密文)。
fn seal_to_b64(
    creds: &CredentialService,
    material: &AuthMaterial,
) -> Result<(String, String), ServersError> {
    let plain = match material {
        AuthMaterial::Password(p) => p.as_bytes().to_vec(),
        AuthMaterial::PrivateKey(pem) => pem.as_bytes().to_vec(),
    };
    let blob = creds
        .seal(&plain)
        .map_err(|e| ServersError::Crypto(e.to_string()))?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, blob);
    let id = Uuid::new_v4().simple().to_string();
    Ok((id, b64))
}

/// 读取服务器凭据并解密为 AuthMaterial（即用即弃）。命令层入口。
pub fn load_auth(state: &AppState, id: i64) -> Result<(AuthMaterial, Server), ServersError> {
    load_auth_parts(&state.db, &state.creds, id)
}

/// 可注入的凭据加载：部署管道（无 AppState）复用同一逻辑。
pub fn load_auth_parts(
    db: &crate::db::Db,
    creds: &CredentialService,
    id: i64,
) -> Result<(AuthMaterial, Server), ServersError> {
    let conn = db.lock();
    let s = repos::servers::get(&conn, id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .ok_or(ServersError::NotFound)?;
    let auth_ref = repos::servers::get_auth_ref(&conn, id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .ok_or(ServersError::Validation("服务器缺少凭据".into()))?;
    let blob = repos::credentials::get_blob(&conn, &auth_ref)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .ok_or(ServersError::Validation("凭据不存在".into()))?;
    drop(conn);
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob)
        .map_err(|e| ServersError::Crypto(e.to_string()))?;
    let plain = creds
        .open(&bytes)
        .map_err(|e| ServersError::Crypto(e.to_string()))?;
    let text = String::from_utf8_lossy(&plain).to_string();
    let auth = if s.auth_kind == "password" {
        AuthMaterial::Password(text)
    } else {
        AuthMaterial::PrivateKey(text)
    };
    Ok((auth, s))
}

pub fn list_servers(state: &AppState) -> Result<Vec<Server>, ServersError> {
    repos::servers::list(&state.db.lock()).map_err(|e| ServersError::Db(e.to_string()))
}

pub fn create_server(state: &AppState, input: CreateServerInput) -> Result<Server, ServersError> {
    // 输入净化：首尾空白与零宽字符（粘贴/输入法常见）会导致 DNS 解析与 SSH 认证静默失败，入库前统一去除
    let name = clean_text(&input.name);
    let host = clean_text(&input.host);
    let host_v6 = clean_host_v6(&input.host_v6);
    let user = clean_text(&input.user);
    validate_fields(
        &name,
        &host,
        &host_v6,
        input.port,
        &user,
        &input.auth_kind,
        &input.kind,
    )?;
    if input.credential.trim().is_empty() {
        return Err(ServersError::Validation("凭据不能为空".into()));
    }
    let material = match input.auth_kind.as_str() {
        "password" => AuthMaterial::Password(input.credential.clone()),
        _ => AuthMaterial::PrivateKey(input.credential.clone()),
    };
    let (cred_id, b64) = seal_to_b64(&state.creds, &material)?;
    let row = Server {
        id: 0,
        name,
        host,
        host_v6,
        addr_family: "v4".into(),
        egress_family: "v4".into(),
        port: input.port,
        user,
        auth_kind: input.auth_kind.clone(),
        kind: input.kind.clone(),
        system: String::new(),
        notes: input.notes.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        latency: None,
        bandwidth: None,
        last_checked_at: None,
        last_check_error: None,
        share_domain: normalize_share_domain(&input.share_domain)
            .map_err(ServersError::Validation)?,
    };
    {
        // 凭据与服务器行同事务写入，避免孤儿凭据
        let mut conn = state.db.lock();
        let tx = conn
            .transaction()
            .map_err(|e| ServersError::Db(e.to_string()))?;
        repos::credentials::insert(&tx, &cred_id, &b64)
            .map_err(|e| ServersError::Db(e.to_string()))?;
        repos::servers::insert(&tx, &row, &cred_id).map_err(|e| ServersError::Db(e.to_string()))?;
        tx.commit().map_err(|e| ServersError::Db(e.to_string()))?;
    }
    let id = state.db.lock().last_insert_rowid();
    Ok(Server { id, ..row })
}

/// 服务器生效地址变化时，把该机名下自建直连节点的 `addr` 一起改。
/// 只动 `relay_rule_id IS NULL` 的行：中转节点的 `addr` 是入口地址，不能跟着落地机变；
/// 它们的 `params_json.direct.addr` 由拆链逻辑管理，不在这里改。
pub(crate) fn sync_nodes_addr(
    conn: &rusqlite::Connection,
    old: &str,
    new: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE nodes SET addr=?2 WHERE addr=?1 AND source='self-built' AND relay_rule_id IS NULL",
        rusqlite::params![old, new],
    )
}

pub fn update_server(state: &AppState, input: UpdateServerInput) -> Result<Server, ServersError> {
    // 同 create_server：首尾空白与零宽字符入库前去除，避免历史行被编辑时重新带入
    let name = clean_text(&input.name);
    let host = clean_text(&input.host);
    let host_v6 = clean_host_v6(&input.host_v6);
    let user = clean_text(&input.user);
    validate_fields(
        &name,
        &host,
        &host_v6,
        input.port,
        &user,
        &input.auth_kind,
        &input.kind,
    )?;
    let mut conn = state.db.lock();
    let existing = repos::servers::get(&conn, input.id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .ok_or(ServersError::NotFound)?;
    // 切换认证方式必须带新凭据：auth_ref 指向旧类型凭据，不带凭据静默更新会造成类型错配
    if input.credential.is_none() && input.auth_kind != existing.auth_kind {
        return Err(ServersError::Validation(
            "切换认证方式时需提供新凭据".into(),
        ));
    }
    // 清除 IPv6 登记时不能继续停在 v6：生效地址必须回落到 host。
    // 用户只改 IPv6 字段、不动族别时，保留当前选择。
    let addr_family = if host_v6.is_empty() {
        "v4".to_string()
    } else {
        existing.addr_family.clone()
    };
    // 出口族与入口族完全独立：这里只决定 VPS→目标域名怎么出站，绝不改 addr_family。
    // None = 保留原值（其它调用方不必关心这个字段）。
    let egress_family = match input.egress_family {
        Some(v) => {
            if v != "v4" && v != "v6" {
                return Err(ServersError::Validation("出站地址族必须是 v4 或 v6".into()));
            }
            v
        }
        None => existing.egress_family.clone(),
    };
    // 分享域名：None = 保留原值；Some("") = 清除。入库前归一化。
    let share_domain = match input.share_domain.as_deref() {
        Some(v) => normalize_share_domain(v).map_err(ServersError::Validation)?,
        None => existing.share_domain.clone(),
    };
    // 生效地址可能因这次编辑改变（改 host / host_v6 / 清空 host_v6）：节点必须一起跟。
    // 必须在把 existing 的字段搬进 row 之前算——否则 last_check_error 已被移走，借用失败。
    let old_effective = crate::domain::effective_host(&existing).to_string();
    let row = Server {
        id: input.id,
        name,
        host,
        host_v6,
        addr_family,
        egress_family,
        port: input.port,
        user,
        auth_kind: input.auth_kind,
        kind: input.kind,
        system: existing.system,
        notes: input.notes,
        created_at: existing.created_at,
        // 更新服务器信息不覆盖测速数据
        latency: existing.latency,
        bandwidth: existing.bandwidth,
        last_checked_at: existing.last_checked_at,
        last_check_error: existing.last_check_error,
        share_domain,
    };
    let new_effective = crate::domain::effective_host(&row).to_string();
    // 凭据密文与数据库事务无关，先算好；事务里只做数据库写。
    let new_cred = match &input.credential {
        None => None,
        Some(text) => {
            if text.trim().is_empty() {
                return Err(ServersError::Validation("凭据不能为空".into()));
            }
            let material = if row.auth_kind == "password" {
                AuthMaterial::Password(text.clone())
            } else {
                AuthMaterial::PrivateKey(text.clone())
            };
            Some(seal_to_b64(&state.creds, &material)?)
        }
    };
    let old_ref = if new_cred.is_some() {
        repos::servers::get_auth_ref(&conn, input.id)
            .map_err(|e| ServersError::Db(e.to_string()))?
            .unwrap_or_default()
    } else {
        String::new()
    };
    // 服务器行、节点地址与凭据切换同事务：任何一步失败都不会留下「服务器切了、节点没切」
    let tx = conn
        .transaction()
        .map_err(|e| ServersError::Db(e.to_string()))?;
    if let Some((cred_id, b64)) = &new_cred {
        repos::credentials::insert(&tx, cred_id, b64)
            .map_err(|e| ServersError::Db(e.to_string()))?;
        tx.execute(
            "UPDATE servers SET auth_ref=?2 WHERE id=?1",
            rusqlite::params![input.id, cred_id],
        )
        .map_err(|e| ServersError::Db(e.to_string()))?;
    }
    repos::servers::update(&tx, &row).map_err(|e| ServersError::Db(e.to_string()))?;
    if old_effective != new_effective {
        sync_nodes_addr(&tx, &old_effective, &new_effective)
            .map_err(|e| ServersError::Db(e.to_string()))?;
    }
    if new_cred.is_some() {
        repos::credentials::delete(&tx, &old_ref).map_err(|e| ServersError::Db(e.to_string()))?;
    }
    tx.commit().map_err(|e| ServersError::Db(e.to_string()))?;
    Ok(row)
}

pub fn delete_server(state: &AppState, id: i64) -> Result<(), ServersError> {
    let mut conn = state.db.lock();
    let s = repos::servers::get(&conn, id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .ok_or(ServersError::NotFound)?;
    if repos::servers::has_dependents(&conn, id).map_err(|e| ServersError::Db(e.to_string()))? {
        return Err(ServersError::HasDependents);
    }
    let auth_ref = repos::servers::get_auth_ref(&conn, id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .unwrap_or_default();
    // 删服务器与删凭据同事务，避免残留密文行；
    // server_throughput 有外键引用 servers(id)：对测历史随服务器删除级联（双向），
    // 否则含对测历史的服务器永远无法删除（FK 违约且无 UI 途径清理历史行）
    let tx = conn
        .transaction()
        .map_err(|e| ServersError::Db(e.to_string()))?;
    tx.execute(
        "DELETE FROM server_throughput WHERE from_server_id=?1 OR to_server_id=?1",
        rusqlite::params![id],
    )
    .map_err(|e| ServersError::Db(e.to_string()))?;
    repos::servers::delete(&tx, id).map_err(|e| ServersError::Db(e.to_string()))?;
    if !auth_ref.is_empty() {
        repos::credentials::delete(&tx, &auth_ref).map_err(|e| ServersError::Db(e.to_string()))?;
    }
    tx.commit().map_err(|e| ServersError::Db(e.to_string()))?;
    let _ = s;
    Ok(())
}

pub async fn test_connection(state: &AppState, id: i64) -> Result<ConnectionTest, ServersError> {
    let (auth, s) = load_auth(state, id)?;
    let host = crate::domain::effective_host(&s).to_string();
    let mut sess =
        match SshSession::connect(&host, s.port as u16, &s.user, &auth, CONNECT_TIMEOUT).await {
            Ok(sess) => sess,
            Err(e) => {
                // 连接/认证失败属于"不可达"而非错误：转成可展示的脱敏结果
                let message = match &e {
                    SshError::Auth { .. } => "认证失败，请检查密码或私钥".to_string(),
                    // 连接错误保留主机/端口/原因：russh 原因串不含凭据，可诊断
                    SshError::Connect { .. } => e.to_string(),
                    SshError::Timeout { .. } => "连接超时".to_string(),
                    other => other.to_string(),
                };
                return Ok(ConnectionTest {
                    ok: false,
                    system: String::new(),
                    message,
                });
            }
        };
    let out = sess
        .run("连通测试", "uname -s", PROBE_TIMEOUT)
        .await
        .map_err(ServersError::from)?;
    if out.exit_code != 0 || out.stdout.trim().is_empty() {
        return Ok(ConnectionTest {
            ok: false,
            system: String::new(),
            message: "远端命令执行失败".into(),
        });
    }
    let system = match sess
        .run(
            "系统探测",
            "grep -m1 '^PRETTY_NAME=' /etc/os-release",
            PROBE_TIMEOUT,
        )
        .await
    {
        Ok(o) if o.exit_code == 0 => parse_pretty_name(&o.stdout),
        _ => String::new(),
    };
    if !system.is_empty() {
        repos::servers::set_system(&state.db.lock(), id, &system)
            .map_err(|e| ServersError::Db(e.to_string()))?;
    }
    Ok(ConnectionTest {
        ok: true,
        system,
        message: String::new(),
    })
}

/// 解析 `PRETTY_NAME="Ubuntu 22.04"` → `Ubuntu 22.04`。
fn parse_pretty_name(line: &str) -> String {
    line.trim()
        .split('=')
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_matches('"')
        .to_string()
}

/// 标准 OpenSSH 私钥名（客户端默认尝试集的常用子集）。
const LOCAL_KEY_NAMES: [&str; 3] = ["id_ed25519", "id_rsa", "id_ecdsa"];

/// 本机 OpenSSH 密钥目录：Windows `%USERPROFILE%\.ssh`（unix `$HOME/.ssh`）。
fn default_ssh_dir() -> Result<std::path::PathBuf, ServersError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| {
            ServersError::LocalKey("无法确定用户主目录（USERPROFILE/HOME 未设置）".into())
        })?;
    Ok(std::path::PathBuf::from(home).join(".ssh"))
}

/// 规范化为「type base64」两段：剥掉注释（避免密钥注释中的引号破坏远程命令），
/// 同时让 grep -qF 幂等匹配串稳定。解析失败返回 None（不是合法的公钥行）。
fn normalize_pub_line(line: &str) -> Option<String> {
    let tokens: Vec<&str> = line.split_whitespace().take(2).collect();
    (tokens.len() == 2).then(|| tokens.join(" "))
}

/// 收集本机免密的公钥行（双钥并行：与 App 的每服务器独立钥互不影响）。
/// 规则：`.pub` 存在 → 直接读；仅私钥存在 → russh 派生公钥（口令加密的私钥
/// 无法派生，跳过——其余已登记钥仍保障本机免密）；一个都没有 → 生成写入。
/// 必然返回至少 1 行。
pub fn prep_local_keys(ssh_dir: &std::path::Path) -> Result<Vec<String>, ServersError> {
    let mut lines = Vec::new();
    for name in LOCAL_KEY_NAMES {
        let pub_path = ssh_dir.join(format!("{name}.pub"));
        let priv_path = ssh_dir.join(name);
        if pub_path.is_file() {
            let raw = std::fs::read_to_string(&pub_path)
                .map_err(|e| ServersError::LocalKey(format!("读取 {pub_path:?} 失败: {e}")))?;
            if let Some(norm) = normalize_pub_line(&raw) {
                lines.push(norm);
            }
            continue;
        }
        if priv_path.is_file() {
            let pem = std::fs::read_to_string(&priv_path)
                .map_err(|e| ServersError::LocalKey(format!("读取 {priv_path:?} 失败: {e}")))?;
            if let Ok(key) = russh::keys::decode_secret_key(&pem, None) {
                if let Ok(line) = key.public_key().to_openssh() {
                    if let Some(norm) = normalize_pub_line(&line) {
                        lines.push(norm);
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push(generate_local_key(ssh_dir)?);
    }
    Ok(lines)
}

/// 生成 ed25519 密钥对写入 ssh_dir（无钥兜底），返回公钥行。
/// 注释 vpn-ctl-local（与 App 钥 vpn-ctl-admin 同风格，authorized_keys 可溯源）。
fn generate_local_key(ssh_dir: &std::path::Path) -> Result<String, ServersError> {
    let keypair = russh::keys::PrivateKey::random(
        &mut rand::rngs::OsRng,
        russh::keys::ssh_key::Algorithm::Ed25519,
    )
    .map_err(|e| ServersError::LocalKey(format!("生成密钥失败: {e}")))?;
    let mut pub_key = keypair.public_key().clone();
    pub_key.set_comment("vpn-ctl-local");
    let pub_line = pub_key
        .to_openssh()
        .map_err(|e| ServersError::LocalKey(format!("公钥序列化失败: {e}")))?;
    let pem = keypair
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map_err(|e| ServersError::LocalKey(format!("私钥序列化失败: {e}")))?
        .to_string();
    std::fs::create_dir_all(ssh_dir)
        .map_err(|e| ServersError::LocalKey(format!("创建 {ssh_dir:?} 失败: {e}")))?;
    let priv_path = ssh_dir.join("id_ed25519");
    std::fs::write(&priv_path, pem.as_bytes())
        .map_err(|e| ServersError::LocalKey(format!("写入 {priv_path:?} 失败: {e}")))?;
    std::fs::write(ssh_dir.join("id_ed25519.pub"), format!("{pub_line}\n"))
        .map_err(|e| ServersError::LocalKey(format!("写入 id_ed25519.pub 失败: {e}")))?;
    lock_key_permissions(ssh_dir, &priv_path)?;
    Ok(normalize_pub_line(&pub_line).unwrap_or(pub_line))
}

/// 权限对齐 ssh-keygen 行为（Win OpenSSH 拒绝「权限过宽」的私钥文件）：
/// Windows — 仅对私钥文件 icacls 关继承，授予当前用户/SYSTEM/Administrators
/// （每个身份用独立 /grant:r——逗号合并在一条内会被 icacls 当作非法参数/退出 87；
/// CREATOR OWNER 不作为参数——当前 icacls 会静默忽略它）；
/// unix — chmod 600 私钥 / 700 .ssh（测试与本地交叉编译可移植）。
fn lock_key_permissions(
    ssh_dir: &std::path::Path,
    priv_path: &std::path::Path,
) -> Result<(), ServersError> {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME")
            .map_err(|_| ServersError::LocalKey("USERNAME 未设置".into()))?;
        let status = crate::util::quiet(std::process::Command::new("icacls"))
            .arg(priv_path)
            .args(["/inheritance:r"])
            .arg("/grant:r")
            .arg(format!("{user}:F"))
            .arg("/grant:r")
            .arg("SYSTEM:F")
            .arg("/grant:r")
            .arg("Administrators:F")
            .status()
            .map_err(|e| ServersError::LocalKey(format!("icacls {priv_path:?} 执行失败: {e}")))?;
        if !status.success() {
            return Err(ServersError::LocalKey(format!(
                "icacls {priv_path:?} 返回非零: {status}"
            )));
        }
        // 目录不锁：新建的 .ssh 继承 %USERPROFILE% 的 ACL（用户/SYSTEM/Administrators）
        // 即为 ssh-keygen 同款；缩窄目录会在受限令牌（沙箱/CI）下反锁自身
        let _ = ssh_dir;
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(priv_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ServersError::LocalKey(format!("chmod 600 {priv_path:?} 失败: {e}")))?;
        std::fs::set_permissions(ssh_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ServersError::LocalKey(format!("chmod 700 {ssh_dir:?} 失败: {e}")))?;
    }
    Ok(())
}

/// 幂等追加单行公钥到 authorized_keys（grep -qF 命中则跳过），末行收紧权限。
fn append_pub_line(pub_line: &str) -> String {
    // 走共用转义而不是手写引号：公钥行来自本机 keygen（当前字符集安全），
    // 但「哪些字符串进过引号」不该靠每一处自己记得（#10）
    let q = crate::util::shell_quote(pub_line);
    format!(
        "grep -qF {q} ~/.ssh/authorized_keys 2>/dev/null || echo {q} >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys"
    )
}

/// 一键免密：生成 ed25519 → 用当前凭据登录下发公钥 → 切换 key 并删密码凭据。
/// 同时登记本机终端钥匙（双钥并行，见 prep_local_keys）。
pub async fn setup_passwordless(state: &AppState, id: i64) -> Result<(), ServersError> {
    let (auth, s) = load_auth(state, id)?;
    let keypair = russh::keys::PrivateKey::random(
        &mut rand::rngs::OsRng,
        russh::keys::ssh_key::Algorithm::Ed25519,
    )
    .map_err(|e| {
        ServersError::Ssh(SshError::KeyFormat {
            reason: e.to_string(),
        })
    })?;
    // 公钥行带固定注释 vpn-ctl-admin：authorized_keys 中可溯源该行来自本工具
    let mut pub_key = keypair.public_key().clone();
    pub_key.set_comment("vpn-ctl-admin");
    let pub_line = pub_key.to_openssh().map_err(|e| {
        ServersError::Ssh(SshError::KeyFormat {
            reason: e.to_string(),
        })
    })?;
    // Zeroizing<String>：OpenSSH 私钥文本即用即弃，不落日志
    let pem = keypair
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map_err(|e| {
            ServersError::Ssh(SshError::KeyFormat {
                reason: e.to_string(),
            })
        })?
        .to_string();
    let host = crate::domain::effective_host(&s).to_string();
    let mut sess =
        SshSession::connect(&host, s.port as u16, &s.user, &auth, CONNECT_TIMEOUT).await?;
    sess.run_ok("免密", "mkdir -p ~/.ssh && chmod 700 ~/.ssh", PROBE_TIMEOUT)
        .await?;
    sess.run_ok("免密", &append_pub_line(&pub_line), PROBE_TIMEOUT)
        .await?;
    // 验证公钥登录可用后（避免密钥未生效就切库导致失联）再切换
    let mut verify = SshSession::connect(
        &host,
        s.port as u16,
        &s.user,
        &AuthMaterial::PrivateKey(pem.clone()),
        CONNECT_TIMEOUT,
    )
    .await?;
    verify.run_ok("免密验证", "true", PROBE_TIMEOUT).await?;
    // 双钥并行：App 钥验证通过后、凭据切换前，同步把本机终端钥匙也登记到服务器——
    // prep_local_keys 保证至少一行（已有私钥则扫描/派生，无钥则自动生成写盘）；
    // 本机准备或下发失败 → 中止，数据库事务不执行（失败即中止）
    let local_lines = prep_local_keys(&default_ssh_dir()?)?;
    for line in &local_lines {
        sess.run_ok("免密-本机", &append_pub_line(line), PROBE_TIMEOUT)
            .await?;
    }
    let (cred_id, b64) = seal_to_b64(&state.creds, &AuthMaterial::PrivateKey(pem))?;
    // 新凭据写入与认证切换、旧凭据删除同事务
    let mut conn = state.db.lock();
    let old_ref = repos::servers::get_auth_ref(&conn, id)
        .map_err(|e| ServersError::Db(e.to_string()))?
        .unwrap_or_default();
    let tx = conn
        .transaction()
        .map_err(|e| ServersError::Db(e.to_string()))?;
    repos::credentials::insert(&tx, &cred_id, &b64).map_err(|e| ServersError::Db(e.to_string()))?;
    repos::servers::set_auth_kind_ref(&tx, id, "key", &cred_id)
        .map_err(|e| ServersError::Db(e.to_string()))?;
    if !old_ref.is_empty() {
        repos::credentials::delete(&tx, &old_ref).map_err(|e| ServersError::Db(e.to_string()))?;
    }
    tx.commit().map_err(|e| ServersError::Db(e.to_string()))?;
    Ok(())
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/servers/mods.rs"
));
