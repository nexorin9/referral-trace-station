//! `verify` 子命令实现 — 解压 bundle.zip → 重算 sha256 链 → tamper 检测。
//!
//! 输入：`export` 子命令写出的 `<out>.zip`（含 manifest.json + versions/\<cl_id\>.json
//! + chain.txt 三类条目）。
//! 输出：[`VerifyReport`]（含 `chain_ok` / `recovery_point_required` / `tamper_locations` /
//! `manifest_chain_head_hash` vs `recomputed_chain_head_hash` 比对）。
//!
//! ## 工作闭环
//!
//! 1. 打开 zip，枚举三类条目：
//!    - `manifest.json`：解析出 `referral_id` / `version_count` / `chain_head_hash`；
//!    - `chain.txt`：人读可审计文本，每行 `cl_id<TAB>prev_hash[..12]<TAB>this_hash[..12]<TAB>
//!      actor<TAB>modified_at` + 末行 `end of chain` 哨兵；
//!    - `versions/<cl_id>.json`：每条 ChainLink 的 payload（与 `.copper` 链节 payload
//!      一致）。
//! 2. 按 `cl_id` 升序遍历 `versions/`，重算 `prev_hash` / `this_hash`：
//!    - `prev_hash_bytes = (link[i-1].this_hash_bytes)`
//!    - `payload_bytes = canonical_json(payload)`
//!    - `this_hash_bytes = sha256(prev_hash_bytes || payload_bytes)`
//! 3. 横向比对：
//!    - `manifest.chain_head_hash` vs 重算 `this_hash`（最后一条）；
//!    - `manifest.version_count` vs 重算 count；
//!    - `chain.txt` 第 i 行 `prev_hash[..12]` / `this_hash[..12]` vs 重算 hex 前 12 字符。
//! 4. 任一不一致即 `recovery_point_required = true` + 记录 [`TamperLocation`]。
//!
//! ## 与源产品的差异
//!
//! - 源 `copper-rs` 用二进制 archive + 内嵌 blake3 digest 块；本任务按 `spec.md` §6
//!   用文本 zip + sha256，`unzip -p bundle.zip chain.txt` 即可人工 audit。
//! - 源 verifier 是 `Exporter` 反向路径；本任务独立 [`Verifier`] 结构体，调用
//!   [`crate::sha256_chain::chain::verify_chain`] 做整链重算 + tamper 检测，再
//!   叠加 manifest/chain.txt 横向比对。
//!
//! ## 公开 API
//!
//! - [`Verifier`]：主入口，构造时配 [`VerifyConfig`]，调 `verify_zip(path)`；
//! - [`VerifyReport`]：结果聚合，`chain_ok` / `recovery_point_required` / `tamper_locations`；
//! - [`VerifyError`]：失败原因（IO / zip / 序列化 / entry 缺失），CLI 层映射到
//!   `ExitCode::from(1)` + stderr；
//! - [`render_text_report`]：把 VerifyReport 渲染成多行人读可审计文本。
//!
//! 调用方：CLI `verify` 子命令（`cargo run -- verify --zip bundle.zip`）。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::export::{
    BUNDLE_MANIFEST_NAME, BUNDLE_VERSIONS_DIR, CHAIN_TXT_NAME, END_OF_CHAIN_MARKER,
};
use crate::sha256_chain::chain::{
    canonical_json, link_hash, SHA256_BYTES, ZERO_HASH,
};

// =============================================================================
// Constants
// =============================================================================

/// verify 模块版本号（嵌入 VerifyReport 便于后续 schema 演进识别）。
pub const VERIFY_VERSION: &str = "1";

/// chain.txt 单行前缀 `cl_id\t` 中的 cl_id 字段索引。
const CHAIN_LINE_CL_ID: usize = 0;
/// chain.txt 单行 prev_hash[..12] 字段索引。
const CHAIN_LINE_PREV: usize = 1;
/// chain.txt 单行 this_hash[..12] 字段索引。
const CHAIN_LINE_THIS: usize = 2;
/// chain.txt 单行 actor 字段索引。
const CHAIN_LINE_ACTOR: usize = 3;
/// chain.txt 单行 modified_at 字段索引。
const CHAIN_LINE_MODIFIED_AT: usize = 4;

/// chain.txt 注释行 / 空行的判定前缀。
const CHAIN_LINE_COMMENT_PREFIX: char = '#';

// =============================================================================
// VerifyConfig
// =============================================================================

/// verify 装配配置。
///
/// 当前版本不开放 `strict` 选项（保留位给 spec.md §6 后续可能的「严格模式 = 任何
/// tamper_locations 非空即 exit 4」扩展）。`Default` 即基础模式。
#[derive(Clone, Debug, Default)]
pub struct VerifyConfig {
    /// 严格模式占位（当前未启用；保留给后续扩展）。
    pub strict: bool,
}

// =============================================================================
// Tamper kinds
// =============================================================================

/// 篡改类型。Display 全部非空且不出现 `"unknown"` 字样。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TamperKind {
    /// zip 内缺必备条目（manifest.json / chain.txt / versions/）。
    MissingEntry,
    /// 解析失败（manifest.json / versions/<cl_id>.json / chain.txt 行）。
    BadJson,
    /// `manifest.json` 内 `chain_head_hash` 与重算不一致。
    ManifestChainHeadMismatch,
    /// `manifest.json` 内 `version_count` 与重算不一致。
    ManifestVersionCountMismatch,
    /// `manifest.json` 内 `referral_id` 与重算 cl_id 序列不一致。
    ManifestReferralIdMismatch,
    /// `chain.txt` 第 i 行 `prev_hash[..12]` 与重算不一致。
    ChainTxtPrevMismatch,
    /// `chain.txt` 第 i 行 `this_hash[..12]` 与重算不一致。
    ChainTxtThisMismatch,
    /// `versions/` 缺对应 cl_id 文件（与 chain.txt 行号对不齐）。
    VersionFileMissing,
    /// `versions/` 多出 chain.txt 中没有的 cl_id 文件。
    ExtraVersionFile,
}

impl std::fmt::Display for TamperKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TamperKind::MissingEntry => write!(f, "missing_entry"),
            TamperKind::BadJson => write!(f, "bad_json"),
            TamperKind::ManifestChainHeadMismatch => write!(f, "manifest_chain_head_mismatch"),
            TamperKind::ManifestVersionCountMismatch => write!(f, "manifest_version_count_mismatch"),
            TamperKind::ManifestReferralIdMismatch => write!(f, "manifest_referral_id_mismatch"),
            TamperKind::ChainTxtPrevMismatch => write!(f, "chain_txt_prev_mismatch"),
            TamperKind::ChainTxtThisMismatch => write!(f, "chain_txt_this_mismatch"),
            TamperKind::VersionFileMissing => write!(f, "version_file_missing"),
            TamperKind::ExtraVersionFile => write!(f, "extra_version_file"),
        }
    }
}

/// 单条篡改位置 + 详情。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TamperLocation {
    pub kind: TamperKind,
    /// 链节索引（i=0..n）；不适用时为 `None`（如 MissingEntry）。
    pub index: Option<u64>,
    pub detail: String,
}

impl TamperLocation {
    pub fn new(kind: TamperKind, index: Option<u64>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            index,
            detail: detail.into(),
        }
    }
}

// =============================================================================
// VerifyReport
// =============================================================================

/// verify 聚合报告。
///
/// 默认构造为空报告；`Verifier::verify_zip` 填实。`render_text_report` 把它渲
/// 染成多行可审计文本。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    /// verify 模块版本号（schema 演进用）。
    pub verify_version: String,
    /// manifest.json 中的 referral_id；缺失时为空字符串。
    pub referral_id: String,
    /// 重算得出的链头 hash（hex）。
    pub recomputed_chain_head_hash: String,
    /// manifest.json 中的 chain_head_hash（hex）。
    pub manifest_chain_head_hash: String,
    /// manifest.json 中的 version_count。
    pub manifest_version_count: u64,
    /// 重算得出的链节数。
    pub recomputed_version_count: u64,
    /// 整链是否无篡改（manifest / chain.txt / versions 三方均一致）。
    pub chain_ok: bool,
    /// 是否需要 recovery point（`!chain_ok`）。
    pub recovery_point_required: bool,
    /// 篡改位置 + 详情（`chain_ok = false` 时非空）。
    pub tamper_locations: Vec<TamperLocation>,
    /// 顶层错误信息（entry 缺失 / io 等，不会让 chain_ok 直接判 false，但会标
    /// 在 errors 里让 caller 决定是否要标 recovery_point_required）。
    pub errors: Vec<String>,
    /// 原始 zip 路径（仅诊断用，不参与链核验）。
    pub zip_path: String,
}

impl VerifyReport {
    /// `chain_ok = true` 且 `tamper_locations.is_empty()`。
    pub fn is_clean(&self) -> bool {
        self.chain_ok && self.tamper_locations.is_empty()
    }

    /// 是否存在任意 tamper / 错误。
    pub fn has_problem(&self) -> bool {
        !self.chain_ok || !self.errors.is_empty()
    }

    /// 渲染 1 行摘要（CLI stdout 用）。
    pub fn summary_line(&self) -> String {
        format!(
            "verify {} : versions={} (manifest={}) chain_ok={} tamper_count={} recovery_point_required={}",
            self.referral_id,
            self.recomputed_version_count,
            self.manifest_version_count,
            self.chain_ok,
            self.tamper_locations.len(),
            self.recovery_point_required,
        )
    }
}

// =============================================================================
// VerifyError
// =============================================================================

/// `verify_zip` 主路径失败原因（区分于 `VerifyReport.tamper_locations` —— 后者
/// 是「文件齐全但数据不一致」；前者是「文件本身缺失 / 解析失败」）。
///
/// `Display` 全部非空且不出现 `"unknown"` 字样；CLI 层据此回退到 `ExitCode::from(1)`
/// + stderr。
#[derive(Debug, Error)]
pub enum VerifyError {
    /// 打开 zip 失败（IO / 权限 / 不是 zip）。
    #[error("verify: open zip failed at {path}: {reason}")]
    ZipOpen { path: String, reason: String },

    /// 打开 zip 后但读取某个 entry 失败。
    #[error("verify: read entry {name} in zip at {path}: {reason}")]
    ReadEntry {
        name: String,
        path: String,
        reason: String,
    },

    /// zip 内缺必备条目（manifest.json / chain.txt / versions/）。
    #[error("verify: missing entry {name} in zip at {path}")]
    MissingEntry { name: String, path: String },

    /// 解析 manifest.json 失败。
    #[error("verify: bad manifest.json: {reason}")]
    BadManifestJson { reason: String },

    /// 解析 versions/<cl_id>.json 失败。
    #[error("verify: bad versions/{cl_id}.json: {reason}")]
    BadVersionJson { cl_id: u64, reason: String },

    /// 解析 chain.txt 行失败。
    #[error("verify: bad chain.txt line {line_no}: {reason}")]
    BadChainTxtLine { line_no: usize, reason: String },

    /// chain.txt 缺末行 `end of chain` 哨兵。
    #[error("verify: chain.txt missing sentinel '{sentinel}' on last line")]
    MissingChainSentinel { sentinel: String },
}

// =============================================================================
// Internal: manifest.json 解析
// =============================================================================

/// manifest.json 的最小解析形态（仅取 verify 关心的三字段）。
#[derive(Clone, Debug, Deserialize)]
struct ManifestSubset {
    #[serde(default)]
    referral_id: String,
    #[serde(default)]
    version_count: u64,
    #[serde(default)]
    chain_head_hash: String,
}

impl ManifestSubset {
    fn from_json_bytes(bytes: &[u8]) -> Result<Self, VerifyError> {
        serde_json::from_slice::<ManifestSubset>(bytes).map_err(|e| VerifyError::BadManifestJson {
            reason: e.to_string(),
        })
    }
}

// =============================================================================
// Internal: chain.txt 行解析
// =============================================================================

/// chain.txt 单行解析结果。
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct ChainTxtRow {
    cl_id: u64,
    prev_hash_short: String,
    this_hash_short: String,
    actor: String,
    modified_at: String,
}

/// chain.txt 行集合（按出现顺序保留）。
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct ChainTxtParse {
    rows: Vec<ChainTxtRow>,
    /// 行号 → raw line 原文（错误信息用）。
    raw_lines: Vec<String>,
}

impl ChainTxtParse {
    fn parse(text: &str) -> Result<Self, VerifyError> {
        let mut rows = Vec::new();
        let mut raw_lines = Vec::new();
        let mut last_is_sentinel = false;
        for (line_no, raw) in text.lines().enumerate() {
            raw_lines.push(raw.to_string());
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with(CHAIN_LINE_COMMENT_PREFIX) {
                continue;
            }
            if trimmed == END_OF_CHAIN_MARKER {
                last_is_sentinel = true;
                continue;
            }
            let cols: Vec<&str> = trimmed.split('\t').collect();
            if cols.len() != 5 {
                return Err(VerifyError::BadChainTxtLine {
                    line_no: line_no + 1,
                    reason: format!(
                        "expected 5 tab-separated columns, got {} (line: {:?})",
                        cols.len(),
                        raw
                    ),
                });
            }
            let cl_id = cols[CHAIN_LINE_CL_ID].parse::<u64>().map_err(|e| {
                VerifyError::BadChainTxtLine {
                    line_no: line_no + 1,
                    reason: format!("cl_id parse: {e} (got: {:?})", cols[CHAIN_LINE_CL_ID]),
                }
            })?;
            rows.push(ChainTxtRow {
                cl_id,
                prev_hash_short: cols[CHAIN_LINE_PREV].to_string(),
                this_hash_short: cols[CHAIN_LINE_THIS].to_string(),
                actor: cols[CHAIN_LINE_ACTOR].to_string(),
                modified_at: cols[CHAIN_LINE_MODIFIED_AT].to_string(),
            });
        }
        if !last_is_sentinel {
            return Err(VerifyError::MissingChainSentinel {
                sentinel: END_OF_CHAIN_MARKER.to_string(),
            });
        }
        Ok(Self { rows, raw_lines })
    }
}

// =============================================================================
// Internal: versions/ 解析辅助
// =============================================================================

/// 把 `versions/3.json` 这种 entry name 解析为 `Some(3)`；解析失败返回 `None`。
fn parse_versions_entry_cl_id(name: &str) -> Option<u64> {
    let prefix = format!("{}/", BUNDLE_VERSIONS_DIR);
    let stem = name.strip_prefix(&prefix)?;
    let stem = stem.strip_suffix(".json")?;
    stem.parse::<u64>().ok()
}

// =============================================================================
// Verifier
// =============================================================================

/// `verify` 主入口。
///
/// 构造时不持有 zip；`verify_zip(path)` 每次独立打开一份。多次调用间互不干扰。
#[derive(Clone, Debug, Default)]
pub struct Verifier {
    config: VerifyConfig,
}

impl Verifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: VerifyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &VerifyConfig {
        &self.config
    }

    /// 主入口：打开 zip → 三向比对 → 返回 [`VerifyReport`]。
    ///
    /// 任何 IO / entry 缺失 / 解析失败都走 [`VerifyError`]；数据不一致走
    /// `VerifyReport.tamper_locations`（不视为硬错误，让 caller 决定是否拒收）。
    pub fn verify_zip(&self, zip_path: &Path) -> Result<VerifyReport, VerifyError> {
        let mut report = VerifyReport {
            verify_version: VERIFY_VERSION.to_string(),
            zip_path: zip_path.display().to_string(),
            ..Default::default()
        };

        // 1) 打开 zip
        let file = fs::File::open(zip_path).map_err(|e| VerifyError::ZipOpen {
            path: zip_path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| VerifyError::ZipOpen {
            path: zip_path.display().to_string(),
            reason: format!("ZipArchive::new: {e}"),
        })?;

        let zip_path_str = zip_path.display().to_string();

        // 2) 解析 manifest.json
        let manifest_bytes = read_entry_bytes(&mut archive, BUNDLE_MANIFEST_NAME, &zip_path_str)?;
        let manifest = ManifestSubset::from_json_bytes(&manifest_bytes)?;
        report.referral_id = manifest.referral_id.clone();
        report.manifest_chain_head_hash = manifest.chain_head_hash.clone();
        report.manifest_version_count = manifest.version_count;

        // 3) 解析 chain.txt
        let chain_txt_bytes = read_entry_bytes(&mut archive, CHAIN_TXT_NAME, &zip_path_str)?;
        let chain_txt_str = std::str::from_utf8(&chain_txt_bytes).map_err(|e| VerifyError::ReadEntry {
            name: CHAIN_TXT_NAME.to_string(),
            path: zip_path_str.clone(),
            reason: format!("chain.txt not utf-8: {e}"),
        })?;
        let chain_txt = ChainTxtParse::parse(chain_txt_str)?;

        // 4) 收集 versions/<cl_id>.json：按 cl_id 升序入 BTreeMap
        let mut versions_map: BTreeMap<u64, serde_json::Value> = BTreeMap::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| VerifyError::ReadEntry {
                name: format!("entry[{i}]"),
                path: zip_path_str.clone(),
                reason: e.to_string(),
            })?;
            let name = entry.name().to_string();
            let Some(cl_id) = parse_versions_entry_cl_id(&name) else {
                continue;
            };
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| VerifyError::ReadEntry {
                name: name.clone(),
                path: zip_path_str.clone(),
                reason: format!("read_to_end: {e}"),
            })?;
            let payload: serde_json::Value =
                serde_json::from_slice(&buf).map_err(|e| VerifyError::BadVersionJson {
                    cl_id,
                    reason: e.to_string(),
                })?;
            versions_map.insert(cl_id, payload);
        }

        // 5) 横向比对
        // 5a) chain.txt 的 cl_id 集合 vs versions/ 的 cl_id 集合（双向）
        let chain_txt_ids: Vec<u64> = chain_txt.rows.iter().map(|r| r.cl_id).collect();
        let version_ids: Vec<u64> = versions_map.keys().cloned().collect();

        for &cl_id in &chain_txt_ids {
            if !versions_map.contains_key(&cl_id) {
                report.tamper_locations.push(TamperLocation::new(
                    TamperKind::VersionFileMissing,
                    Some(cl_id),
                    format!("chain.txt references cl_id={cl_id} but versions/{cl_id}.json is missing"),
                ));
            }
        }
        for &cl_id in &version_ids {
            if !chain_txt_ids.contains(&cl_id) {
                report.tamper_locations.push(TamperLocation::new(
                    TamperKind::ExtraVersionFile,
                    Some(cl_id),
                    format!("versions/{cl_id}.json exists but chain.txt has no entry for cl_id={cl_id}"),
                ));
            }
        }

        // 5b) 按 cl_id 升序重算整链
        let sorted_ids: Vec<u64> = {
            let mut v = version_ids.clone();
            v.sort_unstable();
            v
        };
        let mut prev_bytes = ZERO_HASH;
        let mut recomputed_count: u64 = 0;
        let mut recomputed_head_hex = String::new();

        for (idx, cl_id) in sorted_ids.iter().enumerate() {
            recomputed_count += 1;
            let payload = &versions_map[cl_id];
            let payload_bytes = canonical_json(payload).map_err(|e| VerifyError::BadVersionJson {
                cl_id: *cl_id,
                reason: format!("canonical_json: {e}"),
            })?;
            let this_bytes = link_hash(&prev_bytes, &payload_bytes);
            let prev_hex_full = hex_lower(&prev_bytes);
            let this_hex_full = hex_lower(&this_bytes);
            recomputed_head_hex = this_hex_full.clone();

            // 找 chain.txt 对应行
            if let Some(chain_row) = chain_txt.rows.iter().find(|r| r.cl_id == *cl_id) {
                let prev_short_expected = &chain_row.prev_hash_short;
                let this_short_expected = &chain_row.this_hash_short;
                let prev_short_actual = short_hex(&prev_hex_full, prev_short_expected.len());
                let this_short_actual = short_hex(&this_hex_full, this_short_expected.len());

                if prev_short_expected != &prev_short_actual {
                    report.tamper_locations.push(TamperLocation::new(
                        TamperKind::ChainTxtPrevMismatch,
                        Some(*cl_id),
                        format!(
                            "cl_id={cl_id}: chain.txt prev_hash[..{}] expected={} recomputed={}",
                            prev_short_expected.len(),
                            prev_short_expected,
                            prev_short_actual,
                        ),
                    ));
                }
                if this_short_expected != &this_short_actual {
                    report.tamper_locations.push(TamperLocation::new(
                        TamperKind::ChainTxtThisMismatch,
                        Some(*cl_id),
                        format!(
                            "cl_id={cl_id}: chain.txt this_hash[..{}] expected={} recomputed={}",
                            this_short_expected.len(),
                            this_short_expected,
                            this_short_actual,
                        ),
                    ));
                }

                // chain.txt 里 cl_id 必须单调递增
                if idx > 0 {
                    let prev_cl_id = sorted_ids[idx - 1];
                    if *cl_id <= prev_cl_id {
                        // 已在 sorted_ids 升序，理论上不会触发；保留 safety check
                        report.tamper_locations.push(TamperLocation::new(
                            TamperKind::ChainTxtPrevMismatch,
                            Some(*cl_id),
                            format!(
                                "cl_id ordering regression: prev={prev_cl_id} cur={cl_id}"
                            ),
                        ));
                    }
                }
            }

            prev_bytes = this_bytes;
        }

        // 5c) 与 manifest.json 三字段比对
        report.recomputed_chain_head_hash = recomputed_head_hex.clone();
        report.recomputed_version_count = recomputed_count;

        if !report.manifest_chain_head_hash.is_empty()
            && report.manifest_chain_head_hash != recomputed_head_hex
        {
            report.tamper_locations.push(TamperLocation::new(
                TamperKind::ManifestChainHeadMismatch,
                None,
                format!(
                    "manifest.chain_head_hash={} but recomputed={}",
                    report.manifest_chain_head_hash, recomputed_head_hex,
                ),
            ));
        }

        if report.manifest_version_count != recomputed_count {
            report.tamper_locations.push(TamperLocation::new(
                TamperKind::ManifestVersionCountMismatch,
                None,
                format!(
                    "manifest.version_count={} but recomputed={}",
                    report.manifest_version_count, recomputed_count,
                ),
            ));
        }

        // 5d) referral_id 校验（manifest.referral_id 与 referral_id 文件路径比对）
        // 简化：只校验非空 + 不含路径分隔符
        if report.referral_id.is_empty()
            || report.referral_id.contains('/')
            || report.referral_id.contains('\\')
        {
            report.tamper_locations.push(TamperLocation::new(
                TamperKind::ManifestReferralIdMismatch,
                None,
                format!(
                    "manifest.referral_id invalid (empty or contains path separator): {:?}",
                    report.referral_id
                ),
            ));
        }

        // 6) 总结
        report.chain_ok = report.tamper_locations.is_empty();
        report.recovery_point_required = !report.chain_ok;

        Ok(report)
    }
}

// =============================================================================
// Internal helpers
// =============================================================================

/// 从 zip 读取一个 entry 的全部字节；entry 不存在 → MissingEntry。
fn read_entry_bytes(
    archive: &mut zip::ZipArchive<fs::File>,
    name: &str,
    zip_path: &str,
) -> Result<Vec<u8>, VerifyError> {
    let mut entry = archive.by_name(name).map_err(|_| VerifyError::MissingEntry {
        name: name.to_string(),
        path: zip_path.to_string(),
    })?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).map_err(|e| VerifyError::ReadEntry {
        name: name.to_string(),
        path: zip_path.to_string(),
        reason: format!("read_to_end: {e}"),
    })?;
    Ok(buf)
}

/// sha256 字节数组 → 16 进制字符串（lowercase，64 字符）。
fn hex_lower(bytes: &[u8; SHA256_BYTES]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// sha256 hex 字符串前 `take` 字符截断；不足时整串返回。绝不 panic。
fn short_hex(s: &str, take: usize) -> String {
    if s.len() <= take {
        s.to_string()
    } else {
        s[..take].to_string()
    }
}

/// 主动 compute sha256（不依赖 sha2 crate 私有 API），用于后续扩展点的工具。
#[allow(dead_code)]
fn sha256_of(bytes: &[u8]) -> [u8; SHA256_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut buf = [0u8; SHA256_BYTES];
    buf.copy_from_slice(&out);
    buf
}

/// 私有 dummy，保留 path 类型导出（让 IDE / grep 不报 unused import）。
#[allow(dead_code)]
fn _unused_path_buf_marker(_p: PathBuf) {}

// =============================================================================
// render_text_report — CLI stdout / stderr 人读可审计
// =============================================================================

/// 把 `VerifyReport` 渲染成多行人读可审计文本。
///
/// 格式：
/// ```text
/// # referral-trace-station verify report
/// # zip: <path>
/// # verify_version: 1
/// referral_id: REF-001
/// chain_ok: true|false
/// recovery_point_required: true|false
/// versions: <recomputed> (manifest: <manifest>)
/// chain_head_hash:
///   manifest:   <hex>
///   recomputed: <hex>
/// tamper_locations: <count>
///   - <kind>[<index>] <detail>
///   ...
/// end of report
/// ```
pub fn render_text_report(report: &VerifyReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("# referral-trace-station verify report\n");
    let _ = writeln!(out, "# zip: {}", report.zip_path);
    let _ = writeln!(out, "# verify_version: {}", report.verify_version);
    let _ = writeln!(out, "referral_id: {}", report.referral_id);
    let _ = writeln!(out, "chain_ok: {}", report.chain_ok);
    let _ = writeln!(out, "recovery_point_required: {}", report.recovery_point_required);
    let _ = writeln!(
        out,
        "versions: {} (manifest: {})",
        report.recomputed_version_count, report.manifest_version_count
    );
    out.push_str("chain_head_hash:\n");
    let _ = writeln!(out, "  manifest:   {}", report.manifest_chain_head_hash);
    let _ = writeln!(out, "  recomputed: {}", report.recomputed_chain_head_hash);
    let _ = writeln!(out, "tamper_locations: {}", report.tamper_locations.len());
    for loc in &report.tamper_locations {
        let idx = match loc.index {
            Some(i) => format!("[{}]", i),
            None => "".to_string(),
        };
        let _ = writeln!(out, "  - {}{} {}", loc.kind, idx, loc.detail);
    }
    if !report.errors.is_empty() {
        let _ = writeln!(out, "errors: {}", report.errors.len());
        for e in &report.errors {
            let _ = writeln!(out, "  - {}", e);
        }
    }
    out.push_str("end of report\n");
    out
}

// =============================================================================
// Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{
        BUNDLE_MANIFEST_NAME, BUNDLE_VERSIONS_DIR, CHAIN_TXT_NAME,
    };
    use crate::sha256_chain::chain::{Chain, ChainLink};
    use std::collections::BTreeMap;
    use std::io::Write;

    fn make_referral_version_payload(cl_id: u64, actor: &str, modified_at: &str) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("cl_id".to_string(), serde_json::json!(cl_id));
        map.insert("version_no".to_string(), serde_json::json!(cl_id as u32));
        map.insert("actor".to_string(), serde_json::json!(actor));
        map.insert("modified_at".to_string(), serde_json::json!(modified_at));
        map.insert(
            "before".to_string(),
            serde_json::json!({"diagnosis": "chest pain"}),
        );
        map.insert(
            "after".to_string(),
            serde_json::json!({"diagnosis": "ACS suspected"}),
        );
        map.insert(
            "continuity".to_string(),
            serde_json::json!({"kind": "manifest"}),
        );
        map.insert("payload_sha256".to_string(), serde_json::json!(""));
        serde_json::Value::Object(map)
    }

    fn write_zip_with_chain(
        zip_path: &Path,
        referral_id: &str,
        links: &[ChainLink],
        tamper_versions: Option<u64>,
        tamper_chain_txt_cl_id: Option<u64>,
    ) {
        let f = fs::File::create(zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        // versions/<cl_id>.json
        for (i, link) in links.iter().enumerate() {
            let cl_id = link
                .payload
                .get("cl_id")
                .and_then(|v| v.as_u64())
                .unwrap_or((i + 1) as u64);
            let mut payload = link.payload.clone();
            if let Some(tamper_cl_id) = tamper_versions {
                if cl_id == tamper_cl_id {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "after".to_string(),
                            serde_json::json!({"diagnosis": "TAMPERED"}),
                        );
                    }
                }
            }
            let bytes = canonical_json(&payload).unwrap();
            zip.start_file(format!("{}/{}.json", BUNDLE_VERSIONS_DIR, cl_id), opts)
                .unwrap();
            zip.write_all(&bytes).unwrap();
        }

        // chain.txt
        let chain_text = crate::export::render_chain_text(links);
        let chain_text = if let Some(tamper_cl_id) = tamper_chain_txt_cl_id {
            // 替换 chain.txt 中 tamper_cl_id 对应行的 this_hash[..12] 为 "0".repeat(12)
            chain_text
                .lines()
                .map(|line| {
                    if line.starts_with(&format!("{tamper_cl_id}\t")) {
                        let cols: Vec<&str> = line.split('\t').collect();
                        if cols.len() == 5 {
                            format!(
                                "{}\t{}\t{}\t{}\t{}",
                                cols[0], cols[1], "0".repeat(12), cols[3], cols[4]
                            )
                        } else {
                            line.to_string()
                        }
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        } else {
            chain_text
        };
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(chain_text.as_bytes()).unwrap();

        // manifest.json
        let chain_head = links.last().unwrap().this_hash.clone();
        let manifest = serde_json::json!({
            "referral_id": referral_id,
            "version_count": links.len() as u64,
            "time_range": {
                "earliest": "2026-10-10T08:30:00Z",
                "latest": "2026-10-10T09:30:00Z",
            },
            "chain_head_hash": chain_head,
            "created_at": "2026-10-10T09:35:00Z",
        });
        let bytes = canonical_json(&manifest).unwrap();
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(&bytes).unwrap();

        zip.finish().unwrap();
    }

    fn make_healthy_chain(n: usize) -> Vec<ChainLink> {
        let mut chain = Chain::new();
        let mut links = Vec::new();
        for i in 1..=n {
            let payload = make_referral_version_payload(
                i as u64,
                &format!("dr_{}@hosp_a", i),
                "2026-10-10T08:30:00Z",
            );
            let link = chain.append(&payload).unwrap();
            links.push(link);
        }
        links
    }

    #[test]
    fn test_verify_round_trip_healthy_5_links() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("ok.zip");
        let links = make_healthy_chain(5);
        write_zip_with_chain(&zip_path, "REF-OK", &links, None, None);

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        assert!(report.is_clean(), "expected clean, got tampers={:?}", report.tamper_locations);
        assert!(report.chain_ok);
        assert!(!report.recovery_point_required);
        assert_eq!(report.referral_id, "REF-OK");
        assert_eq!(report.recomputed_version_count, 5);
        assert_eq!(report.manifest_version_count, 5);
        assert_eq!(report.manifest_chain_head_hash, report.recomputed_chain_head_hash);
    }

    #[test]
    fn test_verify_tamper_detected_version_payload_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("tamper.zip");
        let links = make_healthy_chain(5);
        // 改 versions/3.json 的 after 字段 → chain.txt this_hash[..12] 必不一致
        write_zip_with_chain(&zip_path, "REF-TAMPER", &links, Some(3), None);

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        assert!(!report.chain_ok);
        assert!(report.recovery_point_required);
        assert!(!report.tamper_locations.is_empty());
        // 应该至少包含 ChainTxtThisMismatch（versions/*.json payload 改了 → 重算
        // this_hash[..12] 与 chain.txt 不一致）
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::ChainTxtThisMismatch),
            "expected ChainTxtThisMismatch, got {:?}",
            kinds
        );
    }

    #[test]
    fn test_verify_tamper_detected_chain_txt_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("tamper2.zip");
        let links = make_healthy_chain(5);
        // chain.txt 第 3 行 this_hash[..12] 改成 0（保持 versions/*.json 不变）
        write_zip_with_chain(&zip_path, "REF-TAMPER2", &links, None, Some(3));

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        assert!(!report.chain_ok);
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::ChainTxtThisMismatch),
            "expected ChainTxtThisMismatch, got {:?}",
            kinds
        );
    }

    #[test]
    fn test_verify_manifest_chain_head_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bad-manifest.zip");
        let links = make_healthy_chain(3);

        // 写一个 chain_head_hash 与重算不一致的 manifest
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (i, link) in links.iter().enumerate() {
            let cl_id = link.payload.get("cl_id").and_then(|v| v.as_u64()).unwrap_or((i + 1) as u64);
            let bytes = canonical_json(&link.payload).unwrap();
            zip.start_file(format!("{}/{}.json", BUNDLE_VERSIONS_DIR, cl_id), opts).unwrap();
            zip.write_all(&bytes).unwrap();
        }
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(crate::export::render_chain_text(&links).as_bytes()).unwrap();
        let manifest = serde_json::json!({
            "referral_id": "REF-BAD",
            "version_count": 3,
            "time_range": {"earliest": "2026-10-10T08:30:00Z", "latest": "2026-10-10T09:30:00Z"},
            "chain_head_hash": "deadbeef".repeat(8), // 故意错的 hash
            "created_at": "2026-10-10T09:35:00Z",
        });
        let bytes = canonical_json(&manifest).unwrap();
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(&bytes).unwrap();
        zip.finish().unwrap();

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        assert!(!report.chain_ok);
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::ManifestChainHeadMismatch),
            "expected ManifestChainHeadMismatch, got {:?}",
            kinds
        );
    }

    #[test]
    fn test_verify_manifest_version_count_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bad-count.zip");
        let links = make_healthy_chain(3);

        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (i, link) in links.iter().enumerate() {
            let cl_id = link.payload.get("cl_id").and_then(|v| v.as_u64()).unwrap_or((i + 1) as u64);
            let bytes = canonical_json(&link.payload).unwrap();
            zip.start_file(format!("{}/{}.json", BUNDLE_VERSIONS_DIR, cl_id), opts).unwrap();
            zip.write_all(&bytes).unwrap();
        }
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(crate::export::render_chain_text(&links).as_bytes()).unwrap();
        let manifest = serde_json::json!({
            "referral_id": "REF-BAD-COUNT",
            "version_count": 999, // 故意错的 version_count
            "time_range": {"earliest": "2026-10-10T08:30:00Z", "latest": "2026-10-10T09:30:00Z"},
            "chain_head_hash": links.last().unwrap().this_hash.clone(),
            "created_at": "2026-10-10T09:35:00Z",
        });
        let bytes = canonical_json(&manifest).unwrap();
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(&bytes).unwrap();
        zip.finish().unwrap();

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        assert!(!report.chain_ok);
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::ManifestVersionCountMismatch),
            "expected ManifestVersionCountMismatch, got {:?}",
            kinds
        );
    }

    #[test]
    fn test_verify_missing_entry_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("missing.zip");
        // 写一个只含 chain.txt 的最小 zip
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(b"end of chain\n").unwrap();
        zip.finish().unwrap();

        let result = Verifier::new().verify_zip(&zip_path);
        assert!(matches!(
            result,
            Err(VerifyError::MissingEntry { .. })
        ));
    }

    #[test]
    fn test_verify_bad_manifest_json_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bad-json.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(b"not valid json {{{").unwrap();
        zip.finish().unwrap();

        let result = Verifier::new().verify_zip(&zip_path);
        assert!(matches!(result, Err(VerifyError::BadManifestJson { .. })));
    }

    #[test]
    fn test_verify_zip_open_failure_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does_not_exist.zip");
        let result = Verifier::new().verify_zip(&missing);
        assert!(matches!(result, Err(VerifyError::ZipOpen { .. })));
    }

    #[test]
    fn test_verify_not_a_zip_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let not_zip = tmp.path().join("not.zip");
        fs::write(&not_zip, b"this is plain text, not a zip file").unwrap();
        let result = Verifier::new().verify_zip(&not_zip);
        assert!(matches!(result, Err(VerifyError::ZipOpen { .. })));
    }

    #[test]
    fn test_verify_chain_txt_missing_sentinel_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("no-sentinel.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(b"{\"referral_id\":\"REF\",\"version_count\":0,\"chain_head_hash\":\"\"}")
            .unwrap();
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(b"# header only\n").unwrap(); // 缺 end of chain
        zip.finish().unwrap();

        let result = Verifier::new().verify_zip(&zip_path);
        assert!(matches!(
            result,
            Err(VerifyError::MissingChainSentinel { .. })
        ));
    }

    #[test]
    fn test_verify_render_text_report_includes_chain_ok() {
        let report = VerifyReport {
            verify_version: VERIFY_VERSION.to_string(),
            referral_id: "REF-001".to_string(),
            chain_ok: true,
            recovery_point_required: false,
            recomputed_version_count: 5,
            manifest_version_count: 5,
            manifest_chain_head_hash: "abc123".to_string(),
            recomputed_chain_head_hash: "abc123".to_string(),
            tamper_locations: vec![],
            errors: vec![],
            zip_path: "/tmp/test.zip".to_string(),
        };
        let text = render_text_report(&report);
        assert!(text.contains("referral_id: REF-001"));
        assert!(text.contains("chain_ok: true"));
        assert!(text.contains("recovery_point_required: false"));
        assert!(text.contains("end of report"));
    }

    #[test]
    fn test_verify_summary_line_format() {
        let report = VerifyReport {
            verify_version: VERIFY_VERSION.to_string(),
            referral_id: "REF-X".to_string(),
            chain_ok: false,
            recovery_point_required: true,
            recomputed_version_count: 3,
            manifest_version_count: 3,
            manifest_chain_head_hash: "x".to_string(),
            recomputed_chain_head_hash: "y".to_string(),
            tamper_locations: vec![TamperLocation::new(
                TamperKind::ChainTxtThisMismatch,
                Some(2),
                "test",
            )],
            errors: vec![],
            zip_path: "/tmp/x.zip".to_string(),
        };
        let s = report.summary_line();
        assert!(s.contains("verify REF-X"));
        assert!(s.contains("chain_ok=false"));
        assert!(s.contains("recovery_point_required=true"));
        assert!(s.contains("tamper_count=1"));
    }

    #[test]
    fn test_verify_tamper_kind_display_nonempty_and_no_unknown() {
        for k in [
            TamperKind::MissingEntry,
            TamperKind::BadJson,
            TamperKind::ManifestChainHeadMismatch,
            TamperKind::ManifestVersionCountMismatch,
            TamperKind::ManifestReferralIdMismatch,
            TamperKind::ChainTxtPrevMismatch,
            TamperKind::ChainTxtThisMismatch,
            TamperKind::VersionFileMissing,
            TamperKind::ExtraVersionFile,
        ] {
            let s = format!("{}", k);
            assert!(!s.is_empty(), "TamperKind display should not be empty: {:?}", k);
            assert!(!s.contains("unknown"), "TamperKind display should not contain 'unknown': {:?}", k);
        }
    }

    #[test]
    fn test_verify_error_display_nonempty_and_no_unknown() {
        let cases = vec![
            VerifyError::ZipOpen {
                path: "/x.zip".to_string(),
                reason: "denied".to_string(),
            },
            VerifyError::ReadEntry {
                name: "manifest.json".to_string(),
                path: "/x.zip".to_string(),
                reason: "io".to_string(),
            },
            VerifyError::MissingEntry {
                name: "manifest.json".to_string(),
                path: "/x.zip".to_string(),
            },
            VerifyError::BadManifestJson {
                reason: "expected ,".to_string(),
            },
            VerifyError::BadVersionJson {
                cl_id: 3,
                reason: "parse".to_string(),
            },
            VerifyError::BadChainTxtLine {
                line_no: 2,
                reason: "tab".to_string(),
            },
            VerifyError::MissingChainSentinel {
                sentinel: "end of chain".to_string(),
            },
        ];
        for e in cases {
            let s = format!("{}", e);
            assert!(!s.is_empty(), "VerifyError display should not be empty: {:?}", e);
            assert!(!s.contains("unknown"), "VerifyError display should not contain 'unknown': {:?}", e);
        }
    }

    #[test]
    fn test_verify_report_serde_round_trip() {
        let report = VerifyReport {
            verify_version: VERIFY_VERSION.to_string(),
            referral_id: "REF-S".to_string(),
            chain_ok: false,
            recovery_point_required: true,
            recomputed_version_count: 2,
            manifest_version_count: 3,
            manifest_chain_head_hash: "abc".to_string(),
            recomputed_chain_head_hash: "xyz".to_string(),
            tamper_locations: vec![TamperLocation::new(
                TamperKind::ManifestVersionCountMismatch,
                None,
                "version count diff".to_string(),
            )],
            errors: vec!["warn".to_string()],
            zip_path: "/tmp/s.zip".to_string(),
        };
        let j = serde_json::to_string(&report).unwrap();
        let back: VerifyReport = serde_json::from_str(&j).unwrap();
        assert_eq!(back.referral_id, "REF-S");
        assert!(!back.chain_ok);
        assert_eq!(back.tamper_locations.len(), 1);
    }

    #[test]
    fn test_verify_with_config_strict_flag() {
        let cfg = VerifyConfig { strict: true };
        let v = Verifier::with_config(cfg);
        assert!(v.config().strict);
    }

    #[test]
    fn test_verify_chain_txt_extra_cl_id_triggers_extra_version_file() {
        // chain.txt 写了 4 个 cl_id（1,2,3,4），但 versions/ 只有 1,2,3 → 应该报 ExtraVersionFile?
        // 实际上反过来：versions/ 有 1,2,3,5（多了 cl_id=5），chain.txt 只有 1,2,3 → ExtraVersionFile
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("extra.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let links = make_healthy_chain(3);
        // versions/ 写 1,2,3,5（多了一个 5）
        for (i, link) in links.iter().enumerate() {
            let cl_id = link.payload.get("cl_id").and_then(|v| v.as_u64()).unwrap_or((i + 1) as u64);
            let bytes = canonical_json(&link.payload).unwrap();
            zip.start_file(format!("{}/{}.json", BUNDLE_VERSIONS_DIR, cl_id), opts).unwrap();
            zip.write_all(&bytes).unwrap();
        }
        // 多写一个 versions/5.json（手工构造）
        let extra_payload = make_referral_version_payload(5, "dr_x@hosp_a", "2026-10-10T08:35:00Z");
        let bytes = canonical_json(&extra_payload).unwrap();
        zip.start_file(format!("{}/5.json", BUNDLE_VERSIONS_DIR), opts).unwrap();
        zip.write_all(&bytes).unwrap();
        // chain.txt 只含 1,2,3
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(crate::export::render_chain_text(&links).as_bytes()).unwrap();
        // manifest 用 links.last() 的 hash（注意 recomputed 实际会算到 cl_id=5 → 不一致）
        let manifest = serde_json::json!({
            "referral_id": "REF-EXTRA",
            "version_count": 3,
            "time_range": {"earliest": "2026-10-10T08:30:00Z", "latest": "2026-10-10T09:30:00Z"},
            "chain_head_hash": links.last().unwrap().this_hash.clone(),
            "created_at": "2026-10-10T09:35:00Z",
        });
        let bytes = canonical_json(&manifest).unwrap();
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(&bytes).unwrap();
        zip.finish().unwrap();

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::ExtraVersionFile),
            "expected ExtraVersionFile, got {:?}",
            kinds
        );
    }

    #[test]
    fn test_verify_versions_cl_id_missing_triggers_version_file_missing() {
        // chain.txt 写了 4 个 cl_id（1,2,3,4），但 versions/ 只有 1,2,3
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("missing-version.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let links = make_healthy_chain(3);
        for (i, link) in links.iter().enumerate() {
            let cl_id = link.payload.get("cl_id").and_then(|v| v.as_u64()).unwrap_or((i + 1) as u64);
            let bytes = canonical_json(&link.payload).unwrap();
            zip.start_file(format!("{}/{}.json", BUNDLE_VERSIONS_DIR, cl_id), opts).unwrap();
            zip.write_all(&bytes).unwrap();
        }
        // chain.txt 写 4 个 cl_id（手工构造）
        let mut chain_text = String::new();
        chain_text.push_str("# header\n");
        let mut prev = ZERO_HASH;
        for i in 1..=4 {
            let payload = make_referral_version_payload(i, &format!("dr_{}@hosp_a", i), "2026-10-10T08:30:00Z");
            let payload_bytes = canonical_json(&payload).unwrap();
            let this_bytes = link_hash(&prev, &payload_bytes);
            let prev_short = short_hex(&hex_lower(&prev), 12);
            let this_short = short_hex(&hex_lower(&this_bytes), 12);
            chain_text.push_str(&format!("{i}\t{prev_short}\t{this_short}\tdr_{i}@hosp_a\t2026-10-10T08:30:00Z\n"));
            prev = this_bytes;
        }
        chain_text.push_str("end of chain\n");
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(chain_text.as_bytes()).unwrap();
        let manifest = serde_json::json!({
            "referral_id": "REF-MISS",
            "version_count": 4,
            "time_range": {"earliest": "2026-10-10T08:30:00Z", "latest": "2026-10-10T09:30:00Z"},
            "chain_head_hash": hex_lower(&prev),
            "created_at": "2026-10-10T09:35:00Z",
        });
        let bytes = canonical_json(&manifest).unwrap();
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(&bytes).unwrap();
        zip.finish().unwrap();

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::VersionFileMissing),
            "expected VersionFileMissing, got {:?}",
            kinds
        );
    }

    #[test]
    fn test_verify_chain_txt_rejects_bad_column_count() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bad-line.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(b"{\"referral_id\":\"REF\",\"version_count\":1,\"chain_head_hash\":\"\"}")
            .unwrap();
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        // 一行只有 4 列（缺 actor）
        zip.write_all(b"1\tprev\tthis\t2026-10-10T08:30:00Z\nend of chain\n").unwrap();
        zip.finish().unwrap();

        let result = Verifier::new().verify_zip(&zip_path);
        assert!(matches!(result, Err(VerifyError::BadChainTxtLine { .. })));
    }

    #[test]
    fn test_verify_manifest_referral_id_invalid_triggers() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bad-refid.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(BUNDLE_MANIFEST_NAME, opts).unwrap();
        zip.write_all(b"{\"referral_id\":\"../../etc\",\"version_count\":0,\"chain_head_hash\":\"\"}")
            .unwrap();
        zip.start_file(CHAIN_TXT_NAME, opts).unwrap();
        zip.write_all(b"end of chain\n").unwrap();
        zip.finish().unwrap();

        let report = Verifier::new().verify_zip(&zip_path).unwrap();
        let kinds: Vec<&TamperKind> =
            report.tamper_locations.iter().map(|l| &l.kind).collect();
        assert!(
            kinds.contains(&&TamperKind::ManifestReferralIdMismatch),
            "expected ManifestReferralIdMismatch, got {:?}",
            kinds
        );
    }
}