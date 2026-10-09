//! `ReferralVersion`：转诊单的一次修改事件 / 版本快照。
//!
//! 字段对齐 spec.md §3 最小数据格式；
//! - `cl_id` 由 [`crate::copperlist::cl_seq`] 单调计数器分配（Task 3 接入）
//! - `before` / `after` 为字段级快照，{[`crate::his::fhir_map`]} 计算字段级 diff（Task 10 接入）
//! - `continuity` 由 [`crate::continuity::four_state`] 四态适配（Task 4 接入）
//! - `payload_sha256` 由 [`crate::sha256_chain::chain`] 链式摘要（Task 7 接入）

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::continuity_record::ContinuityKind;

/// 转诊单的一次修改事件 / 版本快照。
///
/// 该结构是「被 fsck + replay 校验」的对象；任何对 `cl_id` / `payload_sha256` 的篡改
/// 都会立即在 [`crate::verify::verifier`] 报 `tamper_detected`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferralVersion {
    /// 单调 cl_id（每份 referral 独立计数；持久化重排保护）。
    /// 类型 u64：足够覆盖一份转诊单终其生命周期的所有修改事件。
    pub cl_id: u64,
    /// 版本号（同一 referral 内单调，从 1 开始；与 `cl_id` 同方向但属于业务编号面）。
    pub version_no: u32,
    /// 修改医师（姓名 + 机构，str；不留可识别 ID）。
    pub actor: String,
    /// 修改时间（ISO8601，UTC）。
    pub modified_at: DateTime<Utc>,
    /// 修改前字段级快照（`field → old_value`）。
    /// 排序字段用 BTreeMap：保证持久化重放时字段顺序确定。
    pub before: BTreeMap<String, String>,
    /// 修改后字段级快照（`field → new_value`）。
    pub after: BTreeMap<String, String>,
    /// 流连续性标记（Manifest / Gap / RecoveryPoint / Finished）。
    /// 详细四态契约见 [`crate::continuity::four_state::validate_replay_continuity`]。
    pub continuity: ContinuityKind,
    /// `canonical_json(this)` 的 sha256（链式：prev_hash || canonical_json(this)）。
    pub payload_sha256: String,
}

impl ReferralVersion {
    /// 计算字段级 diff（`add` / `remove` / `replace` 三类），供 [`crate::ui::diff`]
    /// 或 [`crate::ai::openai_compat::ModelGateway`] 消费。
    pub fn field_diff(&self) -> BTreeMap<String, DiffOp> {
        let mut out = BTreeMap::new();
        // 替换 + 新增：after 有，before 无或值不同
        for (k, new_v) in &self.after {
            match self.before.get(k) {
                None => {
                    out.insert(k.clone(), DiffOp::Add(new_v.clone()));
                }
                Some(old_v) if old_v != new_v => {
                    out.insert(
                        k.clone(),
                        DiffOp::Replace {
                            before: old_v.clone(),
                            after: new_v.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        // 删除：before 有，after 无
        for k in self.before.keys() {
            if !self.after.contains_key(k) {
                out.insert(k.clone(), DiffOp::Remove);
            }
        }
        out
    }
}

/// 字段级 diff 三类基本操作（spec.md §6 与 plan.md §4 提到的「字段级 diff」载体）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DiffOp {
    Add(String),
    Remove,
    Replace { before: String, after: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> ReferralVersion {
        let mut before = BTreeMap::new();
        before.insert("diagnosis".to_string(), "高血压待查".to_string());
        let mut after = BTreeMap::new();
        after.insert("diagnosis".to_string(), "高血压 2 级（中危）".to_string());
        after.insert("bp_systolic".to_string(), "162".to_string());
        ReferralVersion {
            cl_id: 1,
            version_no: 1,
            actor: "张医生 / 上转卫生院".to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 1, 0, 0).unwrap(),
            before,
            after,
            continuity: ContinuityKind::Manifest,
            payload_sha256:
                "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        }
    }

    #[test]
    fn field_diff_replace_and_add() {
        let diff = sample().field_diff();
        assert!(matches!(diff.get("diagnosis"), Some(DiffOp::Replace { .. })));
        assert!(matches!(diff.get("bp_systolic"), Some(DiffOp::Add(_))));
    }

    #[test]
    fn referral_version_serde_round_trip() {
        let v = sample();
        let s = serde_json::to_string(&v).unwrap();
        let v2: ReferralVersion = serde_json::from_str(&s).unwrap();
        assert_eq!(v, v2);
    }
}
