//! 自写 HIS REST 协议替身（mock server）— 仅用于测试。
//!
//! 监听 `127.0.0.1:0`（随机端口），按 URL 路径分发：
//!
//! | 路径 | 响应 |
//! |------|------|
//! | `/referrals/200/snapshot` | 200 + `tests/fixtures/his_mock_200.json` 内容 |
//! | `/referrals/5xx/snapshot` | 503 + `{"error":"service unavailable"}` |
//! | `/referrals/slow/snapshot` | 5 秒后 200 + `{"slept":"5s"}`（超时验证用）|
//! | 其他 | 404 + `not found` |
//!
//! 测试场景：
//! - `test_his_200_ok`：路径 `200` → 解析快照成功
//! - `test_his_5xx_fallback`：路径 `5xx` → HisError::Unavailable
//! - `test_his_slow_timeout_fallback`：路径 `slow` → HisError::Network（4000ms 超时）
//! - `test_his_404_returns_client_error`：路径 `notfound` → HisError::ClientError
//!
//! ## 实现要点
//!
//! - 用 `hyper 1.x` + `hyper-util` + `http-body-util`（pure Rust，无 openssl）
//! - `tokio::select!` 监听 `oneshot` 停止信号 + TCP accept，drop 即退出
//! - `tokio::sync::oneshot` 一次性停止信号；client 流程退出前 await `stop()`
//!
//! ## 与源产品差异
//!
//! - copper-rs 的 "monitoring HTTP" 是**入站** 监控端点；
//!   本模块的 mock server 是**测试替身**，不入产线代码。

use std::convert::Infallible;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// 200 快照样例（编译期嵌入）。
const FIXTURE_200: &str = include_str!("../../tests/fixtures/his_mock_200.json");

/// 启动一个 mock HIS REST 服务。
///
/// 监听 `127.0.0.1:0`（随机端口），由 [`MockHisHandle`] 持有停止信号与任务句柄。
/// drop [`MockHisHandle`] 会自动发送停止信号（不阻塞）。
pub async fn spawn_mock_his() -> MockHisHandle {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock his: bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("mock his: local_addr");
    let base_url = format!("http://{addr}");

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

    let server = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => break,
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _peer)) => {
                            let io = TokioIo::new(stream);
                            tokio::spawn(async move {
                                let svc = service_fn(router);
                                let _ = http1::Builder::new()
                                    .serve_connection(io, svc)
                                    .await;
                            });
                        }
                        Err(e) => eprintln!("mock his accept error: {e}"),
                    }
                }
            }
        }
    });

    MockHisHandle {
        base_url,
        stop_tx: Some(stop_tx),
        server: Some(server),
    }
}

/// Mock HIS 服务句柄。
///
/// `Drop` 实现确保服务停止（不阻塞，等待 server task 自然退出由 tokio test runtime 控制）。
pub struct MockHisHandle {
    /// 服务 base URL，例 `http://127.0.0.1:54321`。
    pub base_url: String,
    stop_tx: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl MockHisHandle {
    /// 显式停止 mock 服务（await server task 结束）。
    /// 通常 [`Drop`] 已隐式停止；显式 `stop()` 只在需要确定性清理时使用。
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for MockHisHandle {
    fn drop(&mut self) {
        // 自动发停止信号；server task 自然退出或被 runtime 强制终止
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// HTTP 路由分发器。
async fn router(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path();
    // 期望路径形状：`/referrals/{id}/snapshot`
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // parts 期望 ["referrals", "<id>", "snapshot"]
    if parts.len() == 3 && parts[0] == "referrals" && parts[2] == "snapshot" {
        let id = parts[1];
        match id {
            "200" => return Ok(json_response(200, FIXTURE_200)),
            "5xx" => {
                return Ok(json_response(
                    503,
                    r#"{"error":"service unavailable"}"#,
                ));
            }
            "slow" => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                return Ok(json_response(200, r#"{"slept":"5s"}"#));
            }
            "unauthorized" => {
                // 401 — token 过期 / 不正确。HisClient 走 ClientError 不可自动 fallback；
                // 但 spec.md §5 把 401 也归到「可降级」——caller 决定。这里先返回
                // 401 + WWW-Authenticate 头以便 caller 用 status 码分支判定。
                return Ok(Response::builder()
                    .status(401)
                    .header("Content-Type", "text/plain")
                    .header("WWW-Authenticate", "Bearer realm=\"his\"")
                    .body(Full::new(Bytes::from_static(b"unauthorized")))
                    .expect("build 401 response"));
            }
            // 其他 id 走 404
            _ => return Ok(not_found_response()),
        }
    }
    Ok(not_found_response())
}

fn json_response(status: u16, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
        .expect("build 200 response")
}

fn not_found_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(404)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from_static(b"not found")))
        .expect("build 404 response")
}
