//! 链式 sha256 摘要模块。
//!
//! 公开 API 见 [`chain`] 子模块：
//! - [`chain::Chain`]：append-only 链
//! - [`chain::ChainLink`]：单条链节
//! - [`chain::canonical_json`]：字段排序固定、无空白的序列化
//! - [`chain::verify_chain`]：整链硬核对
//! - [`chain::ChainError`]：三类失败原因
//! - [`chain::SHA256_BYTES`] / [`chain::ZERO_HASH`]：链 anchor 常量
//!
//! 镜像源 `copper-rs/core/cu29_logstream/src/capture.rs::DigestWriter` 的「增量
//! 喂字节 + 单条 digest」语义，改写为「canonical_json + 链 + 全链路 verify」；
//! 算法 blake3 → sha256（详见 `chain.rs` 顶部「与源产品的差异」段）。

pub mod chain;

// 显式 re-export：UI / 报告导出 / verify 子命令直接引用 `crate::sha256_chain::*`。
pub use chain::{
    canonical_json, hex_lower, link_hash, verify_chain, Chain, ChainError, ChainLink, SHA256_BYTES,
    ZERO_HASH,
};