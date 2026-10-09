//! HL7 ORU^R01 简版消息结构。
//!
//! 仅承载「对接区域平台 HL7 ORU 落 ReferralVersionPayload 时戳 + 机构 +
//! 报告 ID」所必需的字段，不实现完整 HL7 状态机。
//!
//! 字段定义对齐 spec.md §3 / plan.md §2：
//! - msh_sending_app: 发送方应用（MSH-3）
//! - pid_patient_id: 患者 ID（PID-3）
//! - obr_report_id: 报告 / 检查 ID（OBR-3，filler order number）
//! - obx_observation_value: 观察值（OBX-5）
//! - obx_observation_time: 观察时间（OBX-14）
//!
//! 设计取舍：上述字段是转诊单留痕回放工作台用于「知道报告来自哪家机构、
//! 给谁、什么时候、关键报告 ID 是多少」所需的最小集合。spec.md 与 plan.md
//! §2 均明确：本轮只对接 ORU 简版；后续任务（FHIR 字段映射）独立处理。

use serde::{Deserialize, Serialize};

/// HL7 ORU^R01 简版消息结构。
///
/// 字段命名遵循「段名_字段业务含义」风格，便于代码审计时直接对应 HL7 段位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OruMessage {
    /// 发送方应用代码（MSH-3）。
    pub msh_sending_app: String,
    /// 患者 ID（PID-3，假定格式 `院内ID^^^发送方`）。
    pub pid_patient_id: String,
    /// 报告 / 检查 ID（OBR-3 filler order number）。
    pub obr_report_id: String,
    /// 观察值（OBX-5，本轮只取第一段 OBX）。
    pub obx_observation_value: String,
    /// 观察时间（OBX-14，UTC ISO8601，原始 HL7 格式 `YYYYMMDDHHMMSS`）。
    pub obx_observation_time: String,
}

impl OruMessage {
    /// 构造测试用完整 ORU。
    #[cfg(test)]
    pub fn for_test(
        msh_sending_app: &str,
        pid_patient_id: &str,
        obr_report_id: &str,
        obx_observation_value: &str,
        obx_observation_time: &str,
    ) -> Self {
        Self {
            msh_sending_app: msh_sending_app.to_string(),
            pid_patient_id: pid_patient_id.to_string(),
            obr_report_id: obr_report_id.to_string(),
            obx_observation_value: obx_observation_value.to_string(),
            obx_observation_time: obx_observation_time.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oru_message_for_test_constructs_all_fields() {
        let m = OruMessage::for_test("HIS_A", "P123", "RPT-200", "OK", "20261010093000");
        assert_eq!(m.msh_sending_app, "HIS_A");
        assert_eq!(m.pid_patient_id, "P123");
        assert_eq!(m.obr_report_id, "RPT-200");
        assert_eq!(m.obx_observation_value, "OK");
        assert_eq!(m.obx_observation_time, "20261010093000");
    }

    #[test]
    fn oru_message_serde_round_trip() {
        let m = OruMessage::for_test("HIS_A", "P123", "RPT-200", "OK", "20261010093000");
        let j = serde_json::to_string(&m).unwrap();
        let back: OruMessage = serde_json::from_str(&j).unwrap();
        assert_eq!(back, m);
    }
}
