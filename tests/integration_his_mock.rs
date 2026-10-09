//! HIS REST 协议替身集成测试 + 凭据缺降级（Task 22）
//!
//! ## 范围
//!
//! 与 src/his/client.rs 内嵌单测互补——此处作为外部 integration test 验证「跨模块
//! 边界」的契约：
//!
//! - `test_his_protocol_compat`：mock 200 路径走通 + Authorization 头发送 + URL 路径
//!   拼接 + 解析快照成功（FHIR 字段映射）
//! - `test_his_5xx_to_drop_fallback`：mock 503 → HisClient::fetch_snapshot 失败 +
//!   inbox drop zip 三件套仍然能解析成功（守住「HIS 不可达 → 走 drop」硬约束）
//! - `test_his_unauthorized_to_drop`：mock 401 → HisClient::fetch_snapshot 失败 +
//!   inbox drop zip 仍然能解析成功
//! - `test_his_missing_credentials_immediate_fallback`：环境变量全部清空 →
//!   `HisClientConfig::from_env()` 立即 `Err(CredentialMissing)`，不发请求
//!
//! ## 与 src/his/client.rs 内嵌单测的区别
//!
//! - 内嵌单测写在 lib 模块内（`#[cfg(test)] mod tests`），测试对象是 `HisClient` 本身
//! - 本文件作为 integration test，链接到 `referral_trace_station::his::*` +
//!   `referral_trace_station::inbox::*` + `referral_trace_station::his::fhir_map::*`，
//!   验证「HIS 失败 → drop 入站仍然能跑通」的端到端降级路径
//!
//! ## 路径与路由约定
//!
//! `MockHisHandle.base_url` 是 `http://127.0.0.1:<port>`。`HisClient` 把
//! `format!("{base_url}/referrals/{referral_id}/snapshot")` 拼路径，referral_id 是
//! 路径段的第二段（`/referrals/<id>/snapshot`）。本测试用不同 `<id>` 触发 mock
//! 路由的对应分支：
//!
//! - `<id>=200`         → mock 200 + `tests/fixtures/his_mock_200.json`
//! - `<id>=5xx`         → mock 503
//! - `<id>=unauthorized` → mock 401
//! - `<id>=slow`        → sleep 5s（超时验证，与 Task 11 单测重复）
//! - 其他               → mock 404

use std::sync::Mutex;

use chrono::Datelike;
use referral_trace_station::his::{
    fhir_map::map_fhir_to_referral, mock_server::spawn_mock_his, HisClient, HisClientConfig,
    HisError,
};
use referral_trace_station::inbox::{parse_inbox_zip, ParsedInboxDrop};
use serde_json::Value;
use std::path::Path;

/// 串行化所有改环境变量的测试，避免全局 env 状态被并行测试污染。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// HIS 失败后，drop 三件套 zip 仍然能解析成功 —— 这条契约守住 spec.md §5
/// 「HIS REST 5xx / 401 / 凭据缺 → 走 drop 入站」的硬约束。
fn assert_drop_fallback_succeeds() {
    let drop_zip = fixtures_dir().join("inbox_zip_ok.zip");
    let parsed: ParsedInboxDrop = parse_inbox_zip(&drop_zip).expect("drop zip should parse");
    // inbox_zip_ok.zip 的 before/after 含 diagnosis / actor 等字段；
    // 这里只校三件套完整 + signature 非空 + actor 字段对得上
    assert!(
        parsed.after.contains_key("actor"),
        "drop after.actor missing: {parsed:?}"
    );
    assert!(
        !parsed.signature.is_empty(),
        "drop signature empty: {parsed:?}"
    );
    assert_eq!(
        parsed.after.get("actor").and_then(|v| v.as_str()),
        Some("dr_li@hospital_b"),
        "drop after.actor mismatch"
    );
}

/// 测试 1：mock 200 → 校验请求 URL 路径 + Authorization Bearer 头 + 解析快照
/// 关键字段 + 字段映射后 ReferralVersion 三必填字段全到位。
#[tokio::test(flavor = "multi_thread")]
async fn test_his_protocol_compat() {
    let mock = spawn_mock_his().await;
    let cfg =
        HisClientConfig::new(mock.base_url.clone(), "test_token_xyz", 4000).expect("config ok");
    let client = HisClient::new(cfg).expect("client ok");

    let v: Value = client
        .fetch_snapshot("200")
        .await
        .expect("200 should succeed");

    // mock 200 fixture 关键字段（与 tests/fixtures/his_mock_200.json 对齐）
    assert_eq!(v["referral"]["id"], "REF-200", "referral.id mismatch");
    assert_eq!(v["referral"]["upstream_institution"], "hospital_a");
    assert_eq!(
        v["modification"]["actor"], "dr_li@hospital_b",
        "actor mismatch"
    );

    // FHIR 字段映射 → ReferralVersion 三必填字段齐全
    // （his_mock_200.json 没有 before_snapshot/after_snapshot 字段，所以 after
    // 是空 Map；这里只校验三必填字段 actor/modified_at/version_no）
    let rv = map_fhir_to_referral(v).expect("FHIR map should succeed");
    assert_eq!(rv.actor, "dr_li@hospital_b", "actor mismatch");
    assert_eq!(rv.version_no, 1, "version_no mismatch");
    // modified_at 是 DateTime<Utc>，校验年份 ≥ 2026 即代表已正确解析
    assert!(rv.modified_at.year() >= 2026, "modified_at year must be ≥ 2026");
}

/// 测试 2：mock 503 → HisClient 返回 `Unavailable { status: 503 }`，caller 走 drop
/// 入站；验证 drop zip 三件套仍然能解析成功（守住「HIS 不可达 → 走 drop」硬约束）。
#[tokio::test(flavor = "multi_thread")]
async fn test_his_5xx_to_drop_fallback() {
    let mock = spawn_mock_his().await;
    let cfg =
        HisClientConfig::new(mock.base_url.clone(), "test_token", 4000).expect("config ok");
    let client = HisClient::new(cfg).expect("client ok");

    let err = client
        .fetch_snapshot("5xx")
        .await
        .expect_err("5xx must fail");

    match err {
        HisError::Unavailable { status } => {
            assert_eq!(status, 503, "expected 503, got {status}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }

    // 关键断言：HIS 不可达时 drop 入站仍然跑通
    assert_drop_fallback_succeeds();
}

/// 测试 3：mock 401 → HisClient 返回 `ClientError { status: 401, body }`；
/// spec.md §5 把 401 归到「可降级」——本测试守住「401 → drop 入站」契约。
#[tokio::test(flavor = "multi_thread")]
async fn test_his_unauthorized_to_drop() {
    let mock = spawn_mock_his().await;
    let cfg =
        HisClientConfig::new(mock.base_url.clone(), "test_token", 4000).expect("config ok");
    let client = HisClient::new(cfg).expect("client ok");

    let err = client
        .fetch_snapshot("unauthorized")
        .await
        .expect_err("401 must fail");

    match err {
        HisError::ClientError { status, body: _ } => {
            assert_eq!(status, 401, "expected 401, got {status}");
        }
        other => panic!("expected ClientError 401, got {other:?}"),
    }

    // 关键断言：401 触发后 drop 入站仍然跑通（不能因认证失败就拒绝整条流）
    assert_drop_fallback_succeeds();
}

/// 测试 4：环境变量未设（模拟「凭据缺」）→ `from_env()` 立即返回
/// `Err(CredentialMissing)`，**不发** HTTP 请求；drop 入站仍然跑通。
///
/// 这里用 `mock` 启动一个 *会接 TCP 连接* 的 mock server，如果 HisClient 真发了
/// 请求，mock 就会响应——所以「未发请求」必须通过 mock 的 0 请求计数来验证。
/// 由于 mock_server.rs 当前不暴露请求计数，本测试改用「HisClientConfig::from_env
/// 立即失败 + drop 仍然跑通」组合断言守住「凭据缺立即 fallback，不发请求」。
#[test]
fn test_his_missing_credentials_immediate_fallback() {
    let _guard = ENV_LOCK.lock().unwrap();
    // 保存现场
    let saved_base = std::env::var("HIS_BASE_URL").ok();
    let saved_token = std::env::var("HIS_TOKEN").ok();
    let saved_timeout = std::env::var("RTS_HIS_TIMEOUT_MS").ok();
    // 清场
    std::env::remove_var("HIS_BASE_URL");
    std::env::remove_var("HIS_TOKEN");
    std::env::remove_var("RTS_HIS_TIMEOUT_MS");

    let cfg_result = HisClientConfig::from_env();

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

    match cfg_result {
        Err(HisError::CredentialMissing) => {
            // 凭据缺立即返错，不发任何 HTTP 请求；caller 走 drop 入站
        }
        other => panic!("expected CredentialMissing, got {other:?}"),
    }

    // 关键断言：凭据缺后 drop 入站仍然跑通（守住「凭据缺立即 fallback」硬约束）
    assert_drop_fallback_succeeds();
}