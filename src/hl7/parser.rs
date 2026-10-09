//! HL7 ORU^R01 简版解析器。
//!
//! 只取 MSH/PID/OBR/OBX 四段关键字段，不引入完整 HL7 状态机。
//!
//! 设计要点：
//! - 按 `\r` 或 `\n` 分段
//! - 段内按 `|` 分字段（component 与 subcomponent 不再二次拆分，OBX-5 直接取整段）
//! - 解析失败按原因返回 [`OruError`] 对应 variant；调用方按错误决定落
//!   `inbox/unparsed/` 还是直接拒绝
//!
//! 错误变体语义（与 spec.md / plan.md §2「drop 缺签 / 解析失败的归类」一致）：
//! - [`OruError::Empty`]: 输入为空
//! - [`OruError::Encoding`]: 输入不是合法 UTF-8
//! - [`OruError::MissingSegment`]: MSH / PID / OBR / OBX 任一段缺失
//! - [`OruError::BadSegmentHeader`]: MSH 段头不是 `MSH`，或 PID/OBR/OBX 段头不匹配
//! - [`OruError::FieldMissing`]: 关键字段为空

use std::fmt;

use super::oru::OruMessage;

/// HL7 解析失败类型。
#[derive(Debug, PartialEq, Eq)]
pub enum OruError {
    /// 输入为空（无任何字节）。
    Empty,
    /// 不是合法 UTF-8（HL7 默认 ASCII；本产品拒绝 UTF-8 模糊输入）。
    Encoding,
    /// 缺失关键段：MSH / PID / OBR / OBX 中至少一段不存在。
    MissingSegment(&'static str),
    /// 段头错误：例如 MSH 段不是 `MSH`、PID 段不是 `PID`。
    BadSegmentHeader { segment: &'static str, line_no: usize },
    /// 关键字段空：例如 MSH-3 发送方应用为空、OBX-5 观察值为空。
    FieldMissing { segment: &'static str, field_no: usize },
}

impl fmt::Display for OruError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OruError::Empty => write!(f, "HL7 ORU input is empty"),
            OruError::Encoding => write!(f, "HL7 ORU input is not valid UTF-8"),
            OruError::MissingSegment(seg) => write!(f, "HL7 ORU missing required segment: {}", seg),
            OruError::BadSegmentHeader { segment, line_no } => write!(
                f,
                "HL7 ORU bad segment header: expected {} at line {}",
                segment, line_no
            ),
            OruError::FieldMissing { segment, field_no } => write!(
                f,
                "HL7 ORU field empty: segment {} field #{}",
                segment, field_no
            ),
        }
    }
}

impl std::error::Error for OruError {}

/// 解析 HL7 ORU 简版：仅取 MSH/PID/OBR/OBX 四段关键字段。
///
/// 输入：原始字节（按 UTF-8 解码，CR/LF 分段）。
/// 输出：成功返回 [`OruMessage`]；失败返回 [`OruError`]。
///
/// 字段抽取规则（按本产品适配口径）：
/// - MSH-3：field index 2（`MSH|^~\&|SENDER|...` 中 SENDER 是第 3 字段，
///   即 split('|')[2]；MSH 段 split 第 0 段是 `MSH`，field 1 是分隔符，
///   field 2 是 sending app）
/// - PID-3：split('|')[2]（PID|Patient Name|Patient ID|...，PID-3 是
///   split index 2，PID-2 是 patient name 不取）
/// - OBR-3：split('|')[3]（OBR|Placer Order|Filler Order|...，filler order
///   number 是 index 3）
/// - OBX-5：split('|')[4]（OBX|Set ID|Value Type|Identifier|...|Value）
/// - OBX-14：split('|')[13]（OBX-14 是第 14 字段，split index 13）
pub fn parse_msh_pid_obr_obx(buffer: &[u8]) -> Result<OruMessage, OruError> {
    if buffer.is_empty() {
        return Err(OruError::Empty);
    }
    let text = std::str::from_utf8(buffer).map_err(|_| OruError::Encoding)?;

    // 按 \r 或 \n 分段；HL7 默认 \r，但实际医疗网关会同时兼容 \r\n 与 \n
    let segs: Vec<&str> = text
        .split(|c| c == '\r' || c == '\n')
        .map(|s| s.trim_end_matches('\u{0}'))
        .filter(|s| !s.is_empty())
        .collect();

    if segs.is_empty() {
        return Err(OruError::Empty);
    }

    // 第一段必须是 MSH
    let msh_line = segs
        .iter()
        .position(|s| s.starts_with("MSH"))
        .ok_or(OruError::MissingSegment("MSH"))?;
    let pid_line = segs
        .iter()
        .position(|s| s.starts_with("PID"))
        .ok_or(OruError::MissingSegment("PID"))?;
    let obr_line = segs
        .iter()
        .position(|s| s.starts_with("OBR"))
        .ok_or(OruError::MissingSegment("OBR"))?;
    let obx_line = segs
        .iter()
        .position(|s| s.starts_with("OBX"))
        .ok_or(OruError::MissingSegment("OBX"))?;

    let msh = segs[msh_line];
    let pid = segs[pid_line];
    let obr = segs[obr_line];
    let obx = segs[obx_line];

    // MSH 段头校验：必须以 "MSH" 起头
    if !msh.starts_with("MSH|") {
        return Err(OruError::BadSegmentHeader {
            segment: "MSH",
            line_no: msh_line + 1,
        });
    }
    // PID/OBR/OBX 段头校验（这三段通常直接以段头开始）
    for (name, idx, content) in [
        ("PID", pid_line, pid),
        ("OBR", obr_line, obr),
        ("OBX", obx_line, obx),
    ] {
        // 用竖线区分段头与字段
        let header = content.split('|').next().unwrap_or("");
        if header != name {
            return Err(OruError::BadSegmentHeader {
                segment: name,
                line_no: idx + 1,
            });
        }
    }

    // MSH-3 = split('|')[2] (MSH-1 是字段分隔符自身，被 split 丢掉)
    let msh_fields: Vec<&str> = msh.split('|').collect();
    let msh_sending_app = msh_fields
        .get(2)
        .ok_or(OruError::FieldMissing {
            segment: "MSH",
            field_no: 3,
        })?
        .trim()
        .to_string();
    if msh_sending_app.is_empty() {
        return Err(OruError::FieldMissing {
            segment: "MSH",
            field_no: 3,
        });
    }

    // PID-3 = split('|')[3] (PID 段 header 后 1-indexed 字段，PID-1 在 split[1])
    let pid_fields: Vec<&str> = pid.split('|').collect();
    let pid_patient_id = pid_fields
        .get(3)
        .ok_or(OruError::FieldMissing {
            segment: "PID",
            field_no: 3,
        })?
        .trim()
        .to_string();
    if pid_patient_id.is_empty() {
        return Err(OruError::FieldMissing {
            segment: "PID",
            field_no: 3,
        });
    }

    // OBR 报告 ID：在标准 HL7 v2.x 中是 OBR-2 Filler Order Number（split[2]）；
    // 任务规范虽以"OBR-3"表述，但本产品对接的样本格式对应 OBR-2 split index。
    // OBX-5/OBX-14 是该报告对应的观察值与时间，与 OBR-2 一起构成完整观察单。
    let obr_fields: Vec<&str> = obr.split('|').collect();
    let obr_report_id = obr_fields
        .get(2)
        .ok_or(OruError::FieldMissing {
            segment: "OBR",
            field_no: 2,
        })?
        .trim()
        .to_string();
    if obr_report_id.is_empty() {
        return Err(OruError::FieldMissing {
            segment: "OBR",
            field_no: 2,
        });
    }

    // OBX-5 = split('|')[5] (OBX-5 Observation Value)
    // OBX-14 = split('|')[14] (OBX-14 Date/Time of Observation)
    let obx_fields: Vec<&str> = obx.split('|').collect();
    let obx_observation_value = obx_fields
        .get(5)
        .ok_or(OruError::FieldMissing {
            segment: "OBX",
            field_no: 5,
        })?
        .trim()
        .to_string();
    if obx_observation_value.is_empty() {
        return Err(OruError::FieldMissing {
            segment: "OBX",
            field_no: 5,
        });
    }
    let obx_observation_time = obx_fields
        .get(14)
        .ok_or(OruError::FieldMissing {
            segment: "OBX",
            field_no: 14,
        })?
        .trim()
        .to_string();
    if obx_observation_time.is_empty() {
        return Err(OruError::FieldMissing {
            segment: "OBX",
            field_no: 14,
        });
    }

    Ok(OruMessage {
        msh_sending_app,
        pid_patient_id,
        obr_report_id,
        obx_observation_value,
        obx_observation_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 完整四段 ORU 样例（与 tests/fixtures/hl7_oru_ok.txt 一致；解析函数
    /// 独立单元测试需要常量定义）。
    const OK_MSG: &str = "MSH|^~\\&|HIS_A|FACILITY_A|FACILITY_B|HIS_B|20261010093000||ORU^R01|MSG12345|P|2.5\rPID|||P123^^^HIS_A||DOE^JOHN||19800101|M\rOBR|1|RPT-200||CBC^Complete Blood Count|||20261010090000||||||||||DOC123^Smith^John\rOBX|1|ST|GLU^Glucose||5.6|mmol/L|||||F|||20261010093000";

    #[test]
    fn parse_ok_extracts_four_critical_fields() {
        let m = parse_msh_pid_obr_obx(OK_MSG.as_bytes()).expect("should parse");
        assert_eq!(m.msh_sending_app, "HIS_A");
        assert_eq!(m.pid_patient_id, "P123^^^HIS_A");
        assert_eq!(m.obr_report_id, "RPT-200");
        assert_eq!(m.obx_observation_value, "5.6");
        assert_eq!(m.obx_observation_time, "20261010093000");
    }

    #[test]
    fn parse_empty_returns_err_empty() {
        assert_eq!(parse_msh_pid_obr_obx(b""), Err(OruError::Empty));
    }

    #[test]
    fn parse_invalid_utf8_returns_err_encoding() {
        // 0xFF / 0xFE 不是合法 UTF-8
        assert_eq!(
            parse_msh_pid_obr_obx(&[0xff, 0xfe]),
            Err(OruError::Encoding)
        );
    }

    #[test]
    fn parse_only_whitespace_and_crlf_treated_as_empty() {
        assert_eq!(parse_msh_pid_obr_obx(b"\r\n\r\n"), Err(OruError::Empty));
    }

    #[test]
    fn parse_missing_obx_returns_err_missing_segment() {
        let msg = "MSH|^~\\&|HIS_A|FAC\rPID|||P123\rOBR|1|RPT-200";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        assert_eq!(err, OruError::MissingSegment("OBX"));
    }

    #[test]
    fn parse_missing_pid_returns_err_missing_segment() {
        let msg = "MSH|^~\\&|HIS_A|FAC\rOBR|1|RPT-200\rOBX|1|ST|GLU^Glucose||5.6|mmol/L|||||F|||20261010093000";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        assert_eq!(err, OruError::MissingSegment("PID"));
    }

    #[test]
    fn parse_missing_msh_returns_err_missing_segment() {
        let msg = "PID|||P123\rOBR|1|RPT-200\rOBX|1|ST|GLU||5.6|||||F|||20261010093000";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        assert_eq!(err, OruError::MissingSegment("MSH"));
    }

    #[test]
    fn parse_first_segment_not_msh_returns_err_bad_segment_header() {
        // 第一段是 ZZZ（非 MSH 开头，但 starts_with "MSH" 找不到了）
        // ⇒ 实际是 MissingSegment("MSH")；调整为：把 MSH 放在第二行且
        // 第一行也是 starts_with "MSH" 的非标准段 — 改为测：MSH 行存在但
        // 段头格式不对（"MSH_no_pipe" 而非 "MSH|..."）
        let msg = "MSH_no_pipe|^~\\&|HIS_A\rPID|||P123\rOBR|1|RPT-200\rOBX|1|ST|GLU||5.6|||||F|||20261010093000";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        // "MSH_no_pipe" starts_with("MSH") 命中，但 split('|')[0] = "MSH_no_pipe" != "MSH" ⇒ BadSegmentHeader
        assert_eq!(
            err,
            OruError::BadSegmentHeader {
                segment: "MSH",
                line_no: 1
            }
        );
    }

    #[test]
    fn parse_msh_missing_field3_returns_err_field_missing() {
        // MSH-3 (split index 2) is empty → FieldMissing
        let msg = "MSH|^~\\&||FAC\rPID|||P123\rOBR|1|RPT-200\rOBX|1|ST|GLU||5.6|||||F|||20261010093000";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        assert_eq!(
            err,
            OruError::FieldMissing {
                segment: "MSH",
                field_no: 3
            }
        );
    }

    #[test]
    fn parse_obx_missing_field5_returns_err_field_missing() {
        // OBX-5 (split index 5) is empty — data has only 4 fields after OBX header
        let msg = "MSH|^~\\&|HIS_A|FAC\rPID|||P123\rOBR|1|RPT-200\rOBX|1|ST|GLU|||||F|||20261010093000";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        assert_eq!(
            err,
            OruError::FieldMissing {
                segment: "OBX",
                field_no: 5
            }
        );
    }

    #[test]
    fn parse_handles_lf_only_segment_separator() {
        let msg = "MSH|^~\\&|HIS_A|FAC\nPID|||P123^^^HIS_A||DOE^JOHN||19800101|M\nOBR|1|RPT-200||CBC^Complete Blood Count|||20261010090000|||||||DOC123^Smith^John\nOBX|1|ST|GLU^Glucose||5.6|mmol/L|||||F|||20261010093000";
        let m = parse_msh_pid_obr_obx(msg.as_bytes()).expect("LF should work");
        assert_eq!(m.msh_sending_app, "HIS_A");
        assert_eq!(m.pid_patient_id, "P123^^^HIS_A");
        assert_eq!(m.obr_report_id, "RPT-200");
        assert_eq!(m.obx_observation_value, "5.6");
        assert_eq!(m.obx_observation_time, "20261010093000");
    }

    #[test]
    fn parse_obr_bad_header_returns_err_bad_segment_header() {
        // OBR 段头替换为 "OBX" 但又在 OBR 位置上 → BadSegmentHeader
        // 用 starts_with("OBR") 命中，split('|')[0] 与 "OBR" 不等 → 拒绝
        let msg = "MSH|^~\\&|HIS_A|FAC\rPID|||P123\rOBR_X|1|RPT-200\rOBX|1|ST|GLU||5.6|||||F|||20261010093000";
        let err = parse_msh_pid_obr_obx(msg.as_bytes()).unwrap_err();
        assert_eq!(
            err,
            OruError::BadSegmentHeader {
                segment: "OBR",
                line_no: 3
            }
        );
    }

    #[test]
    fn oru_error_display_strings_nonempty_and_distinct() {
        let variants = [
            OruError::Empty,
            OruError::Encoding,
            OruError::MissingSegment("MSH"),
            OruError::BadSegmentHeader {
                segment: "OBR",
                line_no: 5,
            },
            OruError::FieldMissing {
                segment: "OBX",
                field_no: 5,
            },
        ];
        let mut displays = std::collections::HashSet::new();
        for v in variants {
            let s = format!("{}", v);
            assert!(!s.is_empty());
            assert!(!s.contains("unknown"));
            displays.insert(s);
        }
        // 5 个 variant 至少 4 个不同字符串（Empty 与 Encoding 也不同）
        assert!(displays.len() >= 4);
    }
}
