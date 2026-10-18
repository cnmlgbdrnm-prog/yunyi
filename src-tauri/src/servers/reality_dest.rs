//! 伪装域名轻量更换：只改落地机 xray config 的 dest/serverNames 并重启，
//! 再回写节点 params_json 的 sni——与全量重部署解耦（公钥/uuid/端口/指纹
//! 一律不动，秒级完成，不再是重部署的分钟级停机窗口）。
//! 安全：dest 域名白名单（ASCII 字母数字/点/连字符）防注入；python 脚本以
//! 单引号包裹传给远程（脚本内无单引号/花括号，无转义面）；服务器改完并验证
//! is-active 成功后，才回写本机节点（失败可整机重试，两端顺序有校验）。

use crate::db::repos;
use crate::servers::{load_auth, ServersError, CONNECT_TIMEOUT, PROBE_TIMEOUT};
use crate::ssh::SshSession;
use crate::state::AppState;

const XRAY_CONFIG: &str = "/usr/local/etc/xray/config.json";

/// dest 域名校验：域名形态（字母/数字/点/连字符），去首尾空白；超长/非法拒绝。
pub fn validate_dest(dest: &str) -> Result<String, ServersError> {
    let d = dest.trim();
    if d.is_empty() {
        return Err(ServersError::Validation("伪装域名不能为空".into()));
    }
    if d.len() > 255
        || !d
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(ServersError::Validation(
            "伪装域名格式不合法（仅允许字母、数字、点与连字符，如 www.apple.com）".into(),
        ));
    }
    Ok(d.to_string())
}

/// 节点 params_json 的 sni 改写（纯函数）：仅当存在字符串 sni 字段时替换（REALITY
/// 节点）；非 reality / 无 sni 返回 None（不触碰）。
pub fn rewrite_sni_in_params(params_json: &str, dest: &str) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(params_json).ok()?;
    let is_sni = v
        .get("sni")
        .and_then(|s| s.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !is_sni {
        return None;
    }
    v["sni"] = serde_json::Value::String(dest.into());
    serde_json::to_string(&v).ok()
}

/// 从节点 params_json 只读取 sni（不改写）：非 REALITY / 无 sni / 空串返回 None。
/// 重部署时用它保留用户自定义的伪装域名——不读回来就会回落默认值，
/// 表现为「只切了出口，伪装域名却被重置」。
pub fn sni_of_params(params_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(params_json).ok()?;
    v.get("sni")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 远程改写命令（纯函数，测试用 base64 解码回来看脚本内容）。
/// 脚本自身含大量单引号（`DEST='…'`、`o.get('dest')`）——**不能**直接
/// `python3 -c '<脚本>'`：内层单引号会提前闭合外层引用、把引号全部吃掉，
/// python 收到 `p=/path` 直接语法错误（2026-09-18 实机复现）。
/// 改走 base64 传输后，命令里只剩包裹引号，脚本内容不再进入 shell 的解析面。
pub fn dest_change_cmd(dest: &str) -> String {
    // 逐行成字面量再 join——**不能**用 `"…\n\` 行继续：Rust 会吃掉下一行行首空白，
    // 用它拼缩进敏感的 python 会直接产出 IndentationError（2026-09-18 实机复现，
    // 与上面的引号问题一样，都是"看着对"的写法）。
    let lines: Vec<String> = vec![
        "import json".into(),
        format!("DEST='{dest}'"),
        format!("p='{XRAY_CONFIG}'"),
        "cfg=json.load(open(p))".into(),
        "def walk(o):".into(),
        "    if isinstance(o,dict):".into(),
        "        d=o.get('dest')".into(),
        "        sn=o.get('serverNames')".into(),
        "        if isinstance(d,str) and isinstance(sn,list) and sn and sn==[d.rsplit(':',1)[0]]:"
            .into(),
        "            o['dest']=DEST+':443'".into(),
        "            o['serverNames']=[DEST]".into(),
        "        for v in o.values():".into(),
        "            walk(v)".into(),
        "    elif isinstance(o,list):".into(),
        "        for v in o:".into(),
        "            walk(v)".into(),
        "walk(cfg)".into(),
        "json.dump(cfg,open(p,'w'),ensure_ascii=False,indent=2)".into(),
        "print('dest changed to '+DEST)".into(),
    ];
    let py = lines.join("\n");
    // 与 deploy::write_remote_file 同法：整体 base64 后单引号包裹。
    // base64 字母表不含单引号，外层包裹因此无法被脚本内容击穿。
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, py.as_bytes());
    format!("echo '{b64}' | base64 -d | python3 -")
}

/// 落地机就地更换：python 改 config.json（JSON 结构不符/失败→非零退出）→
/// xray 重启并验证 active → 回写本机节点 sni（vless/trojan 且 addr 为落地机）。
/// 失败时服务器端可能已改或未改：is-active 校验兜底，节点回写失败仅报错不
/// 回滚（重跑会把两端幂等拨正）。
pub async fn change_reality_dest(
    state: &AppState,
    server_id: i64,
    dest: &str,
) -> Result<(), ServersError> {
    let dest = validate_dest(dest)?;
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
        .run("更换伪装域名", &dest_change_cmd(&dest), PROBE_TIMEOUT)
        .await
        .map_err(ServersError::Ssh)?;
    if out.exit_code != 0 {
        return Err(ServersError::Validation(format!(
            "配置文件改写失败: {}",
            first_error(&out.stderr, &out.stdout)
        )));
    }
    // 只看退出码不够：管道里 `base64 -d` 失败时 python 收到空脚本、以 0 退出，
    // 表现为「改成功但其实一个字节都没改」。脚本末尾的标记才是真的跑完了的证据。
    if !out.stdout.contains(&format!("dest changed to {dest}")) {
        return Err(ServersError::Validation(format!(
            "伪装域名改写未生效（脚本未产出确认标记）: {}",
            first_error(&out.stderr, &out.stdout)
        )));
    }
    let restart = sess
        .run(
            "重启 xray",
            "systemctl restart xray && sleep 1 && systemctl is-active xray",
            PROBE_TIMEOUT,
        )
        .await
        .map_err(ServersError::Ssh)?;
    if restart.exit_code != 0 || restart.stdout.trim() != "active" {
        return Err(ServersError::Validation(format!(
            "xray 重启后未处于 active: {}",
            first_error(&restart.stderr, &restart.stdout)
        )));
    }
    let conn = state.db.lock();
    repos::nodes::update_reality_sni(
        &conn,
        crate::domain::effective_host(&server),
        &dest,
        &state.creds,
    )
    .map_err(|e| ServersError::Db(e.to_string()))?;
    Ok(())
}

fn first_error(stderr: &str, stdout: &str) -> String {
    let s = if !stderr.is_empty() { stderr } else { stdout };
    s.trim().chars().take(300).collect()
}
