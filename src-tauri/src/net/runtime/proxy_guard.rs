//! 与网络会话绑定的系统代理维护；不依赖用户停留在仪表盘。
use super::*;

/// 持有 state.net 锁时调用；先核验实际代理进程，避免指向退出后的死端口。
pub(super) async fn ensure(n: &RunningNet, force: bool) -> Result<(), String> {
    if !force && sysproxy_set::proxy_matches(n.mixed_port)? {
        return Ok(());
    }
    if !force {
        let live = sysproxy_set::live_system_proxy()?;
        if !may_repair(live.as_deref(), n.mixed_port) {
            return Err("系统代理已被其它代理接管，请关闭其它代理或手动恢复系统代理".into());
        }
    }
    sidecar::wait_ready(
        None::<&mut std::process::Child>,
        n.ctrl_port,
        &n.token,
        std::time::Duration::from_secs(3),
    )
    .await
    .map_err(|e| format!("代理进程未就绪，未修改系统代理: {e}"))?;
    // apply 内部有独立回滚快照；成功也不替换会话最初的 snapshot，关闭时仍还原用户原值。
    sysproxy_set::apply_net_proxy(n.mixed_port)?;
    Ok(())
}

#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/net/proxy_recovery_tests.rs"
));

pub(crate) fn may_repair(live: Option<&str>, port: u16) -> bool {
    live.is_none_or(|server| server.trim() == sysproxy_set::proxy_server_value(port))
}

pub(super) fn spawn(state: &AppState, token: String) {
    let Some(weak) = state.self_ref.get().cloned() else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let mut last_error = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Some(state) = weak.upgrade() else {
                return;
            };
            let mut guard = state.net.lock().await;
            let Some(n) = guard.as_mut().filter(|n| n.token == token) else {
                return;
            };
            let active = if n.child.is_some() {
                process_alive(n)
            } else {
                // 服务状态未知或 TUN 已开时不写系统代理。
                service_state().is_some_and(|(running, tun)| running && !tun)
            };
            if !active {
                continue;
            }
            let error = ensure(n, false).await.err();
            if error != last_error {
                if let Some(message) = &error {
                    eprintln!("[vpn-ctl] 系统代理维护失败: {message}");
                }
                last_error = error;
            }
        }
    });
}
