# 连接日志流量列（拆上行/下行 + 会话总计）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把连接日志的「流量」一列拆成「上行」「下行」两列，并在面板顶部加一条「本次会话（经 mihomo）」的累计总量，口径随「开启网络 → 关闭网络」起落。

**Architecture:** mihomo `/connections` 响应**顶层**的 `uploadTotal`/`downloadTotal` 已经是进程生命周期累计，而 mihomo 进程恰好活在一次网络会话里（`stop` 时 `kill()`，`start` 时重新 spawn），所以不需要任何累加器——每次采样顺手把这两个数带进既有快照结构，前端拆列并加一条总计条即可。

**Tech Stack:** Rust（tokio + reqwest + serde_json）、React 19 + TypeScript 5.8（strict）、vitest + Testing Library。

## Global Constraints

- 平台：Windows only。后端测试跑在真 Win32 上（`winprobe` 会枚举真实 socket 表）
- 后端全量测试命令必须带跳过项：`cargo test --manifest-path src-tauri/Cargo.toml -- --skip net::runtime_tests --skip net::runtime_service_tests`
  （这两个模块会真开网络、真写系统代理）
- 不新增任何依赖
- 不改 IPC 命令名（`net_conn_log_cmd`）、不改配置格式、不新增实体
- 注释一律中文，且写「为什么」不写「是什么」——与仓库现有注释同一口径
- 不改判决（`Verdict`）、不改 `reason`、不改 `rowIdentity`、不改 `—` 约定
- 提交信息中文，结尾带 `Co-Authored-By: Claude <noreply@anthropic.com>`

## File Structure

| 文件 | 职责 | 本计划中的改动 |
|---|---|---|
| `src-tauri/src/net/connlog.rs` | 采样器 + 判决 + IPC 快照类型 | 新增 `MihomoTotals`/`parse_totals`；`fetch_connections` 带出总量；`Shared`/`ConnLogSnapshot`/`snapshot_of`/`sample_once`/`stop` 带上总量 |
| `src-tauri/src/net/connlog_tests.rs` | 上述模块的测试 | 新增假 mihomo 夹具 + 6 条用例 |
| `src/api/types.ts` | IPC 线格式的 TS 镜像 | `ConnLogSnapshot` 加两个字段 |
| `src/components/ConnLogPanel.tsx` | 面板渲染 | `COLUMNS` 拆两列、行内两格、顶部总计条、`bytesText` 补 GB 档 |
| `src/components/ConnLogPanel.test.tsx` | 面板测试 | 夹具补字段 + 4 条用例 |
| `CHANGELOG.md` | 变更记录 | 一条「新增」 |

---

### Task 1: 解析 mihomo 顶层会话总量（纯函数）

**Files:**
- Modify: `src-tauri/src/net/connlog.rs`（`parse_connections` 之后，约 :401 附近）
- Test: `src-tauri/src/net/connlog_tests.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub struct MihomoTotals { pub upload: u64, pub download: u64 }`（`Debug + Clone + Copy + Default + PartialEq + Eq`）
  - `pub fn parse_totals(v: &serde_json::Value) -> MihomoTotals`

- [ ] **Step 1: 写失败的测试**

在 `connlog_tests.rs` 末尾追加（实测数值取自 2026-10-02 本机 sidecar：经混合端口下 3 MB 后的真实读数）：

```rust
/// `/connections` 顶层的会话累计。数值取自 2026-10-02 本机 sidecar 的真实读数
/// （经混合端口下 3 MB 之后）。
#[test]
fn parse_totals_reads_the_real_response_shape() {
    let v = serde_json::json!({
        "connections": [], "uploadTotal": 79, "downloadTotal": 3_000_128, "memory": 0
    });
    let t = crate::net::connlog::parse_totals(&v);
    assert_eq!(t.upload, 79);
    assert_eq!(t.download, 3_000_128);
}

/// 畸形/缺字段一律给 0，不 panic——与 `parse_connections` 同款宽容：
/// 采样任务被一条坏响应打死才是真故障，输出缺一块只是少一个数。
#[test]
fn parse_totals_defaults_to_zero_on_missing_fields() {
    for v in [
        serde_json::json!({}),
        serde_json::json!({"connections": []}),
        serde_json::json!({"uploadTotal": "nope", "downloadTotal": null}),
    ] {
        let t = crate::net::connlog::parse_totals(&v);
        assert_eq!((t.upload, t.download), (0, 0), "不该 panic 也不该瞎猜：{v}");
    }
}
```

- [ ] **Step 2: 跑测试确认它失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog:: -- --nocapture`
Expected: 编译失败，`cannot find function parse_totals in module connlog`（或 `MihomoTotals` 未定义）

- [ ] **Step 3: 实现**

在 `connlog.rs` 的 `parse_connections` 函数之后插入：

```rust
/// mihomo `/connections` 顶层的会话累计（字节）。
///
/// **语义是进程生命周期**——2026-10-02 实测：经混合端口下 3 MB 后读到
/// `uploadTotal=79 / downloadTotal=3000128`，杀掉进程用同一个 `-d` 目录重启后
/// 两个数都回到 0，且落盘目录只有 `cache.db`（流量不落盘）。
/// 而 mihomo 的进程恰好活在一次「开启网络 → 关闭网络」里（`runtime::stop` 会
/// `kill()`，`runtime::start` 每次重新 spawn），所以它**天然**就是界面要的
/// 「本次会话」口径，不需要任何累加器。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MihomoTotals {
    pub upload: u64,
    pub download: u64,
}

/// 解析 `/connections` 顶层的 `uploadTotal`/`downloadTotal`。
/// 字段缺失/形状不认识给 0——与 `parse_connections` 同款宽容。
pub fn parse_totals(v: &serde_json::Value) -> MihomoTotals {
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    MihomoTotals {
        upload: n("uploadTotal"),
        download: n("downloadTotal"),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog:: -- --nocapture`
Expected: `parse_totals_reads_the_real_response_shape` 与 `parse_totals_defaults_to_zero_on_missing_fields` 均 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/net/connlog.rs src-tauri/src/net/connlog_tests.rs
git commit -m "feat(connlog): 解析 mihomo 顶层的会话累计总量

/connections 顶层的 uploadTotal/downloadTotal 是进程生命周期累计。
2026-10-02 实测：下 3MB 后读到 79/3000128，杀进程用同一 -d 重启即归零、
不落盘。而 mihomo 进程恰好活在一次「开启网络→关闭网络」里，故它天然
就是界面要的「本次会话」口径。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: 假 mihomo 夹具 + `fetch_connections` 带出总量

**Files:**
- Modify: `src-tauri/src/net/connlog.rs`（`fetch_connections`，约 :1093-1113）
- Test: `src-tauri/src/net/connlog_tests.rs`

**Interfaces:**
- Consumes: `MihomoTotals`、`parse_totals`（Task 1）
- Produces:
  - `pub(crate) async fn fetch_connections(client: &reqwest::Client, ctrl_port: u16, token: &str) -> Result<(Vec<MihomoConn>, MihomoTotals), String>`
  - 测试夹具 `struct FakeMihomo`（定义在 `connlog_tests.rs`），`FakeMihomo::start(body_for) -> Self`、`fn stop(&mut self)`、`impl Drop`

**为什么必须新建这个夹具**：`sample_once` 每次采样**读两次** mihomo（前后夹住以消灭假泄漏），而总量是累计快照不是增量——「取后一次而非相加」这条断言只有在真发两次请求的路径上才钉得住，纯函数测不到。`connlog_tests.rs` 现有的失败路径用例是靠「端口 1 上必然没人监听」造的（`:970-973`），没有能答话的服务器。

- [ ] **Step 1: 写夹具与失败的测试**

在 `connlog_tests.rs` 末尾追加：

```rust
/// 假 mihomo 控制器：**只**答 `GET /connections`，其余路径一律 404。
///
/// 回落成「什么都能答」的夹具等于没有夹具（本仓库已经踩过一次：FakeSshServer
/// 未命中时回落成 exit 0 + 空 stdout，(0,"","") 那类夹具条目因此是装饰性的）。
/// 所以下面单独有一条用例钉住它的 404。
struct FakeMihomo {
    port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeMihomo {
    /// `body_for(call_index)` 决定第 n 次（从 0 起）请求返回什么，
    /// 方便做「两次读取值不同」的用例。
    fn start(mut body_for: impl FnMut(usize) -> String + Send + 'static) -> Self {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut n = 0usize;
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                let Ok((mut sock, _)) = listener.accept() else {
                    // 非阻塞空转：10ms 一跳，够快也够省
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                };
                // 只读请求头，到空行为止
                let mut line = String::new();
                {
                    let mut r = BufReader::new(sock.try_clone().unwrap());
                    let _ = r.read_line(&mut line);
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                }
                let body = body_for(n);
                n += 1;
                let (status, payload) = if line.starts_with("GET /connections") {
                    ("200 OK", body)
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let _ = sock.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                    .as_bytes(),
                );
            }
        });
        Self { port, stop, handle: Some(handle) }
    }

    fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FakeMihomo {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tokio::test]
async fn fake_mihomo_404s_paths_it_does_not_serve() {
    // 夹具自证：只答 /connections。回落成「什么都能答」的话，它上面那些用例全是装饰
    let srv = FakeMihomo::start(|_| "{}".to_string());
    let resp = reqwest::get(format!("http://127.0.0.1:{}/version", srv.port))
        .await
        .expect("请求假 mihomo");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fetch_connections_surfaces_the_session_totals() {
    let srv = FakeMihomo::start(|_| {
        r#"{"connections":[],"uploadTotal":79,"downloadTotal":3000128,"memory":0}"#.to_string()
    });
    let client = reqwest::Client::new();
    let (conns, totals) =
        crate::net::connlog::fetch_connections(&client, srv.port, "tok").await.expect("取连接表");
    assert!(conns.is_empty());
    assert_eq!((totals.upload, totals.download), (79, 3_000_128));
}
```

- [ ] **Step 2: 跑测试确认它失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog:: -- --nocapture`
Expected: 编译失败——`fetch_connections` 是私有函数（`E0603`），且它现在只返回 `Vec<MihomoConn>`（解构 `(conns, totals)` 报 `E0308` 类型不匹配）

- [ ] **Step 3: 实现**

把 `connlog.rs` 的 `fetch_connections` 改成（**只改签名与最后一行**，其余原样）：

```rust
pub(crate) async fn fetch_connections(
    client: &reqwest::Client,
    ctrl_port: u16,
    token: &str,
) -> Result<(Vec<MihomoConn>, MihomoTotals), String> {
    // ……请求与错误处理原样不动……
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 mihomo 连接表失败: {e}"))?;
    // 同一次响应上顺手取出顶层总量，**不额外发请求**
    Ok((parse_connections(&v), parse_totals(&v)))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog:: -- --nocapture`
Expected: 三条新用例 PASS。**注意**：此刻 `sample_once` 里两处 `fetch_connections(...).await?` 的返回值还是 tuple，编译会报错——先临时把它们写成 `let (before, _) = …` / `let (after, _) = …` 让它编过，Task 3 再正式接上。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/net/connlog.rs src-tauri/src/net/connlog_tests.rs
git commit -m "feat(connlog): fetch_connections 带出会话总量，并补假 mihomo 夹具

夹具只答 GET /connections，其余路径 404——回落成「什么都能答」的夹具
等于没有夹具，故单独一条用例钉住它的 404。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: 总量进快照（取后一次、失败保留、停止清零）

**Files:**
- Modify: `src-tauri/src/net/connlog.rs`（`ConnLogSnapshot` :251、`Shared` :874、`snapshot_of` :1047、`stop` :943、`drive` :992-1045、`sample_once` :1064-1091）
- Test: `src-tauri/src/net/connlog_tests.rs`

**Interfaces:**
- Consumes: `MihomoTotals`、`fetch_connections`（Task 2）
- Produces: `ConnLogSnapshot` 新增 `pub upload_total: u64, pub download_total: u64`（IPC 线格式字段名即 `upload_total`/`download_total`）

- [ ] **Step 1: 写失败的测试**

在 `connlog_tests.rs` 末尾追加（沿用既有采样器用例的写法：真 tokio、真 spawn、轮询 `snapshot()`）。把下面这段辅助函数放在用例之前：

```rust
/// 起一个指向假 mihomo 的采样器。geo 路径给个不存在的目录 = 空库 = 不降级
/// （与既有两个采样器用例同一取舍）。
fn start_sampler_on(
    sampler: &crate::net::connlog::ConnSampler,
    port: u16,
    state: &crate::state::AppState,
) {
    sampler.start(
        port,
        "tok".into(),
        state.db.clone(),
        state.creds.clone(),
        std::path::PathBuf::from("no-such-dir/country.mmdb"),
    );
}

fn test_state(dir: &tempfile::TempDir) -> crate::state::AppState {
    crate::state::AppState::new_with_store(
        &dir.path().join("t.db"),
        Arc::new(crate::crypto::MemoryStore::default()),
    )
    .unwrap()
}
```

（已核对：`connlog_tests.rs` 里**没有**同名函数——既有两个采样器用例是各自内联构造
`AppState` 的。引入这个辅助函数不构成重复定义。）

```rust
/// 总量要经整条路径进到快照里。夹具返回的是**固定值**：若实现把前后两次读取
/// 相加（它们是累计快照不是增量），这里会读到两倍——故这一条同时钉住 §3.3。
#[tokio::test]
async fn sampler_reports_session_totals_without_double_counting() {
    let srv = FakeMihomo::start(|_| {
        r#"{"connections":[],"uploadTotal":1000,"downloadTotal":2000,"memory":0}"#.to_string()
    });
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir);
    let sampler = crate::net::connlog::ConnSampler::new();
    start_sampler_on(&sampler, srv.port, &state);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let snap = sampler.snapshot(None, false);
        if snap.upload_total > 0 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "采样器没有把总量带进快照");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let snap = sampler.snapshot(None, false);
    assert_eq!(
        (snap.upload_total, snap.download_total),
        (1000, 2000),
        "两次读取是累计快照不是增量，相加会翻倍"
    );
    sampler.stop();
}

/// 一次查询失败不该把用户正在看的数字抹成 0——「这一秒没问到」与「这轮没流量」
/// 是两件事。同款理由见 `drive` 的 Err 分支为什么保留上一轮的行。
#[tokio::test]
async fn failed_sample_keeps_previous_totals() {
    let mut srv = FakeMihomo::start(|_| {
        r#"{"connections":[],"uploadTotal":1000,"downloadTotal":2000,"memory":0}"#.to_string()
    });
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir);
    let sampler = crate::net::connlog::ConnSampler::new();
    start_sampler_on(&sampler, srv.port, &state);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while sampler.snapshot(None, false).upload_total == 0 {
        assert!(std::time::Instant::now() < deadline, "总量没进来，后面的断言无意义");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // 关掉假 mihomo：后续采样必然失败
    srv.stop();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let snap = sampler.snapshot(None, false);
    assert_eq!(
        (snap.upload_total, snap.download_total),
        (1000, 2000),
        "查询失败把总量抹成 0 了"
    );
    sampler.stop();
}

/// 停采样要清零，与清行同步：网络关了就不存在「本次会话」。
#[tokio::test]
async fn stopping_the_sampler_clears_totals() {
    let srv = FakeMihomo::start(|_| {
        r#"{"connections":[],"uploadTotal":1000,"downloadTotal":2000,"memory":0}"#.to_string()
    });
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir);
    let sampler = crate::net::connlog::ConnSampler::new();
    start_sampler_on(&sampler, srv.port, &state);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while sampler.snapshot(None, false).upload_total == 0 {
        assert!(std::time::Instant::now() < deadline, "总量没进来");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    sampler.stop();
    let snap = sampler.snapshot(None, false);
    assert_eq!((snap.upload_total, snap.download_total), (0, 0));
    assert!(snap.rows.is_empty(), "行与总量必须同步清掉");
}
```

- [ ] **Step 2: 跑测试确认它失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog:: -- --nocapture`
Expected: 编译失败——`ConnLogSnapshot` 没有 `upload_total` 字段（`E0609`）

- [ ] **Step 3: 实现**

按顺序改 `connlog.rs` 六处：

1. `ConnLogSnapshot`（:251）加两个字段：

```rust
pub struct ConnLogSnapshot {
    pub rows: Vec<ConnRow>,
    pub sampling: bool,
    pub last_error: Option<String>,
    pub sampled_at: Option<String>,
    pub watch: Vec<String>,
    /// 本次会话（＝当前 mihomo 进程）经 mihomo 的累计上下行，单位字节。
    /// **只统计经过 mihomo 的流量**——绕过它的泄漏流量它看不见，界面文案必须写清楚。
    pub upload_total: u64,
    pub download_total: u64,
}
```

2. `Shared`（:874）加两个字段（`#[derive(Default)]` 已足够，无需手写 `Default`）：

```rust
    /// 会话累计。与 `rows` 同生命周期：查询失败保留、`stop` 清零。
    upload_total: u64,
    download_total: u64,
```

3. `snapshot_of`（:1047）带上：

```rust
        watch: st.watch.clone(),
        upload_total: st.upload_total,
        download_total: st.download_total,
```

4. `sample_once`（:1064）改返回值与取用。签名改为
   `-> Result<(Vec<ConnRow>, Vec<String>, MihomoTotals), String>`，体内：

```rust
    let (before, _) = fetch_connections(client, run.ctrl_port, &run.token).await?;
    // ……socket_snapshot / after 原样……
    let (after, totals) = fetch_connections(client, run.ctrl_port, &run.token).await?;
    // ……其余原样……
    Ok((classify(&sample), watch, totals))
```

**关键：`totals` 取自后一次（`after`）。两次是累计快照不是增量，相加会翻倍。**

5. `drive`（:1004-1017）的 `Ok` 分支接上：

```rust
        match sample_once(&client, &run, geo.clone()).await {
            Ok((rows, watch, totals)) => {
                fails = 0;
                let snap = {
                    let mut st = crate::util::lock_ignore_poison(&shared);
                    st.rows = rows;
                    st.watch = watch;
                    st.upload_total = totals.upload;
                    st.download_total = totals.download;
                    st.last_error = None;
                    st.sampled_at = Some(chrono::Local::now().to_rfc3339());
                    snapshot_of(&st)
                };
                (run.emit)(snap);
            }
```

`Err` 分支**一个字都不改**——它不碰总量，于是天然保留了上一轮的值。

6. `stop`（:943）清零，与 `st.rows.clear()` 并排：

```rust
        st.rows.clear();
        // 网络关了就不存在「本次会话」：总量与行必须同步清掉，
        // 否则下次开启前会显示上一次会话的旧数字
        st.upload_total = 0;
        st.download_total = 0;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog:: -- --nocapture`
Expected: 三条新用例 PASS，`connlog_tests` 其余用例无回归

**若卡在「总量没进来」超时**：这三条用例是**首次**让采样器走成功路径的（既有两个用例
只走失败路径）。先分清是谁的错——把断言前打一行 `sampler.snapshot(None,false).last_error`
出来看：报的是 `winprobe`/`process_images` 相关的错，就是测试环境跑不了真 Win32 枚举
（与本次改动无关），得改成注入式；报的是 `fetch`/`parse` 相关的错，才是本次改动的问题。

- [ ] **Step 5: 反向验证「取后一次」**

把第 4 步里 `sample_once` 的 `let (after, totals) = …` 临时改成
`let (after, totals) = …; let totals = MihomoTotals { upload: before_totals.upload + totals.upload, download: before_totals.download + totals.download };`（或更直接：把 `_` 改成 `before_totals` 后相加）。
Run: `cargo test --manifest-path src-tauri/Cargo.toml net::connlog::sampler_reports_session_totals_without_double_counting`
Expected: **FAIL**，报文含「相加会翻倍」，实际值 `(2000, 4000)`。
然后**改回**。这条 RED 证明该用例真的抓得住这个错误，而不是恰好通过。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/net/connlog.rs src-tauri/src/net/connlog_tests.rs
git commit -m "feat(connlog): 快照带上会话累计总量

取自后一次读取——前后两次是累计快照不是增量，相加会翻倍（有反向验证
钉着）。查询失败保留上一轮、stop 清零，与行的生命周期同步。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: 前端拆列 + 顶部会话总计

**Files:**
- Modify: `src/api/types.ts`（`ConnLogSnapshot`，:144-152）
- Modify: `src/components/ConnLogPanel.tsx`（`COLUMNS` :34-40、`bytesText` :137-142、行渲染 :338、表头区 :176-203）
- Test: `src/components/ConnLogPanel.test.tsx`

**Interfaces:**
- Consumes: `ConnLogSnapshot.upload_total` / `.download_total`（Task 3）
- Produces: 无（末端消费者）

- [ ] **Step 1: 写失败的测试**

先在 `ConnLogPanel.test.tsx` 的 `snapshot()` 夹具（:50-59）补上两个新字段，否则所有既有用例都会因类型不全而编译失败：

```tsx
function snapshot(over: Partial<ConnLogSnapshot> = {}): ConnLogSnapshot {
  return {
    rows: [],
    sampling: true,
    last_error: null,
    sampled_at: '2026-09-19T17:00:00+08:00',
    watch: ['chrome.exe', 'codex.exe'],
    upload_total: 0,
    download_total: 0,
    ...over,
  }
}
```

再在 `describe` 内追加：

```tsx
  it('顶部给出本次会话的累计上下行，并写明只统计经 mihomo 的流量', async () => {
    mocked.netConnLog.mockResolvedValue(
      snapshot({ upload_total: 12_400_000, download_total: 183_700_000 }),
    )
    render(<ConnLogPanel />)
    // 文案必须写明口径：绕过 mihomo 的泄漏流量它根本看不见，写「总流量」是假话
    expect(await screen.findByText(/经 mihomo/)).toBeInTheDocument()
    expect(screen.getByText(/↑ 11\.8 MB/)).toBeInTheDocument()
    expect(screen.getByText(/↓ 175\.2 MB/)).toBeInTheDocument()
  })

  it('未采样时不显示会话总计：不在采样＝不在读 mihomo，留着就是个冻住的假数字', async () => {
    mocked.netConnLog.mockResolvedValue(snapshot({ sampling: false }))
    render(<ConnLogPanel />)
    await screen.findByText('采样未运行')
    expect(screen.queryByText(/经 mihomo/)).not.toBeInTheDocument()
  })

  it('上行与下行分列渲染，不再相加', async () => {
    mocked.netConnLog.mockResolvedValue(
      snapshot({
        rows: [
          row({
            process: 'codex.exe',
            domain: 'auth.openai.com',
            upload: 3_200,
            download: 12_800,
            verdict: 'proxied',
          }),
        ],
      }),
    )
    render(<ConnLogPanel />)
    const tr = (await screen.findByText('codex.exe')).closest('tr')!
    expect(within(tr).getByText('3.1 KB')).toBeInTheDocument()
    expect(within(tr).getByText('12.5 KB')).toBeInTheDocument()
    // 相加的结果（15.6 KB）不许出现——那正是这次要改掉的行为
    expect(within(tr).queryByText('15.6 KB')).not.toBeInTheDocument()
  })

  it('会话总量超过 1GB 时用 GB 档，不能渲染成 1234.5 MB', async () => {
    mocked.netConnLog.mockResolvedValue(
      snapshot({ upload_total: 0, download_total: 4_294_967_296 }),
    )
    render(<ConnLogPanel />)
    expect(await screen.findByText(/↓ 4\.00 GB/)).toBeInTheDocument()
  })
```

- [ ] **Step 2: 跑测试确认它失败**

Run: `npx vitest run src/components/ConnLogPanel.test.tsx`
Expected: 前两条与第四条 FAIL（`经 mihomo` 找不到、`11.8 MB` 找不到、`4.00 GB` 找不到）；第三条 FAIL（`3.1 KB` 找不到，只有相加后的 `15.6 KB`）

- [ ] **Step 3: 实现**

1. `src/api/types.ts` 的 `ConnLogSnapshot` 补两个字段：

```ts
  /** 当前的泄漏关注名单（镜像名），面板展示给用户看「在盯谁」 */
  watch: string[]
  /**
   * 本次会话（＝当前 mihomo 进程）经 mihomo 的累计上下行，单位字节。
   * **只统计经过 mihomo 的流量**——绕过它的泄漏流量它看不见，文案必须写清楚。
   */
  upload_total: number
  download_total: number
```

2. `ConnLogPanel.tsx` 的 `COLUMNS`：把末尾的 `'流量'` 换成两列

```tsx
  '上行',
  '下行',
] as const
```

3. `bytesText` 补 GB 档：

```tsx
function bytesText(n: number): string {
  if (!n) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
  // 会话总量轻易过 1GB，没有这档就会渲染成 1234.5 MB
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
}
```

4. 行渲染（原 :338 那一格）拆成两格：

```tsx
        <td className="py-1 pr-3">{bytesText(row.upload)}</td>
        <td className="py-1">{bytesText(row.download)}</td>
```

5. 表头区（:176-203 那个 `div` 之后、`{error && …}` 之前）插入总计条：

```tsx
      {/* 会话总计。**必须写明「经 mihomo」**：mihomo 的累计只统计经过它的流量，
          绕过它的泄漏流量它根本看不见（表格里那些显示 — 的行就是证据），
          写成「总流量」是假话。
          sampling 为假时整条隐藏：不在采样＝不在读 mihomo，留着一个冻住的数字
          看着可信、其实过期——未知不许冒充已知。 */}
      {snap?.sampling && (
        <p className="mt-2 flex flex-wrap items-center gap-3 text-xs text-neutral-400">
          <span>本次会话（经 mihomo）</span>
          <span className="text-neutral-200">↑ {bytesText(snap.upload_total)}</span>
          <span className="text-neutral-200">↓ {bytesText(snap.download_total)}</span>
        </p>
      )}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npx vitest run src/components/ConnLogPanel.test.tsx`
Expected: 四条新用例 PASS，既有用例无回归

- [ ] **Step 5: 检查列数相关断言**

Run: `grep -n "八列\|COLUMNS\|getAllByRole('columnheader')" src/components/ConnLogPanel.test.tsx`
若有断言写死了列数或列名，一并更新（`流量` → `上行`/`下行`）。

- [ ] **Step 6: 提交**

```bash
git add src/api/types.ts src/components/ConnLogPanel.tsx src/components/ConnLogPanel.test.tsx
git commit -m "feat(connlog): 流量列拆上行/下行，顶部加会话总计

总计文案写明「经 mihomo」：绕过 mihomo 的泄漏流量它看不见，写「总流量」
是假话。未采样时整条隐藏——冻住的数字是未知冒充已知。

bytesText 补 GB 档：会话总量轻易过 1GB，没有这档会渲染成 1234.5 MB。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: 全量回归与变更记录

**Files:**
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: 前四个任务的全部改动
- Produces: 无

- [ ] **Step 1: 后端全量测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- --skip net::runtime_tests --skip net::runtime_service_tests`
Expected: 全绿。**若报 `LNK1104 无法打开文件 …vpn_ctl-*.exe`**：这是本机杀软瞬时锁新链接的 exe，原样重跑一次即可（本仓库已遇到多次）

- [ ] **Step 2: 前端全量测试**

Run: `npm test`
Expected: 全绿

- [ ] **Step 3: lint 与类型**

Run: `npm run lint && npx tsc --noEmit`
Expected: 无输出/无错误

- [ ] **Step 4: 写 CHANGELOG**

在 `CHANGELOG.md` 最新版本的「新增」区追加一条（沿用该文件既有措辞风格，一句话说清行为）：

```markdown
- **连接日志：流量拆成上行/下行，并给出本次会话总量。** 原来那一列是「单条连接」的
  累计（一条 HTTP/2 连接往往只承载一两个请求，所以看着永远只有几百 KB）。现在行内
  分两列，面板顶部多一条「本次会话（经 mihomo）」的累计上下行——口径取自 mihomo
  `/connections` 顶层的 `uploadTotal`/`downloadTotal`，随 mihomo 进程起落，也就是
  随「开启网络 → 关闭网络」归零。**它只统计经过 mihomo 的流量**：绕过 mihomo 的
  连接（表格里显示为 — 的那些）不计入。
```

- [ ] **Step 5: 提交**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): 记录连接日志流量拆列与会话总计

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 完成后的人工验证（不在 CI 内）

1. 重编后端、重启应用（**不要**用后台任务启动，也不要 SendKeys 驱动——把应用强杀会留下死代理断网）
2. 开网络，随便刷几个网页，看顶部「本次会话（经 mihomo）」是否在涨、上行下行是否分开
3. 关网络再开：总量应归零（证明「重新开启才重新记录」）
4. 与 `系统代理` 模式的对照：`windows` 系统代理开着时只有听混合端口的程序计入，TUN 模式才全部计入——文案已说明，核对实际读数是否符合
