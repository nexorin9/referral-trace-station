//! `StreamContinuityRecord` 四态适配与 replay 校验契约。
//!
//! 源产品：`copper-rs/core/cu29_runtime/src/continuity.rs`
//! - `StreamContinuityRecord<B = Vec<u8>>`：四态枚举（Manifest / Gap / RecoveryPoint / Finished）
//! - `validate_replay_continuity(expected, actual, keyframe)`：sender-side 校验
//! - `SourceGapReason`：RlcWindowExpired / SessionEnded / LateJoin / RecoveryPoint
//!
//! 本产品适配：
//! - `record: Vec<u8>` → `ReferralVersion`（即 `ContinuityKind` 嵌入 ReferralVersion.continuity 字段）
//! - 把 SourceGapReason 的 RlcWindowExpired / RecoveryPoint 替换为本产品的 `ManualDrop`
//!   （来源更准确：「drop 目录写入时人为指定丢弃」）
//! - 用 `validate_continuity_record` / `validate_replay_continuity` /
//!   `ensure_no_append_after_finished` 三个函数守源三条 contract：
//!     ① Gap 必须带 first/last/reason 三字段
//!     ② RecoveryPoint 后接收方对未观察发送端尾部不臆造
//!     ③ Finished 表示该 referral 流已结束
//!
//! 该模块不持有持久化或网络副作用；只承担「形状」校验 + 业务语义拒绝。

pub mod four_state;

// 业务层常用的同名列名 re-export，方便调用方直接写
// `use crate::continuity::{ContinuityKind, SourceGapReason, ContinuityRecord}`。
pub use crate::model::continuity_record::{ContinuityKind, ContinuityRecord, SourceGapReason};
pub use four_state::{
    ensure_no_append_after_finished, validate_continuity_record, validate_replay_continuity,
    ContinuityError,
};
