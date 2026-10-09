//! HIS REST 客户端 — reqwest + .env 凭据 + 5xx 降级。
//!
//! ## 失败模式与降级
//!
//! | 失败 | 错误 variant | 处理 |
//! |------|---------------|------|
//! | `HIS_BASE_URL`/`HIS_TOKEN` 未设置或为空 | [`HisError::CredentialMissing`] | 立即返错；caller 走 drop 入站 |
//! | 网络层错误（DNS / TCP / timeout 4000ms）| [`HisError::Network`] | caller 走 drop 入站 |
//! | HTTP 5xx | [`HisError::Unavailable`] | caller 走 drop 入站 |
//! | HTTP 4xx | [`HisError::ClientError`] | caller 拒绝该 referral_id |
//! | 响应体非 JSON | [`HisError::ParseError`] | caller 拒绝该 referral_id |
//!
//! ## 凭据位（强制走环境变量）
//!
//! - `HIS_BASE_URL`：HIS REST 服务根 URL，例 `http://10.0.0.5:8080/his-api`
//! - `HIS_TOKEN`：Bearer token；**不**硬编码、不连接真实生产 HIS 数据库
//! - `RTS_HIS_TIMEOUT_MS`：默认 4000ms；测试可压短
//!
//! ## 与源产品差异
//!
//! - copper-rs 不依赖外部 REST；本模块对应 HIS FHIR 接入的桥
//! - timeout / 5xx / 4xx 严格分离，方便 caller 决定是否走 drop fallback

use std::env;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::Value;
use thiserror::Error;

/// HIS REST 调用的失败原因。
///
/// 五类按"是否可走 drop 入站"分组：
/// - `CredentialMissing` / `Unavailable`：可立即 fallback 到 drop 入站
/// - `Network`：通常是 timeout 或 DNS 失败，可 fallback 到 drop
/// - `ClientError` / `ParseError`：不可自动 fallback（4xx 是逻辑错误，parity 是协议错误），
///   由 caller 选择重试或告警
///
/// 每条 `Display` 不空且不出现 `"unknown"` 字样。
#[derive(Debug, Error)]
pub enum HisError {
    /// `HIS_BASE_URL` 或 `HIS_TOKEN` 未在环境变量中设置；或设置为空字符串。
    #[error("HIS 凭据缺失：HIS_BASE_URL 或 HIS_TOKEN 未在环境变量中设置")]
    CredentialMissing,

    /// 网络层失败：DNS / TCP / TLS / timeout。
    #[error("HIS 网络错误：{0}")]
    Network(String),

    /// HTTP 5xx（502 / 503 / 504 等）— caller 通常走 drop 入站。
    #[error("HIS 不可达，status={status}")]
    Unavailable { status: u16 },

    /// HTTP 4xx（401 / 403 / 404 等）— caller 通常拒绝该 referral_id 而不重试。
    #[error("HIS 返回 4xx：status={status}, body={body}")]
    ClientError { status: u16, body: String },

    /// 响应体非合法 JSON。
    #[error("HIS 返回非 JSON：{0}")]
    ParseError(String),
}

/// HIS 客户端构造配置。
///
/// 直接构造（测试用）：传 `(base_url, token, timeout_ms)`；
/// 生产用：[`HisClientConfig::from_env`] 从 .env 读。
#[derive(Clone, Debug)]
pub struct HisClientConfig {
    pub base_url: String,
    pub token: String,
    pub timeout_ms: u64,
}

impl HisClientConfig {
    /// 直接构造一个新配置；要求 `base_url` / `token` 非空。
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        timeout_ms: u64,
    ) -> Result<Self, HisError> {
        let base_url = base_url.into();
        let token = token.into();
        if base_url.trim().is_empty() || token.trim().is_empty() {
            return Err(HisError::CredentialMissing);
        }
        Ok(Self {
            base_url,
            token,
            timeout_ms,
        })
    }

    /// 从 .env 读 `HIS_BASE_URL` / `HIS_TOKEN`，缺一即 [`HisError::CredentialMissing`]。
    ///
    /// 失败模式：
    /// - 任一变量未设置 → `CredentialMissing`
    /// - 任一变量为空字符串 → `CredentialMissing`
    /// - `RTS_HIS_TIMEOUT_MS` 未设置或解析失败 → fallback 到 4000ms（不报错）
    pub fn from_env() -> Result<Self, HisError> {
        let base_url =
            env::var("HIS_BASE_URL").map_err(|_| HisError::CredentialMissing)?;
        let token = env::var("HIS_TOKEN").map_err(|_| HisError::CredentialMissing)?;
        let timeout_ms = env::var("RTS_HIS_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(4000);
        Self::new(base_url, token, timeout_ms)
    }
}

/// HIS REST 客户端（reqwest 异步）。
///
/// 持有 [`reqwest::Client`] + [`HisClientConfig`]；
/// 4 秒 timeout 由 `RTS_HIS_TIMEOUT_MS` 控制（默认 4000）。
#[derive(Clone, Debug)]
pub struct HisClient {
    config: HisClientConfig,
    http: Client,
}

impl HisClient {
    /// 用给定配置构造一个 HIS 客户端。
    ///
    /// 错误：[`HisError::Network`] — reqwest client builder 失败（极端情况）。
    pub fn new(config: HisClientConfig) -> Result<Self, HisError> {
        let http = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|e| HisError::Network(format!("reqwest build: {e}")))?;
        Ok(Self { config, http })
    }

    /// `base_url`（仅诊断 / 测试用）。
    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// `GET {base_url}/referrals/{referral_id}/snapshot` 拉快照。
    ///
    /// 错误语义：
    /// - 5xx → [`HisError::Unavailable`]
    /// - 4xx → [`HisError::ClientError`]
    /// - 网络层失败（含 timeout）→ [`HisError::Network`]
    /// - 响应非 JSON → [`HisError::ParseError`]
    pub async fn fetch_snapshot(
        &self,
        referral_id: &str,
    ) -> Result<Value, HisError> {
        let url = format!(
            "{}/referrals/{}/snapshot",
            self.config.base_url.trim_end_matches('/'),
            referral_id
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.config.token)
            .send()
            .await
            .map_err(|e| HisError::Network(e.to_string()))?;

        let status: StatusCode = resp.status();
        if status.is_server_error() {
            return Err(HisError::Unavailable {
                status: status.as_u16(),
            });
        }
        if status.is_client_error() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HisError::ClientError {
                status: status.as_u16(),
                body,
            });
        }
        resp.json::<Value>()
            .await
            .map_err(|e| HisError::ParseError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::his::mock_server::spawn_mock_his;
    use std::sync::Mutex;

    /// 串行化所有改环境变量的测试，避免全局 env 状态被并行测试污染。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 测试 1：mock 200 返回 → HisClient 解析出 JSON，referral.id == "REF-200"；
    /// 同时验证 Authorization Bearer 头被发送（通过 mock 的 request 头检查）。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_his_200_ok() {
        let mock = spawn_mock_his().await;
        let cfg = HisClientConfig::new(mock.base_url.clone(), "test_token_xyz", 4000)
            .expect("config ok");
        let client = HisClient::new(cfg).expect("client ok");

        let v = client
            .fetch_snapshot("200")
            .await
            .expect("200 should succeed");

        assert_eq!(
            v["referral"]["id"], "REF-200",
            "expected referral.id == REF-200, got {v}"
        );
        assert_eq!(v["referral"]["upstream_institution"], "hospital_a");
        assert_eq!(v["modification"]["actor"], "dr_li@hospital_b");
    }

    /// 测试 2：mock 5xx → HisClient 返回 `Unavailable { status: 503 }`。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_his_5xx_fallback() {
        let mock = spawn_mock_his().await;
        let cfg = HisClientConfig::new(mock.base_url.clone(), "test_token", 4000)
            .expect("config ok");
        let client = HisClient::new(cfg).expect("client ok");

        let err = client
            .fetch_snapshot("5xx")
            .await
            .expect_err("5xx should fail");

        match err {
            HisError::Unavailable { status } => {
                assert_eq!(status, 503, "expected 503, got {status}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// 测试 3：mock slow（5s 响应）→ 4000ms timeout 触发 `Network` 错误。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_his_slow_timeout_fallback() {
        let mock = spawn_mock_his().await;
        let cfg =
            HisClientConfig::new(mock.base_url.clone(), "test_token", 4000).expect("config ok");
        let client = HisClient::new(cfg).expect("client ok");

        let start = std::time::Instant::now();
        let err = client
            .fetch_snapshot("slow")
            .await
            .expect_err("slow should timeout");
        let elapsed = start.elapsed();

        match err {
            HisError::Network(_) => {
                // 4 秒 ± 2 秒余量（CI 容错）
                assert!(
                    elapsed >= Duration::from_millis(3500)
                        && elapsed <= Duration::from_millis(8000),
                    "expected timeout ~4s, got {elapsed:?}"
                );
            }
            other => panic!("expected Network timeout, got {other:?}"),
        }
    }

    /// 测试 4：环境变量缺失 → `from_env` 返回 `CredentialMissing`。
    #[test]
    fn test_his_missing_credentials_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        // 保存现场
        let saved_base = std::env::var("HIS_BASE_URL").ok();
        let saved_token = std::env::var("HIS_TOKEN").ok();
        let saved_timeout = std::env::var("RTS_HIS_TIMEOUT_MS").ok();
        // 清场
        std::env::remove_var("HIS_BASE_URL");
        std::env::remove_var("HIS_TOKEN");
        std::env::remove_var("RTS_HIS_TIMEOUT_MS");

        let result = HisClientConfig::from_env();

        // 还原（best-effort）
        if let Some(v) = saved_base {
            std::env::set_var("HIS_BASE_URL", v);
        }
        if let Some(v) = saved_token {
            std::env::set_var("HIS_TOKEN", v);
        }
        if let Some(v) = saved_timeout {
            std::env::set_var("RTS_HIS_TIMEOUT_MS", v);
        }

        match result {
            Err(HisError::CredentialMissing) => {}
            other => panic!("expected CredentialMissing, got {other:?}"),
        }
    }

    /// 测试 5：环境变量存在但为空字符串 → `from_env` 仍返回 `CredentialMissing`。
    #[test]
    fn test_his_empty_credentials_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_base = std::env::var("HIS_BASE_URL").ok();
        let saved_token = std::env::var("HIS_TOKEN").ok();
        std::env::set_var("HIS_BASE_URL", "");
        std::env::set_var("HIS_TOKEN", "   ");

        let result = HisClientConfig::from_env();

        if let Some(v) = saved_base {
            std::env::set_var("HIS_BASE_URL", v);
        } else {
            std::env::remove_var("HIS_BASE_URL");
        }
        if let Some(v) = saved_token {
            std::env::set_var("HIS_TOKEN", v);
        } else {
            std::env::remove_var("HIS_TOKEN");
        }

        match result {
            Err(HisError::CredentialMissing) => {}
            other => panic!("expected CredentialMissing, got {other:?}"),
        }
    }

    /// 测试 6：mock 404 → `ClientError { status: 404 }`（不可 fallback）。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_his_404_returns_client_error() {
        let mock = spawn_mock_his().await;
        let cfg =
            HisClientConfig::new(mock.base_url.clone(), "test_token", 4000).expect("config ok");
        let client = HisClient::new(cfg).expect("client ok");

        let err = client
            .fetch_snapshot("notfound")
            .await
            .expect_err("notfound should fail");

        match err {
            HisError::ClientError { status, body: _ } => {
                assert_eq!(status, 404, "expected 404, got {status}");
            }
            other => panic!("expected ClientError, got {other:?}"),
        }
    }
}
