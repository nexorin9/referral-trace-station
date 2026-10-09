//! continuity 四态契约测试（Task 27）
//!
//! ## 范围
//!
//! 与 `src/continuity/four_state.rs` 内嵌单测互补 —— 此处作为外部 integration test
//! 覆盖 `validate_replay_continuity` 三条 contract 的**业务级守护边界**：
//!
//! 1. **contract ① — Gap 字段完整性**：
//!    - `test_continuity_gap_missing_first_id`：构造路径下 first_id==0 必须被拒
//!    - `test_continuity_gap_missing_last_id`：构造路径下 last_id==0 必须被拒
//!    - `test_continuity_gap_missing_reason`：反序列化路径下 reason 缺失必须被拒
//! 2. **contract ② — RecoveryPoint 后尾部不臆造**：
//!    - `test_continuity_recovery_point_no_fabricate_tail`：keyframe 与 actual 不匹配
//!      且 expected ≠ actual → 拒绝；任何与 keyframe 不一致的恢复点都视为伪造
//! 3. **contract ③ — Finished 终态不可再追加**：
//!    - `test_continuity_finished_terminal`：流已 Finished 后再 append 必须被拒
//!
//! ## 与 src/continuity/four_state.rs 内嵌单测的区别
//!
//! - 内嵌单测在 lib 模块内（`#[cfg(test)] mod tests`），覆盖四态 happy path +
//!   serde 边界 + sentinel + Display 不空；
//! - 本文件作为 integration test，从**业务契约**角度做「不可漏掉的关键拒绝路径」
//!   守护，每条都对应 spec.md §5 的一条硬约束。
//!
//! ## 与源 copper-rs `validate_replay_continuity` 对齐
//!
//! 源 copper-rs/core/cu29_runtime/src/continuity.rs：
//! - `validate_replay_continuity(expected, actual, keyframe)`：sender-side 校验
//! - `StreamContinuityRecord`：四态枚举（Manifest / Gap / RecoveryPoint / Finished）
//! - Gap 必须有 first_id/last_id/reason 三字段
//! - Finished 后 receiver 不得臆造 unobserved sender tail
//!
//! 本任务适配：与 Task 4 的 `continuity::four_state` 模块共用 API
//! （`validate_continuity_record` / `validate_replay_continuity` /
//!  `ensure_no_append_after_finished`）。

use referral_trace_station::continuity::{
    ensure_no_append_after_finished, validate_continuity_record, validate_replay_continuity,
    ContinuityError, ContinuityKind, ContinuityRecord, SourceGapReason,
};

// -- helpers --

fn record(kind: ContinuityKind, cl_id: u64) -> ContinuityRecord {
    ContinuityRecord {
        kind,
        cl_id,
        ref_version_id: format!("REF-CONTRACT@v{cl_id}"),
    }
}

// -- contract ①：Gap 字段完整性 --

#[test]
fn test_continuity_gap_missing_first_id() {
    // 构造路径下 first_id==0 视为「缺 first_id」，必须被拒
    let gap = record(
        ContinuityKind::Gap {
            first_id: 0,
            last_id: 5,
            reason: SourceGapReason::LateJoin,
        },
        6,
    );
    let err = validate_continuity_record(&gap).expect_err("first_id == 0 must reject");
    assert_eq!(err, ContinuityError::GapMissingFirstId);

    // 反序列化路径再守一次：JSON 缺失 first_id → 解析即失败
    let bad_json = r#"{"kind":{"kind":"gap","last_id":5,"reason":"late_join"},"cl_id":6,"ref_version_id":"REF-CONTRACT@v6"}"#;
    let res: Result<ContinuityRecord, _> = serde_json::from_str(bad_json);
    assert!(
        res.is_err(),
        "Gap missing first_id MUST reject at deserialize stage"
    );
}

#[test]
fn test_continuity_gap_missing_last_id() {
    // 构造路径下 last_id==0 视为「缺 last_id」，必须被拒
    let gap = record(
        ContinuityKind::Gap {
            first_id: 3,
            last_id: 0,
            reason: SourceGapReason::LateJoin,
        },
        6,
    );
    let err = validate_continuity_record(&gap).expect_err("last_id == 0 must reject");
    assert_eq!(err, ContinuityError::GapMissingLastId);

    // 反序列化路径再守一次
    let bad_json = r#"{"kind":{"kind":"gap","first_id":3,"reason":"late_join"},"cl_id":6,"ref_version_id":"REF-CONTRACT@v6"}"#;
    let res: Result<ContinuityRecord, _> = serde_json::from_str(bad_json);
    assert!(
        res.is_err(),
        "Gap missing last_id MUST reject at deserialize stage"
    );
}

#[test]
fn test_continuity_gap_missing_reason() {
    // 构造路径类型系统保证 reason 存在；只能通过反序列化路径触发
    let bad_json = r#"{"kind":{"kind":"gap","first_id":3,"last_id":5},"cl_id":6,"ref_version_id":"REF-CONTRACT@v6"}"#;
    let res: Result<ContinuityRecord, _> = serde_json::from_str(bad_json);
    assert!(
        res.is_err(),
        "Gap missing reason MUST reject at deserialize stage"
    );

    // 同时确认 reason=null 也被反序列化拒绝 —— 不是 fallback 到某种默认值
    let null_reason_json = r#"{"kind":{"kind":"gap","first_id":3,"last_id":5,"reason":null},"cl_id":6,"ref_version_id":"REF-CONTRACT@v6"}"#;
    let res_null: Result<ContinuityRecord, _> = serde_json::from_str(null_reason_json);
    assert!(
        res_null.is_err(),
        "Gap reason=null MUST reject at deserialize stage (no fallback)"
    );
}

// -- contract ②：RecoveryPoint 后尾部不臆造 --

#[test]
fn test_continuity_recovery_point_no_fabricate_tail() {
    // 场景：expected_cl_id=3, 实际到达 cl_id=5（中间 Gap），声称 keyframe=5
    // 关键：keyframe 必须与 actual_cl_id 严格匹配 —— 不允许「恢复点在前/后错位」
    //
    // (1) keyframe == actual → 允许（合法恢复点）
    let r1 = validate_replay_continuity(3, 5, Some(5));
    assert!(
        r1.is_ok(),
        "keyframe matching actual must allow recovery: got {r1:?}"
    );

    // (2) keyframe != actual → 立即拒绝（不允许在边界外虚构后续内容）
    let r2 = validate_replay_continuity(3, 5, Some(7));
    let err2 = r2.expect_err("misaligned keyframe must reject");
    assert_eq!(
        err2,
        ContinuityError::RecoveryPointTailFabrication {
            expected_cl_id: 3,
            actual_cl_id: 5,
            keyframe: Some(7),
        },
        "keyframe != actual must surface as RecoveryPointTailFabrication"
    );

    // (3) 没有 keyframe 但 actual != expected → 拒绝（不允许跳过 Gap 后臆造尾巴）
    let r3 = validate_replay_continuity(3, 5, None);
    let err3 = r3.expect_err("missing keyframe with gap must reject");
    assert_eq!(
        err3,
        ContinuityError::RecoveryPointTailFabrication {
            expected_cl_id: 3,
            actual_cl_id: 5,
            keyframe: None,
        }
    );

    // (4) expected == actual 且无 keyframe → 放行（连续 cl_id，无 Gap 跳跃）
    let r4 = validate_replay_continuity(7, 7, None);
    assert!(
        r4.is_ok(),
        "no gap and matching cl_id must pass: got {r4:?}"
    );

    // (5) keyframe 与 actual 一致，即使 expected != actual 也允许（这是 keyframe 的语义）
    let r5 = validate_replay_continuity(3, 7, Some(7));
    assert!(
        r5.is_ok(),
        "keyframe matching actual even when expected differs: got {r5:?}"
    );
}

// -- contract ③：Finished 终态不可再追加 --

#[test]
fn test_continuity_finished_terminal() {
    // (1) 已有 Finished → 任何 new_cl_id 都被拒
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
            ContinuityKind::Finished {
                next_copperlist_id: 9,
            },
            8,
        ),
    ];

    // 任何 new_cl_id 都必须拒绝
    for new_cl_id in [9u64, 10, 100, u64::MAX] {
        let r = ensure_no_append_after_finished(&prior, new_cl_id);
        let err = r.unwrap_err();
        assert_eq!(
            err,
            ContinuityError::FinishedTerminal {
                finished_next_id: 9,
                attempted_cl_id: new_cl_id,
            },
            "Finished terminal must reject append for new_cl_id={new_cl_id}"
        );
    }

    // (2) 反向断言：prior 中没有 Finished → 允许 append（即使 cl_id 很大）
    let prior_no_finished = vec![
        record(ContinuityKind::Manifest, 1),
        record(
            ContinuityKind::RecoveryPoint { copperlist_id: 5 },
            5,
        ),
    ];
    let r2 = ensure_no_append_after_finished(&prior_no_finished, 100);
    assert!(
        r2.is_ok(),
        "no Finished in prior records must allow append: got {r2:?}"
    );

    // (3) Finished 自身 next_copperlist_id==0 是 sentinel（构造路径已由 validate_continuity_record 拒），
    //     这里额外验证 ensure_no_append_after_finished 在 prior 含 sentinel Finished 时仍按
    //     当前 next_copperlist_id 报错（不爆炸）
    let prior_sentinel_finished = vec![record(
        ContinuityKind::Finished {
            next_copperlist_id: 0,
        },
        0,
    )];
    // prior 里的 sentinel Finished 在 ensure_no_append_after_finished 看来就是一个普通的
    // Finished 标记；它会以 next_copperlist_id=0 报错。这是预期的「构造路径先把关，
    // 运行时不再二次校验」的设计 —— 测试只保证不 panic。
    let _ = ensure_no_append_after_finished(&prior_sentinel_finished, 1);

    // (4) 多个 Finished 时只取第一个（业务上 Finished 不可重复，但防御性验证）
    let prior_two_finished = vec![
        record(ContinuityKind::Manifest, 1),
        record(
            ContinuityKind::Finished {
                next_copperlist_id: 5,
            },
            4,
        ),
        record(
            ContinuityKind::Finished {
                next_copperlist_id: 9,
            },
            8,
        ),
    ];
    let r4 = ensure_no_append_after_finished(&prior_two_finished, 10);
    let err4 = r4.expect_err("first Finished in prior must win");
    assert_eq!(
        err4,
        ContinuityError::FinishedTerminal {
            finished_next_id: 5,
            attempted_cl_id: 10,
        },
        "Finished terminal must surface the first encountered Finished"
    );

    // (5) 空 prior 永远允许
    let empty_prior: Vec<ContinuityRecord> = vec![];
    let r5 = ensure_no_append_after_finished(&empty_prior, 1);
    assert!(
        r5.is_ok(),
        "empty prior must allow any append: got {r5:?}"
    );
}

// (本测试文件无自定义 Result helper —— Result<T, E>::unwrap_err() 已直接提供。)