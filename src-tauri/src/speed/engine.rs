//! 测速引擎：SpeedEngine trait 抽象控制器 REST 与代理下载（MihomoEngine 走真 HTTP，
//! 测试用 FakeEngine 注入结果/闸门/取消敏感）。输入：代理名/代理 JSON/下载 URL 与
//! 超时/取消标志；输出：延迟 ms 或下载 bytes/s，失败带中文原因。
//! 依赖：reqwest（控制器请求经 Bearer token）、tokio（下载超时）、async-trait。

use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 测速引擎统一接口：注入/清理代理、延迟、经 mixed-port 代理下载（返回已成功下载的字节数）。
#[async_trait]
pub trait SpeedEngine: Send + Sync {
    async fn put_proxy(&self, name: &str, proxy: serde_json::Value) -> Result<(), String>;
    async fn delete_proxy(&self, name: &str) -> Result<(), String>;
    async fn delay(&self, name: &str, url: &str, timeout_ms: u64) -> Result<i64, String>;
    async fn download_bytes(
        &self,
        mixed_port: u16,
        url: &str,
        size_limit: usize,
        timeout: std::time::Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<f64, String>;

    /// 经当前节点取回一个 URL 的**正文**（出口 IP 探测用：请求 ipify 读回显）。
    /// 与 download_bytes 走同一条 mixed-port 代理路径，区别只是要正文而不是字节数。
    /// 不带取消标志：探测是即时的单次命令，没有 job 取消语义。
    async fn fetch_text(
        &self,
        mixed_port: u16,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<String, String>;
}

/// 真实引擎：mihomo 控制器 REST（Bearer token）+ mixed-port HTTP 代理下载。
/// 注：mihomo 无运行时增删 proxy 的 API——注入/清理 = 经 /configs 热重载
/// （render_sidecar_config(_with_proxy) 生成 payload，path 为 sidecar config.yaml）。
pub struct MihomoEngine {
    http: reqwest::Client,
    controller_port: u16,
    mixed_port: u16,
    token: String,
    config_path: std::path::PathBuf,
}

impl MihomoEngine {
    /// 控制器请求整体 10s 超时：mihomo 进程存活但 API 挂起时 put/delay/delete 不会
    /// 无限等待（终审随修项；delay 另有 mihomo 内部 5s 兜底，此上限防进程僵死）。
    /// config_path：sidecar config.yaml 路径（/configs 热重载的 path）。
    pub fn new(
        controller_port: u16,
        mixed_port: u16,
        token: String,
        config_path: std::path::PathBuf,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client 构建失败"),
            controller_port,
            mixed_port,
            token,
            config_path,
        }
    }

    fn controller(&self) -> String {
        format!("http://127.0.0.1:{}", self.controller_port)
    }

    /// 经 /configs 热重载（PUT configs {path, payload, force} —— mihomo 1.19 支持）。
    async fn reload(&self, yaml: &str) -> Result<(), String> {
        let resp = self
            .http
            .put(format!("{}/configs", self.controller()))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "path": self.config_path.to_string_lossy(),
                "payload": yaml,
                "force": true,
            }))
            .send()
            .await
            .map_err(|e| format!("重载配置失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(http_error("重载配置失败", resp).await);
        }
        Ok(())
    }
}

/// 非 2xx → 状态码 + body 前 200 字符（诊断足够且不吞大响应）。
async fn http_error(action: &str, resp: reqwest::Response) -> String {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let trimmed: String = body.chars().take(200).collect();
    format!("{action}: HTTP {status} {trimmed}")
}

#[async_trait]
impl SpeedEngine for MihomoEngine {
    async fn put_proxy(&self, name: &str, proxy: serde_json::Value) -> Result<(), String> {
        let yaml = crate::speed::sidecar::render_sidecar_config_with_proxy(
            self.controller_port,
            self.mixed_port,
            &self.token,
            name,
            &proxy,
        );
        self.reload(&yaml)
            .await
            .map_err(|e| format!("注入代理失败: {e}"))
    }

    async fn delete_proxy(&self, name: &str) -> Result<(), String> {
        let _ = name;
        let yaml = crate::speed::sidecar::render_sidecar_config(
            self.controller_port,
            self.mixed_port,
            &self.token,
        );
        self.reload(&yaml)
            .await
            .map_err(|e| format!("清理代理失败: {e}"))
    }

    async fn delay(&self, name: &str, url: &str, timeout_ms: u64) -> Result<i64, String> {
        let resp = self
            .http
            .get(format!(
                "{}/proxies/{}/delay?url={}&timeout={}",
                self.controller(),
                name,
                url,
                timeout_ms
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("延迟测试失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(http_error("延迟测试失败", resp).await);
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("延迟响应解析失败: {e}"))?;
        json.get("delay")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "延迟响应缺少 delay 字段".to_string())
    }

    async fn download_bytes(
        &self,
        mixed_port: u16,
        url: &str,
        size_limit: usize,
        timeout: std::time::Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<f64, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".to_string());
        }
        let proxy_url = format!("http://127.0.0.1:{mixed_port}");
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| format!("构建代理客户端失败: {e}"))?)
            .build()
            .map_err(|e| format!("构建代理客户端失败: {e}"))?;
        let fut = async {
            let mut resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("下载请求失败: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("下载失败: HTTP {}", resp.status().as_u16()));
            }
            // 流式读 body：累计到 size_limit 提前结束；每块检查取消标志
            let mut total: usize = 0;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err("已取消".to_string());
                }
                match resp.chunk().await.map_err(|e| format!("下载中断: {e}"))? {
                    Some(c) => {
                        total += c.len();
                        if total >= size_limit {
                            break;
                        }
                    }
                    None => break,
                }
            }
            Ok(total)
        };
        let total = tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| "下载超时".to_string())??;
        // 只返回字节：聚合（Σ字节/墙钟）由执行器 download_aggregate 负责
        Ok(total as f64)
    }

    async fn fetch_text(
        &self,
        mixed_port: u16,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<String, String> {
        let proxy_url = format!("http://127.0.0.1:{mixed_port}");
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| format!("构建代理客户端失败: {e}"))?)
            .build()
            .map_err(|e| format!("构建代理客户端失败: {e}"))?;
        let fut = async {
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("请求失败: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("请求失败: HTTP {}", resp.status().as_u16()));
            }
            resp.text().await.map_err(|e| format!("读取响应失败: {e}"))
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| "请求超时".to_string())?
    }
}

/// 测试用假引擎：注入按代理名延迟结果与下载结果；download_release 闸门挂起下载直到
/// release 或取消（模拟慢下载/取消敏感）；active/max_active 记录并发活跃窗口。
/// 仅 cfg(test) + local-tests 特性编译（测试源码在 tests/rust），供 engine_tests
/// 与测速执行器不变量测试使用。
#[cfg(all(test, feature = "local-tests"))]
pub struct FakeEngine {
    /// 按代理名注入的延迟结果；缺省 Ok(1)
    pub delay_results: std::sync::Mutex<std::collections::HashMap<String, Result<i64, String>>>,
    /// 下载字节结果；缺省 Ok(1.0)
    pub download_result: std::sync::Mutex<Result<f64, String>>,
    /// fetch_text 返回的正文结果；缺省 Ok("1.0.0.0")
    pub fetch_text_result: std::sync::Mutex<Result<String, String>>,
    /// 下载闸门：Some 时 download 挂起直到 watch 广播（发送即释放全部在途下载，或取消）
    pub download_release: std::sync::Mutex<Option<tokio::sync::watch::Receiver<u64>>>,
    /// 前 N 次下载正常返回、第 N+1 次起**永久挂起**：模拟「小量探测跑得完、大量确认
    /// 拉不满」的中低速链路（配 start_paused，外层 timeout 瞬时触发）
    pub hang_after: std::sync::Mutex<Option<usize>>,
    /// 前 N 次下载正常返回、第 N+1 次起**直接返回错误**：模拟确认阶段的大请求被服务端
    /// 拒绝（测速源按请求量限流 → 8 路全 429），探针通过但确认拿不到值
    pub fail_after: std::sync::Mutex<Option<usize>>,
    pub puts: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    pub deletes: std::sync::Mutex<Vec<String>>,
    pub delays_called: std::sync::Mutex<Vec<String>>,
    /// 延迟按序出队（取完为止，末尾那个重复用）；None = 走 [`Self::delay_results`] 定值。
    /// 存在的理由：返定值的桩**分不出「取最小」和「取最后一次」**，而串行 N 次取最小
    /// 正是这个采集口径的全部意义。
    pub delay_sequence: std::sync::Mutex<Option<Vec<Result<i64, String>>>>,
    pub downloads_called: std::sync::Mutex<Vec<String>>,
    /// 当前活跃调用数（并发窗口不变量用）
    pub active: std::sync::Mutex<usize>,
    /// 观测到的最大并发活跃数
    pub max_active: std::sync::Mutex<usize>,
    /// 当前挂起在下载闸门上的调用数（测试同步用确定性信号：闸门挂起才置位，
    /// 避免与 put/delay 的瞬时活跃计数竞态）
    pub gated: std::sync::atomic::AtomicUsize,
}

#[cfg(all(test, feature = "local-tests"))]
impl FakeEngine {
    pub fn new() -> Self {
        Self {
            delay_results: std::sync::Mutex::new(std::collections::HashMap::new()),
            download_result: std::sync::Mutex::new(Ok(1.0)),
            fetch_text_result: std::sync::Mutex::new(Ok("1.0.0.0".to_string())),
            download_release: std::sync::Mutex::new(None),
            hang_after: std::sync::Mutex::new(None),
            fail_after: std::sync::Mutex::new(None),
            puts: std::sync::Mutex::new(vec![]),
            deletes: std::sync::Mutex::new(vec![]),
            delays_called: std::sync::Mutex::new(vec![]),
            delay_sequence: std::sync::Mutex::new(None),
            downloads_called: std::sync::Mutex::new(vec![]),
            active: std::sync::Mutex::new(0),
            max_active: std::sync::Mutex::new(0),
            gated: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 令后续 fetch_text 返回指定正文（出口 IP 探测测试用）。
    pub fn with_fetch_text(body: &str) -> Self {
        let fake = Self::new();
        *fake.fetch_text_result.lock().unwrap() = Ok(body.to_string());
        fake
    }

    /// 挂起后续下载：返回的 watch Sender 用于广播释放全部在途下载（send 一次全部放行）。
    pub fn hang_downloads(&self) -> tokio::sync::watch::Sender<u64> {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        *self.download_release.lock().unwrap() = Some(rx);
        tx
    }

    fn enter(&self) -> FakeActiveGuard<'_> {
        let mut active = self.active.lock().unwrap();
        *active += 1;
        let mut max = self.max_active.lock().unwrap();
        *max = (*max).max(*active);
        FakeActiveGuard { fake: self }
    }
}

/// 活跃计数守卫：方法返回时递减（Drop 保证异常路径不泄漏计数）。
#[cfg(all(test, feature = "local-tests"))]
struct FakeActiveGuard<'a> {
    fake: &'a FakeEngine,
}

#[cfg(all(test, feature = "local-tests"))]
impl Drop for FakeActiveGuard<'_> {
    fn drop(&mut self) {
        let mut active = self.fake.active.lock().unwrap();
        *active -= 1;
    }
}

/// 闸门计数守卫：download_bytes 退出时复位（含取消/超时等 Err 路径）。
#[cfg(all(test, feature = "local-tests"))]
struct FakeGatedGuard<'a> {
    fake: &'a FakeEngine,
}

#[cfg(all(test, feature = "local-tests"))]
impl Drop for FakeGatedGuard<'_> {
    fn drop(&mut self) {
        self.fake.gated.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(all(test, feature = "local-tests"))]
#[async_trait]
impl SpeedEngine for FakeEngine {
    async fn put_proxy(&self, name: &str, proxy: serde_json::Value) -> Result<(), String> {
        let _g = self.enter();
        self.puts.lock().unwrap().push((name.to_string(), proxy));
        Ok(())
    }

    async fn delete_proxy(&self, name: &str) -> Result<(), String> {
        let _g = self.enter();
        self.deletes.lock().unwrap().push(name.to_string());
        Ok(())
    }

    async fn delay(&self, name: &str, _url: &str, _timeout_ms: u64) -> Result<i64, String> {
        let _g = self.enter();
        self.delays_called.lock().unwrap().push(name.to_string());
        {
            let mut seq = self.delay_sequence.lock().unwrap();
            if let Some(v) = seq.as_mut() {
                match v.len() {
                    0 => {}
                    1 => return v[0].clone(),
                    _ => return v.remove(0),
                }
            }
        }
        self.delay_results
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or(Ok(1))
    }

    async fn fetch_text(
        &self,
        _mixed_port: u16,
        _url: &str,
        _timeout: std::time::Duration,
    ) -> Result<String, String> {
        let _g = self.enter();
        self.fetch_text_result.lock().unwrap().clone()
    }

    async fn download_bytes(
        &self,
        _mixed_port: u16,
        url: &str,
        _size_limit: usize,
        _timeout: std::time::Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<f64, String> {
        let _g = self.enter();
        // 闸门计数：进入闸门等待即置位（测试同步用；break/Err 路径由 guard 复位）
        self.gated.fetch_add(1, Ordering::SeqCst);
        let _gg = FakeGatedGuard { fake: self };
        self.downloads_called.lock().unwrap().push(url.to_string());
        // 闸门挂起：10ms 轮询检查 watch 广播（send 一次全部放行）与取消标志；
        // 虚拟时钟（tokio::time::pause）下由 runtime idle 自动推进，不卡死测试
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("已取消".to_string());
            }
            {
                let release = self.download_release.lock().unwrap();
                match release.as_ref() {
                    None => break,
                    Some(rx) if *rx.borrow() > 0 => break,
                    Some(_) => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // 第 N+1 次起永久挂起：调用方（download_aggregate）的外层 timeout 会先触发。
        // 用 pending 而非 sleep：虚拟时钟下 sleep 会被自动推进导致"挂起"提前结束。
        // 守卫必须先绑定再落地：`if let Some(n) = *guard` 的临时量会活到整个 body，
        // 跨 await 持有 MutexGuard 会让 future 不是 Send
        let hang_after = *self.hang_after.lock().unwrap();
        if let Some(n) = hang_after {
            let called = self.downloads_called.lock().unwrap().len();
            if called > n {
                std::future::pending::<()>().await;
            }
        }
        // 同上的计数语义：第 N+1 次起直接拒绝，模拟服务端按请求量限流（确认阶段全 429）
        let fail_after = *self.fail_after.lock().unwrap();
        if let Some(n) = fail_after {
            let called = self.downloads_called.lock().unwrap().len();
            if called > n {
                return Err("下载失败: HTTP 429".to_string());
            }
        }
        self.download_result.lock().unwrap().clone()
    }
}
