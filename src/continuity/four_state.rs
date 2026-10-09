//! 四态适配 + 校验契约实现。
//!
//! 直接镜像源 `copper-rs/core/cu29_runtime/src/continuity.rs` 的语义，
//! 用本产品的字段级 `ContinuityKind` 替代 `record: Vec<u8>` 字节载荷。
//!
//! ## 三条 contract（spec.md §5 / research.md 核心差异化模块）
//!
//! 1. **Gap 字段完整性**：`Gap{first_id, last_id, reason}` 三字段必须齐备（`validate_continuity_record`）
//! 2. **RecoveryPoint 不臆造尾巴**：replay 跨过 Gap 必须有匹配的 keyframe（`validate_replay_continuity`）
//! 3. **Finished 终态**：在该 referral 流已 Finished 后不可再 append 新 entry（`ensure_no_append_after_finished`）

use crate::model::continuity_record::{ContinuityKind, ContinuityRecord};
use std::fmt;

/// 四态适配校验失败原因。
///
/// 区分点：
/// - `Gap*` 字段缺失：仅在反序列化路径能发生（构造路径已被 Rust 类型系统阻止），
///   但 deserializer 允许字段可选 + 默认值，因此缺字段时仍需要明确报错。
/// - `RecoveryPointTailFabrication` 与源 `validate_replay_continuity` 行为对齐：
///   `actual != expected && keyframe != Some(actual)` 即视为「在 RecoveryPoint 后臆造尾巴」。
/// - `FinishedTerminal`：spec §5 第 3 条 contract 的业务级拒绝。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContinuityError {
    /// Gap{} 缺 first_id 字段（JSON 反序列化路径上字段缺失或为 null）。
    GapMissingFirstId,
    /// Gap{} 缺 last_id 字段。
    GapMissingLastId,
    /// Gap{} 缺 reason 字段。
    GapMissingReason,
    /// Gap first_id > last_id（区间非法）。
    GapInvalidRange { first_id: u64, last_id: u64 },
    /// replay 在未匹配的 keyframe 处跨过 Gap 边界，违反「不臆造尾巴」。
    RecoveryPointTailFabrication {
        expected_cl_id: u64,
        actual_cl_id: u64,
        keyframe: Option<u64>,
    },
    /// Finished{} 后继续追加新 entry，违反「流已结束」语义。
    FinishedTerminal {
        finished_next_id: u64,
        attempted_cl_id: u64,
    },
    /// RecoveryPoint{} 自身 cl_id 为 0（哨兵值）：跳过 keyframe 边界直接接受是禁忌。
    RecoveryPointZeroCopperlistId,
    /// Finished{} next_copperlist_id 为 0：哨兵终态不允许。
    FinishedZeroNextId,
}

impl fmt::Display for ContinuityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContinuityError::GapMissingFirstId => {
                write!(f, "Gap record is missing required field 'first_id'")
            }
            ContinuityError::GapMissingLastId => {
                write!(f, "Gap record is missing required field 'last_id'")
            }
            ContinuityError::GapMissingReason => {
                write!(f, "Gap record is missing required field 'reason'")
            }
            ContinuityError::GapInvalidRange { first_id, last_id } => write!(
                f,
                "Gap range invalid: first_id={first_id} > last_id={last_id}"
            ),
            ContinuityError::RecoveryPointTailFabrication {
                expected_cl_id,
                actual_cl_id,
                keyframe,
            } => write!(
                f,
                "RecoveryPoint tail fabrication rejected: expected_cl_id={expected_cl_id} actual_cl_id={actual_cl_id} keyframe={keyframe:?}"
            ),
            ContinuityError::FinishedTerminal {
                finished_next_id,
                attempted_cl_id,
            } => write!(
                f,
                "stream already Finished at next_copperlist_id={finished_next_id}, cannot append cl_id={attempted_cl_id}"
            ),
            ContinuityError::RecoveryPointZeroCopperlistId => {
                write!(f, "RecoveryPoint record has copperlist_id == 0 (sentinel)")
            }
            ContinuityError::FinishedZeroNextId => {
                write!(f, "Finished record has next_copperlist_id == 0 (sentinel)")
            }
        }
    }
}

impl std::error::Error for ContinuityError {}

/// 单条 `ContinuityRecord` 形状校验。
///
/// contract ①（Gap 字段完整性）+ 边界哨兵：
/// - Manifest：无 payload，形状校验放行。
/// - Gap：first_id / last_id / reason 三个字段均不允许是 0 哨兵
///   （构造路径类型系统已保证字段存在；反序列化路径允许默认值，需 here 校验）。
/// - RecoveryPoint：copperlist_id == 0 视为哨兵拒绝。
/// - Finished：next_copperlist_id == 0 视为哨兵拒绝；append 由调用方另用
///   `ensure_no_append_after_finished` 守护。
pub fn validate_continuity_record(rec: &ContinuityRecord) -> Result<(), ContinuityError> {
    match &rec.kind {
        ContinuityKind::Manifest => Ok(()),
        ContinuityKind::Gap {
            first_id,
            last_id,
            reason: _,
        } => {
            if *first_id == 0 {
                return Err(ContinuityError::GapMissingFirstId);
            }
            if *last_id == 0 {
                return Err(ContinuityError::GapMissingLastId);
            }
            // reason 是 enum，类型系统保证其存在；不单独校验 reason。
            // 但若 JSON 反序列化路径碰到 `reason=null` → serde 反序列化失败，
            // 因此这里不再补哨兵。
            if *first_id > *last_id {
                return Err(ContinuityError::GapInvalidRange {
                    first_id: *first_id,
                    last_id: *last_id,
                });
            }
            Ok(())
        }
        ContinuityKind::RecoveryPoint { copperlist_id } => {
            if *copperlist_id == 0 {
                return Err(ContinuityError::RecoveryPointZeroCopperlistId);
            }
            Ok(())
        }
        ContinuityKind::Finished { next_copperlist_id } => {
            if *next_copperlist_id == 0 {
                return Err(ContinuityError::FinishedZeroNextId);
            }
            Ok(())
        }
    }
}

/// Sender-side `validate_replay_continuity`：仿源同名函数。
///
/// contract ②（RecoveryPoint 不臆造尾巴）：
/// - 若给定了 keyframe 但 keyframe ≠ actual_cl_id，立即拒绝。
/// - 若 actual_cl_id ≠ expected_cl_id，且没有与 actual_cl_id 匹配的 keyframe，拒绝。
/// - 否则放行。
pub fn validate_replay_continuity(
    expected_cl_id: u64,
    actual_cl_id: u64,
    keyframe: Option<u64>,
) -> Result<(), ContinuityError> {
    if let Some(boundary) = keyframe {
        if boundary != actual_cl_id {
            return Err(ContinuityError::RecoveryPointTailFabrication {
                expected_cl_id,
                actual_cl_id,
                keyframe,
            });
        }
    }
    if actual_cl_id != expected_cl_id && keyframe != Some(actual_cl_id) {
        return Err(ContinuityError::RecoveryPointTailFabrication {
            expected_cl_id,
            actual_cl_id,
            keyframe,
        });
    }
    Ok(())
}

/// 检查：在该 referral 流的 `prior_records` 内若出现过 `Finished`，
/// 则不允许 `new_cl_id` 继续追加。
///
/// contract ③（Finished 终态）的运行时守护。
/// 仅扫描到第一条 `Finished` 即返回错误（业务上 `Finished` 不可重复）。
pub fn ensure_no_append_after_finished(
    prior_records: &[ContinuityRecord],
    new_cl_id: u64,
) -> Result<(), ContinuityError> {
    for rec in prior_records {
        if let ContinuityKind::Finished { next_copperlist_id } = &rec.kind {
            return Err(ContinuityError::FinishedTerminal {
                finished_next_id: *next_copperlist_id,
                attempted_cl_id: new_cl_id,
            });
        }
    }
    Ok(())
}

/// helper：从 JSON 反序列化 `ContinuityRecord`（构造路径 + 反序列化路径覆盖）。
///
/// 单独提取出来便于后续 Task 9/19 用到的「.copper 存档解析」
/// 与「drop zip 解析」共用同一条 JSON 解析语义。
pub fn parse_continuity_record(json: &str) -> Result<ContinuityRecord, String> {
    serde_json::from_str::<ContinuityRecord>(json)
        .map_err(|e| format!("continuity_record JSON parse failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::continuity_record::{ContinuityKind, ContinuityRecord, SourceGapReason};

    fn record(kind: ContinuityKind, cl_id: u64) -> ContinuityRecord {
        ContinuityRecord {
            kind,
            cl_id,
            ref_version_id: format!("REF-001@v{cl_id}"),
        }
    }

    // -- contract ①：四态各一条样例 --

    #[test]
    fn test_four_states() {
        // Manifest
        let manifest = record(ContinuityKind::Manifest, 1);
        assert!(validate_continuity_record(&manifest).is_ok());

        // Gap{first_id, last_id, reason}
        let gap = record(
            ContinuityKind::Gap {
                first_id: 3,
                last_id: 5,
                reason: SourceGapReason::LateJoin,
            },
            6,
        );
        assert!(validate_continuity_record(&gap).is_ok());

        // RecoveryPoint{copperlist_id}
        let rp = record(
            ContinuityKind::RecoveryPoint { copperlist_id: 7 },
            7,
        );
        assert!(validate_continuity_record(&rp).is_ok());

        // Finished{next_copperlist_id}
        let finished = record(
            ContinuityKind::Finished {
                next_copperlist_id: 9,
            },
            8,
        );
        assert!(validate_continuity_record(&finished).is_ok());

        // 同时核对序列化输出便于审计（note: ContinuityKind 用 internally-tagged 风格，
        // 作为 ContinuityRecord 的字段时序列化为嵌套对象 `{"kind":{"kind":"...",...}}`）
        let s = serde_json::to_string(&gap).unwrap();
        assert!(s.contains("\"first_id\":3"));
        assert!(s.contains("\"last_id\":5"));
        assert!(s.contains("\"reason\":\"late_join\""));
        // nested form 校验
        assert!(s.contains("\"kind\":{\"kind\":\"gap\""));

        let s2 = serde_json::to_string(&finished).unwrap();
        assert!(s2.contains("\"next_copperlist_id\":9"));
        assert!(s2.contains("\"kind\":{\"kind\":\"finished\""));
    }

    // -- contract ①：Gap 字段缺失（反序列化路径）--

    #[test]
    fn test_gap_validation_missing_first_id_via_json() {
        // ContinuityKind 用 internally-tagged 风格，作为 struct 字段时序列化为嵌套对象
        // 故意省 first_id → serde 反序列化应失败
        let bad = r#"{"kind":{"kind":"gap","last_id":5,"reason":"late_join"},"cl_id":6,"ref_version_id":"REF-001@v6"}"#;
        let res = serde_json::from_str::<ContinuityRecord>(bad);
        assert!(
            res.is_err(),
            "Gap missing first_id MUST reject at deserialize stage"
        );
    }

    #[test]
    fn test_gap_validation_missing_last_id_via_json() {
        let bad = r#"{"kind":{"kind":"gap","first_id":3,"reason":"late_join"},"cl_id":6,"ref_version_id":"REF-001@v6"}"#;
        let res = serde_json::from_str::<ContinuityRecord>(bad);
        assert!(
            res.is_err(),
            "Gap missing last_id MUST reject at deserialize stage"
        );
    }

    #[test]
    fn test_gap_validation_missing_reason_via_json() {
        let bad = r#"{"kind":{"kind":"gap","first_id":3,"last_id":5},"cl_id":6,"ref_version_id":"REF-001@v6"}"#;
        let res = serde_json::from_str::<ContinuityRecord>(bad);
        assert!(
            res.is_err(),
            "Gap missing reason MUST reject at deserialize stage"
        );
    }

    // -- contract ①：构造路径 sentinel 哨兵（first_id==0 / last_id==0 / first_id>last_id）--

    #[test]
    fn test_gap_sentinel_first_id_zero_rejected() {
        let gap = record(
            ContinuityKind::Gap {
                first_id: 0,
                last_id: 5,
                reason: SourceGapReason::LateJoin,
            },
            6,
        );
        let err = validate_continuity_record(&gap).unwrap_err();
        assert_eq!(err, ContinuityError::GapMissingFirstId);
    }

    #[test]
    fn test_gap_sentinel_last_id_zero_rejected() {
        let gap = record(
            ContinuityKind::Gap {
                first_id: 3,
                last_id: 0,
                reason: SourceGapReason::LateJoin,
            },
            6,
        );
        let err = validate_continuity_record(&gap).unwrap_err();
        assert_eq!(err, ContinuityError::GapMissingLastId);
    }

    #[test]
    fn test_gap_invalid_range_rejected() {
        let gap = record(
            ContinuityKind::Gap {
                first_id: 10,
                last_id: 5,
                reason: SourceGapReason::LateJoin,
            },
            6,
        );
        let err = validate_continuity_record(&gap).unwrap_err();
        assert_eq!(
            err,
            ContinuityError::GapInvalidRange {
                first_id: 10,
                last_id: 5
            }
        );
    }

    // -- contract ②：RecoveryPoint 不臆造尾巴（replay）--

    #[test]
    fn test_validate_replay_continuity_keyframe_matches() {
        // keyframe == actual → 允许跨过 Gap
        let r = validate_replay_continuity(3, 5, Some(5));
        assert!(r.is_ok());
    }

    #[test]
    fn test_validate_replay_continuity_no_keyframe_match_fail() {
        // 期望 cl_id=3, 实际到达 cl_id=5, 无 keyframe → 拒绝
        let r = validate_replay_continuity(3, 5, None);
        assert_eq!(
            r.unwrap_err(),
            ContinuityError::RecoveryPointTailFabrication {
                expected_cl_id: 3,
                actual_cl_id: 5,
                keyframe: None
            }
        );
    }

    #[test]
    fn test_validate_replay_continuity_misaligned_keyframe() {
        // keyframe != actual → 立即拒绝
        let r = validate_replay_continuity(3, 5, Some(7));
        assert_eq!(
            r.unwrap_err(),
            ContinuityError::RecoveryPointTailFabrication {
                expected_cl_id: 3,
                actual_cl_id: 5,
                keyframe: Some(7)
            }
        );
    }

    #[test]
    fn test_validate_replay_continuity_no_gap_pass() {
        // 期望与实际一致 → 放行
        let r = validate_replay_continuity(7, 7, None);
        assert!(r.is_ok());
    }

    // -- contract ③：Finished 终态不允许再 append --

    #[test]
    fn test_finished_terminal_rejects_append() {
        let finished = record(
            ContinuityKind::Finished {
                next_copperlist_id: 9,
            },
            8,
        );
        let prior = vec![finished];
        let r = ensure_no_append_after_finished(&prior, 10);
        assert_eq!(
            r.unwrap_err(),
            ContinuityError::FinishedTerminal {
                finished_next_id: 9,
                attempted_cl_id: 10
            }
        );
    }

    #[test]
    fn test_finished_terminal_allows_append_when_none_prior() {
        let prior: Vec<ContinuityRecord> = vec![];
        let r = ensure_no_append_after_finished(&prior, 10);
        assert!(r.is_ok());
    }

    #[test]
    fn test_finished_terminal_allows_append_when_prior_no_finished() {
        let prior = vec![
            record(ContinuityKind::Manifest, 1),
            record(
                ContinuityKind::Gap {
                    first_id: 3,
                    last_id: 5,
                    reason: SourceGapReason::LateJoin,
                },
                6,
            ),
            record(
                ContinuityKind::RecoveryPoint { copperlist_id: 7 },
                7,
            ),
        ];
        let r = ensure_no_append_after_finished(&prior, 8);
        assert!(r.is_ok());
    }

    // -- source-gap-reason 三类枚举保持 distinct 且正确序列化 --

    #[test]
    fn test_source_gap_reason_three_variants_serde() {
        // 与 source `source_gap_reason_three_variants_distinct` 等价：
        // 通过 serde 路径断言三类都参与序列化并区分。
        let a = serde_json::to_string(&SourceGapReason::LateJoin).unwrap();
        let b = serde_json::to_string(&SourceGapReason::SessionEnded).unwrap();
        let c = serde_json::to_string(&SourceGapReason::ManualDrop).unwrap();
        assert_eq!(a, "\"late_join\"");
        assert_eq!(b, "\"session_ended\"");
        assert_eq!(c, "\"manual_drop\"");
    }

    // -- RecoveryPoint/Finished sentinel --

    #[test]
    fn test_recovery_point_zero_copperlist_id_rejected() {
        let rp = record(ContinuityKind::RecoveryPoint { copperlist_id: 0 }, 0);
        let err = validate_continuity_record(&rp).unwrap_err();
        assert_eq!(err, ContinuityError::RecoveryPointZeroCopperlistId);
    }

    #[test]
    fn test_finished_zero_next_id_rejected() {
        let f = record(
            ContinuityKind::Finished {
                next_copperlist_id: 0,
            },
            0,
        );
        let err = validate_continuity_record(&f).unwrap_err();
        assert_eq!(err, ContinuityError::FinishedZeroNextId);
    }

    // -- parse_continuity_record helper --

    #[test]
    fn test_parse_continuity_record_ok() {
        // ContinuityKind internally-tagged 作为 struct 字段时嵌套对象
        let s = r#"{"kind":{"kind":"manifest"},"cl_id":1,"ref_version_id":"REF-001@v1"}"#;
        let r = parse_continuity_record(s).expect("manifest JSON must parse");
        assert_eq!(r.cl_id, 1);
        assert_eq!(r.kind, ContinuityKind::Manifest);
    }

    #[test]
    fn test_parse_continuity_record_rejects_bad_gap_json() {
        // Gap 缺 first_id → 反序列化应失败
        let s = r#"{"kind":{"kind":"gap","last_id":5,"reason":"late_join"},"cl_id":3,"ref_version_id":"REF-001@v3"}"#;
        assert!(parse_continuity_record(s).is_err());
    }

    // -- Display impl 不恐慌且能完整给出可核对字符串（避免 silent empty-error）--

    #[test]
    fn test_error_display_strings_nonempty() {
        for e in [
            ContinuityError::GapMissingFirstId,
            ContinuityError::GapMissingLastId,
            ContinuityError::GapMissingReason,
            ContinuityError::GapInvalidRange {
                first_id: 5,
                last_id: 3,
            },
            ContinuityError::RecoveryPointTailFabrication {
                expected_cl_id: 1,
                actual_cl_id: 3,
                keyframe: None,
            },
            ContinuityError::FinishedTerminal {
                finished_next_id: 9,
                attempted_cl_id: 10,
            },
            ContinuityError::RecoveryPointZeroCopperlistId,
            ContinuityError::FinishedZeroNextId,
        ] {
            let s = format!("{e}");
            assert!(!s.is_empty());
            assert!(!s.contains("unknown"));
        }
    }
}
