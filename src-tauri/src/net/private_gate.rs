//! 隐私线切换前的 Tor 就绪门。
//!
//! Tor 的 SOCKS 端口从进程启动那一刻就开始 accept，但电路还没建好。此时 mihomo
//! 的延迟测试会立刻返回 503（An error occurred in the delay test）——这不是线路
//! 故障，只是还没就绪。旧实现直接开测，用户刚点隐私节点、界面还显示「正在建立
//! 14%」时就收到一条「HTTPS 核验失败」，按提示重试也可能一直撞在这个窗口里。

use crate::state::AppState;
use std::time::Duration;

/// 等 Tor 就绪的上限。实测正常 5–45 秒（路由回退时更久）；超过它继续空转只会让
/// 界面一直停在「正在验证出口…」，不如如实告诉用户「还在建立」。
pub const TOR_READY_WAIT: Duration = Duration::from_secs(90);
/// 网络刚起时 `tor` 字段短暂为 off（看门狗还没回写 Starting），给一点宽限。
const OFF_GRACE: Duration = Duration::from_secs(5);
/// 轮询间隔：与界面读 Tor 进度的节奏一致，足够跟手又不打满 CPU。
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
pub enum TorGate {
    /// 已就绪（或使用本机外部 Tor），可以继续核验。
    Ready,
    /// 仍在启动，继续等。
    Wait,
    /// 明确不可用（启动失败 / 未运行 / 等超时），立即报错。
    Reject(String),
}

/// 纯决策函数，便于单测。`detail` 是界面上的进度文案，如
/// 「Tor 上游 2/7：ashura-vmess · 已建立链路 14%」。
pub fn tor_gate(state: &str, detail: Option<&str>, elapsed: Duration) -> TorGate {
    match state {
        "ready" | "external" => TorGate::Ready,
        "failed" => {
            let reason = detail
                .filter(|d| !d.trim().is_empty())
                .map(|d| format!("：{d}"))
                .unwrap_or_default();
            TorGate::Reject(format!("隐私入口启动失败{reason}"))
        }
        // off 只在网络刚起、看门狗尚未回写的窗口里合法；宽限期后仍是 off，
        // 说明本次会话确实没有 Tor 在跑。
        "off" if elapsed >= OFF_GRACE => {
            TorGate::Reject("隐私入口未在运行，请先开启网络后再切换".into())
        }
        _ if elapsed >= TOR_READY_WAIT => {
            let progress = detail
                .filter(|d| !d.trim().is_empty())
                .map(|d| format!("（当前进度：{d}）"))
                .unwrap_or_default();
            TorGate::Reject(format!("隐私入口仍在建立{progress}，请等 Tor 就绪后再切换"))
        }
        _ => TorGate::Wait,
    }
}

/// 等到 Tor 就绪再返回。`select` 与隐私部署都在核验前调用。
///
/// 只短暂持有 `state.net` 读状态，睡眠期间不持锁；网络在这期间被关闭时返回
/// 「客户端未运行」，交给调用方按普通错误处理。
pub(crate) async fn wait_ready(state: &AppState) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    loop {
        let (st, detail) = {
            let guard = state.net.lock().await;
            let n = guard.as_ref().ok_or("客户端未运行")?;
            (n.tor.clone(), n.tor_detail.clone())
        };
        match tor_gate(&st, detail.as_deref(), started.elapsed()) {
            TorGate::Ready => return Ok(()),
            TorGate::Reject(e) => return Err(e),
            TorGate::Wait => tokio::time::sleep(POLL).await,
        }
    }
}
