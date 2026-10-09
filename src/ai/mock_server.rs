//! 自写 OpenAI-compatible 协议替身（mock server）— 仅用于测试。
//!
//! 监听 `127.0.0.1:0`（随机端口），对 `POST /v1/chat/completions` 返回固定的合法
//! OpenAI-compatible 响应（`choices[0].message.content` 含结构化 JSON）。
//!
//! ## 路由（Task 14 扩展）
//!
//! - `/ok`（默认 `/{suffix}/v1/chat/completions`，suffix 缺失）→ 合法响应
//! - `/malformed`（`/malformed/v1/chat/completions`）→ 200 但 JSON 缺字段
//! - `/timeout`（`/timeout/v1/chat/completions`）→ 10 秒延迟后返回 ok（>4000ms 超时）
//! - `/5xx`（`/5xx/v1/chat/completions`）→ 503 Service Unavailable
//!
//! 用法：`ModelGatewayConfig::new(base_url, ...)` 时把 base_url 设为
//! `mock.base_url + "/malformed"` 等，让 `format!("{base_url}/v1/chat/completions")`
//! 自动带上 marker。
//!
//! ## 录制
//!
//! - `last_request_body()`：取最近一次请求体原文（用于校验结构化 prompt + model）
//! - `last_authorization()`：取最近一次 `Authorization` 头值（用于校验 Bearer token）
//! - `last_request_path()`：取最近一次请求的 path（用于断言路由确实打到目标 marker）
//!
//! ## 实现要点
//!
//! - 用 `hyper 1.x` + `hyper-util` + `http-body-util`（pure Rust，无 openssl）
//! - `tokio::select!` 监听 oneshot 停止信号 + TCP accept；drop handle 自动停
//! - JSON Lines 风格录制：每次请求把 body 追加到 `Arc<Mutex<Vec<u8>>>`

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::AUTHORIZATION;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Mock 响应：合法 OpenAI-compatible 完整 body，含 `choices[0].message.content`
/// 是 JSON 字符串，里面是结构化 AiSummary schema。
const OK_RESPONSE_BODY: &str = r#"{
    "id": "chatcmpl-test-001",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "gpt-4o-mini",
    "choices": [
        {
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"summary\":\"诊断从 chest pain 更新到 ACS suspected；血压 130→162；新增 SpO2 94\",\"fields_added\":[\"spo2\"],\"fields_removed\":[],\"semantic_class\":\"diagnosis_update\"}"
            },
            "finish_reason": "stop"
        }
    ],
    "usage": { "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150 }
}"#;

/// Malformed 响应：HTTP 200 但 `choices[0].message.content` 是非 JSON 字符串
/// （触发 [`AiError::ParseFailed`]）。
const MALFORMED_RESPONSE_BODY: &str = r#"{
    "id": "chatcmpl-test-malformed",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "gpt-4o-mini",
    "choices": [
        {
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "this is not parseable as AiSummary JSON: 字段级 diff 文本"
            },
            "finish_reason": "stop"
        }
    ],
    "usage": { "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150 }
}"#;

/// Schema 缺字段响应：HTTP 200 但 `content` 是合法 JSON 但缺 `fields_added` /
/// `fields_removed` 等必填字段（触发 [`AiError::SchemaMissing`]）。
const SCHEMA_MISSING_FIELDS_BODY: &str = r#"{
    "id": "chatcmpl-test-schema-missing",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "gpt-4o-mini",
    "choices": [
        {
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"summary\":\"only summary, no other fields\"}"
            },
            "finish_reason": "stop"
        }
    ],
    "usage": { "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150 }
}"#;

/// 5xx 响应体。
const ERROR_5XX_BODY: &str = r#"{"error":{"message":"upstream unavailable","code":"service_overloaded"}}"#;

/// 启动一个 mock OpenAI-compatible 服务。
///
/// 监听 `127.0.0.1:0`（随机端口），由 [`MockOpenAiHandle`] 持有停止信号 + 任务句柄 + 录制数据。
/// drop [`MockOpenAiHandle`] 会自动发送停止信号（不阻塞）。
pub async fn spawn_mock_openai() -> MockOpenAiHandle {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock openai: bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("mock openai: local_addr");
    let base_url = format!("http://{addr}");

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

    // 录制共享 buffer
    let recorded: Arc<Mutex<RecordedRequest>> = Arc::new(Mutex::new(RecordedRequest::default()));

    let rec_for_task = Arc::clone(&recorded);
    let server = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => break,
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _peer)) => {
                            let io = TokioIo::new(stream);
                            let rec_clone = Arc::clone(&rec_for_task);
                            tokio::spawn(async move {
                                let svc = service_fn(move |req| {
                                    let rec_clone2 = Arc::clone(&rec_clone);
                                    async move {
                                        handle_request(req, rec_clone2).await
                                    }
                                });
                                let _ = http1::Builder::new()
                                    .serve_connection(io, svc)
                                    .await;
                            });
                        }
                        Err(e) => eprintln!("mock openai accept error: {e}"),
                    }
                }
            }
        }
    });

    MockOpenAiHandle {
        base_url,
        stop_tx: Some(stop_tx),
        server: Some(server),
        recorded,
    }
}

/// 单次请求的录制数据（mock 内部用）。
#[derive(Default, Debug)]
struct RecordedRequest {
    body: Vec<u8>,
    authorization: Option<String>,
    path: Option<String>,
}

impl RecordedRequest {
    fn record_body(&mut self, bytes: Bytes) {
        self.body = bytes.to_vec();
    }

    fn record_authorization(&mut self, value: String) {
        self.authorization = Some(value);
    }

    fn record_path(&mut self, path: String) {
        self.path = Some(path);
    }
}

/// Mock OpenAI 服务句柄。
///
/// `Drop` 实现确保服务停止（不阻塞，等待 server task 自然退出由 tokio test runtime 控制）。
pub struct MockOpenAiHandle {
    /// 服务 base URL，例 `http://127.0.0.1:54321`。
    pub base_url: String,
    stop_tx: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    recorded: Arc<Mutex<RecordedRequest>>,
}

impl MockOpenAiHandle {
    /// 显式停止 mock 服务（await server task 结束）。
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server.take() {
            let _ = handle.await;
        }
    }

    /// 取最近一次请求体原文（用于断言 prompt 结构）。
    pub fn last_request_body(&self) -> Option<String> {
        let rec = self.recorded.lock().expect("recorded lock");
        String::from_utf8(rec.body.clone()).ok()
    }

    /// 取最近一次 `Authorization` 头值（用于断言 Bearer token）。
    pub fn last_authorization(&self) -> Option<String> {
        let rec = self.recorded.lock().expect("recorded lock");
        rec.authorization.clone()
    }

    /// 取最近一次请求的 path（用于断言路由命中 marker）。
    pub fn last_request_path(&self) -> Option<String> {
        let rec = self.recorded.lock().expect("recorded lock");
        rec.path.clone()
    }
}

impl Drop for MockOpenAiHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// HTTP 路由分发器（生产路径：合法响应）。
async fn handle_request(
    req: Request<Incoming>,
    recorded: Arc<Mutex<RecordedRequest>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // === 1. 读取 path（在 into_body 之前）===
    let path = req.uri().path().to_string();

    // === 2. 记录 Authorization 头 ===
    if let Some(auth) = req.headers().get(AUTHORIZATION) {
        if let Ok(s) = auth.to_str() {
            recorded.lock().expect("recorded lock").record_authorization(s.to_string());
        }
    }
    // 记录 path（Task 14：用于断言路由确实命中目标 marker）
    recorded.lock().expect("recorded lock").record_path(path.clone());

    // === 3. 收集 body ===
    let body_bytes = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_else(|_| Bytes::new());
    recorded.lock().expect("recorded lock").record_body(body_bytes.clone());

    // === 4. 路由 ===
    if path.ends_with("/chat/completions") {
        let body_json: Value = serde_json::from_slice(&body_bytes).unwrap_or(json!({}));
        // 校验 model 字段非空 + response_format.json_schema 存在
        if body_json.get("model").is_none() {
            return Ok(json_response(400, r#"{"error":{"message":"missing model"}}"#));
        }
        if body_json
            .get("response_format")
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            != Some("json_schema")
        {
            return Ok(json_response(400, r#"{"error":{"message":"response_format must be json_schema"}}"#));
        }

        // Task 14：基于 path marker 分发
        // - 默认 /ok
        // - /5xx → 503
        // - /timeout → sleep 10s 后返回 ok（让 ModelGateway 4000ms 超时）
        // - /malformed → 200 + 非 JSON content（触发 ParseFailed）
        // - /schema_missing → 200 + 缺字段 JSON（触发 SchemaMissing）
        if path.contains("/5xx/") || path.ends_with("/5xx") {
            return Ok(json_response(503, ERROR_5XX_BODY));
        }
        if path.contains("/timeout/") || path.ends_with("/timeout") {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            return Ok(json_response(200, OK_RESPONSE_BODY));
        }
        if path.contains("/malformed/") || path.ends_with("/malformed") {
            return Ok(json_response(200, MALFORMED_RESPONSE_BODY));
        }
        if path.contains("/schema_missing/") || path.ends_with("/schema_missing") {
            return Ok(json_response(200, SCHEMA_MISSING_FIELDS_BODY));
        }
        return Ok(json_response(200, OK_RESPONSE_BODY));
    }

    Ok(not_found_response())
}

fn json_response(status: u16, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
        .expect("build response")
}

fn not_found_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(404)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from_static(b"not found")))
        .expect("build 404 response")
}
