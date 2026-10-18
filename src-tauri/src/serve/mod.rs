//! 本地订阅服务：axum 监听 127.0.0.1，GET /sub/{token} 返回合并配置 YAML。
//! 只回环绑定 + 路径 token 校验；配置变化无需重启（每次请求读共享缓存）。

use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

/// 订阅服务错误。
#[derive(Debug, Error)]
pub enum ServeError {
    #[error("端口绑定失败: {0}")]
    BindFailed(String),
    #[error("订阅服务已在运行")]
    // P2 接线：已运行判定目前由命令层以 Message 表达，此变体为服务层契约保留
    #[allow(dead_code)]
    AlreadyRunning,
}

/// 运行中的订阅服务句柄。
#[derive(Debug)]
pub struct SubServer {
    pub port: u16,
    pub handle: tokio::task::JoinHandle<()>,
}

struct ServeState {
    config: Arc<RwLock<String>>,
    token: String,
}

/// 启动订阅服务；port=0 时由系统分配空闲端口。
pub async fn start(
    config: Arc<RwLock<String>>,
    token: String,
    port: u16,
) -> Result<SubServer, ServeError> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| ServeError::BindFailed(e.to_string()))?;
    let actual_port = listener
        .local_addr()
        .map_err(|e| ServeError::BindFailed(e.to_string()))?
        .port();
    let state = Arc::new(ServeState { config, token });
    let app = Router::new()
        .route("/sub/{token}", get(handler))
        .with_state(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(SubServer {
        port: actual_port,
        handle,
    })
}

async fn handler(Path(token): Path<String>, State(state): State<Arc<ServeState>>) -> Response {
    if token != state.token {
        return Response::builder()
            .status(404)
            .body(axum::body::Body::empty())
            .expect("404 body");
    }
    let yaml = state.config.read().await.clone();
    Response::builder()
        .status(200)
        .header("content-type", "text/yaml; charset=utf-8")
        .body(axum::body::Body::from(yaml))
        .expect("200 body")
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/serve/mods.rs"
));
