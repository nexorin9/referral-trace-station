//! `CopperListSequence`：序列空洞检测（fsck 核心模块 D）。
//!
//! 直译源 copper-rs `core/cu29_export/src/fsck.rs::CopperListSequence::observe`
//! 与 `push_hole`：每观察到一个 `cl_id`，若它与上一次观察的 id 出现间隙，
//! 立即记录 `[prev + 1, id - 1]` 区间为一个空洞（hole）。
//!
//! ## 与源产品的差异
//!
//! - 源用 `MAX_REPORTED_COPPERLIST_HOLES = 16` 仅用于打印截断；本任务按
//!   `spec.md` §3 + `plan.md` §3 改成 `MAX_HOLE = 256` 真实上限 ——
//// 超过后不再 push hole，但 `missing` 计数仍累计 + `truncated` 标记置位。
//! - 源仅记录 hole 的 `first/last`；本任务额外携带 `reason: SourceGapReason`
//!   （LateJoin / SessionEnded / ManualDrop）作为可观察、可回写、可审计的
//!   归因元数据 —— 让 fsck 报告既能告知「哪里缺」也能告知「为什么」。
//! - 源 `reason` 仅出现在 `StreamContinuityRecord::Gap` 内被打印；本任务让
//!   空洞检测阶段就 infer 出 reason，而不是事后补标。
//!
//! ## 不变量
//!
//! - `entries` 严格递增（已观察的 cl_id 个数，含重复/倒退 —— 与源对齐）。
//! - `last_id` 单调不减（observe 的 id 倒退时 last_id 不变；前进了再更新）。
//! - `missing` 是累计「理论可达但未观察到的 id」格点数（含 truncated 部分的）。
//! - `holes.len() ≤ MAX_HOLE`；超限 `truncated = true`，但 missing 继续累加。
//! - 同一 id 重复观察 / 倒退观察不计入新 hole（与源
//!   `copperlist_sequence_does_not_invent_holes_for_duplicates_or_reordering`
//!   safety_case 完全一致 —— 重复/重排不应当被误判为空洞）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::model::continuity_record::SourceGapReason;

/// fsck 报告空洞上限：超过后只置 `truncated = true`，不再 push 新 Hole 元素。
///
/// 256 是 `spec.md` §3 与 `plan.md` §3 的产品决策：一份转诊单生命周期 4
/// 阶段、平均 8–16 次修改，全字段 diff 形态下健康流产生空洞数为 0；病理态
/// 下洞数超过 16 即提示投递链路严重异常，超过 256 直接截断并要求人工对账。
pub const MAX_HOLE: usize = 256;

/// 起始 cl_id：与 `cl_seq::CL_ID_FIRST` 对齐，本模块因 LateJoin 判定需要
/// 引入同名常量。本产品的 cl_id 严格从 1 起算（与 [`crate::copperlist::cl_seq`]
/// 的 `CL_ID_FIRST` 一致），不再把 0 当成合法 cl_id —— 源 copper-rs 把 0 当
/// 成合法 id 是源产品决策，本产品在 Task 3 已锁为 1。
pub const FS_CL_ID_FIRST: u64 = 1;

/// 单条空洞：闭区间 `[first_id, last_id]`。
///
/// 携带 `reason` 供 UI 顶部状态条 + 报告导出使用；携带 `span = last - first + 1`
/// 方便做合规审计时直接给「缺多少条」而不必再算一次（避免溢出风险）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hole {
    pub first_id: u64,
    pub last_id: u64,
    pub span: u64,
    pub reason: SourceGapReason,
}

impl Hole {
    pub fn len(&self) -> u64 {
        self.span
    }

    pub fn is_single(&self) -> bool {
        self.first_id == self.last_id
    }
}

/// fsck 检测结果汇总。
///
/// `healthy = missing == 0 && !truncated`；
/// `truncated = holes.len() 达到上限，仍有未列出空洞`,
/// `missing` 含截断部分累计（与源 `copperlist_sequence.missing` 对齐）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsckReport {
    pub first_id: Option<u64>,
    pub last_id: Option<u64>,
    pub entries: u64,
    pub missing: u64,
    pub holes: Vec<Hole>,
    pub truncated: bool,
    pub healthy: bool,
}

impl FsckReport {
    /// 列出空洞区间数（受截断上限影响 —— 仅反映已结构化存储的部分）。
    pub fn hole_ranges(&self) -> usize {
        self.holes.len()
    }

    /// 列出缺失的 cl_id 格点总数（含已截断部分累计的 missing）。
    pub fn missing_count(&self) -> u64 {
        self.missing
    }

    /// 单一展示文案：UI 状态条 / 报告导出 / 医务科签收对话文。
    pub fn summary_line(&self) -> String {
        if self.holes.is_empty() {
            return format!(
                "fsck ok: entries={}, missing=0, truncated=false",
                self.entries
            );
        }
        let truncated_flag = if self.truncated { ", truncated=true" } else { "" };
        format!(
            "fsck: entries={}, missing={}, hole_ranges={}{}",
            self.entries,
            self.missing,
            self.holes.len(),
            truncated_flag
        )
    }
}

/// 序列空洞检测器：对一系列 `cl_id` 调用 `observe`，累计空洞。
///
/// 不持有任何 I/O；纯函数式。持久化（state/<referral_id>.copper 反序列化 → 批量回放）
/// 应在调用本检测器前完成。
#[derive(Default, Debug, Clone)]
pub struct CopperListSequence {
    entries: u64,
    first_id: Option<u64>,
    last_id: Option<u64>,
    missing: u64,
    holes: Vec<Hole>,
    truncated: bool,
    /// 可选：每个观察过的 id 附带 unix epoch 秒。`infer_reason` 据此推断
    /// 邻接空洞是 SessionEnded 还是 ManualDrop；无时间戳则退化为 LastObservedHeuristic。
    timestamps: BTreeMap<u64, i64>,
}

impl CopperListSequence {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已观察的 cl_id 个数（含重复 / 倒退 —— 与源对齐）。
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// 首次观察到的 cl_id；未见任何调用则 None。
    pub fn first_id(&self) -> Option<u64> {
        self.first_id
    }

    /// 最近一次「前进方向」观察到的 cl_id；与 `ClIdStore::last_allocated` 不同：
    /// 倒退 observe 不推进 last_id。源对该字段的语义正是如此。
    pub fn last_id(&self) -> Option<u64> {
        self.last_id
    }

    pub fn missing(&self) -> u64 {
        self.missing
    }

    pub fn holes(&self) -> &[Hole] {
        &self.holes
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// 是否曾观察过任何 id。
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// 观察一个 id。倒退 / 重复不计入新 hole（守住源
    /// `copperlist_sequence_does_not_invent_holes_for_duplicates_or_reordering`）。
    ///
    /// 起始 LateJoin：仅当首观察 id **严格大于** [`FS_CL_ID_FIRST`]（=1）时才
    /// 记一个 `[FS_CL_ID_FIRST, id - 1]` 起始空洞。本产品的 cl_id 严格从 1
    /// 起算（与 [`crate::copperlist::cl_seq::CL_ID_FIRST`] 一致），把 0 当成
    /// 「未观察」而非合法 id —— 与源 copper-rs 把 0 视为合法 id 是源产品决策。
    pub fn observe(&mut self, id: u64) {
        self.entries = self.entries.saturating_add(1);

        let Some(previous) = self.last_id else {
            self.first_id = Some(id);
            self.last_id = Some(id);
            // 初始空洞：仅当 id 晚于 FS_CL_ID_FIRST 时，记 [FS_CL_ID_FIRST, id-1]
            // 为 LateJoin；首观察就是 FS_CL_ID_FIRST 视为健康起点。
            if id > FS_CL_ID_FIRST {
                self.push_hole(FS_CL_ID_FIRST, id - 1);
            }
            return;
        };

        if id > previous.saturating_add(1) {
            self.push_hole(previous + 1, id - 1);
        }
        if id > previous {
            self.last_id = Some(id);
        }
    }

    /// 观察一个 id + 其 unix epoch 秒。配合 [`observe`] 一起调用；
    /// 时间戳供 `infer_reason` 区分 SessionEnded 与 ManualDrop。
    pub fn observe_with_time(&mut self, id: u64, unix_ts: i64) {
        self.timestamps.insert(id, unix_ts);
        self.observe(id);
    }

    /// 内部推入一个空洞；超过 [`MAX_HOLE`] 时只置 truncated、不再 push，
    /// `missing` 仍累计 —— 与源 `push_hole` 累加 missing 的语义对齐。
    fn push_hole(&mut self, first: u64, last: u64) {
        let span = last.saturating_sub(first).saturating_add(1);
        let reason = self.infer_reason(first, last);
        // 累加 missing 永远要做（与源 `missing = missing.saturating_add(hole.len())` 对齐），
        // 不论 holes 是否被截断 —— UI 顶部状态条要的就是「共缺多少」。
        self.missing = self.missing.saturating_add(span);

        if self.holes.len() >= MAX_HOLE {
            self.truncated = true;
            return;
        }

        self.holes.push(Hole {
            first_id: first,
            last_id: last,
            span,
            reason,
        });
    }

    /// 推断空洞 reason：
    ///
    /// - **LateJoin**：起始空洞，从 cl_id `FS_CL_ID_FIRST` 起的初始段 —— 接收方
    ///   在流已开始之后才接入，自然缺前面一段（与源 `SourceGapReason::LateJoin`
    ///   对齐，本产品 cl_id 起点为 1 而非 0）。
    /// - **ManualDrop**：缺段跨度大、邻接观察的时间戳跳跃显著 —— 强烈提示
    ///   人工剔除或投递链路整段丢失（与源 RecoveryPoint 邻近状态对齐改写）。
    /// - **SessionEnded**（默认）：单条 / 小跨度缺段，可能是发送端一侧掉线
    ///   或重启后 1–2 个 id 被消费侧丢失 —— 常见可恢复形态。
    ///
    /// 没有时间戳退化：跨度 > 100 → ManualDrop，否则 SessionEnded。
    fn infer_reason(&self, first: u64, last: u64) -> SourceGapReason {
        // LateJoin：起始空洞（从 FS_CL_ID_FIRST 起而非从 0 起）
        if first == FS_CL_ID_FIRST {
            return SourceGapReason::LateJoin;
        }

        let span = last.saturating_sub(first).saturating_add(1);
        let prev_id = first.saturating_sub(1);
        let next_id = last.saturating_add(1);

        let prev_ts = self.timestamps.get(&prev_id).copied();
        let next_ts = self.timestamps.get(&next_id).copied();

        match (prev_ts, next_ts) {
            (Some(p), Some(n)) => {
                // 时间跨度判定：
                //   - 跨度 ≥ 60 秒且 span ≥ 4 → ManualDrop（人工批量剔除/整段丢失）
                //   - 跨度 ≥ 10 秒                → ManualDrop（运维干预）
                //   - 跨度较小、单条              → SessionEnded（瞬时掉线/丢包）
                let dt = n.saturating_sub(p).max(0) as u64;
                if dt >= 60 && span >= 4 {
                    SourceGapReason::ManualDrop
                } else if dt >= 10 {
                    SourceGapReason::ManualDrop
                } else {
                    SourceGapReason::SessionEnded
                }
            }
            (Some(_), None) | (None, Some(_)) => {
                // 单侧时间戳信息 → 跨度判定
                if span >= 4 {
                    SourceGapReason::ManualDrop
                } else {
                    SourceGapReason::SessionEnded
                }
            }
            (None, None) => {
                // 没有时间戳信息：纯跨度启发式
                if span >= 8 {
                    SourceGapReason::ManualDrop
                } else {
                    SourceGapReason::SessionEnded
                }
            }
        }
    }

    /// 汇总 fsck 报告。
    pub fn fsck_report(&self) -> FsckReport {
        let healthy = self.missing == 0 && !self.truncated;
        FsckReport {
            first_id: self.first_id,
            last_id: self.last_id,
            entries: self.entries,
            missing: self.missing,
            holes: self.holes.clone(),
            truncated: self.truncated,
            healthy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_hole() {
        // 源测试精神同款：id = [2, 3, 6, 9, 10] 触发多个空洞。但本产品 cl_id
        // 从 1 起算，所以 (id=2) 首观察会先产生一个 LateJoin [1..1]。
        // —— 验证连续观察 + 故意缺失触发空洞。
        let mut seq = CopperListSequence::new();
        for id in [2u64, 3, 6, 9, 10] {
            seq.observe(id);
        }
        assert_eq!(seq.entries(), 5);
        assert_eq!(seq.first_id(), Some(2));
        assert_eq!(seq.last_id(), Some(10));
        // missing = 1 (LateJoin [1..1]) + 2 ([4..5]) + 2 ([7..8]) = 5
        assert_eq!(seq.missing(), 5);
        assert_eq!(seq.holes().len(), 3);
        assert_eq!(seq.holes()[0].first_id, 1);
        assert_eq!(seq.holes()[0].last_id, 1);
        assert_eq!(seq.holes()[1].first_id, 4);
        assert_eq!(seq.holes()[1].last_id, 5);
        assert_eq!(seq.holes()[2].first_id, 7);
        assert_eq!(seq.holes()[2].last_id, 8);
        // 起始段是 LateJoin（first == FS_CL_ID_FIRST）
        assert_eq!(seq.holes()[0].reason, SourceGapReason::LateJoin);
        // 中段跨度小、邻接观察大概率 SessionEnded（无时间戳退化路径下也成立）
        assert_eq!(seq.holes()[1].reason, SourceGapReason::SessionEnded);
        assert_eq!(seq.holes()[2].reason, SourceGapReason::SessionEnded);

        let report = seq.fsck_report();
        assert!(!report.healthy);
        assert_eq!(report.missing_count(), 5);
        assert_eq!(report.hole_ranges(), 3);
        assert!(!report.truncated);
    }

    #[test]
    fn test_continuous_ids_no_hole() {
        // 健康流：连续观察 1..=10，应无空洞。
        let mut seq = CopperListSequence::new();
        for id in 1u64..=10 {
            seq.observe(id);
        }
        assert_eq!(seq.entries(), 10);
        assert_eq!(seq.first_id(), Some(1));
        assert_eq!(seq.last_id(), Some(10));
        assert_eq!(seq.missing(), 0);
        assert!(seq.holes().is_empty());
        assert!(!seq.is_truncated());

        let report = seq.fsck_report();
        assert!(report.healthy);
        assert!(report.summary_line().contains("fsck ok"));
    }

    #[test]
    fn test_duplicate_no_hole_invented() {
        // 源 safety_case 移植：duplicate / reordering 不应当被误判为空洞。
        // 源用 [0, 1, 1, 0, 2]（cl_id 0 是合法的），本产品 cl_id 从 1 起算，
        // 因此等价地构造为 [1, 2, 2, 1, 3] —— 末段 [1..3] 一样没有空洞。
        let mut seq = CopperListSequence::new();
        for id in [1u64, 2, 2, 1, 3] {
            seq.observe(id);
        }
        assert_eq!(seq.entries(), 5);
        assert_eq!(seq.first_id(), Some(1));
        assert_eq!(seq.last_id(), Some(3));
        assert_eq!(seq.missing(), 0);
        assert!(seq.holes().is_empty());
    }

    #[test]
    fn test_max_hole_truncated() {
        // 构造 257 个空洞：本产品 cl_id 从 1 起算，所以先 observe(1)
        // （= FS_CL_ID_FIRST，不产生 LateJoin），然后观察 257 个外部 id，
        // 每次的跨度都是 257 - 1 = 256（除了第一次跨度是 256 - 1 = 255）。
        // 累计 missing = 255 + 256 * 256 = 65591；holes 累计 257 个触发截断。
        let mut seq = CopperListSequence::new();
        seq.observe(FS_CL_ID_FIRST);
        for i in 1u64..=257 {
            seq.observe(i * 257);
        }
        // holes.len() 严格 ≤ MAX_HOLE
        assert!(
            seq.holes().len() <= MAX_HOLE,
            "holes.len() = {} > MAX_HOLE = {}",
            seq.holes().len(),
            MAX_HOLE
        );
        assert_eq!(seq.holes().len(), MAX_HOLE);
        // truncated 必须置位
        assert!(seq.is_truncated());
        // missing 累计全部 257 个空洞（不因截断而漏数）——
        //   第一个空洞 [2..256] 跨度 = 255
        //   后续 256 个空洞，每个跨度 = 256
        let expected_missing = 255u64 + 256u64 * 256;
        assert_eq!(seq.missing(), expected_missing);
        let report = seq.fsck_report();
        assert!(report.truncated);
        assert!(!report.healthy);
        assert_eq!(report.missing_count(), expected_missing);
        assert_eq!(report.hole_ranges(), MAX_HOLE);
        assert!(report.summary_line().contains("truncated=true"));
    }

    #[test]
    fn test_single_id_hole_after_gap() {
        // 单元空洞：observe(1), observe(3) 应记录 [2..2]
        let mut seq = CopperListSequence::new();
        seq.observe(1);
        seq.observe(3);
        assert_eq!(seq.holes().len(), 1);
        let h = &seq.holes()[0];
        assert_eq!(h.first_id, 2);
        assert_eq!(h.last_id, 2);
        assert_eq!(h.span, 1);
        assert!(h.is_single());
        // 单条跨度小 → SessionEnded
        assert_eq!(h.reason, SourceGapReason::SessionEnded);
    }

    #[test]
    fn test_manual_drop_with_timestamps() {
        // 时间戳跨度大 → ManualDrop
        let mut seq = CopperListSequence::new();
        // cl_id 1 @ t=0
        seq.observe_with_time(1, 0);
        // cl_id 50 @ t=120 秒 → 跨度 120 ≥ 60、span=48 ≥ 4 → ManualDrop
        seq.observe_with_time(50, 120);
        assert_eq!(seq.holes().len(), 1);
        assert_eq!(seq.holes()[0].reason, SourceGapReason::ManualDrop);
    }

    #[test]
    fn test_late_join_initial_gap() {
        // 起始空洞：首次观察 id > FS_CL_ID_FIRST, [FS_CL_ID_FIRST, id-1] 都是 LateJoin
        let mut seq = CopperListSequence::new();
        seq.observe(100);
        assert_eq!(seq.holes().len(), 1);
        assert_eq!(seq.holes()[0].first_id, FS_CL_ID_FIRST);
        assert_eq!(seq.holes()[0].last_id, 99);
        assert_eq!(seq.holes()[0].span, 99);
        assert_eq!(seq.holes()[0].reason, SourceGapReason::LateJoin);
    }

    #[test]
    fn test_no_holes_for_clean_first_observation_at_fs_first() {
        // 首次观察 id 恰好 = FS_CL_ID_FIRST (= 1)：无起始空洞
        let mut seq = CopperListSequence::new();
        seq.observe(FS_CL_ID_FIRST);
        assert!(seq.holes().is_empty());
        assert_eq!(seq.first_id(), Some(FS_CL_ID_FIRST));
        assert_eq!(seq.last_id(), Some(FS_CL_ID_FIRST));
        assert_eq!(seq.missing(), 0);
    }
}
