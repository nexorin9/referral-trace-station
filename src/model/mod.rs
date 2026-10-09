//! 数据结构层：转诊单业务实体与版本快照。
//!
//! 对应 spec.md §3 最小数据格式；该模块只承担「形状」声明，不持有持久化或副作用。

pub mod continuity_record;
pub mod referral;
pub mod referral_version;
