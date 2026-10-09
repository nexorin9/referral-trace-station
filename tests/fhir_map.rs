//! 集成测试：从 tests/fixtures/ 真实读 FHIR JSON 文件，验证 map_fhir_to_referral。
//!
//! 对应 Task 12 step 4：
//! - `test_fhir_map_ok`：从 `fhir_001_ok.json` 解析，验证三关键字段齐全；
//! - `test_fhir_missing_field_partial`：从 `http_response_002_missing_actor.json`
//!   解析，验证缺失 actor 字段返回 `MissingField`。

use std::fs;
use std::path::PathBuf;

use referral_trace_station::his::{map_fhir_to_referral, FhirError};

fn fixtures_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by cargo for integration tests");
    PathBuf::from(manifest_dir).join("tests").join("fixtures")
}

#[test]
fn test_fhir_map_ok() {
    let path = fixtures_root().join("fhir_001_ok.json");
    let raw = fs::read_to_string(&path).expect("read fhir_001_ok.json");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse json");
    let rv = map_fhir_to_referral(v).expect("map ok");

    // actor 必填：来自 fixture 的 modification.actor
    assert_eq!(rv.actor, "dr_li@hospital_b");
    // timestamp 必填：来自 modification.timestamp
    assert_eq!(
        rv.modified_at.to_rfc3339(),
        "2026-10-10T08:30:00+00:00",
        "modified_at must round-trip to RFC3339"
    );
    // referral.id 必填：来自 fixture 的 referral.id
    assert_eq!(rv.cl_id, 1, "cl_id is 1 (caller will override via cl_seq)");
    assert_eq!(rv.version_no, 1, "version_no = 1 from modification.version");
    // after 字段填充
    assert_eq!(
        rv.after.get("diagnosis").map(String::as_str),
        Some("ACS suspected"),
        "after.diagnosis must be present"
    );
    assert_eq!(
        rv.after.get("bp_systolic").map(String::as_str),
        Some("162"),
        "after.bp_systolic must be present"
    );
    assert_eq!(
        rv.after.get("spo2").map(String::as_str),
        Some("94"),
        "after.spo2 must be present"
    );
    // before 在 fixture 中是空对象
    assert!(rv.before.is_empty(), "before must be empty");
}

#[test]
fn test_fhir_missing_field_partial() {
    let path = fixtures_root().join("http_response_002_missing_actor.json");
    let raw = fs::read_to_string(&path).expect("read http_response_002_missing_actor.json");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse json");
    let err = map_fhir_to_referral(v).expect_err("actor missing → err");

    // 守住 spec.md §5 「FHIR 缺关键字段 → 拒绝该 referral_id」
    match err {
        FhirError::MissingField("modification.actor") => {}
        other => panic!("expected MissingField(\"modification.actor\"), got {other:?}"),
    }
}
