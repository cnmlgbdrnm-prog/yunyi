//! 设备 peer 管理：X25519 密钥对（与 `wg genkey` 同构的 32 字节原始密钥，base64 文本）+ PSK
//! 本地生成 → 私钥/PSK 原始字节 seal 入库 → job 下发中转机 wg0.conf（追加 peer 段 +
//! `wg-quick strip | wg syncconf /dev/stdin` 热加载，无进程替换）；
//! 删除按公钥移除段并同步删行；配置导出解密即用即弃渲染 .conf。
//! 输入：rule_id/peer_id/名称；输出：job_id / 设备列表 / 渲染后的 .conf 文本。
//! 设备私钥/PSK 明文不出本层、不进日志与错误文本；中转机公钥经 SSH 读回（库中不存）。

use crate::crypto::CredentialService;
use crate::db::repos;
use crate::domain::{DevicePeer, DevicePeerRow, JobProgress, PeerAddReq, PeerRemoveReq};
use crate::jobs::{EmitFn, JobRequest};
use crate::relay::config::{
    alloc_peer_ip, remove_peer_section, render_device_conf, upsert_peer_section,
};
use crate::relay::{map_servers_error, RelayError, WG_CONF_PATH, WG_IFACE};
use crate::servers::deploy::{
    read_remote_file, run_cmd_ok, write_remote_file, RemoteFile, CONFIG_TIMEOUT,
};
use crate::servers::{load_auth, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;
use rand::RngCore;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 添加设备：生成密钥对 + PSK → seal 入库 → 建 jobs 行（kind='peer'）→ 提交 PeerAdd。
/// 返回 job_id；实际下发由 worker 执行 run_peer_add。
pub fn add_device_peer(state: &AppState, rule_id: i64, name: String) -> Result<i64, RelayError> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(RelayError::Validation("设备名不能为空".into()));
    }
    let rule = {
        let conn = state.db.lock();
        repos::relay::get(&conn, rule_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?
    };
    if !matches!(rule.status.as_str(), "built" | "imported") {
        return Err(RelayError::Validation(
            "仅已建立或已接管的链路支持添加设备".into(),
        ));
    }
    if rule.client_mode != "peer" {
        return Err(RelayError::Validation("仅 peer 模式链路支持设备".into()));
    }
    // X25519 密钥对（与 wg genkey 同构：32 字节原始密钥，base64 文本进 conf）与
    // 32 字节 PSK 本地生成；私钥/PSK 均以原始字节 seal 入库，即用即弃。
    let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
    let priv_raw = secret.to_bytes();
    let pubkey = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        x25519_dalek::PublicKey::from(&secret).to_bytes(),
    );
    let mut psk_bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut psk_bytes);
    let priv_sealed = seal_b64(&state.creds, &priv_raw)?;
    let psk_sealed = seal_b64(&state.creds, &psk_bytes)?;
    // 设备 IP：末段最大值 +1（删除不复用）；末段 ≥ 254 视为耗尽
    let conn = state.db.lock();
    let max_ip = repos::device_peers::max_wg_ip(&conn, rule_id)
        .map_err(|e| RelayError::Db(e.to_string()))?;
    if let Some(n) = max_ip
        .as_deref()
        .and_then(|ip| ip.rsplit('.').next())
        .and_then(|s| s.parse::<u32>().ok())
    {
        if n >= 254 {
            return Err(RelayError::PeerIpExhausted);
        }
    }
    let wg_ip = alloc_peer_ip(&rule, max_ip.as_deref());
    let row = DevicePeerRow {
        id: 0,
        rule_id,
        name,
        public_key: pubkey,
        private_key_b64: priv_sealed,
        psk_b64: psk_sealed,
        wg_ip,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let peer_id =
        repos::device_peers::insert(&conn, &row).map_err(|e| RelayError::Db(e.to_string()))?;
    drop(conn);
    let job_id = repos::jobs::insert(&state.db.lock(), "peer", "queued")
        .map_err(|e| RelayError::Db(e.to_string()))?;
    state
        .job_queue
        .submit(JobRequest::PeerAdd(PeerAddReq { job_id, peer_id }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            RelayError::Validation(e)
        })?;
    Ok(job_id)
}

/// 删除设备：查行 → 建 jobs 行 → 提交 PeerRemove。返回 job_id。
pub fn remove_device_peer(state: &AppState, peer_id: i64) -> Result<i64, RelayError> {
    let peer = {
        let conn = state.db.lock();
        repos::device_peers::get(&conn, peer_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?
    };
    let job_id = repos::jobs::insert(&state.db.lock(), "peer", "queued")
        .map_err(|e| RelayError::Db(e.to_string()))?;
    state
        .job_queue
        .submit(JobRequest::PeerRemove(PeerRemoveReq {
            job_id,
            rule_id: peer.rule_id,
            public_key: peer.public_key.clone(),
        }))
        .map_err(|e| {
            let _ = repos::jobs::update(&state.db.lock(), job_id, "failed", 0, Some(&e));
            RelayError::Validation(e)
        })?;
    Ok(job_id)
}

/// 设备列表（映射掉 sealed 字段，不出 IPC）。
pub fn list_device_peers(state: &AppState, rule_id: i64) -> Result<Vec<DevicePeer>, RelayError> {
    let rows = repos::device_peers::list_by_rule(&state.db.lock(), rule_id)
        .map_err(|e| RelayError::Db(e.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|p| DevicePeer {
            id: p.id,
            rule_id: p.rule_id,
            name: p.name,
            public_key: p.public_key,
            wg_ip: p.wg_ip,
            created_at: p.created_at,
        })
        .collect())
}

/// 导出设备 .conf：中转机公钥经 SSH 读回，私钥/PSK 解密即用即弃。
pub async fn get_peer_config(state: &AppState, peer_id: i64) -> Result<String, RelayError> {
    let (rule, peer) = {
        let conn = state.db.lock();
        let peer = repos::device_peers::get(&conn, peer_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?;
        let rule = repos::relay::get(&conn, peer.rule_id)
            .map_err(|e| RelayError::Db(e.to_string()))?
            .ok_or(RelayError::NotFound)?;
        (rule, peer)
    };
    // 中转机公钥服务器端生成、库中不存冗余，导出时读回
    let (relay_auth, relay_srv) =
        load_auth(state, rule.relay_server_id).map_err(map_servers_error)?;
    let mut sess = SshSession::connect(
        &relay_srv.host,
        relay_srv.port as u16,
        &relay_srv.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()))?;
    let out = sess
        .run(
            "配置",
            &format!("wg show {WG_IFACE} public-key"),
            PROBE_TIMEOUT,
        )
        .await
        .map_err(|e| RelayError::Ssh(e.to_string()))?;
    let relay_pubkey = out.stdout.trim().to_string();
    if relay_pubkey.is_empty() {
        return Err(RelayError::Ssh("无法读取中转机公钥".into()));
    }
    let priv_raw = open_b64(&state.creds, &peer.private_key_b64)?;
    let psk_raw = open_b64(&state.creds, &peer.psk_b64)?;
    Ok(render_device_conf(
        &peer,
        &base64::Engine::encode(&base64::engine::general_purpose::STANDARD, priv_raw),
        &base64::Engine::encode(&base64::engine::general_purpose::STANDARD, psk_raw),
        &rule,
        &relay_pubkey,
        &relay_srv.host,
    ))
}

/// 设备下发执行器：中转机 conf 追加设备段 → 写回 → syncconf 热加载。
pub async fn run_peer_add(
    state: &AppState,
    req: PeerAddReq,
    cancel: &Arc<AtomicBool>,
    emit: &EmitFn,
) -> Result<(), String> {
    let (rule, peer) = {
        let conn = state.db.lock();
        let peer = repos::device_peers::get(&conn, req.peer_id)
            .map_err(|e| e.to_string())?
            .ok_or("设备不存在")?;
        let rule = repos::relay::get(&conn, peer.rule_id)
            .map_err(|e| e.to_string())?
            .ok_or("链路不存在")?;
        (rule, peer)
    };
    let emit_progress = |step: &str, message: &str, progress: i64| {
        emit(JobProgress {
            job_id: req.job_id,
            kind: "peer".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    emit_progress("下发", "连接中转机", 10);
    let (relay_auth, relay_srv) =
        load_auth(state, rule.relay_server_id).map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        &relay_srv.host,
        relay_srv.port as u16,
        &relay_srv.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    // 读回 conf；不存在则中止（built 状态理论必存在，imported 用户自建也应存在），
    // 读失败则如实报读失败
    let conf = read_conf_or_missing(&mut sess, cancel).await?;
    let psk = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        open_b64(&state.creds, &peer.psk_b64).map_err(|e| e.to_string())?,
    );
    // 设备段 AllowedIPs 为该设备自身 wg_ip/32：与落地段（landing_wg_ip/32）前缀
    // 互不重叠，避免 WG cryptokey 路由同前缀覆盖（subnet/24 会抹除落地段条目）
    let section = format!(
        "[Peer]\nPublicKey = {}\nPresharedKey = {}\nAllowedIPs = {}/32\nPersistentKeepalive = 25\n",
        peer.public_key, psk, peer.wg_ip
    );
    let updated = upsert_peer_section(&conf, &section);
    write_remote_file(
        &mut sess,
        cancel,
        "下发",
        WG_CONF_PATH,
        &updated,
        CONFIG_TIMEOUT,
    )
    .await?;
    emit_progress("热加载", "wg syncconf 热加载", 60);
    run_cmd_ok(
        &mut sess,
        cancel,
        "下发",
        &format!("wg-quick strip {WG_CONF_PATH} | wg syncconf {WG_IFACE} /dev/stdin"),
        CONFIG_TIMEOUT,
    )
    .await?;
    emit_progress("完成", "设备已下发", 100);
    Ok(())
}

/// 设备移除执行器：中转机 conf 按公钥移除段 → 写回 → syncconf → 删行。
pub async fn run_peer_remove(
    state: &AppState,
    req: PeerRemoveReq,
    cancel: &Arc<AtomicBool>,
    emit: &EmitFn,
) -> Result<(), String> {
    let rule = {
        let conn = state.db.lock();
        repos::relay::get(&conn, req.rule_id)
            .map_err(|e| e.to_string())?
            .ok_or("链路不存在")?
    };
    // 链路已拆（not-built）：接口已 down，syncconf/conf 操作无意义，只删行不走 SSH
    if rule.status == "not-built" {
        let conn = state.db.lock();
        let rows =
            repos::device_peers::list_by_rule(&conn, req.rule_id).map_err(|e| e.to_string())?;
        if let Some(pid) = rows
            .iter()
            .find(|p| p.public_key == req.public_key)
            .map(|p| p.id)
        {
            repos::device_peers::delete(&conn, pid).map_err(|e| e.to_string())?;
        }
        drop(conn);
        return Ok(());
    }
    let emit_progress = |step: &str, message: &str, progress: i64| {
        emit(JobProgress {
            job_id: req.job_id,
            kind: "peer".into(),
            step: step.to_string(),
            message: message.to_string(),
            progress,
            error: None,
        });
    };
    emit_progress("移除", "连接中转机", 10);
    let (relay_auth, relay_srv) =
        load_auth(state, rule.relay_server_id).map_err(|e| e.to_string())?;
    let mut sess = SshSession::connect(
        &relay_srv.host,
        relay_srv.port as u16,
        &relay_srv.user,
        &relay_auth,
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| RelayError::Ssh(e.to_string()).to_string())?;
    let conf = read_conf_or_missing(&mut sess, cancel).await?;
    let updated = remove_peer_section(&conf, &req.public_key);
    if updated != conf {
        write_remote_file(
            &mut sess,
            cancel,
            "移除",
            WG_CONF_PATH,
            &updated,
            CONFIG_TIMEOUT,
        )
        .await?;
        emit_progress("热加载", "wg syncconf 热加载", 60);
        run_cmd_ok(
            &mut sess,
            cancel,
            "移除",
            &format!("wg-quick strip {WG_CONF_PATH} | wg syncconf {WG_IFACE} /dev/stdin"),
            CONFIG_TIMEOUT,
        )
        .await?;
    } else {
        // 段不存在 = 已移除（如添加 job 失败、conf 从未含该段）：幂等成功，仅删行
        emit_progress("移除", "设备段已不存在，仅清理记录", 60);
    }
    // 删行：PeerRemoveReq 不含 peer_id，按公钥定位
    let conn = state.db.lock();
    let rows = repos::device_peers::list_by_rule(&conn, req.rule_id).map_err(|e| e.to_string())?;
    if let Some(pid) = rows
        .iter()
        .find(|p| p.public_key == req.public_key)
        .map(|p| p.id)
    {
        repos::device_peers::delete(&conn, pid).map_err(|e| e.to_string())?;
    }
    drop(conn);
    emit_progress("完成", "设备已移除", 100);
    Ok(())
}

/// seal 并 base64 编码（参照 servers::seal_to_b64）。
fn seal_b64(creds: &CredentialService, plain: &[u8]) -> Result<String, RelayError> {
    let blob = creds
        .seal(plain)
        .map_err(|e| RelayError::Crypto(e.to_string()))?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        blob,
    ))
}

/// base64 解码 + 解密（即用即弃，明文不出本层）；返回原始字节，渲染时按需编码。
fn open_b64(creds: &CredentialService, b64: &str) -> Result<Vec<u8>, RelayError> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        .map_err(|e| RelayError::Crypto(e.to_string()))?;
    creds
        .open(&bytes)
        .map_err(|e| RelayError::Crypto(e.to_string()))
}

/// 读回远端 conf，区分「不存在」与「读失败」。
///
/// 与 `relay::build` 收敛到同一个原语：早先这里把非零退出一律归成 `None`，于是
/// **读失败被报成「配置不存在」**——用户会去重建链路，而链路早就在，真正的原因是
/// 权限/IO 故障。更危险的是语义：读失败退化成空串后，`upsert_peer_section` 会以
/// 空配置为底写出**只剩本设备段**的 conf，同机上别的链路与设备一次清空。
async fn read_conf(sess: &mut SshSession, cancel: &AtomicBool) -> Result<RemoteFile, String> {
    read_remote_file(sess, cancel, "下发", WG_CONF_PATH, CONFIG_TIMEOUT).await
}

/// 读回 conf，把「真的不存在」翻译成给用户的那句话；读失败原样上报。
/// 调用点统一走这里，避免每个调用点各自决定「读失败算什么」。
async fn read_conf_or_missing(
    sess: &mut SshSession,
    cancel: &AtomicBool,
) -> Result<String, String> {
    match read_conf(sess, cancel).await? {
        RemoteFile::Content(c) => Ok(c),
        RemoteFile::Absent => Err("中转机 wg0 配置不存在，请先建链".into()),
    }
}
