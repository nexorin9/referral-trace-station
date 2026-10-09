//! fsck 边界单元测试（Task 24）
//!
//! ## 范围
//!
//! 与 `src/copperlist/fsck.rs` 内嵌单测互补 —— 此处作为外部 integration test
//! 覆盖「关键边界」六类：
//!
//! 1. `test_fsck_empty_stream`：从未 observe 任何 id → 空流（`is_empty()=true` +
//!    `entries=0` + `first_id=None` + `last_id=None` + `missing=0` + 报告 `healthy=true`）
//! 2. `test_fsck_single_element`：仅 observe(id=1) → 单元素（无空洞、`healthy=true`、
//!    报告 `first_id=Some(1)`）
//! 3. `test_fsck_healthy_declaration`：连续 1..=N → 健康声明（0 空洞、`healthy=true`、
//!    报告 summary_line 含 "fsck ok"）
//! 4. `test_fsck_max_hole_256`：构造 256 个空洞 → 恰好达到 MAX_HOLE 上限、
//!    `truncated=false`
//! 5. `test_fsck_max_hole_257_truncated`：构造 257 个空洞 → `holes.len()=MAX_HOLE=256` +
//!    `truncated=true` + `missing` 继续累加（不因截断漏数）
//! 6. `test_fsck_adjacent_holes_no_merge`：紧邻空洞 [2..2]+[4..4]+[6..6] 不会
//!    自动合并为 [2..6] —— 本产品 fsck **保持**每个空洞独立成 entry，
//!    便于 UI 状态条分别归因（LateJoin / SessionEnded / ManualDrop 可独立）
//!
//! ## 与 src/copperlist/fsck.rs 内嵌单测的区别
//!
//! - 内嵌单测写在 lib 模块内（`#[cfg(test)] mod tests`），粒度是「主路径 +
//!   重复/重排 + 截断 + 时间戳 reason 推断」；
//! - 本文件作为 integration test，链接到
//!   `referral_trace_station::copperlist::{CopperListSequence, FsckReport,
//!   Hole, MAX_HOLE, FS_CL_ID_FIRST}`，从外部视角覆盖**完整 6 类边界**，包括
//!   空流 / 单元素 / 健康声明 / 256 上限 / 257 截断 / 邻接不合并。
//!
//! ## 不变量（与 fsck.rs 模块注释一致）
//!
//! - `entries` 严格递增（含重复 / 倒退）
//! - `last_id` 单调不减（observe 倒退时不变）
//! - `missing` 是累计「理论可达但未观察到的 id」格点数（含 truncated 部分）
//! - `holes.len() ≤ MAX_HOLE`，超限 `truncated = true`
//! - **邻接空洞不合并**：每次 `observe(id)` 仅当 `id > previous + 1` 时
//!   push 一个新 Hole，**不**回看上一条 Hole 是否邻接 —— 由调用方按需
//!   自行聚合（避免丢失逐段 reason 归因）。
//!
//! ## SourceGapReason 三类归因（来自 model::continuity_record）
//!
//! - `LateJoin`：起始空洞（first == FS_CL_ID_FIRST）
//! - `ManualDrop`：跨度大 + 时间戳跳跃显著（≥60 秒 / ≥10 秒 / ≥4 跨度）
//! - `SessionEnded`：单条 / 小跨度（默认）

use referral_trace_station::copperlist::{CopperListSequence, Hole, MAX_HOLE};
use referral_trace_station::model::continuity_record::SourceGapReason;

// `Hole::reason` 字段类型 = `SourceGapReason`，通过 `model::continuity_record`
// 公开导出。本测试直接 import 该枚举用于断言 reason 归因。

/// 测试 1：空流。
///
/// 从未 observe 任何 id → 序列处于「未开始」状态：
/// - `entries == 0`
/// - `first_id() == None` & `last_id() == None`
/// - `missing == 0`
/// - `holes().is_empty()`
/// - `is_empty() == true`
/// - `is_truncated() == false`
/// - `fsck_report().healthy == true`（空流视作「健康起点」）
/// - `summary_line()` 含 "fsck ok"
///
/// 边界意义：避免 fsck 报告把「还没开始观察」误判为「有空洞」。
#[test]
fn test_fsck_empty_stream() {
    let seq = CopperListSequence::new();
    assert_eq!(seq.entries(), 0, "empty stream must have 0 entries");
    assert_eq!(seq.first_id(), None, "empty stream first_id must be None");
    assert_eq!(seq.last_id(), None, "empty stream last_id must be None");
    assert_eq!(seq.missing(), 0, "empty stream must have 0 missing");
    assert!(seq.holes().is_empty(), "empty stream must have 0 holes");
    assert!(seq.is_empty(), "is_empty() must be true on empty stream");
    assert!(!seq.is_truncated(), "empty stream must not be truncated");

    let report = seq.fsck_report();
    assert_eq!(report.entries, 0);
    assert_eq!(report.first_id, None);
    assert_eq!(report.last_id, None);
    assert_eq!(report.missing, 0);
    assert!(report.holes.is_empty());
    assert!(!report.truncated);
    assert!(
        report.healthy,
        "empty stream must surface as healthy=true (no observations yet)"
    );
    assert_eq!(report.missing_count(), 0);
    assert_eq!(report.hole_ranges(), 0);
    assert!(
        report.summary_line().contains("fsck ok"),
        "empty stream summary must say 'fsck ok'; got: {}",
        report.summary_line()
    );
}

/// 测试 2：单元素（仅 observe id = FS_CL_ID_FIRST）。
///
/// 单条 entry 不应产生任何空洞：
/// - `entries == 1`
/// - `first_id() == last_id() == Some(1)`
/// - `missing == 0`
/// - `holes().is_empty()`
/// - `healthy == true`
///
/// 边界意义：守住 `FS_CL_ID_FIRST` 起点 = 「健康起点」的设计 —— 单一观察不
/// 触发 LateJoin，也不应因「只有一条」而被误判为可疑。
#[test]
fn test_fsck_single_element() {
    let mut seq = CopperListSequence::new();
    seq.observe(1);
    assert_eq!(seq.entries(), 1, "single observation must count 1 entry");
    assert_eq!(
        seq.first_id(),
        Some(1),
        "single observation first_id must be Some(1)"
    );
    assert_eq!(
        seq.last_id(),
        Some(1),
        "single observation last_id must be Some(1)"
    );
    assert_eq!(seq.missing(), 0, "single observation must have 0 missing");
    assert!(
        seq.holes().is_empty(),
        "single observation must not invent holes"
    );
    assert!(!seq.is_truncated());

    let report = seq.fsck_report();
    assert_eq!(report.entries, 1);
    assert_eq!(report.first_id, Some(1));
    assert_eq!(report.last_id, Some(1));
    assert_eq!(report.missing, 0);
    assert!(report.holes.is_empty());
    assert!(!report.truncated);
    assert!(
        report.healthy,
        "single observation must surface as healthy=true"
    );
}

/// 测试 3：健康声明（连续 1..=N，N 较大验证健康路径稳定）。
///
/// 连续观察 1..=50 应保持健康：
/// - `entries == 50`
/// - `missing == 0`
/// - `holes().is_empty()`
/// - `healthy == true`
/// - `summary_line()` 含 "fsck ok" + "missing=0"
///
/// 边界意义：守住 fsck 「连续观察 = 0 空洞」的平凡路径，确保真实规模 N=50
/// 的健康流不会被任何累积误差污染。
#[test]
fn test_fsck_healthy_declaration() {
    let n: u64 = 50;
    let mut seq = CopperListSequence::new();
    for id in 1u64..=n {
        seq.observe(id);
    }
    assert_eq!(seq.entries(), n);
    assert_eq!(seq.first_id(), Some(1));
    assert_eq!(seq.last_id(), Some(n));
    assert_eq!(seq.missing(), 0, "healthy stream must have 0 missing");
    assert!(seq.holes().is_empty(), "healthy stream must have 0 holes");
    assert!(!seq.is_truncated());

    let report = seq.fsck_report();
    assert!(report.healthy, "healthy stream must surface as healthy=true");
    assert_eq!(report.missing_count(), 0);
    assert_eq!(report.hole_ranges(), 0);
    assert!(report.summary_line().contains("fsck ok"));
    assert!(
        report.summary_line().contains("missing=0"),
        "summary must say missing=0; got: {}",
        report.summary_line()
    );
}

/// 测试 4：恰好 256 个空洞 —— 触及 MAX_HOLE 上限但**不**触发截断。
///
/// 构造：先 observe(1)（= FS_CL_ID_FIRST，不产生 LateJoin），然后观察
/// `[2*257, 3*257, ..., 257*257]` 共 256 个外部 id，每次跨越 256 跨度 →
///
/// 第一次 observe(2*257=514) → 空洞 [2..513] 跨度 512
/// 后续 255 次 observe((i+2)*257) → 每条空洞跨度 256
/// 累计 missing = 512 + 255*256 = 65792（**不**含 truncated 部分）
/// holes.len() = 256 恰好等于 MAX_HOLE，但 truncated = false（因为 257 次 push 才到 257 = MAX_HOLE + 1）
///
/// 等等 —— 重算：第一个空洞 [2..513]，跨度 = 513 - 2 + 1 = 512。
/// 第二次 observe(3*257=771) → previous = 514, hole [515..770], span = 256。
/// ... 后续每次跨度 = 256。
/// 总 missing = 512 + 255 * 256 = 512 + 65280 = 65792。
///
/// 实际期望：构造 256 个空洞后 holes.len() = 256 = MAX_HOLE，
/// 但因为 push_hole 在 holes.len() >= MAX_HOLE 时才置 truncated=true，
/// 而「push 第 256 条」之前 holes.len() = 255，所以这条 push 成功，
/// truncated 仍为 false。
///
/// 边界意义：MAX_HOLE 是「上限」不是「触发点」 —— 恰好达到上限应仍可报告。
#[test]
fn test_fsck_max_hole_256() {
    let mut seq = CopperListSequence::new();
    seq.observe(1); // FS_CL_ID_FIRST = 1，无 LateJoin
    // 构造 256 个跨度：观察 [2*257, 3*257, ..., 257*257]
    // 每次跨度 257，所以第一个空洞 [2..513] = 跨度 512（实际是 513-2+1=512）
    // 后续跨度 257-1=256
    let n_external: u64 = 256;
    for i in 2u64..=(n_external + 1) {
        seq.observe(i * 257);
    }
    // 验证：holes.len() 恰好 == MAX_HOLE
    assert_eq!(
        seq.holes().len(),
        MAX_HOLE,
        "exactly 256 holes must fill MAX_HOLE without truncation; got {}",
        seq.holes().len()
    );
    // 验证：truncated 仍未触发（第 257 条才会触发）
    assert!(
        !seq.is_truncated(),
        "256 holes must NOT trigger truncated; should only trigger on 257th"
    );
    // missing 累计：第一个空洞 [2..513] 跨度 512 + 后续 255 个跨度 256 = 512 + 65280 = 65792
    let expected_missing: u64 = 512 + (n_external - 1) * 256;
    assert_eq!(
        seq.missing(),
        expected_missing,
        "missing count must equal sum of all hole spans (no truncation yet)"
    );

    let report = seq.fsck_report();
    assert!(!report.truncated);
    assert!(!report.healthy);
    assert_eq!(report.hole_ranges(), MAX_HOLE);
    assert_eq!(report.missing_count(), expected_missing);
}

/// 测试 5：构造 257 个空洞 → holes.len() = MAX_HOLE + truncated=true。
///
/// 边界意义：MAX_HOLE + 1 触发截断；holes.len() 严格 ≤ MAX_HOLE，但
/// missing 仍继续累计（不因截断漏数）—— 与源 `push_hole` 语义对齐。
///
/// 构造：先 observe(1)，然后观察 [257, 514, 771, ...] 共 257 个外部 id：
/// - 第一个 observe(257) → hole [2..256] 跨度 255
/// - 后续 256 次 observe(i*257, i=2..=257) → 每条跨度 256
/// - 累计 missing = 255 + 256*256 = 65791
/// - holes.len() = 257 但 MAX_HOLE = 256 → 第 257 次 push_hole 时
///   holes.len() >= MAX_HOLE → truncated=true 但 holes.len() 仍 256
#[test]
fn test_fsck_max_hole_257_truncated() {
    let mut seq = CopperListSequence::new();
    seq.observe(1); // FS_CL_ID_FIRST

    // 构造 257 个跨度：观察 [1*257, 2*257, ..., 257*257]
    let n_external: u64 = 257;
    for i in 1u64..=n_external {
        seq.observe(i * 257);
    }

    // holes.len() 严格 == MAX_HOLE（被截断）
    assert_eq!(
        seq.holes().len(),
        MAX_HOLE,
        "holes.len() must equal MAX_HOLE after truncation; got {}",
        seq.holes().len()
    );
    // truncated 必须置位
    assert!(
        seq.is_truncated(),
        "257th push_hole must set truncated=true"
    );
    // missing 继续累计全部 257 个空洞的跨度 —— 第一个跨度 255 + 后续 256*256
    let expected_missing: u64 = 255 + 256u64 * 256;
    assert_eq!(
        seq.missing(),
        expected_missing,
        "missing must continue accumulating even after truncation (no data loss)"
    );

    let report = seq.fsck_report();
    assert!(report.truncated, "report.truncated must be true");
    assert!(
        !report.healthy,
        "truncated + missing>0 must not surface as healthy"
    );
    assert_eq!(report.missing_count(), expected_missing);
    assert_eq!(report.hole_ranges(), MAX_HOLE);
    assert!(
        report.summary_line().contains("truncated=true"),
        "summary must mark truncated=true; got: {}",
        report.summary_line()
    );
}

/// 测试 6：紧邻空洞**不**合并 —— 验证 fsck.rs 当前实现语义。
///
/// 构造：observe [1, 3, 5, 7] → 三个空洞 [2..2] + [4..4] + [6..6]，每个之间
/// 由一个观察过的 id（3, 5）隔开。本产品 fsck **保持**每个空洞独立成 entry，
/// **不**自动合并成 [2..6]。
///
/// 边界意义：合并会丢失逐段 reason 归因（LateJoin / SessionEnded /
/// ManualDrop 可独立），所以 fsck 必须保持每条 Hole 独立。调用方如需聚合，
/// 按 `holes[i].last_id < holes[i+1].first_id - 1` 检测自行合并（注意
/// 由于每次 observe 必触发「last+1 = 新 first」的观察，holes 之间**必然**
/// 有 1 个观察点做桥接 —— 真正的「last+1 == next.first」相邻永远不出现，
/// 故本测试不检测 strict 邻接性）。
///
/// 本测试断言：
/// - `holes().len() == 3`（不是 1，也不是 2）
/// - 每条 Hole 的 span = 1
/// - 每条 Hole 的 first_id = 2 / 4 / 6（不连续到 6）
/// - 三条 reason 均为 SessionEnded（小跨度 + 无时间戳 → SessionEnded）
/// - `missing == 3`（三个各缺一个 id）
/// - 反向断言：不存在 first_id=2, last_id=6 的合并空洞
#[test]
fn test_fsck_adjacent_holes_no_merge() {
    let mut seq = CopperListSequence::new();
    for id in [1u64, 3, 5, 7] {
        seq.observe(id);
    }

    // 关键断言：三个空洞，**不**合并成 [2..6]
    assert_eq!(
        seq.holes().len(),
        3,
        "tightly-spaced holes must remain separate entries (no auto-merge); got {}",
        seq.holes().len()
    );
    assert_eq!(seq.entries(), 4);
    assert_eq!(seq.first_id(), Some(1));
    assert_eq!(seq.last_id(), Some(7));
    assert_eq!(seq.missing(), 3, "three single-id holes = 3 missing");

    // 验证每条 Hole 字段
    let h0: &Hole = &seq.holes()[0];
    assert_eq!(h0.first_id, 2);
    assert_eq!(h0.last_id, 2);
    assert_eq!(h0.span, 1);
    assert!(h0.is_single());
    // 单条跨度小 → SessionEnded
    assert_eq!(h0.reason, SourceGapReason::SessionEnded);

    let h1: &Hole = &seq.holes()[1];
    assert_eq!(h1.first_id, 4);
    assert_eq!(h1.last_id, 4);
    assert_eq!(h1.span, 1);
    assert!(h1.is_single());
    assert_eq!(h1.reason, SourceGapReason::SessionEnded);

    let h2: &Hole = &seq.holes()[2];
    assert_eq!(h2.first_id, 6);
    assert_eq!(h2.last_id, 6);
    assert_eq!(h2.span, 1);
    assert!(h2.is_single());
    assert_eq!(h2.reason, SourceGapReason::SessionEnded);

    // 反向断言：不存在 first_id=2, last_id=6 的合并空洞
    for h in seq.holes() {
        assert!(
            !(h.first_id == 2 && h.last_id == 6),
            "fsck must NOT merge tightly-spaced holes into [2..6]; got [{}..{}]",
            h.first_id,
            h.last_id
        );
    }

    // FsckReport 字段一致
    let report = seq.fsck_report();
    assert!(!report.healthy);
    assert_eq!(report.hole_ranges(), 3);
    assert_eq!(report.missing_count(), 3);
    assert!(!report.truncated);

    // 由于 observe(3) 是 [2..2] 与 [4..4] 之间的「桥接观察点」，
    // 这两个 hole **永远不可能**真正的 strict-adjacent（last+1 == next.first）。
    // 验证：holes[0].last_id + 1 (= 3) < holes[1].first_id (= 4)，由 observe(3) 桥接。
    assert!(
        seq.holes()[0].last_id + 1 < seq.holes()[1].first_id,
        "holes are bridged by an observed id; strict adjacency impossible"
    );
}