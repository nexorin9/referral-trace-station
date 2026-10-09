//! `Referral`：跨机构转诊单的父实体（脱敏；落点不落患者可识别明细）。
//!
//! 字段对齐 spec.md §3 最小数据格式；不持有子表，子表（版本链）由 `ReferralVersion`
//! 的 `referral_id` 外键聚合。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 跨机构转诊单父实体。
///
/// 该结构是「被观察的对象」，本工具不会主动修改其字段；修改事件以
/// `ReferralVersion` 形式聚合入链，由 `ReferralVersion.payload_sha256`
/// 保证不可静默篡改。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Referral {
    /// 区域平台业务 ID（跨机构唯一锚点）。
    pub referral_id: String,
    /// 院内就诊索引（脱敏到最小可定位粒度，不落患者可识别明细）。
    pub patient_ref: String,
    /// 上转机构名（来源医院 / 科室）。
    pub upstream_institution: String,
    /// 上转时的初步诊断文字（仅用于留痕回放展示，不参与业务判断）。
    pub diagnosis_initial: String,
    /// 创建时间（ISO8601，UTC）。
    pub created_at: DateTime<Utc>,
}

impl Referral {
    /// 仅做最小字段校验，避免在反序列化层外接受空 ID。
    pub fn is_well_formed(&self) -> bool {
        !self.referral_id.trim().is_empty()
            && !self.upstream_institution.trim().is_empty()
            && !self.patient_ref.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Referral {
        Referral {
            referral_id: "REF-2026-001".to_string(),
            patient_ref: "VISIT-9001".to_string(),
            upstream_institution: "某基层卫生院".to_string(),
            diagnosis_initial: "高血压待查".to_string(),
            created_at: DateTime::parse_from_rfc3339("2026-10-10T01:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn referral_well_formed_ok() {
        assert!(sample().is_well_formed());
    }

    #[test]
    fn referral_empty_id_rejected() {
        let mut r = sample();
        r.referral_id = "".to_string();
        assert!(!r.is_well_formed());
    }

    #[test]
    fn referral_serde_round_trip() {
        let r = sample();
        let s = serde_json::to_string(&r).unwrap();
        let r2: Referral = serde_json::from_str(&s).unwrap();
        assert_eq!(r, r2);
    }
}
