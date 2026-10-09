//! `verify` 子命令模块 — 解压 bundle.zip → 重算 sha256 链 → tamper 检测。
//!
//! 由 CLI `verify --zip <bundle.zip>` 触发；消费 `export` 子命令（Task 20）写出
//! 的 zip 核对包。报告 [`VerifyReport`] 含三向比对（manifest / chain.txt /
//! versions/*.json 任一不一致均标 tamper）。
//!
//! ## 源产品能力参考
//!
//! - 源 `copper-rs/core/cu29_export/src/exporter.rs` 反向路径：源 verifier 与
//!   exporter 共享二进制 archive + blake3 digest；本任务独立模块 + 文本 zip
//!   + sha256，**沿用** `sha256_chain::verify_chain` 的核心重算语义。
//!
//! ## 公开 API
//!
//! - [`Verifier`]：构造时配 [`VerifyConfig`]，调 `verify_zip(path)` 返回
//!   [`VerifyReport`]；
//! - [`VerifyReport`]：聚合 `chain_ok` / `recovery_point_required` /
//!   `tamper_locations` / `manifest_chain_head_hash` vs `recomputed_chain_head_hash`；
//! - [`VerifyError`]：硬错误（IO / entry 缺失 / 解析失败），CLI 层映射到
//!   `ExitCode::from(1)` + stderr；
//! - [`render_text_report`]：把 VerifyReport 渲染成多行人读可审计文本；
//! - [`TamperKind`] / [`TamperLocation`]：篡改类型 + 位置描述；
//! - [`VERIFY_VERSION`]：当前 verify 模块 schema 版本。
//!
//! 调用方：CLI `verify` 子命令（`cargo run -- verify --zip bundle.zip`）。
//! 后续 Task 28 集成测试 + Task 30 验收测试会复用本模块。

pub mod verifier;

pub use verifier::{
    render_text_report, TamperKind, TamperLocation, VerifyConfig, VerifyError, VerifyReport,
    Verifier, VERIFY_VERSION,
};