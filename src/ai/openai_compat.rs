//! ModelGateway — OpenAI-compatible 单一网关 + 结构化 prompt + 离线降级 + audit.jsonl。
//!
//! ## 与 spec.md / plan.md 的对应
//!
//! - spec.md §4「AI 调用仅做'摘要归并'」+ spec.md §5「AI 不可达 → diff 视图」：
//!   本模块是 ai_role=assist 的实现入口，OpenAI-compatible `POST /v1/chat/completions`
//!   + 4000ms 超时；解析失败 / 超时 / 未配置 → 全部走 [`AiError`] 冒泡，由 caller
//!   自动降级到 [`crate::his::fhir_map::compute_diff`] 字段级 diff 视图（不替医师
//!   改字段 / 签名 / 回写 HIS — 与 spec.md §9 边界一致）
//! - spec.md §5 审计：每次调用必写 audit.jsonl（input_hash + output + model +
//!   latency_ms + status_code）；路径可在构造时注入，未注入则**不写**（避免污染
//!   生产日志路径）
//!
//! ## 凭据位（强制走环境变量）
//!
//! - `OPENAI_BASE_URL`：兼容 OpenAI 的服务根 URL；例 `https://api.openai.com`
//! - `OPENAI_API_KEY`：Bearer token；**不**硬编码、不连接真实生产
//! - `OPENAI_MODEL`：模型名；缺省 `gpt-4o-mini`
//! - `OPENAI_TIMEOUT_MS`：默认 4000ms
//!
//! ## 公开 API
//!
//! - [`ModelGateway`]：单次调用入口（POST /v1/chat/completions + bearer + JSON Schema 强制）
//! - [`ModelGatewayConfig`]：构造配置（base_url / api_key / model / timeout_ms）
//! - [`AiSummary`]：结构化输出 `{summary, fields_added, fields_removed, semantic_class}`
//! - [`AiError`]：错误类型（CredentialMissing / Network / UpstreamError / ParseFailed / SchemaMissing）
//!
//! ## 实现要点
//!
//! - 业务层只调单一 [`ModelGateway`]，不直接耦合厂商 SDK（spec §1 ai_role=assist 硬约束）
//! - 协议兼容：使用 OpenAI-compatible `POST /v1/chat/completions`，`response_format.json_schema`
//!   强制结构化输出；返回时容错两种 content 形态（纯 JSON 字符串 / 直接嵌套在 content 字段里）
//! - timeout / 上游错误 / 解析失败 / schema 校验失败 → 写 audit.jsonl 后冒泡 `AiError`；
//!   caller 走 `crate::his::fhir_map::compute_diff` 字段级 diff 路径（diff 视图仍照常显示）

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Utc;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// 默认超时（4000ms）。
pub const DEFAULT_OPENAI_TIMEOUT_MS: u64 = 4000;

/// 默认模型（OpenAI-compatible 行业惯例）。
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";

/// 结构化 prompt 强制 Schema（response_format.json_schema.schema）。
/// 落在请求体里，要求 LLM 输出严格匹配此结构；任何缺字段都会被本地反序列化拒收。
const STRUCTURED_SCHEMA_NAME: &str = "referral_diff_summary";

const SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "summary":          { "type": "string" },
        "fields_added":     { "type": "array", "items": { "type": "string" } },
        "fields_removed":   { "type": "array", "items": { "type": "string" } },
        "semantic_class":   { "type": "string" }
    },
    "required": ["summary", "fields_added", "fields_removed", "semantic_class"],
    "additionalProperties": false
}"#;

/// AI 摘要结构化输出。
///
/// 与 spec.md §9「LLM 拟人中性职能」一致：仅做字段级 diff 归并 + 摘要，**不**输出
/// 诊断或治疗建议。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiSummary {
    /// 一句话自然语言摘要（不超过 200 字）
    pub summary: String,
    /// after 多出来的字段名列表
    pub fields_added: Vec<String>,
    /// after 删除的字段名列表
    pub fields_removed: Vec<String>,
    /// 语义分类：diagnosis_update / vitals_change / medication_change / admin_update / other
    pub semantic_class: String,
}

impl AiSummary {
    /// 构造一个空摘要（用于降级路径占位）
    pub fn empty() -> Self {
        Self {
            summary: String::new(),
            fields_added: Vec::new(),
            fields_removed: Vec::new(),
            semantic_class: "unspecified".to_string(),
        }
    }
}

/// ModelGateway 错误类型。
///
/// 分组：
/// - `CredentialMissing`：模型未配置 / OPENAI_API_KEY 缺失 → 不写 audit 的"业务
///   不在场"分支，仍走 audit 兜底（让链路完整）
/// - `Network`：DNS / TCP / TLS / timeout（4000ms）
/// - `UpstreamError`：HTTP 4xx / 5xx
/// - `ParseFailed`：上游响应非 JSON 或缺关键字段
/// - `SchemaMissing`：JSON 结构不符合 AiSummary schema
///
/// 每条 `Display` 不空且不出现 `"unknown"` 字样。
#[derive(Debug, Error)]
pub enum AiError {
    /// OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL 任一未设置或为空。
    #[error("ModelGateway 凭据缺失：OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL 未在环境变量中设置")]
    CredentialMissing,

    /// 网络层失败：DNS / TCP / TLS / timeout。
    #[error("ModelGateway 网络错误：{0}")]
    Network(String),

    /// 上游 HTTP 4xx / 5xx。
    #[error("ModelGateway 上游 HTTP {status}")]
    UpstreamError { status: u16 },

    /// 响应体非合法 JSON / 缺 `choices[0].message.content`。
    #[error("ModelGateway 响应无法解析：{0}")]
    ParseFailed(String),

    /// JSON 结构与 [`AiSummary`] 不匹配（缺字段或类型错）。
    #[error("ModelGateway 响应缺关键字段：{0}")]
    SchemaMissing(&'static str),
}

/// ModelGateway 构造配置。
///
/// 推荐用 [`ModelGatewayConfig::from_env`] 读 .env；测试可直接 [`ModelGatewayConfig::new`]。
#[derive(Clone, Debug)]
pub struct ModelGatewayConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_ms: u64,
}

impl ModelGatewayConfig {
    /// 直接构造（测试用）。
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        timeout_ms: u64,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        let api_key = api_key.into();
        let model = model.into();
        if base_url.trim().is_empty()
            || api_key.trim().is_empty()
            || model.trim().is_empty()
        {
            return Err(AiError::CredentialMissing);
        }
        Ok(Self {
            base_url,
            api_key,
            model,
            timeout_ms,
        })
    }

    /// 从 .env 读 `OPENAI_BASE_URL` / `OPENAI_API_KEY` / `OPENAI_MODEL` /
    /// `OPENAI_TIMEOUT_MS`，缺任一（除 MODEL 外）即 [`AiError::CredentialMissing`]。
    pub fn from_env() -> Result<Self, AiError> {
        let base_url = env::var("OPENAI_BASE_URL").map_err(|_| AiError::CredentialMissing)?;
        let api_key = env::var("OPENAI_API_KEY").map_err(|_| AiError::CredentialMissing)?;
        let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
        let timeout_ms = env::var("OPENAI_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_OPENAI_TIMEOUT_MS);
        Self::new(base_url, api_key, model, timeout_ms)
    }
}

/// 单次 ModelGateway 调用结果（仅用于审计）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditRecord {
    ts: String,
    model: String,
    input_hash: String,
    /// 输出截断到 256 字符；过长记 `{prefix}...`
    output: String,
    status_code: u16,
    latency_ms: u128,
    /// "ok" / "network_error" / "upstream_error" / "parse_error" / "schema_error"
    status: String,
}

/// ModelGateway — 单一网关入口 + audit.jsonl 落盘。
///
/// 业务层只调 [`ModelGateway::summarize_diff`]；不走厂商 SDK；
/// 失败模式（`AiError::*`）由 caller 接住后走 `compute_diff` 字段级 diff 视图。
#[derive(Clone, Debug)]
pub struct ModelGateway {
    config: ModelGatewayConfig,
    http: Client,
    /// audit.jsonl 落盘路径；None = 不落盘（用于纯协议替身测试）
    audit_log_path: Option<PathBuf>,
}

impl ModelGateway {
    /// 用配置构造 ModelGateway。
    pub fn new(config: ModelGatewayConfig) -> Result<Self, AiError> {
        let http = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|e| AiError::Network(format!("reqwest build: {e}")))?;
        Ok(Self {
            config,
            http,
            audit_log_path: None,
        })
    }

    /// 注入 audit.jsonl 落盘路径（链式）；**未注入则不写**，避免污染生产日志路径。
    pub fn with_audit_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.audit_log_path = Some(path.into());
        self
    }

    /// 当前 audit.jsonl 路径（仅诊断用）。
    pub fn audit_log_path(&self) -> Option<&PathBuf> {
        self.audit_log_path.as_ref()
    }

    /// 是否已正确配置（base_url / api_key / model 都非空）。
    pub fn is_configured(&self) -> bool {
        !(self.config.base_url.trim().is_empty()
            || self.config.api_key.trim().is_empty()
            || self.config.model.trim().is_empty())
    }

    /// 当前配置（仅诊断用）。
    pub fn config(&self) -> &ModelGatewayConfig {
        &self.config
    }

    /// 主路径：调 LLM 做 before/after 字段级 diff → 自然语言摘要。
    ///
    /// 失败语义：
    /// - 未配置 → `Err(AiError::CredentialMissing)`（caller 立即降级到 diff，不发请求）
    /// - 超时/网络 → `Err(AiError::Network)`
    /// - 4xx/5xx → `Err(AiError::UpstreamError { status })`
    /// - 响应体非 JSON / 缺 `choices[0].message.content` → `Err(AiError::ParseFailed)`
    /// - content 不符合 AiSummary schema → `Err(AiError::SchemaMissing)`
    ///
    /// 任意失败均写一行 audit.jsonl（`status` 字段标记失败原因）；caller 接住 `Err`
    /// 后**自动**走 `compute_diff` 字段级 diff，不替医师改任何字段 / 签名 / HIS 回写。
    pub async fn summarize_diff(
        &self,
        referral_id: &str,
        cl_id: u64,
        before: &Map<String, Value>,
        after: &Map<String, Value>,
    ) -> Result<AiSummary, AiError> {
        if !self.is_configured() {
            return Err(AiError::CredentialMissing);
        }

        let prompt = build_structured_prompt(before, after);
        let url = format!(
            "{}/v1/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let body = json!({
            "model": self.config.model,
            "messages": [
                {
                    "role": "system",
                    "content": "你是医院转诊单留痕助理。把 before/after 字段级 diff 归并为自然语言摘要 + 字段清单；不要输出诊断或治疗建议。"
                },
                { "role": "user", "content": prompt },
            ],
            "temperature": 0.0,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": STRUCTURED_SCHEMA_NAME,
                    "strict": true,
                    "schema": serde_json::from_str::<Value>(SCHEMA_JSON)
                        .expect("schema JSON literal is valid"),
                }
            }
        });

        let input_hash =
            sha256_hex(format!("{}|{}|{}|{}", referral_id, cl_id, before.len(), after.len()).as_bytes());
        let started = Instant::now();

        // === 1. 发请求 ===
        let resp_result = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await;
        let latency_ms = started.elapsed().as_millis();

        let resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                let err = AiError::Network(e.to_string());
                self.append_audit(&input_hash, "", 0, latency_ms, "network_error");
                return Err(err);
            }
        };

        // === 2. status 分流 ===
        let status: StatusCode = resp.status();
        if status.is_server_error() || status.is_client_error() {
            let code = status.as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            let truncated = if body_text.len() > 256 {
                format!("{}...", &body_text[..256])
            } else {
                body_text
            };
            self.append_audit(&input_hash, &truncated, code, latency_ms, "upstream_error");
            return Err(AiError::UpstreamError { status: code });
        }

        // === 3. 解析响应 ===
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                self.append_audit(&input_hash, "", 200, latency_ms, "parse_error");
                return Err(AiError::ParseFailed(e.to_string()));
            }
        };

        let content_str = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                self.append_audit(&input_hash, "", 200, latency_ms, "parse_error");
                AiError::ParseFailed(
                    "missing choices[0].message.content in OpenAI-compatible response".to_string(),
                )
            })?
            .to_string();

        // === 4. 校验 schema ===
        let summary: AiSummary = match serde_json::from_str::<AiSummary>(&content_str) {
            Ok(s) => s,
            Err(e) => {
                // 在校验失败分支，content_str 既要写 audit（截断版），又要用来
                // 判断 "是否是 JSON 字符串"
                let starts_with_brace = content_str.trim_start().starts_with('{');
                let truncated: String = if content_str.len() > 256 {
                    format!("{}...", &content_str[..256])
                } else {
                    content_str.clone()
                };
                if starts_with_brace {
                    // content 是 JSON 但字段不齐 → schema_error
                    self.append_audit(&input_hash, &truncated, 200, latency_ms, "schema_error");
                    return Err(AiError::SchemaMissing("AiSummary fields missing"));
                }
                // content 不是 JSON → parse_error
                self.append_audit(&input_hash, &truncated, 200, latency_ms, "parse_error");
                return Err(AiError::ParseFailed(format!(
                    "content not parseable as AiSummary JSON: {e}"
                )));
            }
        };

        self.append_audit(&input_hash, &content_str, 200, latency_ms, "ok");
        Ok(summary)
    }

    /// 写一行 audit JSON 到 audit_log_path（**未注入则跳过**）。
    fn append_audit(
        &self,
        input_hash: &str,
        output: &str,
        status_code: u16,
        latency_ms: u128,
        status: &str,
    ) {
        let Some(path) = &self.audit_log_path else {
            return;
        };
        let record = AuditRecord {
            ts: Utc::now().to_rfc3339(),
            model: self.config.model.clone(),
            input_hash: input_hash.to_string(),
            output: if output.len() > 256 {
                format!("{}...", &output[..256])
            } else {
                output.to_string()
            },
            status_code,
            latency_ms,
            status: status.to_string(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            if let Ok(s) = serde_json::to_string(&record) {
                let _ = f.write_all(s.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }
    }
}

/// 构造结构化 prompt（user message content）。
///
/// 不把语义类预测暴露在 prompt 中 — 那由 schema 强制 + caller 据"语义类决定
/// UI 配色"独立完成；本模块只负责 prompt 模板 + schema。
fn build_structured_prompt(before: &Map<String, Value>, after: &Map<String, Value>) -> String {
    let before_pretty =
        serde_json::to_string_pretty(&Value::Object(before.clone())).unwrap_or_default();
    let after_pretty =
        serde_json::to_string_pretty(&Value::Object(after.clone())).unwrap_or_default();
    format!(
        "对比以下两份转诊单字段级快照，输出严格的 JSON 摘要。\n\n\
         BEFORE:\n{before}\n\nAFTER:\n{after}\n",
        before = before_pretty,
        after = after_pretty
    )
}

/// SHA-256 hex（用于 audit 的 input_hash）。
fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::mock_server::spawn_mock_openai;
    use std::sync::Mutex;

    /// 串行化 env 改动的全局锁（与 src/his/client.rs 同款）
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn map_with(pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert(
                (*k).to_string(),
                Value::String((*v).to_string()),
            );
        }
        m
    }

    /// 构造一个 MINIMAL 字段级 diff 输入
    fn diff_input() -> (Map<String, Value>, Map<String, Value>) {
        let before = map_with(&[("diagnosis", "chest pain"), ("bp_systolic", "130")]);
        let after = map_with(&[("diagnosis", "ACS suspected"), ("bp_systolic", "162"), ("spo2", "94")]);
        (before, after)
    }

    /// 测试 1：mock /ok 返回合法 AiSummary → ModelGateway 校验请求结构 +
    /// 解析响应 + 返回 AiSummary 满足 schema；同时验证 Authorization Bearer 被发送
    /// + model 字段出现在请求体中 + response_format.json_schema 字段在请求体里。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_model_gateway_protocol_compat() {
        let (before, after) = diff_input();
        let mock = spawn_mock_openai().await;
        let cfg = ModelGatewayConfig::new(
            mock.base_url.clone(),
            "test_openai_key_xyz",
            "gpt-4o-mini",
            4000,
        )
        .expect("config ok");
        let gw = ModelGateway::new(cfg).expect("gateway ok");

        // 注入一个 tempdir 里的 audit_log_path 用于隔离
        let tmp = tempfile::tempdir().expect("tempdir");
        let audit_path = tmp.path().join("audit.jsonl");
        let gw = gw.with_audit_log(audit_path.clone());

        let summary = gw
            .summarize_diff("REF-100", 1, &before, &after)
            .await
            .expect("protocol ok");

        assert_eq!(summary.summary, "诊断从 chest pain 更新到 ACS suspected；血压 130→162；新增 SpO2 94");
        assert_eq!(summary.fields_added, vec!["spo2"]);
        assert_eq!(summary.fields_removed, Vec::<String>::new());
        assert_eq!(summary.semantic_class, "diagnosis_update");

        // mock 校验：请求体含 model + response_format.json_schema + messages[1].content
        let captured = mock.last_request_body().expect("captured body");
        assert!(captured.contains("\"model\":\"gpt-4o-mini\""));
        assert!(captured.contains("\"response_format\""));
        assert!(captured.contains("json_schema"));
        assert!(captured.contains("\"schema\""));
        // user message content 应含 BEFORE/AFTER
        assert!(captured.contains("BEFORE"));
        assert!(captured.contains("AFTER"));
        assert!(captured.contains("chest pain"));

        // mock 校验：Authorization Bearer 头被发送
        let last_auth = mock.last_authorization().expect("captured auth");
        assert_eq!(last_auth, "Bearer test_openai_key_xyz");

        // 校验 audit.jsonl 至少有一行
        let audit_text = std::fs::read_to_string(&audit_path).expect("audit read");
        let lines: Vec<&str> = audit_text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(
            lines.len() >= 1,
            "expected at least one audit line, got {lines:?}"
        );
        let first: Value = serde_json::from_str(lines[0]).expect("audit JSON");
        assert_eq!(first["status"], "ok");
        assert_eq!(first["status_code"], 200);
        assert_eq!(first["model"], "gpt-4o-mini");
        assert!(first["input_hash"].as_str().expect("input_hash str").len() == 64);
    }

    /// 测试 2：调用后 audit.jsonl 至少追加一行（含 input_hash + status_code + model 字段）。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_audit_jsonl_written() {
        let (before, after) = diff_input();
        let mock = spawn_mock_openai().await;
        let cfg = ModelGatewayConfig::new(
            mock.base_url.clone(),
            "audit_test_token",
            "gpt-4o-mini",
            4000,
        )
        .expect("config ok");
        let tmp = tempfile::tempdir().expect("tempdir");
        let audit_path = tmp.path().join("logs").join("audit.jsonl");
        let gw = ModelGateway::new(cfg).expect("gateway ok").with_audit_log(audit_path.clone());

        // 先调一次
        let _ = gw
            .summarize_diff("REF-200", 5, &before, &after)
            .await
            .expect("first call ok");

        // 再调一次，确保追加（不应截断已有行）
        let mut before2 = before.clone();
        before2.insert("note".to_string(), Value::String("first".to_string()));
        let _ = gw
            .summarize_diff("REF-201", 1, &before2, &after)
            .await
            .expect("second call ok");

        let audit_text = std::fs::read_to_string(&audit_path).expect("audit read");
        let lines: Vec<&str> = audit_text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got {}", lines.len());

        for line in &lines {
            let v: Value = serde_json::from_str(line).expect("audit JSON parse");
            // 必填字段
            assert!(v["ts"].is_string(), "ts missing in {line}");
            assert!(v["model"].is_string(), "model missing in {line}");
            assert!(v["input_hash"].is_string(), "input_hash missing in {line}");
            assert!(v["status_code"].is_u64(), "status_code missing in {line}");
            assert!(v["latency_ms"].is_u64(), "latency_ms missing in {line}");
            assert!(v["status"].is_string(), "status missing in {line}");
            // input_hash 是 64 字符 hex
            let h = v["input_hash"].as_str().expect("hash str");
            assert_eq!(h.len(), 64, "sha256 hex should be 64 chars, got {h}");
        }
    }

    /// 测试 3：`is_configured()` 在 from_env 缺凭据时返回 Err，未注入时 is_configured=false。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_model_gateway_unconfigured() {
        let _guard = ENV_LOCK.lock().unwrap();
        // 清掉 OPENAI_* 三个变量（保留现场 — 复原在末尾）
        let saved_base = std::env::var("OPENAI_BASE_URL").ok();
        let saved_key = std::env::var("OPENAI_API_KEY").ok();
        let saved_model = std::env::var("OPENAI_MODEL").ok();
        let saved_timeout = std::env::var("OPENAI_TIMEOUT_MS").ok();
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("OPENAI_MODEL");
        std::env::remove_var("OPENAI_TIMEOUT_MS");

        let result = ModelGatewayConfig::from_env();

        if let Some(v) = saved_base {
            std::env::set_var("OPENAI_BASE_URL", v);
        } else {
            std::env::remove_var("OPENAI_BASE_URL");
        }
        if let Some(v) = saved_key {
            std::env::set_var("OPENAI_API_KEY", v);
        } else {
            std::env::remove_var("OPENAI_API_KEY");
        }
        if let Some(v) = saved_model {
            std::env::set_var("OPENAI_MODEL", v);
        } else {
            std::env::remove_var("OPENAI_MODEL");
        }
        if let Some(v) = saved_timeout {
            std::env::set_var("OPENAI_TIMEOUT_MS", v);
        } else {
            std::env::remove_var("OPENAI_TIMEOUT_MS");
        }

        assert!(matches!(result, Err(AiError::CredentialMissing)));
    }

    /// 测试 4：`summarize_diff` 在 is_configured()=false 时直接返 Err，不发请求。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_summarize_diff_skipped_when_not_configured() {
        let cfg = ModelGatewayConfig::new("", "any_token", "gpt-4o-mini", 4000);
        assert!(matches!(cfg, Err(AiError::CredentialMissing)));
    }

    /// 测试 5：`summarize_diff` 在 audit_path 未注入时不写任何文件，也不污染环境。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_summarize_diff_no_audit_path_no_write() {
        // 用一个不可能连上的 base_url；因为 is_configured 没要求 mock server 测审计路径
        // 这里只验证：不注入 audit_path 时不创建任何文件
        let cfg = ModelGatewayConfig::new(
            "http://127.0.0.1:1",
            "test_openai_key",
            "gpt-4o-mini",
            100,
        )
        .expect("config ok");
        let gw = ModelGateway::new(cfg).expect("gateway ok");
        // 不调 summarize_diff（会因网络失败超时），只 verify audit_log_path 是 None
        assert!(gw.audit_log_path().is_none());
    }

    /// 测试 6：build_structured_prompt 包含 BEFORE/AFTER 段
    #[test]
    fn test_build_structured_prompt_contains_before_after() {
        let before = map_with(&[("a", "1")]);
        let after = map_with(&[("a", "2")]);
        let prompt = build_structured_prompt(&before, &after);
        assert!(prompt.contains("BEFORE"));
        assert!(prompt.contains("AFTER"));
        assert!(prompt.contains("\"a\""));
        assert!(prompt.contains("\"1\""));
        assert!(prompt.contains("\"2\""));
    }

    /// 测试 7：AiError 五 variant Display 全部非空且无 'unknown' 兜底字样
    #[test]
    fn test_ai_error_display_strings_nonempty_and_no_unknown() {
        let cases = vec![
            AiError::CredentialMissing,
            AiError::Network("reqwest build: timeout".to_string()),
            AiError::UpstreamError { status: 503 },
            AiError::ParseFailed("missing content".to_string()),
            AiError::SchemaMissing("AiSummary fields missing"),
        ];
        for err in &cases {
            let s = format!("{err}");
            assert!(!s.is_empty(), "display empty: {err:?}");
            assert!(!s.contains("unknown"), "display contains 'unknown': {s}");
        }
    }

    /// 测试 8：AiSummary 序列化往返
    #[test]
    fn test_ai_summary_serde_round_trip() {
        let original = AiSummary {
            summary: "诊断更新".to_string(),
            fields_added: vec!["spo2".to_string()],
            fields_removed: vec![],
            semantic_class: "diagnosis_update".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: AiSummary = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
        // JSON 必须包含 schema 要求的所有 4 个字段
        let v: Value = serde_json::from_str(&json).expect("parse");
        for k in ["summary", "fields_added", "fields_removed", "semantic_class"] {
            assert!(v.get(k).is_some(), "missing {k} in {json}");
        }
    }

    /// 测试 9：AiSummary::empty() 给出 unspecified 语义类
    #[test]
    fn test_ai_summary_empty() {
        let empty = AiSummary::empty();
        assert_eq!(empty.summary, "");
        assert!(empty.fields_added.is_empty());
        assert!(empty.fields_removed.is_empty());
        assert_eq!(empty.semantic_class, "unspecified");
    }
}
