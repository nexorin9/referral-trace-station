//! `FHIR` JSON → [`ReferralVersion`] 字段映射 + 字段级 diff。
//!
//! ## 与 spec.md / plan.md 的对应
//!
//! - spec.md §3 最小数据格式：本模块仅取 FHIR 关键三字段
//!   (`referral.id` / `modification.timestamp` / `modification.actor`)，
//!   不解析全部 FHIR；
//! - plan.md §2 集成面：HIS REST 拉到 FHIR JSON 后由本模块做字段映射；
//! - spec.md §5 失败模式：本模块 `FhirError::MissingField` 守住"缺关键字段
//!   → 拒绝该 referral_id 而不静默推进"。
//!
//! ## 与源产品差异
//!
//! - `copper-rs` 不依赖外部 FHIR；本模块是「HIS FHIR → 铜版 monotonic 流」的桥；
//! - `compute_diff` 使用 `similar` crate 做字段级 diff（spec.md §6 字段级 diff
//!   载体），与 [`crate::model::referral_version::ReferralVersion::field_diff`]
//!   双轨：`field_diff` 是 ReferralVersion 内嵌方法，`compute_diff` 是
//!   `Map → Map<field, DiffEntry>` 的工具函数，二者结果等价。
//!
//! ## 公开 API
//!
//! - [`map_fhir_to_referral`]：把 FHIR 快照 JSON 解析为 `ReferralVersion`；
//! - [`compute_diff`]：用 `similar` 计算字段级 diff，输出 `Add` / `Remove` /
//!   `Replace` 三类。
//! - [`flatten_value_map`]：把 `Value::Object` 拍扁为 `BTreeMap<String, String>`，
//!   供 `before` / `after` 字段填充。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use similar::TextDiff;
use thiserror::Error;

use crate::model::continuity_record::ContinuityKind;
use crate::model::referral_version::ReferralVersion;

/// FHIR JSON 解析失败原因。
///
/// 每条 `Display` 不空且不出现 `"unknown"` 字样。
#[derive(Debug, Error)]
pub enum FhirError {
    /// 必填字段缺失或字段类型非法（非 str / 非 ISO8601 字符串）。
    #[error("FHIR 缺关键字段：{0}")]
    MissingField(&'static str),

    /// 顶层不是合法 JSON 对象。
    #[error("FHIR 顶层不是对象：{0}")]
    NotAnObject(String),

    /// 时间戳不是合法 ISO8601 / RFC3339。
    #[error("FHIR 时间戳非法：field={field}, value={value}")]
    BadTimestamp {
        field: &'static str,
        value: String,
    },
}

/// 字段级 diff 一条。
///
/// - `Add(after)`：字段只出现在 `after`；
/// - `Remove(before)`：字段只出现在 `before`；
/// - `Replace { before, after, unified_diff }`：字段都出现但值不同；
///   `unified_diff` 由 `similar::TextDiff` 计算（多行值走行级 diff，
///   单行值退化为 `"- {before}\n+ {after}\n"`）。
#[derive(Clone, Debug, PartialEq)]
pub enum DiffEntry {
    Add(String),
    Remove(String),
    Replace {
        before: String,
        after: String,
        unified_diff: String,
    },
}

impl DiffEntry {
    /// 该 entry 的 op 标签（供 UI / log 用）。
    pub fn op_label(&self) -> &'static str {
        match self {
            DiffEntry::Add(_) => "add",
            DiffEntry::Remove(_) => "remove",
            DiffEntry::Replace { .. } => "replace",
        }
    }

    /// 取出 before 端的字段值（Add 返回空串）。
    pub fn before_value(&self) -> &str {
        match self {
            DiffEntry::Add(_) => "",
            DiffEntry::Remove(b) => b.as_str(),
            DiffEntry::Replace { before, .. } => before.as_str(),
        }
    }

    /// 取出 after 端的字段值（Remove 返回空串）。
    pub fn after_value(&self) -> &str {
        match self {
            DiffEntry::Add(a) => a.as_str(),
            DiffEntry::Remove(_) => "",
            DiffEntry::Replace { after, .. } => after.as_str(),
        }
    }
}

/// 把 FHIR 快照 JSON 映射为 [`ReferralVersion`]。
///
/// **必填字段**（缺一即返回 [`FhirError::MissingField`]）：
/// - `referral.id`（str，非空）
/// - `modification.timestamp`（RFC3339 字符串）
/// - `modification.actor`（str，非空）
///
/// **可选字段**：
/// - `modification.version`（整数；缺省视作 1）
/// - `before_snapshot` / `after_snapshot` 或 `before` / `after`
///   （对象；缺省视作空 `BTreeMap`）
///
/// 返回的 `ReferralVersion`：
/// - `cl_id = 1`（caller 用 [`crate::copperlist::cl_seq`] 的
///   `record_alloc_next` 覆盖；本模块只做字段映射，不持有计数器）；
/// - `version_no` 优先取 `modification.version`，缺省 1；
/// - `continuity = ContinuityKind::Manifest`（"快照" 语义，不臆造 RecoveryPoint /
///   Gap / Finished，由 [`crate::continuity::four_state`] 守 contract）；
/// - `payload_sha256` 留 `"0" * 64` 占位（caller 落 sha256 链时由
///   [`crate::sha256_chain::chain`] 重算）。
pub fn map_fhir_to_referral(json: Value) -> Result<ReferralVersion, FhirError> {
    let obj = json
        .as_object()
        .ok_or_else(|| FhirError::NotAnObject("expected JSON object at top level".to_string()))?;

    let referral = obj
        .get("referral")
        .and_then(|v| v.as_object())
        .ok_or(FhirError::NotAnObject(
            "missing 'referral' object".to_string(),
        ))?;

    // `referral.id` 必填且非空；不在 ReferralVersion 落点（由 `cl_id` + filename
    // 锚定），但仍做存在性校验 → 拒绝缺关键字段的 referral_id。
    let _: String = referral
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(FhirError::MissingField("referral.id"))?
        .to_string();

    let modification = obj
        .get("modification")
        .and_then(|v| v.as_object())
        .ok_or(FhirError::NotAnObject(
            "missing 'modification' object".to_string(),
        ))?;

    let timestamp_str = modification
        .get("timestamp")
        .and_then(|v| v.as_str())
        .ok_or(FhirError::MissingField("modification.timestamp"))?;

    let modified_at = parse_iso8601("modification.timestamp", timestamp_str)?;

    let actor = modification
        .get("actor")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(FhirError::MissingField("modification.actor"))?
        .to_string();

    let version_no = modification
        .get("version")
        .and_then(|v| v.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32)
        .unwrap_or(1);

    // before / after：优先 before_snapshot/after_snapshot，再退到 before/after，
    // 再退到空 Map。
    let before = obj
        .get("before_snapshot")
        .or_else(|| obj.get("before"))
        .map(flatten_value_map)
        .unwrap_or_default();
    let after = obj
        .get("after_snapshot")
        .or_else(|| obj.get("after"))
        .map(flatten_value_map)
        .unwrap_or_default();

    Ok(ReferralVersion {
        cl_id: 1,
        version_no,
        actor,
        modified_at,
        before,
        after,
        continuity: ContinuityKind::Manifest,
        payload_sha256: "0".repeat(64),
    })
}

fn parse_iso8601(field: &'static str, raw: &str) -> Result<DateTime<Utc>, FhirError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| FhirError::BadTimestamp {
            field,
            value: raw.to_string(),
        })
}

/// 把 `Value::Object` 拍扁为 `BTreeMap<String, String>`：
/// - 标量（string/number/bool）→ `field → string repr`；
/// - null → 字段不写入；
/// - 嵌套对象 / 数组 → `field → json_serialize(value)`（紧凑无空格），
///   保证字段值跨环境可比。
///
/// 不递归展平嵌套 key：spec.md §3 要求 `Map<str, str>`，嵌套对象必须被
/// 序列化为字符串。
pub fn flatten_value_map(v: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(obj) = v.as_object() else {
        return out;
    };
    for (k, val) in obj {
        if val.is_null() {
            continue;
        }
        let s = match val {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            // 嵌套对象 / 数组：紧凑序列化，与 canonical_json 行为兼容
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        out.insert(k.clone(), s);
    }
    out
}

/// 计算字段级 diff（基于 `similar` crate）。
///
/// 输出 `BTreeMap<field, DiffEntry>`：
/// - `Add(after)`：`field` 只在 `after` 中；
/// - `Remove(before)`：`field` 只在 `before` 中；
/// - `Replace { before, after, unified_diff }`：两端都有但值不同，
///   `unified_diff` 由 `similar::TextDiff::from_lines` 生成的 unified diff 文本
///   （单行值退化为 `"- {before}\n+ {after}\n"`）。
pub fn compute_diff(before: &Map<String, Value>, after: &Map<String, Value>) -> BTreeMap<String, DiffEntry> {
    let mut out = BTreeMap::new();

    // 替换 / 新增：after 有
    for (k, new_v) in after {
        match before.get(k) {
            None => {
                out.insert(
                    k.clone(),
                    DiffEntry::Add(serialize_value(new_v)),
                );
            }
            Some(old_v) => {
                let old_s = serialize_value(old_v);
                let new_s = serialize_value(new_v);
                if old_s != new_s {
                    let diff = TextDiff::from_lines(&old_s, &new_s)
                        .unified_diff()
                        .header("before", "after")
                        .to_string();
                    out.insert(
                        k.clone(),
                        DiffEntry::Replace {
                            before: old_s,
                            after: new_s,
                            unified_diff: diff,
                        },
                    );
                }
                // 值相等：不输出
            }
        }
    }

    // 删除：before 有，after 无
    for (k, old_v) in before {
        if !after.contains_key(k) {
            out.insert(
                k.clone(),
                DiffEntry::Remove(serialize_value(old_v)),
            );
        }
    }

    out
}

fn serialize_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        // 嵌套结构：紧凑 JSON（与 flatten_value_map 行为一致）
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_fixture() -> Value {
        json!({
            "referral": {
                "id": "REF-FHIR-001",
                "patient_ref": "P-001^^^HIS_A",
                "upstream_institution": "hospital_a",
                "diagnosis_initial": "chest pain"
            },
            "modification": {
                "version": 1,
                "timestamp": "2026-10-10T08:30:00Z",
                "actor": "dr_li@hospital_b"
            },
            "after": {
                "diagnosis": "ACS suspected",
                "bp_systolic": "162"
            },
            "before": {}
        })
    }

    fn parse(v: &str) -> Value {
        serde_json::from_str(v).expect("valid json fixture")
    }

    #[test]
    fn fhir_map_ok_extracts_three_critical_fields() {
        let v = ok_fixture();
        let rv = map_fhir_to_referral(v).expect("map ok");
        assert_eq!(rv.actor, "dr_li@hospital_b");
        assert_eq!(rv.cl_id, 1);
        assert_eq!(rv.version_no, 1);
        assert_eq!(
            rv.modified_at,
            DateTime::parse_from_rfc3339("2026-10-10T08:30:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(rv.after.get("diagnosis").map(String::as_str), Some("ACS suspected"));
        assert_eq!(rv.after.get("bp_systolic").map(String::as_str), Some("162"));
        assert!(rv.before.is_empty());
        assert_eq!(rv.continuity, ContinuityKind::Manifest);
    }

    #[test]
    fn fhir_missing_actor_returns_err_missing_field() {
        let raw = r#"{
            "referral": { "id": "REF-FHIR-002" },
            "modification": {
                "version": 1,
                "timestamp": "2026-10-10T08:30:00Z"
            }
        }"#;
        let v = parse(raw);
        let err = map_fhir_to_referral(v).expect_err("actor missing → err");
        match err {
            FhirError::MissingField("modification.actor") => {}
            other => panic!("expected MissingField(modification.actor), got {other:?}"),
        }
    }

    #[test]
    fn fhir_missing_referral_id_returns_err_missing_field() {
        let raw = r#"{
            "referral": { "patient_ref": "P-002" },
            "modification": {
                "timestamp": "2026-10-10T08:30:00Z",
                "actor": "dr_wang@hospital_b"
            }
        }"#;
        let v = parse(raw);
        let err = map_fhir_to_referral(v).expect_err("referral.id missing → err");
        match err {
            FhirError::MissingField("referral.id") => {}
            other => panic!("expected MissingField(referral.id), got {other:?}"),
        }
    }

    #[test]
    fn fhir_missing_timestamp_returns_err_missing_field() {
        let raw = r#"{
            "referral": { "id": "REF-FHIR-003" },
            "modification": { "actor": "dr_li@hospital_b" }
        }"#;
        let v = parse(raw);
        let err = map_fhir_to_referral(v).expect_err("timestamp missing → err");
        match err {
            FhirError::MissingField("modification.timestamp") => {}
            other => panic!("expected MissingField(modification.timestamp), got {other:?}"),
        }
    }

    #[test]
    fn fhir_bad_timestamp_returns_err_bad_timestamp() {
        let raw = r#"{
            "referral": { "id": "REF-FHIR-004" },
            "modification": {
                "timestamp": "not-an-iso8601",
                "actor": "dr_li@hospital_b"
            }
        }"#;
        let v = parse(raw);
        let err = map_fhir_to_referral(v).expect_err("bad timestamp → err");
        match err {
            FhirError::BadTimestamp { field, value } => {
                assert_eq!(field, "modification.timestamp");
                assert_eq!(value, "not-an-iso8601");
            }
            other => panic!("expected BadTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn fhir_top_level_not_object_returns_err_not_an_object() {
        let v = json!("a string at top level");
        let err = map_fhir_to_referral(v).expect_err("top-level not object → err");
        match err {
            FhirError::NotAnObject(_) => {}
            other => panic!("expected NotAnObject, got {other:?}"),
        }
    }

    #[test]
    fn fhir_empty_referral_id_returns_err_missing_field() {
        let raw = r#"{
            "referral": { "id": "   " },
            "modification": {
                "timestamp": "2026-10-10T08:30:00Z",
                "actor": "dr_li@hospital_b"
            }
        }"#;
        let v = parse(raw);
        let err = map_fhir_to_referral(v).expect_err("empty id → err");
        match err {
            FhirError::MissingField("referral.id") => {}
            other => panic!("expected MissingField(referral.id), got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_three_ops_from_two_maps() {
        let before_raw = r#"{
            "diagnosis": "chest pain",
            "bp_systolic": "120",
            "old_field": "to be removed"
        }"#;
        let after_raw = r#"{
            "diagnosis": "ACS suspected",
            "bp_systolic": "120",
            "new_field": "to be added"
        }"#;
        let before: Map<String, Value> = parse(before_raw)
            .as_object()
            .cloned()
            .expect("object");
        let after: Map<String, Value> = parse(after_raw)
            .as_object()
            .cloned()
            .expect("object");

        let diff = compute_diff(&before, &after);
        assert_eq!(diff.len(), 3);

        // diagnosis: Replace
        match diff.get("diagnosis").expect("diagnosis present") {
            DiffEntry::Replace {
                before: b,
                after: a,
                unified_diff: u,
            } => {
                assert_eq!(b, "chest pain");
                assert_eq!(a, "ACS suspected");
                assert!(u.contains("-chest pain"), "unified diff should show old: {u}");
                assert!(u.contains("+ACS suspected"), "unified diff should show new: {u}");
            }
            other => panic!("expected Replace, got {other:?}"),
        }

        // bp_systolic: 值相等，不输出
        assert!(!diff.contains_key("bp_systolic"));

        // new_field: Add
        match diff.get("new_field").expect("new_field present") {
            DiffEntry::Add(v) => assert_eq!(v, "to be added"),
            other => panic!("expected Add, got {other:?}"),
        }

        // old_field: Remove
        match diff.get("old_field").expect("old_field present") {
            DiffEntry::Remove(v) => assert_eq!(v, "to be removed"),
            other => panic!("expected Remove, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_multiline_value_uses_similar_lines() {
        let before_raw = r#"{"note":"line one\nline two"}"#;
        let after_raw = r#"{"note":"line one\nline three"}"#;
        let before: Map<String, Value> = parse(before_raw)
            .as_object()
            .cloned()
            .expect("object");
        let after: Map<String, Value> = parse(after_raw)
            .as_object()
            .cloned()
            .expect("object");

        let diff = compute_diff(&before, &after);
        match diff.get("note").expect("note present") {
            DiffEntry::Replace {
                unified_diff, ..
            } => {
                assert!(
                    unified_diff.contains("-line two"),
                    "expected -line two in diff: {unified_diff}"
                );
                assert!(
                    unified_diff.contains("+line three"),
                    "expected +line three in diff: {unified_diff}"
                );
            }
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_empty_inputs_returns_empty() {
        let empty = Map::new();
        let diff = compute_diff(&empty, &empty);
        assert!(diff.is_empty());
    }

    #[test]
    fn flatten_value_map_skips_null_values() {
        let raw = r#"{"a": "x", "b": null, "c": 42, "d": true}"#;
        let v = parse(raw);
        let m = flatten_value_map(&v);
        assert_eq!(m.get("a").map(String::as_str), Some("x"));
        assert!(!m.contains_key("b"), "null fields must be skipped");
        assert_eq!(m.get("c").map(String::as_str), Some("42"));
        assert_eq!(m.get("d").map(String::as_str), Some("true"));
    }

    #[test]
    fn flatten_value_map_handles_non_object() {
        // 非对象输入 → 空 Map（不应 panic）
        let m = flatten_value_map(&json!("a string"));
        assert!(m.is_empty());
        let m = flatten_value_map(&json!(42));
        assert!(m.is_empty());
    }

    #[test]
    fn diff_entry_op_label_and_value_helpers() {
        let add = DiffEntry::Add("v".to_string());
        assert_eq!(add.op_label(), "add");
        assert_eq!(add.before_value(), "");
        assert_eq!(add.after_value(), "v");

        let rm = DiffEntry::Remove("v".to_string());
        assert_eq!(rm.op_label(), "remove");
        assert_eq!(rm.before_value(), "v");
        assert_eq!(rm.after_value(), "");

        let rp = DiffEntry::Replace {
            before: "a".to_string(),
            after: "b".to_string(),
            unified_diff: "-a\n+b\n".to_string(),
        };
        assert_eq!(rp.op_label(), "replace");
        assert_eq!(rp.before_value(), "a");
        assert_eq!(rp.after_value(), "b");
    }

    #[test]
    fn fhir_error_display_nonempty_and_no_unknown() {
        let e1 = FhirError::MissingField("referral.id");
        let e2 = FhirError::NotAnObject("test".to_string());
        let e3 = FhirError::BadTimestamp {
            field: "modification.timestamp",
            value: "bad".to_string(),
        };
        for e in [&e1, &e2, &e3] {
            let s = format!("{e}");
            assert!(!s.is_empty(), "display must be non-empty: {e:?}");
            assert!(
                !s.to_lowercase().contains("unknown"),
                "display must not contain 'unknown': {s}"
            );
        }
        // 互不相同（保证错误可识别）
        let s1 = format!("{e1}");
        let s2 = format!("{e2}");
        let s3 = format!("{e3}");
        assert_ne!(s1, s2);
        assert_ne!(s2, s3);
        assert_ne!(s1, s3);
    }

    #[test]
    fn fhir_map_default_version_when_missing() {
        let raw = r#"{
            "referral": { "id": "REF-FHIR-005" },
            "modification": {
                "timestamp": "2026-10-10T08:30:00Z",
                "actor": "dr_li@hospital_b"
            }
        }"#;
        let v = parse(raw);
        let rv = map_fhir_to_referral(v).expect("map ok");
        assert_eq!(rv.version_no, 1, "default version_no = 1 when missing");
    }

    #[test]
    fn fhir_map_accepts_before_snapshot_after_snapshot_keys() {
        let raw = r#"{
            "referral": { "id": "REF-FHIR-006" },
            "modification": {
                "timestamp": "2026-10-10T08:30:00Z",
                "actor": "dr_li@hospital_b"
            },
            "before_snapshot": { "diagnosis": "chest pain" },
            "after_snapshot": { "diagnosis": "ACS suspected" }
        }"#;
        let v = parse(raw);
        let rv = map_fhir_to_referral(v).expect("map ok");
        assert_eq!(
            rv.before.get("diagnosis").map(String::as_str),
            Some("chest pain")
        );
        assert_eq!(
            rv.after.get("diagnosis").map(String::as_str),
            Some("ACS suspected")
        );
    }
}
