//! `ClIdStore`：每份转诊单一份 u64 单调计数器；同款 `safety_case CLM-TEST-002`
//! （copperlist_ids_are_monotonic_across_reuse）守住 id 严格单调且不重用。
//!
//! - [`alloc_next`](ClIdStore::alloc_next)：出站生成（UI / 主动登记时分配下一个 id）；
//!   严格 `last_allocated + 1`，永不重用，永不倒退。
//! - [`observe`](ClIdStore::observe)：入站校验（drop 目录 / REST 拉来的事件声称的 cl_id）；
//!   `proposed > last_allocated` 才接受；否则返回 [`ClIdError::ReorderOrDuplicate`]。
//!
//! 持久化层 [`crate::copperlist::cl_seq_store`] 在每次 `alloc_next` / `observe` 后自动
//! 落盘（`state/cl_seq.json`），重启后从同一文件加载 —— 保证「重启后能续读 + 续写」，
//! 同时也对「先来一个迟到的旧 id」给出明确拒绝。
//!
//! 不持有任何 I/O 副作用；本模块只承担「形状」声明与纯函数式校验。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// 计数起点：每份转诊单首次分配返回 1，永不返回 0。
///
/// 0 留给「尚未观察到任何 cl_id」的语义状态（与源 `CuListsManager::next_cl_id()`
/// 在空状态下返回 0 等价）。
pub const CL_ID_FIRST: u64 = 1;

/// 单调计数器错误。
///
/// 与源 `safety_case CLM-TEST-002` 守住同一组不变量 —— id 必须严格单调且永不重用；
/// 一旦发现违反，事件接收端必须**拒绝并显式报错**，不得偷偷接受。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClIdError {
    /// proposed ≤ last_allocated；可能来自：
    /// - 重复 observe 同一 id
    /// - 重排到达（文件乱序先到高 id 后到低 id）
    /// - 重启后从盘上读到较高 last_allocated，新事件却声称较低的 id
    #[error(
        "cl_id 重排或重复：referral_id={referral_id}, last_allocated={last_allocated}, proposed={proposed}"
    )]
    ReorderOrDuplicate {
        referral_id: String,
        last_allocated: u64,
        proposed: u64,
    },
}

/// 序列化形态：`BTreeMap<referral_id, last_allocated_cl_id>`。
///
/// 字段顺序由 BTreeMap 固定（按字典序），便于跨进程对比与人工核验。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClIdStore {
    /// referral_id → last_allocated_cl_id；缺键表示该 referral 尚未观察到任何 cl_id。
    entries: BTreeMap<String, u64>,
}

impl ClIdStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已知 referral 数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 查某 referral 的最后分配值；未见则返回 0。
    pub fn last_allocated(&self, referral_id: &str) -> u64 {
        self.entries.get(referral_id).copied().unwrap_or(0)
    }

    /// 出站分配：返回该 referral 下一个单调 id（严格 `last + 1`）。
    ///
    /// 调用方通常对应 UI 主动登记 / 内部恢复点的主动落盘；不适用于外部声称的 id，
    /// 外部请走 [`observe`](Self::observe)。
    pub fn alloc_next(&mut self, referral_id: &str) -> Result<u64, ClIdError> {
        let last = self.last_allocated(referral_id);
        let next = last
            .checked_add(1)
            .expect("cl_id u64 单调计数；saturation 在现实转诊单上不可达");
        self.entries.insert(referral_id.to_string(), next);
        Ok(next)
    }

    /// 入站观察：要求 `proposed` 严格大于 `last_allocated`，否则拒收。
    ///
    /// 拒绝策略对源 `CLM-TEST-002` 的「永不重用」语义构成守门：一旦发现声称 id
    /// 倒退或重复，立即报错；上游 drop / REST 适配层不得静默接受。
    ///
    /// 成功时返回**通过的** cl_id（即 `proposed` 本身），并把 last_allocated 推进到该值。
    pub fn observe(
        &mut self,
        referral_id: &str,
        proposed_cl_id: u64,
    ) -> Result<u64, ClIdError> {
        let last = self.last_allocated(referral_id);
        if proposed_cl_id <= last {
            return Err(ClIdError::ReorderOrDuplicate {
                referral_id: referral_id.to_string(),
                last_allocated: last,
                proposed: proposed_cl_id,
            });
        }
        self.entries.insert(referral_id.to_string(), proposed_cl_id);
        Ok(proposed_cl_id)
    }

    /// 列出全部 (referral_id, last_allocated) 对，按字典序。
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// 合并外部 entries（如从盘上加载后合并到内存）；保留 `self` 中已有的更新值。
    ///
    /// 仅在 `other` 中 last_allocated **严格大于** self 同键时覆盖 —— 保证
    /// 「盘上记录不允许回退到内存」；典型用法：进程启动时把 disk 加载项 merge 进空 store。
    pub fn merge_from_disk(&mut self, other: ClIdStore) {
        for (k, v) in other.entries {
            match self.entries.get(&k).copied() {
                Some(existing) if existing >= v => {
                    // 内存或更新或相等：保留较大的（其实是 existing）
                }
                _ => {
                    self.entries.insert(k, v);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cl_id_monotonic() {
        let mut store = ClIdStore::new();
        // 同一 referral 连续 alloc_next 严格单调，从 1 开始
        assert_eq!(store.alloc_next("REF-001").unwrap(), 1);
        assert_eq!(store.alloc_next("REF-001").unwrap(), 2);
        assert_eq!(store.alloc_next("REF-001").unwrap(), 3);
        assert_eq!(store.last_allocated("REF-001"), 3);

        // 不同 referral 各自独立计数
        assert_eq!(store.alloc_next("REF-002").unwrap(), 1);
        assert_eq!(store.alloc_next("REF-001").unwrap(), 4);
        assert_eq!(store.alloc_next("REF-002").unwrap(), 2);

        // observe 路径：proposed > last_allocated 通过；等于/小于拒绝
        assert_eq!(store.observe("REF-001", 10).unwrap(), 10);
        assert_eq!(store.last_allocated("REF-001"), 10);
        assert_eq!(
            store.observe("REF-001", 10),
            Err(ClIdError::ReorderOrDuplicate {
                referral_id: "REF-001".to_string(),
                last_allocated: 10,
                proposed: 10,
            })
        );
        assert_eq!(
            store.observe("REF-001", 5),
            Err(ClIdError::ReorderOrDuplicate {
                referral_id: "REF-001".to_string(),
                last_allocated: 10,
                proposed: 5,
            })
        );
    }

    #[test]
    fn test_cl_id_reorder_protection() {
        let mut store = ClIdStore::new();
        // 模拟乱序到达：先 observe(5)
        assert_eq!(store.observe("REF-A", 5).unwrap(), 5);
        // 再 observe(3)：3 ≤ 5 → 拒收
        let err = store.observe("REF-A", 3).unwrap_err();
        match err {
            ClIdError::ReorderOrDuplicate {
                referral_id,
                last_allocated,
                proposed,
            } => {
                assert_eq!(referral_id, "REF-A");
                assert_eq!(last_allocated, 5);
                assert_eq!(proposed, 3);
            }
        }
        // observe 失败后 last_allocated 不变
        assert_eq!(store.last_allocated("REF-A"), 5);

        // 再来一个中间值 4：依然 4 ≤ 5 → 拒收
        assert_eq!(
            store.observe("REF-A", 4),
            Err(ClIdError::ReorderOrDuplicate {
                referral_id: "REF-A".to_string(),
                last_allocated: 5,
                proposed: 4,
            })
        );
        // 下一个合法 id 仍可分配
        assert_eq!(store.observe("REF-A", 6).unwrap(), 6);
        assert_eq!(store.last_allocated("REF-A"), 6);
    }

    #[test]
    fn test_cl_id_merge_disk_wins_when_strictly_larger() {
        // 模拟：内存中有 REF-001 → 5；盘上记录 REF-001 → 3（被回退过，不合法）
        // 合并后应保留内存中的 5（不能回退）。
        let mut mem = ClIdStore::new();
        mem.alloc_next("REF-001").unwrap();
        mem.alloc_next("REF-001").unwrap();
        mem.alloc_next("REF-001").unwrap();
        mem.alloc_next("REF-001").unwrap();
        mem.alloc_next("REF-001").unwrap();
        assert_eq!(mem.last_allocated("REF-001"), 5);

        let mut disk = ClIdStore::new();
        disk.observe("REF-001", 3).unwrap();
        assert_eq!(disk.last_allocated("REF-001"), 3);

        mem.merge_from_disk(disk);
        assert_eq!(mem.last_allocated("REF-001"), 5, "内存值不能被盘上回退覆盖");
    }

    #[test]
    fn test_cl_id_merge_disk_wins_when_strictly_smaller_in_mem() {
        // 模拟：内存刚启动空着，盘上有 REF-001 → 7；合并后内存应取盘上值。
        let mut mem = ClIdStore::new();
        let mut disk = ClIdStore::new();
        disk.observe("REF-001", 7).unwrap();
        mem.merge_from_disk(disk);
        assert_eq!(mem.last_allocated("REF-001"), 7);
    }
}