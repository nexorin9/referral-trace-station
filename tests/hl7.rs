//! HL7 ORU 简版解析集成测试（含 fixtures 加载）
//!
//! 对应 Task 8 的三条验收：
//! - test_oru_parse_ok: 加载 tests/fixtures/hl7_oru_ok.txt 解析成功
//! - test_oru_malformed_fails: 加载 tests/fixtures/hl7_oru_malformed.txt 报
//!   MissingSegment("OBX") 错误
//! - test_oru_unparsed_fallback: 把 malformed fixture mv 到 unparsed/ 并打
//!   .unparsed tag

use std::fs;
use std::io::Write;
use std::path::Path;

use referral_trace_station::hl7::{fallback_to_unparsed, parse_msh_pid_obr_obx, OruError};

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

#[test]
fn test_oru_parse_ok() {
    let path = fixtures_dir().join("hl7_oru_ok.txt");
    let buf = fs::read(&path).expect("read ok fixture");
    let m = parse_msh_pid_obr_obx(&buf).expect("should parse ok fixture");
    assert_eq!(m.msh_sending_app, "HIS_A");
    assert_eq!(m.pid_patient_id, "P123^^^HIS_A");
    assert_eq!(m.obr_report_id, "RPT-200");
    assert_eq!(m.obx_observation_value, "5.6");
    assert_eq!(m.obx_observation_time, "20261010093000");
}

#[test]
fn test_oru_malformed_fails() {
    let path = fixtures_dir().join("hl7_oru_malformed.txt");
    let buf = fs::read(&path).expect("read malformed fixture");
    let err = parse_msh_pid_obr_obx(&buf).unwrap_err();
    // malformed fixture 故意只有 MSH/PID/OBR 三段，缺 OBX
    assert_eq!(err, OruError::MissingSegment("OBX"));
}

#[test]
fn test_oru_unparsed_fallback() {
    use std::collections::HashSet;

    // 把 malformed 复制到 tempdir，避免污染 fixtures 目录
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("hl7_oru_malformed_copy.txt");
    let original = fixtures_dir().join("hl7_oru_malformed.txt");
    let body = fs::read(&original).expect("read");
    let mut f = fs::File::create(&src).expect("create");
    f.write_all(&body).expect("write");
    assert!(src.exists());

    let buf = fs::read(&src).expect("read copy");
    let err = parse_msh_pid_obr_obx(&buf).unwrap_err();
    assert_eq!(err, OruError::MissingSegment("OBX"));

    let dst = fallback_to_unparsed(&src).expect("fallback ok");
    assert!(!src.exists());
    assert!(dst.exists());
    assert!(dst.starts_with(dir.path().join("unparsed")));

    let dst_name = dst.file_name().unwrap().to_string_lossy().to_string();
    let mut names = HashSet::new();
    names.insert(dst_name);
    // 必须包含 ".unparsed.hl7"
    let any_with_tag = names.iter().any(|n| n.contains(".unparsed.hl7"));
    assert!(any_with_tag, "at least one file must carry .unparsed tag");
}
