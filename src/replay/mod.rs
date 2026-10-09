//! 回放模块 — 把 `state/<referral_id>.copper`（JSON Lines，每行一条 `ChainLink`）
//! 解析回来，校验 sha256 链并渲染为可读时间线，供 CLI 的 `replay` 子命令消费。
//!
//! ## 源产品能力参考
//!
//! - `copper-rs/core/cu29_logstream/src/archive.rs::NativeArchive::accept_capture`：
//!   receiver-side 把每个事件 `log(&StreamContinuityRecord)` / `log(&CopperList)`，
//!   写入本地的 native archive。
//! - `copper-rs/core/cu29_export/src/fsck.rs`：消费 archive 的 `fsck` 主路径。
//!
//! ## 与源产品的差异
//!
//! - 源 archive 用 bincode + MmapSectionStorage 二进制格式；本任务按 `spec.md`
//!   §6 改成 **JSON Lines**（每行一条 `ChainLink`）—— 医务科人工读 + 跨环境
//!   传递 + 文本审计天然受益，且 `verify_chain` 重算仍是 sha256 链，与源
//!   `blake3 DigestWriter` 不同但语义等价（参见 [`crate::sha256_chain::chain`]）。
//! - 源 archive 内含铜版序列化二进制 CopperList；本任务把 `ReferralVersion` 直
//!   接 JSON 化进 `ChainLink.payload`，链结构本身保持「prev_hash || payload →
//!   sha256」。
//!
//! ## 不变量
//!
//! - **链 anchor**：首条 `prev_hash` 必须等于全 0 sha256 hex；
//! - **链衔接**：第 `i` 条的 `prev_hash` 必须等于第 `i-1` 条的 `this_hash`；
//! - **payload tamper**：重算 `sha256(prev_hash || canonical_json(payload))` 与链
//!   上 `this_hash` 不一致即视为 `RecoveryPoint required`，UI/报告都必须标
//!   `recovery_point_required = true`。
//!
//! ## 公开 API
//!
//! - [`ReplayRunner`]：读取 `.copper` → `ReplayReport` 的核心；
//! - [`TimelineRow`] / [`ReplayReport`] / [`ReplayError`]：调用方直接 re-export；
//! - [`render_text_report`]：CLI `replay` 子命令把人读可审计的文本写到 stdout。
//!
//! 调用方（Task 20 export / Task 21 verify）只用 `ReplayRunner::run` 与
//! `ReplayReport` 的字段；不直接接触 `.copper` 解析细节。

pub mod runner;

pub use runner::{
    render_text_report, ReplayConfig, ReplayError, ReplayReport, ReplayRunner, TimelineRow,
    COPPER_EXTENSION, COPPER_FILE_PREFIX, RECOVERY_POINT_REQUIRED_HINT,
};
