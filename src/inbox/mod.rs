//! 区域平台 drop 入站模块。
//!
//! 当前版本仅含 zip 三件套解析器（[`zip_parser`]）—— 区域平台推送的转诊单版本
//! 改动以 zip 形式落到 `inbox/<date>/*.zip`，由 inbox watcher（Task 10）调用
//! `parse_inbox_zip` 解出 before/after + signature 三件套，注入 cl_id 单调流。
//!
//! ## 与 spec.md / plan.md 的对应
//!
//! - spec.md §3 最小数据格式：before/after 是 `Map<str, str>` 的 JSON 表达
//! - spec.md §5 失败模式：「drop 缺签 → 拒绝生成核对包」由 `SignatureMissing` 守护
//! - plan.md §5 「drop 缺签」风险缓解：本模块是该风险点的硬关
//!
//! ## 不照搬
//!
//! - copper-rs 没有「外部 drop 入站」概念（它是进程内 CuLogEntry 流）
//! - 本模块填补区域平台推送 → cl_seq 单调流的桥

pub mod watcher;
pub mod zip_parser;

pub use watcher::{start_watching, InboxWatcherConfig, InboxWatcherHandle, WatcherError};
pub use zip_parser::{parse_inbox_zip, InboxZipError, ParsedInboxDrop, ENTRY_AFTER, ENTRY_BEFORE, ENTRY_SIGNATURE};