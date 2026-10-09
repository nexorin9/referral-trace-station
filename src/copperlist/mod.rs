//! `copperlist`：cl_seq 单调计数器 + 序列空洞检测（fsck）+ 持久化重排保护。
//!
//! 模块镜像源 copper-rs 的 `CopperListSequence::observe()`（fsck 序列空洞检测）
//! 与 `safety_case CLM-TEST-002 copperlist_ids_are_monotonic_across_reuse`
//! （cl_id 跨复用严格单调）。
//!
//! - [`cl_seq`]：每份转诊单一份内嵌 u64 单调计数器；提供 `alloc_next`（出站生成）
//!   与 `observe`（入站校验）；守住同源 `safety_case CLM-TEST-002` 的语义。
//! - [`cl_seq_store`]：把 `ClIdStore` 落盘到 `state/cl_seq.json`；支持重启续读 + 重排保护。
//! - [`fsck`]：序列空洞检测（`MAX_HOLE = 256`）+ `SourceGapReason` 三类归因 +
//!   `truncated` 截断标记；直译源 `core/cu29_export/src/fsck.rs::CopperListSequence`。

pub mod cl_seq;
pub mod cl_seq_store;
pub mod fsck;

pub use fsck::{CopperListSequence, FsckReport, Hole, MAX_HOLE};