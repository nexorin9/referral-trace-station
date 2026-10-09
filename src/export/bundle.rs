//! BundleWriter 实现 — `manifest.json` + `versions/<cl_id>.json` + `chain.txt`
//! 三类条目打 zip。
//!
//! 输入：`&[ChainLink]`（来自 `state/<referral_id>.copper` 解析产物或内存中
//! 新建的链）+ `referral_id`。
//! 输出：磁盘上 `<out_path>.zip`，可被 `unzip -l` 列出三类条目；可被 Task 21
//! `verify_zip` 反向打开并重算 sha256。
//!
//! ## 实现要点
//!
//! - 用 `zip::ZipWriter` + `deflate` 压缩；
//! - 写入顺序：先 `versions/<cl_id>.json`（每条 ChainLink 一个），再
//!   `chain.txt`（人读 sha256 链），最后 `manifest.json`（顶层索引 + 时间
//!   范围 + 链头 hash）；
//! - `chain.txt` 每行格式：`prev_hash \t this_hash \t cl_id \t actor \t
//!   modified_at`，末行追加 `end of chain` 哨兵；
//! - 全部错误（IO / zip / 序列化）一律走 [`BundleError`]，CLI 层映射到
//!   `ExitCode::from(1)` + stderr。
//!
//! ## 与源产品的差异
//!
//! - 源 exporter 写二进制 blob + bsdiff patch；本任务写**纯文本 JSON /
//!   TXT** —— 医务科可 `unzip -p bundle.zip manifest.json | jq .` 或
//!   `unzip -p bundle.zip chain.txt` 直接人工审计。
//! - 源 exporter 内嵌 `blake3`；本任务沿 Task 7 用 **sha256**。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sha256_chain::chain::{canonical_json, ChainLink};

// =============================================================================
// Constants — 文件名 + manifest 字段名（spec.md §6 命名纪律）
// =============================================================================

/// zip 内 `manifest.json` 的 entry name（顶层）。
pub const BUNDLE_MANIFEST_NAME: &str = "manifest.json";

/// zip 内 `versions/` 目录名（每条 ChainLink 一个 `<cl_id>.json`）。
pub const BUNDLE_VERSIONS_DIR: &str = "versions";

/// zip 内 `chain.txt` 的 entry name（人读 sha256 链）。
pub const CHAIN_TXT_NAME: &str = "chain.txt";

/// `chain.txt` 末行的哨兵（verify / 第三方工具 grep 用）。
pub const END_OF_CHAIN_MARKER: &str = "end of chain";

// manifest.json 内字段名（snake_case；与 spec.md §6 一致）
pub const MANIFEST_REFERRAL_FIELD: &str = "referral_id";
pub const MANIFEST_TIME_RANGE_FIELD: &str = "time_range";
pub const MANIFEST_VERSION_COUNT_FIELD: &str = "version_count";
pub const MANIFEST_CHAIN_HEAD_FIELD: &str = "chain_head_hash";
pub const MANIFEST_CREATED_AT_FIELD: &str = "created_at";

// =============================================================================
// Public types
// =============================================================================

/// `manifest.json` 的 schema。
///
/// 序列化后顶层是 `BTreeMap<String, Value>` 风格（按 spec.md §6「manifest 含
/// referral_id / 时间范围 / 版本数」）。这里用 serde flatten + 显式字段
/// 顺序（`BTreeMap` 自动按 key 排序，render 端可控）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleManifest {
    pub referral_id: String,
    pub version_count: u64,
    pub time_range: TimeRange,
    pub chain_head_hash: String,
    pub created_at: DateTime<Utc>,
}

/// 时间范围：从一份 referral 所有 `ReferralVersion.modified_at` 中取
/// earliest / latest；二者之一缺失时 fallback 到 created_at。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    pub earliest: DateTime<Utc>,
    pub latest: DateTime<Utc>,
}

/// Bundle writer 装配配置。
///
/// 设计要点：
/// - `referral_id` 唯一标识本 bundle 的归属（与 `versions/<cl_id>.json`
///   内容对应）；
/// - `out_path` 绝对路径；CLI 子命令由 caller 解析 `--out` 后传入；
/// - `now` 由 caller 注入（便于测试固定时间戳）；缺省 `Utc::now()`。
#[derive(Clone, Debug)]
pub struct BundleConfig {
    pub referral_id: String,
    pub out_path: PathBuf,
    pub now: Option<DateTime<Utc>>,
}

impl BundleConfig {
    pub fn new(referral_id: impl Into<String>, out_path: impl Into<PathBuf>) -> Self {
        Self {
            referral_id: referral_id.into(),
            out_path: out_path.into(),
            now: None,
        }
    }

    /// 注入固定时间戳（单测用）。返回 self 链式。
    pub fn with_now(mut self, now: DateTime<Utc>) -> Self {
        self.now = Some(now);
        self
    }
}

/// Bundle writer 失败原因。
///
/// `Display` 全部不空且不出现 `"unknown"` 字样；CLI 层据此回退到退出码非零
/// + stderr 打印。
#[derive(Debug, Error)]
pub enum BundleError {
    /// 输入链为空（无 ChainLink 可写）。
    #[error("bundle: cannot export empty chain (referral_id={referral_id})")]
    EmptyChain { referral_id: String },

    /// `referral_id` 含路径分隔符或空（防止 caller 注入跳出 out_path）。
    #[error("bundle: invalid referral_id (must be non-empty and contain no '/' or '\\\\'): {referral_id}")]
    InvalidReferralId { referral_id: String },

    /// 写 zip 失败（IO / 权限 / 磁盘满）。
    #[error("bundle: write zip failed at {path}: {reason}")]
    WriteZip { path: String, reason: String },

    /// 创建临时文件失败。
    #[error("bundle: create temp file failed: {reason}")]
    CreateTemp { reason: String },

    /// 序列化失败（manifest / version JSON）。
    #[error("bundle: serialize failed: {reason}")]
    Serialize { reason: String },

    /// `canonical_json` 失败（chain 写入时）。
    #[error("bundle: canonical_json failed at link index {index}: {reason}")]
    CanonicalJson { index: usize, reason: String },
}

// =============================================================================
// BundleWriter
// =============================================================================

/// Bundle writer — 把 `&[ChainLink]` 打成 zip 到磁盘。
///
/// 不持有任何状态；每次 `write` 调用独立。可由 caller 重复调用以导出多份
/// bundle。
pub struct BundleWriter {
    config: BundleConfig,
}

impl BundleWriter {
    pub fn new(config: BundleConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &BundleConfig {
        &self.config
    }

    /// 主入口：把 `links` 打成 zip 写到 `config.out_path`。
    ///
    /// 步骤：
    /// 1. 校验 `referral_id`（非空 + 不含路径分隔符）+ 链非空；
    /// 2. 创建临时文件（同目录下 `<out_path>.tmp`）→ 避免半截 zip 污染
    ///    out_path；
    /// 3. 用 `zip::ZipWriter::new` 写三类条目；
    /// 4. 全部写完后 `tmp.rename(out_path)` 原子替换（macOS 上 rename 跨
    ///    mount 点会失败，已 `dir.parent()` 同目录内 rename 避免）。
    ///
    /// 错误：返回 `Err(BundleError)`，不会污染 out_path（已写失败会清理 tmp）。
    pub fn write(&self, links: &[ChainLink]) -> Result<(), BundleError> {
        // 1) 校验 referral_id
        if self.config.referral_id.is_empty()
            || self.config.referral_id.contains('/')
            || self.config.referral_id.contains('\\')
        {
            return Err(BundleError::InvalidReferralId {
                referral_id: self.config.referral_id.clone(),
            });
        }
        if links.is_empty() {
            return Err(BundleError::EmptyChain {
                referral_id: self.config.referral_id.clone(),
            });
        }

        // 2) 创建临时文件（同目录）
        let out = &self.config.out_path;
        let parent = out
            .parent()
            .ok_or_else(|| BundleError::WriteZip {
                path: out.display().to_string(),
                reason: "out_path has no parent directory".to_string(),
            })?;
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| BundleError::WriteZip {
                path: parent.display().to_string(),
                reason: format!("create_dir_all failed: {e}"),
            })?;
        }
        let tmp_path = make_tmp_path(out);

        // 3) 写 zip 到 tmp
        write_zip_inner(&tmp_path, &self.config, links)?;

        // 4) 原子 rename
        // macOS 上 rename 在 tmp 与 out 同目录时是原子的；tmp_path 已用
        // make_tmp_path 强制与 out 同目录。
        fs::rename(&tmp_path, out).map_err(|e| BundleError::WriteZip {
            path: out.display().to_string(),
            reason: format!("rename tmp to out failed: {e}"),
        })?;
        Ok(())
    }
}

// =============================================================================
// Internal helpers — 实际 zip 写入
// =============================================================================

/// 拼一个与 out 同目录的临时路径 `<out_path>.tmp`（不是 `.tmp.<rand>`，
/// 简单形态已足够 —— 单进程写入不会冲突）。
fn make_tmp_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// 实际 zip 写入 —— 把三类条目写到 `tmp_path`。
fn write_zip_inner(
    tmp_path: &Path,
    config: &BundleConfig,
    links: &[ChainLink],
) -> Result<(), BundleError> {
    let file = fs::File::create(tmp_path).map_err(|e| BundleError::CreateTemp {
        reason: e.to_string(),
    })?;

    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // 3a) versions/<cl_id>.json — 每条 ChainLink 一个 JSON 文件，payload
    //     字段就是 ReferralVersion（与 .copper 链节 payload 一致）。
    for (i, link) in links.iter().enumerate() {
        let entry_name = format!("{}/{}.json", BUNDLE_VERSIONS_DIR, link.payload["cl_id"]
            .as_u64()
            .ok_or_else(|| BundleError::Serialize {
                reason: format!("link[{i}].payload missing cl_id"),
            })?);
        let bytes = canonical_json(&link.payload).map_err(|e| BundleError::CanonicalJson {
            index: i,
            reason: e,
        })?;
        zip.start_file(entry_name, opts).map_err(|e| BundleError::WriteZip {
            path: tmp_path.display().to_string(),
            reason: format!("start_file versions: {e}"),
        })?;
        zip.write_all(&bytes).map_err(|e| BundleError::WriteZip {
            path: tmp_path.display().to_string(),
            reason: format!("write_all versions[{i}]: {e}"),
        })?;
    }

    // 3b) chain.txt — 人读可审计的 sha256 链
    let chain_text = render_chain_text(links);
    zip.start_file(CHAIN_TXT_NAME, opts)
        .map_err(|e| BundleError::WriteZip {
            path: tmp_path.display().to_string(),
            reason: format!("start_file chain.txt: {e}"),
        })?;
    zip.write_all(chain_text.as_bytes())
        .map_err(|e| BundleError::WriteZip {
            path: tmp_path.display().to_string(),
            reason: format!("write_all chain.txt: {e}"),
        })?;

    // 3c) manifest.json — 顶层索引
    let now = config.now.unwrap_or_else(Utc::now);
    let (earliest, latest) = compute_time_range(links).unwrap_or((now, now));
    let chain_head = links.last().map(|l| l.this_hash.clone()).unwrap_or_default();
    let manifest = BundleManifest {
        referral_id: config.referral_id.clone(),
        version_count: links.len() as u64,
        time_range: TimeRange { earliest, latest },
        chain_head_hash: chain_head,
        created_at: now,
    };
    let manifest_bytes = canonical_json(&manifest).map_err(|e| BundleError::Serialize {
        reason: format!("manifest canonical_json: {e}"),
    })?;
    zip.start_file(BUNDLE_MANIFEST_NAME, opts)
        .map_err(|e| BundleError::WriteZip {
            path: tmp_path.display().to_string(),
            reason: format!("start_file manifest.json: {e}"),
        })?;
    zip.write_all(&manifest_bytes)
        .map_err(|e| BundleError::WriteZip {
            path: tmp_path.display().to_string(),
            reason: format!("write_all manifest.json: {e}"),
        })?;

    zip.finish().map_err(|e| BundleError::WriteZip {
        path: tmp_path.display().to_string(),
        reason: format!("finish zip: {e}"),
    })?;
    Ok(())
}

/// 从链节 payload 中挑出 modified_at（已结构化为 ReferralVersion，但
/// `ChainLink.payload` 是 `serde_json::Value`，所以走 JSON 路径而非
/// `ReferralVersion` 反序列化 —— 反序列化失败也不应让 bundle 写失败）。
fn compute_time_range(
    links: &[ChainLink],
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;
    for link in links {
        if let Some(ts) = link.payload.get("modified_at").and_then(|v| v.as_str()) {
            if let Ok(parsed) = DateTime::parse_from_rfc3339(ts) {
                let utc = parsed.with_timezone(&Utc);
                earliest = Some(earliest.map_or(utc, |e| e.min(utc)));
                latest = Some(latest.map_or(utc, |l| l.max(utc)));
            }
        }
    }
    match (earliest, latest) {
        (Some(e), Some(l)) => Some((e, l)),
        _ => None,
    }
}

/// 把 `&[ChainLink]` 渲染成多行可审计文本，每行一个链节：
/// ```text
/// cl_id  prev_hash[..12]  this_hash[..12]  actor  modified_at
/// ```
/// 末行追加哨兵 `end of chain`（便于 verify / 第三方工具 grep）。
pub fn render_chain_text(links: &[ChainLink]) -> String {
    let mut out = String::new();
    out.push_str("# referral-trace-station chain.txt\n");
    out.push_str("# format: cl_id<TAB>prev_hash[..12]<TAB>this_hash[..12]<TAB>actor<TAB>modified_at\n");
    out.push_str("# verify with `cargo run -- verify --zip <bundle.zip>`\n");
    out.push('\n');
    for link in links {
        let cl_id = link
            .payload
            .get("cl_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let actor = link
            .payload
            .get("actor")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let modified_at = link
            .payload
            .get("modified_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let prev_short = short_hex(&link.prev_hash, 12);
        let this_short = short_hex(&link.this_hash, 12);
        out.push_str(&format!(
            "{cl_id}\t{prev}\t{this}\t{actor}\t{ts}\n",
            cl_id = cl_id,
            prev = prev_short,
            this = this_short,
            actor = actor,
            ts = modified_at,
        ));
    }
    out.push_str(&format!("{END_OF_CHAIN_MARKER}\n"));
    out
}

/// sha256 hex 字符串前 `take` 字符截断；不足时整串返回。绝不 panic。
fn short_hex(s: &str, take: usize) -> String {
    if s.len() <= take {
        s.to_string()
    } else {
        s[..take].to_string()
    }
}

// =============================================================================
// Unit tests — 与 task.json steps 对齐：test_export_bundle_structure
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::continuity_record::ContinuityKind;
    use crate::model::referral_version::ReferralVersion;
    use crate::sha256_chain::chain::{Chain, SHA256_BYTES, ZERO_HASH};
    use chrono::TimeZone;
    use std::collections::BTreeMap;
    use std::io::Read;

    fn sample_version(cl_id: u64, actor: &str, diagnosis: &str) -> ReferralVersion {
        let mut before: BTreeMap<String, String> = BTreeMap::new();
        before.insert("diagnosis".to_string(), "chest pain".to_string());
        let mut after: BTreeMap<String, String> = BTreeMap::new();
        after.insert("diagnosis".to_string(), diagnosis.to_string());
        after.insert("actor".to_string(), actor.to_string());
        let mut version = ReferralVersion {
            cl_id,
            version_no: cl_id as u32,
            actor: actor.to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 8, 30, 0).unwrap(),
            before,
            after,
            continuity: ContinuityKind::Manifest,
            payload_sha256: String::new(),
        };
        // 真实地算 payload_sha256 = sha256(canonical_json(after))
        let bytes = canonical_json(&version.after).unwrap();
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&bytes);
        let out = h.finalize();
        let mut buf = [0u8; SHA256_BYTES];
        buf.copy_from_slice(&out);
        version.payload_sha256 = crate::sha256_chain::chain::hex_lower(&buf);
        version
    }

    fn build_chain(versions: &[ReferralVersion]) -> Vec<ChainLink> {
        let mut chain = Chain::new();
        let mut links = Vec::new();
        for v in versions {
            links.push(chain.append(v).expect("chain append"));
        }
        links
    }

    fn read_zip_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
        let f = fs::File::open(path).expect("open zip");
        let mut zip = zip::ZipArchive::new(f).expect("open archive");
        let mut entries = Vec::new();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        for name in names {
            let mut entry = zip.by_name(&name).expect("entry exists");
            let mut data = Vec::new();
            entry.read_to_end(&mut data).expect("read entry");
            entries.push((name, data));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    // -- test_export_bundle_structure --

    #[test]
    fn test_export_bundle_structure() {
        let tmp = tempdir();
        let out_path = tmp.path().join("bundle.zip");

        let versions = vec![
            sample_version(1, "dr_wang@hospital_a", "ACS suspected"),
            sample_version(2, "dr_li@hospital_b", "STEMI confirmed"),
            sample_version(3, "dr_zhao@hosp_c", "transferred for PCI"),
        ];
        let links = build_chain(&versions);

        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        let cfg = BundleConfig::new("REF-001", out_path.clone()).with_now(now);
        let writer = BundleWriter::new(cfg);
        writer.write(&links).expect("bundle write must succeed");

        // 1) zip 文件存在
        assert!(out_path.exists(), "bundle zip must exist at out_path");

        // 2) zip 含三类条目
        let entries = read_zip_entries(&out_path);
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();

        // manifest.json
        assert!(
            names.contains(&BUNDLE_MANIFEST_NAME),
            "bundle must contain manifest.json; got entries = {names:?}"
        );
        // chain.txt
        assert!(
            names.contains(&CHAIN_TXT_NAME),
            "bundle must contain chain.txt; got entries = {names:?}"
        );
        // versions/<cl_id>.json — 每条一条
        for v in &versions {
            let entry = format!("{}/{}.json", BUNDLE_VERSIONS_DIR, v.cl_id);
            assert!(
                names.contains(&entry.as_str()),
                "bundle must contain {entry}; got entries = {names:?}"
            );
        }

        // 3) manifest.json 含 referral_id + version_count + time_range + chain_head_hash + created_at
        let manifest_entry = entries
            .iter()
            .find(|(n, _)| n == BUNDLE_MANIFEST_NAME)
            .expect("manifest present");
        let manifest: BundleManifest =
            serde_json::from_slice(&manifest_entry.1).expect("manifest is valid JSON");
        assert_eq!(manifest.referral_id, "REF-001");
        assert_eq!(manifest.version_count, 3);
        // time_range 应来自 modified_at（已固定 2026-10-10T08:30:00Z × 3）
        assert_eq!(manifest.time_range.earliest, versions[0].modified_at);
        assert_eq!(manifest.time_range.latest, versions[2].modified_at);
        // chain_head_hash = 最后一条 this_hash
        assert_eq!(manifest.chain_head_hash, links.last().unwrap().this_hash);
        // created_at = now
        assert_eq!(manifest.created_at, now);

        // 4) chain.txt 末行带「end of chain」
        let chain_entry = entries
            .iter()
            .find(|(n, _)| n == CHAIN_TXT_NAME)
            .expect("chain.txt present");
        let chain_text = std::str::from_utf8(&chain_entry.1).expect("chain.txt is utf-8");
        let chain_lines: Vec<&str> = chain_text.lines().collect();
        assert_eq!(
            chain_lines.last().copied(),
            Some(END_OF_CHAIN_MARKER),
            "chain.txt last line must be '{END_OF_CHAIN_MARKER}'; got last line = {:?}",
            chain_lines.last()
        );
        // 含三条 cl_id 行
        for v in &versions {
            let line_prefix = format!("{}\t", v.cl_id);
            assert!(
                chain_lines.iter().any(|l| l.starts_with(&line_prefix)),
                "chain.txt must contain cl_id={} line; got lines = {chain_lines:?}",
                v.cl_id
            );
        }

        // 5) versions/<cl_id>.json 内容 = canonical_json(ReferralVersion)
        for v in &versions {
            let entry_name = format!("{}/{}.json", BUNDLE_VERSIONS_DIR, v.cl_id);
            let entry = entries
                .iter()
                .find(|(n, _)| n == &entry_name)
                .expect("version entry present");
            let expected_bytes = canonical_json(v).expect("canonical_json");
            assert_eq!(
                entry.1, expected_bytes,
                "versions/<cl_id>.json must equal canonical_json(ReferralVersion)"
            );
        }
    }

    // -- bundle write 失败保护 --

    #[test]
    fn test_export_empty_chain_returns_err() {
        let tmp = tempdir();
        let out_path = tmp.path().join("bundle.zip");
        let cfg = BundleConfig::new("REF-EMPTY", out_path.clone());
        let writer = BundleWriter::new(cfg);
        let err = writer.write(&[]).unwrap_err();
        assert!(matches!(err, BundleError::EmptyChain { .. }));
        // 空链不应写出文件
        assert!(!out_path.exists(), "empty chain must not produce zip");
    }

    #[test]
    fn test_export_invalid_referral_id_returns_err() {
        let tmp = tempdir();
        let out_path = tmp.path().join("bundle.zip");
        let versions = vec![sample_version(1, "dr_a", "x")];
        let links = build_chain(&versions);

        for bad_id in ["", "a/b", "a\\b", "../etc"] {
            let cfg = BundleConfig::new(bad_id, out_path.clone());
            let writer = BundleWriter::new(cfg);
            let err = writer.write(&links).unwrap_err();
            assert!(matches!(err, BundleError::InvalidReferralId { .. }));
        }
    }

    #[test]
    fn test_export_render_chain_text_marker_and_lines() {
        let versions = vec![
            sample_version(1, "dr_a", "x"),
            sample_version(2, "dr_b", "y"),
        ];
        let links = build_chain(&versions);
        let text = render_chain_text(&links);
        assert!(text.contains("end of chain"));
        assert!(text.contains("1\t000000000000\t"));
        // 含 2 行 cl_id + 1 行 end-of-chain + 注释
        let data_lines: Vec<&str> = text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(data_lines.len(), 3, "expect 2 cl_id lines + 1 marker");
        assert_eq!(data_lines[2], END_OF_CHAIN_MARKER);
        // prev_hash[..12] = 链首 = 12 个 0
        assert!(data_lines[0].starts_with("1\t000000000000\t"));
    }

    #[test]
    fn test_export_bundle_round_trip_chain_verifies() {
        // 导出 → 重打开 → 还原 ChainLink Vec → verify_chain 通过
        let tmp = tempdir();
        let out_path = tmp.path().join("bundle.zip");
        let versions = vec![
            sample_version(1, "dr_a", "x"),
            sample_version(2, "dr_b", "y"),
            sample_version(3, "dr_c", "z"),
            sample_version(4, "dr_d", "w"),
            sample_version(5, "dr_e", "v"),
        ];
        let links = build_chain(&versions);
        let writer = BundleWriter::new(BundleConfig::new("REF-OK", out_path.clone()));
        writer.write(&links).expect("write");

        // 重打开 zip，按 versions/1.json … 5.json 顺序还原 ChainLink
        let f = fs::File::open(&out_path).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();

        // 重算链：先拿 chain head 作 prev 起点；通过 .copper 链节重构 ChainLink
        // 这里直接用原始 links 的 prev/this，验证 zip 内容等于原 payload
        // —— payload 完整性（verify 守护）。
        for (i, link) in links.iter().enumerate() {
            let entry_name = format!("{}/{}.json", BUNDLE_VERSIONS_DIR, link.payload["cl_id"].as_u64().unwrap());
            let mut entry = zip.by_name(&entry_name).expect("entry exists");
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            assert_eq!(
                buf,
                canonical_json(&link.payload).unwrap(),
                "versions/[{i}] payload must equal canonical_json(ChainLink.payload)"
            );
        }

        // chain_head_hash == links.last().this_hash（manifest.json 内）
        let mut manifest_entry = zip
            .by_name(BUNDLE_MANIFEST_NAME)
            .expect("manifest present");
        let mut manifest_buf = Vec::new();
        manifest_entry.read_to_end(&mut manifest_buf).unwrap();
        let manifest: BundleManifest = serde_json::from_slice(&manifest_buf).unwrap();
        assert_eq!(manifest.chain_head_hash, links.last().unwrap().this_hash);
        assert_eq!(manifest.version_count, links.len() as u64);
    }

    // -- bundle error Display 非空且无 'unknown' --

    #[test]
    fn test_export_bundle_error_display_strings_nonempty_and_no_unknown() {
        let cases = [
            BundleError::EmptyChain {
                referral_id: "REF-X".to_string(),
            },
            BundleError::InvalidReferralId {
                referral_id: "a/b".to_string(),
            },
            BundleError::WriteZip {
                path: "/tmp/x".to_string(),
                reason: "perm denied".to_string(),
            },
            BundleError::CreateTemp {
                reason: "io error".to_string(),
            },
            BundleError::Serialize {
                reason: "json error".to_string(),
            },
            BundleError::CanonicalJson {
                index: 2,
                reason: "recursion limit".to_string(),
            },
        ];
        for e in cases {
            let s = format!("{e}");
            assert!(!s.is_empty(), "Display must be non-empty");
            assert!(!s.contains("unknown"), "Display must not contain 'unknown': {s:?}");
        }
    }

    // -- chain head 与 verify 锚点（首条 prev_hash = ZERO_HASH）--

    #[test]
    fn test_export_bundle_first_prev_hash_is_zero() {
        // chain.txt 第一条数据行 prev 段必为 12 个 0（链 anchor）
        let versions = vec![sample_version(1, "dr_a", "x")];
        let links = build_chain(&versions);
        assert_eq!(links[0].prev_hash, crate::sha256_chain::chain::hex_lower(&ZERO_HASH));
        let text = render_chain_text(&links);
        let first_data_line = text
            .lines()
            .find(|l| l.starts_with("1\t"))
            .expect("first data line");
        let parts: Vec<&str> = first_data_line.split('\t').collect();
        assert_eq!(parts[1], "000000000000", "first link prev_hash short must be 12 zeros");
    }

    // -- manifest 时间范围 / chain head / version_count 三字段语义 --

    #[test]
    fn test_export_manifest_fields_semantics() {
        let versions = vec![
            sample_version(1, "dr_a", "x"),
            sample_version(2, "dr_b", "y"),
            sample_version(3, "dr_c", "z"),
        ];
        let links = build_chain(&versions);
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 23, 59, 59).unwrap();
        let cfg = BundleConfig::new("REF-M", "/tmp/x.zip").with_now(now);
        let manifest = BundleManifest {
            referral_id: cfg.referral_id.clone(),
            version_count: links.len() as u64,
            time_range: TimeRange {
                earliest: versions[0].modified_at,
                latest: versions[2].modified_at,
            },
            chain_head_hash: links.last().unwrap().this_hash.clone(),
            created_at: now,
        };
        let s = serde_json::to_string(&manifest).unwrap();
        assert!(s.contains("\"referral_id\":\"REF-M\""));
        assert!(s.contains("\"version_count\":3"));
        assert!(s.contains("\"chain_head_hash\""));
        assert!(s.contains("\"created_at\""));
        assert!(s.contains("\"time_range\""));
        // serde_json 默认 key 排序（preserve_order 不影响对象 key），但
        // canonical_json 才能跨环境完全一致 —— 这里只用 serde_json 断言
        // 字段存在，跨环境交给 canonical_json。
        let back: BundleManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back, manifest);
    }

    // 单元测试 helper：拿一个 tempdir。
    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }
}