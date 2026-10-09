//! 转诊单核对包导出模块（BundleWriter）。
//!
//! 把 `state/<referral_id>.copper`（JSON Lines 链节存档）+ `ReplayReport`
//! （已校验 + 时间线渲染过的中间产物）打成一个 **可脱敏审计的 zip 包**，给
//! 医务科 / 医保办 / 接收方医院人工签收用。
//!
//! ## 源产品能力参考
//!
//! - `copper-rs/core/cu29_export/src/exporter.rs`：源产品把 CopperList 二进
//!   制 archive 导出成可移植 artifact（含 manifest + 序列化 payload + 链
//!   摘要）。本任务按 `spec.md` §6 改成「**manifest.json + versions/\<cl_id\>
//!   .json + chain.txt** 三类文本条目」，与源 binary artifact 形态不同，但
//!   「可移植 + 可核对 + 链式 sha256」语义等价。
//!
//! ## 与源产品的差异
//!
//! - 源 artifact 是二进制；本任务产**纯文本 zip** —— `manifest.json` /
//!   `versions/<cl_id>.json` / `chain.txt` 三类条目均 UTF-8 可读、医务科
//!   人工 audit 不用解码。
//! - 源 artifact 链摘要用 `blake3`；本任务沿 Task 7 用 **sha256**（行业合规
//!   偏好 + OpenSSL 自带 + 与 `verify` 子命令同源）。
//!
//! ## 不变量
//!
//! - **完整性**：`versions/` 内的 `<cl_id>.json` 与 `.copper` 内 `chain_link.
//!   payload` 必须逐字节一致 —— 否则 `verify` 会在重算 sha256 时报
//!   `ThisHashMismatch`（即 tamper 检测必拦）。
//! - **链 anchor**：`chain.txt` 第一行的 `prev_hash` 必为全 0 hex；
//!   `verify_chain` 重算 `link[i].prev_hash == link[i-1].this_hash` 全衔
//!   接才视为「整链无篡改」。
//! - **可脱敏**：所有版本快照只含字段级 `before/after` + `actor`（已规范
//!   为 `姓名 / 机构`），不出现完整 sha256 / 真实身份证号 / 真实电话。
//!
//! ## 公开 API
//!
//! - [`BundleWriter`]：消费 `&[ChainLink]` + `referral_id` → 写出 zip 到磁盘；
//! - [`BundleManifest`]：写入 `manifest.json` 的结构体（referral_id / 时间
//!   范围 / 版本数 / 链头 hash / 导出时间戳）；
//! - [`BundleError`]：失败原因（IO / zip / 序列化 / 空链）；`Display` 全部
//!   非空且不出现 `"unknown"` 字样。
//!
//! 调用方：CLI `export` 子命令（`cargo run -- export --referral X --out
//! bundle.zip`）。后续 Task 21 `verify` 子命令消费该 zip。

pub mod bundle;

pub use bundle::{
    render_chain_text, BundleConfig, BundleError, BundleManifest, BundleWriter,
    BUNDLE_MANIFEST_NAME, BUNDLE_VERSIONS_DIR, CHAIN_TXT_NAME, END_OF_CHAIN_MARKER,
    MANIFEST_CHAIN_HEAD_FIELD, MANIFEST_CREATED_AT_FIELD, MANIFEST_REFERRAL_FIELD,
    MANIFEST_TIME_RANGE_FIELD, MANIFEST_VERSION_COUNT_FIELD,
};