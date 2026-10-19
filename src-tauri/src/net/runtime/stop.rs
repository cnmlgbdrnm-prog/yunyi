//! 关闭网络 / 程序退出时的收尾策略。
//!
//! 单独成子模块只为守住「单文件 ≤1500 行」的硬规范；逻辑仍属于 runtime，
//! 因此直接用 `super::*` 访问父模块的内部类型与幂等清理原语。

use super::*;

/// 把主程序自己起的 mihomo 切到直连（保留进程与端口）。
///
/// 只改 `mode`，不重载整份配置：本地子进程没有 TUN（TUN 只能由服务打开），
/// 而 `PATCH /configs` 是 mihomo 支持的运行时开关（实测 204，端口不断）。
async fn switch_direct(n: &RunningNet) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .patch(format!("http://127.0.0.1:{}/configs", n.ctrl_port))
        .bearer_auth(&n.token)
        .json(&serde_json::json!({ "mode": "direct" }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("请求控制器切换直连失败: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let trimmed: String = body.chars().take(200).collect();
        return Err(format!("控制器拒绝切换直连: HTTP {code} {trimmed}"));
    }
    Ok(())
}

/// 关闭网络时的收尾策略。
#[derive(Clone, Copy, PartialEq, Eq)]
enum StopPolicy {
    /// 用户点「关闭网络」：服务托管的 mihomo 切直连保留；主程序自己起的子进程
    /// 仍然杀掉（无服务时没有 TUN，测试与收尾也需要它退出）。
    KeepService,
    /// 程序退出：两种托管方式都保留一个直连端点。
    KeepAll,
}

/// 关闭：服务托管的切直连、本地子进程杀进程 → 还原系统代理快照 → 清槽。
/// 未运行时空操作（幂等）。
pub async fn stop(state: &AppState) -> Result<NetStatus, String> {
    stop_with_policy(state, StopPolicy::KeepService).await
}

/// 程序退出专用：不杀本机代理，只把它切到直连。
///
/// 根因见 [`super::pinned_mixed_port`] 的注释：用户级 `HTTP_PROXY` 是进程启动那一刻的
/// 死快照。关闭程序时如果杀掉 7890，浏览器/终端/Claude Code 之类已经跑着的
/// 程序会整片 `ECONNREFUSED`。切直连后它们仍有一个活着的本地代理，新进程按
/// 已还原的系统代理设置走直连。
pub async fn stop_for_exit(state: &AppState) -> Result<NetStatus, String> {
    stop_with_policy(state, StopPolicy::KeepAll).await
}

async fn stop_with_policy(state: &AppState, policy: StopPolicy) -> Result<NetStatus, String> {
    let mut guard = state.net.lock().await;
    if let Some(mut n) = guard.take() {
        match n.child.take() {
            Some(mut c) => {
                if policy == StopPolicy::KeepAll {
                    if let Err(e) = switch_direct(&n).await {
                        // 切不过去也**不杀**：留在原模式（通常是 VPN）至少还能上网。
                        eprintln!("[vpn-ctl] 退出时切换直连失败，保留代理进程: {e}");
                    }
                    // 丢掉句柄不会杀进程（std::process::Child 的 Drop 只关句柄）；
                    // 下次 start 的 kill_own_orphans 会按镜像与 -d 收掉它。
                    drop(c);
                } else {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
            // 进程归服务管，主程序没有句柄——只能请服务切直连。服务此刻不可达
            // 也不报错：系统代理与登记表照样要还原，留着才是真断网。
            None => match svc::call(&SvcRequest::Detach) {
                Ok(SvcResponse::Detached) => {}
                Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {
                    // 旧服务不认识 Detach，它的 reap_owner 仍会杀掉 mihomo；
                    // 如实留一条线索，别把「没修上」伪装成成功。
                    eprintln!(
                        "[vpn-ctl] 网络服务版本过旧，无法保留直连代理；更新网络服务后，\
                         关闭程序才不会再让已运行的程序断网: {message}"
                    );
                }
                Ok(SvcResponse::Failed { message }) => {
                    eprintln!("[vpn-ctl] 保留直连代理失败（进程不杀）: {message}");
                }
                Ok(other) => eprintln!("[vpn-ctl] 保留直连代理收到非预期响应: {other:?}"),
                Err(e) => eprintln!("[vpn-ctl] 请求网络服务保留直连代理失败: {e}"),
            },
        }
        // 退出兜底：服务不可达、或服务侧 mihomo 已经死了时，Detach 不会留下活端口。
        // 已运行程序的环境变量是进程启动时的死快照，端口一消失它们就整片
        // `ECONNREFUSED`；这里直接起一个最小 direct mihomo 把端口占住。先兜底再
        // 还原系统代理——这样退出等待的这几秒里，新进程也不会撞上一个空端口。
        if policy == StopPolicy::KeepAll && !crate::net::winprobe::port_is_listening(n.mixed_port) {
            if let Err(e) = spawn_direct_fallback(state, n.mixed_port).await {
                eprintln!("[vpn-ctl] 退出兜底直连失败: {e}");
            }
        }
        sysproxy_set::restore_proxy(n.snapshot.as_ref())?;
    } else {
        // 界面刚启动、还没开网络也要清掉服务归属：服务可能在上一轮退出时留了
        // 一个直连 mihomo，归属不清会让下一次开启/退出都被「另一个窗口持有」拦住。
        match svc::call(&SvcRequest::Detach) {
            Ok(SvcResponse::Detached) => {}
            Ok(SvcResponse::Failed { message }) if svc::is_unknown_command(&message) => {}
            Ok(SvcResponse::Failed { message }) => {
                eprintln!("[vpn-ctl] 释放网络服务归属失败: {message}");
            }
            Ok(other) => eprintln!("[vpn-ctl] 释放网络服务归属收到非预期响应: {other:?}"),
            // 服务未安装/未启动：没有归属可释放
            Err(_) => {}
        }
    }
    // 无条件注销：幂等 stop（本就未运行）也不该留下可疑登记。自我托管集合一起清
    // ——留着它会让下次「没开网络」时的拨号白试一次直连（那时本来就直连，无害但
    // 没道理），而它只在开启时才有意义。
    local_proxy::set(None);
    local_proxy::set_self_hosted(std::iter::empty());
    // 阻断规则**同样无条件撤**，且刻意放在 `if let Some(n)` 之外（四重清扫之一）：
    // 与上面那句 `local_proxy::set(None)` 同一个理由——幂等 stop（本就未运行）也不该
    // 留下可疑状态。而这里还有更强的理由：残留的阻断规则是**真会挡住用户网络的**，
    // 撤不掉比留一个陈旧登记严重得多。
    clear_session_rules_quietly("撤销会话阻断规则");
    // 采样器同样无条件停：网络关了它只会每秒失败一次，最后留下一屏过期数据
    state.conn_sampler.stop();
    drop(guard);
    // 关网络同时收掉本机 Tor。三条理由，缺一条都不该留它热乎：
    // 1. Tor 的出站**借道 mihomo 的混合端口**（见 tor::upstream_for）——mihomo 一死，
    //    它的电路全断、bootstrap 回不去，留着只是每几秒失败一次、刷一屏日志；
    // 2. 它**占着 9050**——用户回头开自己的 Tor 会发现端口被一个没人管的 tor.exe 占着；
    // 3. 与阻断规则、采样器同属「本次会话的增益」，会话结束就撤，是既有口径。
    //
    // **必须放在 `drop(guard)` 之后**：`stop_all` 拿 `tor` 锁，而并发的看门狗是
    // 「持 tor 锁 → 等 net 锁」（回写状态）。两边同时发生就是 AB-BA 死锁——
    // `state.rs` 那条 tor→net 的规矩对**停止路径同样成立**，不只是启动路径。
    // 顺带：停掉 Tor 也正好让看门狗的下一次 publish 拿到 None 而安静退出。
    crate::tor::stop_all(state).await;
    Ok(NetStatus {
        running: false,
        port: None,
        conflicts: vec![],
        degraded_geo: degraded(state),
        via_service: false,
        // 没在跑就没有 TUN——网卡是进程退出即销毁的临时网卡（spec 0c）
        tun: false,
        // 没在跑就没有阻断：规则刚被无条件撤掉了
        firewall: FirewallState::Off.as_str().to_string(),
        firewall_detail: None,
        // 接管同理：那是「开启那一刻」的事，关了网络就没有可说的了（下次开启会重新算）
        takeover_killed: None,
        takeover_detail: None,
        // Tor 刚被 stop_all 收掉，如实归零
        tor: crate::tor::TorState::Off.as_str().to_string(),
        tor_detail: None,
    })
}

/// 退出兜底：Detach/本地子进程都没能留下活端口时，直接起一个最小 direct mihomo。
///
/// 不改系统代理/环境变量——它们由调用方随后 `restore_proxy` 还原；这个端点只为让
/// 那些仍握着旧环境变量的**已运行**程序不再 `ECONNREFUSED`。配置与登录兜底同源
/// （[`super::direct_config_yaml`]），两条路不会各自漂移。
///
/// 失败只返回错误给调用方记日志：退出清理已经做了能做的，不能因为兜底失败反过来
/// 阻断退出或跳过代理还原（那会把“端口可能没保住”升级成“代理指针也没还原”）。
async fn spawn_direct_fallback(state: &AppState, port: u16) -> Result<(), String> {
    let net_dir = state.data_dir.join(NET_DIR);
    let resource = state
        .sidecar_resource
        .read()
        .map(|g| g.clone())
        .unwrap_or(None);
    let exe = sidecar::ensure_binary(&net_dir, resource.as_deref())?;
    std::fs::write(net_dir.join("config.yaml"), super::direct_config_yaml(port))
        .map_err(|e| format!("写入退出兜底配置失败: {e}"))?;
    let mut child = crate::util::quiet(std::process::Command::new(&exe))
        .arg("-d")
        .arg(&net_dir)
        .current_dir(&net_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("启动退出兜底 mihomo 失败: {e}"))?;
    let attempts = FALLBACK_TIMEOUT.as_millis() / FALLBACK_INTERVAL.as_millis();
    for _ in 0..attempts {
        if crate::net::winprobe::port_is_listening(port) {
            // 丢掉句柄不杀进程：应用马上退出，mihomo 要继续占着端口给已运行程序用。
            // std::process::Child 的 Drop 只关句柄，不终止子进程。
            drop(child);
            return Ok(());
        }
        tokio::time::sleep(FALLBACK_INTERVAL).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "退出兜底 mihomo 未能在 {}s 内监听 127.0.0.1:{port}",
        FALLBACK_TIMEOUT.as_secs()
    ))
}

/// 等退出兜底 mihomo 监听的上限：正常几百毫秒，慢盘/杀软扫描留足余量。
const FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const FALLBACK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
