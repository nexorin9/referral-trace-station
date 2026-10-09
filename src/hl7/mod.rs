//! HL7 ORU^R01 简版解析模块。
//!
//! 三件套：
//! - [`oru`]: OruMessage 数据结构（MSH/PID/OBR/OBX 四段共 5 关键字段）
//! - [`parser`]: 解析函数 `parse_msh_pid_obr_obx` + [`OruError`]
//! - [`fallback`]: 解析失败时 mv 到 `inbox/unparsed/` 并打 `.unparsed` tag
//!
//! 设计取舍（spec.md / plan.md §2 一致）：
//! - 本轮只对接 HL7 ORU 简版，不实现完整 HL7 状态机
//! - 解析失败默认落 unparsed/ 并打 tag，不阻断主路径
//! - 成功后由 caller 落 ReferralVersionPayload 时戳 + 机构 + 报告 ID

pub mod fallback;
pub mod oru;
pub mod parser;

pub use fallback::{fallback_to_unparsed, FallbackError};
pub use oru::OruMessage;
pub use parser::{parse_msh_pid_obr_obx, OruError};
