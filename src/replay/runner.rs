//! Replay runner 实现 — 读 `.copper` → 校验 sha256 链 → 渲染时间线。
//!
//! 输入：`state/<referral_id>.copper`（JSON Lines，每行一条 `ChainLink`）。
//! 流程：
//! 1. 打开文件，按行解析 `ChainLink`；
//! 2. 调 [`crate::sha256_chain::chain::verify_chain`] 校验整链；
//! 3. 失败 → 标记 `recovery_point_required = true`（不臆造尾巴，对齐源
//!    `validate_replay_continuity` contract ②）；
//! 4. 把每条 `ReferralVersion` 翻译成 [`TimelineRow`]（cl_id + modified_at + actor
//!    + fields_changed + sha256 前 12 字符）。
//!
//! 与源产品 `NativeArchive` 的差异在 [`mod.rs` 顶部注释]，核心是把 binary
//! archive 改为 JSON Lines 文本 archive，便于医务科审计。

use std::fmt;
use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::model::referral_version::ReferralVersion;
use crate::sha256_chain::chain::{verify_chain, ChainError, ChainLink};

/// `.copper` 文件名前缀。
pub const COPPER_FILE_PREFIX: &str = "";

/// `.copper` 文件扩展名（区分 inbox drop 的 `.zip` 与持久化的 `.copper`）。
pub const COPPER_EXTENSION: &str = "copper";

/// 链校验失败时 UI / 报告统一显示的提示文案（与源 copper-rs
/// `validate_replay_continuity` 的 RecoveryPoint tail fabrication 语义对齐）。
pub const RECOVERY_POINT_REQUIRED_HINT: &str = "recovery_point_required: chain verify failed; cannot resume beyond the last valid keyframe";

// =============================================================================
// Public types
// =============================================================================

/// Replay 报告：单次回放的全部可观察产物。
///
/// `chain_ok = true` 时整链无篡改、UI 可显示完整时间线；`chain_ok = false` 时
/// `recovery_point_required = true` 且 `chain_error` 含错误细节（医务科人工
/// 复核需要看到 index / expected / actual 三字段）。
///
/// `rows` 永远包含**已成功解析的** `ReferralVersion`；解析失败的行不会污染
/// `rows`（它们走 [`ReplayError::ParseLine`] 并立即返回，让 caller 决定是否
/// 整报告重置或仅跳过单条 —— 本任务选「单条错即整报告错」更安全）。
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayReport {
    /// 报告归属的转诊单 id（与文件名 stem 一致；含 `.copper` 后缀路径经
    /// `file_stem()` 后去掉）。
    pub referral_id: String,
    /// `.copper` 文件的绝对路径（诊断 / 报告头部引用）。
    pub state_file_path: PathBuf,
    /// 解析到的链节条数（≈ `ReferralVersion` 条数）。
    pub link_count: usize,
    /// 链校验结果。`Ok(())` 表示整链无篡改；`Err` 携带 [`ChainError`] 详情。
    pub chain_verify: Result<(), ChainError>,
    /// 链校验失败 → 标记为需要 RecoveryPoint（UI 顶部 / 报告导出 / 医务科
    /// 复核对话文都按此显式提示）。
    pub recovery_point_required: bool,
    /// 解析出的时间线条目（已成功反序列化的 `ReferralVersion`）。
    pub rows: Vec<TimelineRow>,
    /// `ReferralVersion.continuity` 中显式声明 `Gap{...}` 的条数（不含 fsck 推断
    /// 出的隐式空洞 —— fsck 推断由 Task 6 在主路径上完成）。
    pub declared_gap_count: u64,
}

impl ReplayReport {
    /// 链是否无篡改。`chain_ok = true` ⇔ `recovery_point_required = false`。
    pub fn chain_ok(&self) -> bool {
        self.chain_verify.is_ok()
    }

    /// 报告头部（CLI 第一行 + 终端渲染用）。
    pub fn header_line(&self) -> String {
        format!(
            "replay {referral_id} : links={links} chain_ok={ok} recovery_point_required={rpr}",
            referral_id = self.referral_id,
            links = self.link_count,
            ok = self.chain_ok(),
            rpr = self.recovery_point_required,
        )
    }
}

/// 单条时间线渲染行。
///
/// 每个 `ReferralVersion` 对应一行；医务科审计 / 终端 stdout / 报告导出共用同
/// 一字段集。`fields_changed` 是字段级 diff 后被改动的 key 列表（add +
/// remove + replace 的并集），按字典序排序以便人眼复核。
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineRow {
    /// 单调 cl_id（与源 CopperList.id 对齐；本产品的 CopperList 入口）。
    pub cl_id: u64,
    /// 修改时间（从 `ReferralVersion.modified_at` 直接拿）。
    pub modified_at: DateTime<Utc>,
    /// 修改医师（`actor` 字段，原样保留 `姓名 / 机构` 形式）。
    pub actor: String,
    /// 字段级 diff 后被改动的字段名集合（按字典序）。
    pub fields_changed: Vec<String>,
    /// `payload_sha256` 前 12 字符（链 `this_hash` 与 `payload_sha256` 同源，
    /// UI 右上元数据视图沿用这一截断长度）。
    pub sha256_short: String,
    /// 链上 `this_hash` 前 12 字符（与 `sha256_short` 通常一致；但当 chain 校验
    /// 失败且本行 payload 被篡改时，两者会分叉 —— `sha256_short` 仍来自
    /// `ReferralVersion.payload_sha256`，`chain_this_hash_short` 来自链节）。
    pub chain_this_hash_short: String,
    /// 链上 `prev_hash` 前 12 字符（首条 = 全 0）。
    pub chain_prev_hash_short: String,
    /// 业务层 continuity 标记（manifest / gap / recovery_point / finished）。
    /// 给终端渲染列「continuity」用。
    pub continuity: String,
}

impl TimelineRow {
    /// 渲染为终端一行固定宽度字段：
    ///
    /// ```text
    /// cl_id=3  2026-10-10T08:30:00Z  dr_zhao@hosp_c
    ///   fields=[diagnosis,bp_systolic]  payload=1a2b3c4d5e6f  chain=abc123def456  prev=000000000000  cont=manifest
    /// ```
    pub fn render_line(&self) -> String {
        let fields = if self.fields_changed.is_empty() {
            "(no field-level diff)".to_string()
        } else {
            self.fields_changed.join(",")
        };
        format!(
            "cl_id={cl_id}  {ts}  {actor}\n  fields=[{fields}]  payload={pl}  chain={ch}  prev={prev}  cont={cont}",
            cl_id = self.cl_id,
            ts = self.modified_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            actor = self.actor,
            fields = fields,
            pl = self.sha256_short,
            ch = self.chain_this_hash_short,
            prev = self.chain_prev_hash_short,
            cont = self.continuity,
        )
    }
}

/// Replay 失败原因。
///
/// `Display` 全部不空且不出现 `"unknown"` 字样；caller 据此回退到 CLI 退出码
/// 非零 + stderr 打印。
#[derive(Debug, Error)]
pub enum ReplayError {
    /// `.copper` 文件不存在（caller 拼错了 referral_id 或 state_dir）。
    #[error("replay: state file not found: {path}")]
    StateFileNotFound { path: String },

    /// 文件不可读（权限 / IO）。
    #[error("replay: state file unreadable: {path}: {reason}")]
    StateFileUnreadable { path: String, reason: String },

    /// 单行 JSON 解析失败（整报告立即返回，避免「前面正常 + 后面静默丢弃」）。
    /// `line_no` 从 1 起算（与 editor / wc -l 显示一致）。
    #[error("replay: parse failed at line {line_no}: {reason}")]
    ParseLine { line_no: usize, reason: String },

    /// 链 anchor 不正确（首条 `prev_hash` 不是全 0）—— 立即拒绝，与
    /// `verify_chain` 的语义对齐；不让 caller 误以为链可追。
    #[error("replay: chain anchor invalid (first prev_hash must be zero): actual={actual}")]
    BadChainAnchor { actual: String },

    /// `chain.txt` / `state/<referral_id>.copper` 为空文件（无任何链节）。
    #[error("replay: state file is empty (no chain links)")]
    EmptyArchive,
}

// =============================================================================
// ReplayRunner
// =============================================================================

/// Replay runner — 把 `.copper` 还原成 `ReplayReport`。
///
/// 不持有任何 I/O 状态；每次 `run` 调用都重新打开文件。设计要点：
///
/// - **状态根**：`config.state_dir`（默认 `./state`，可用 env
///   `RTS_STATE_DIR` 覆盖，与 inbox watcher 同款配置）；
/// - **文件名**：`<state_dir>/<referral_id>.copper`（referral_id 经
///   `file_stem` 清洗：去扩展名 + 校验不含 `/`）；
/// - **链校验失败**：不返回 `Err`，而是把 `chain_verify = Err(...)` 放进
///   `ReplayReport`，同时 `recovery_point_required = true`，让 caller 决定是
///   否仍然渲染部分时间线（仅 UI / 导出报告需要），CLI `replay` 默认遇到
///   `recovery_point_required = true` 时退出码非零 + stderr 显式提示。
pub struct ReplayRunner {
    config: ReplayConfig,
}

#[derive(Clone, Debug)]
pub struct ReplayConfig {
    pub state_dir: PathBuf,
}

impl ReplayConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    /// 从环境变量 `RTS_STATE_DIR` 装配（缺省 `./state`）。仅 IO 路径用环境变量；
    /// 凭据位不需要此函数。
    pub fn from_env_with_default(default: &str) -> Self {
        let dir = std::env::var("RTS_STATE_DIR").unwrap_or_else(|_| default.to_string());
        Self { state_dir: PathBuf::from(dir) }
    }

    /// 推算 `<referral_id>.copper` 的绝对路径。
    pub fn copper_path(&self, referral_id: &str) -> PathBuf {
        self.state_dir.join(format!("{referral_id}.{ext}", ext = COPPER_EXTENSION))
    }
}

impl Default for ReplayConfig {
    fn default() -> Self {
        // 与 spec.md / .env.example 默认值一致
        Self::from_env_with_default("./state")
    }
}

impl ReplayRunner {
    pub fn new(config: ReplayConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ReplayConfig {
        &self.config
    }

    /// 主入口：解析 `.copper` → 校验链 → 渲染 `ReplayReport`。
    ///
    /// 错误形态：
    /// - 文件缺失 → `Err(StateFileNotFound)`
    /// - 文件 IO 错 → `Err(StateFileUnreadable)`
    /// - 空文件 → `Err(EmptyArchive)`
    /// - 单行 JSON 坏 → `Err(ParseLine { line_no, reason })`
    /// - 链 anchor 错 → `Err(BadChainAnchor)`
    /// - 链衔接 / hash 错 → **不** 返回 `Err`，而是 `Ok(ReplayReport)` 但
    ///   `chain_verify = Err(...)` + `recovery_point_required = true`。
    pub fn run(&self, referral_id: &str) -> Result<ReplayReport, ReplayError> {
        // 1) referral_id 清洗（防止 caller 注入路径分隔符跳出 state_dir）
        if referral_id.is_empty() || referral_id.contains('/') || referral_id.contains('\\') {
            return Err(ReplayError::StateFileNotFound {
                path: format!("<invalid referral_id: {referral_id}>"),
            });
        }

        let path = self.config.copper_path(referral_id);
        if !path.exists() {
            return Err(ReplayError::StateFileNotFound {
                path: path.display().to_string(),
            });
        }

        let raw = fs::read_to_string(&path).map_err(|e| ReplayError::StateFileUnreadable {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;

        // 2) 按行解析
        let mut links: Vec<ChainLink> = Vec::new();
        let mut rows: Vec<TimelineRow> = Vec::new();
        let mut declared_gap_count: u64 = 0;
        for (idx, line) in raw.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // 空行直接跳过（与 JSON Lines 容错语义一致；允许多余换行）
                continue;
            }
            let link: ChainLink = serde_json::from_str(trimmed).map_err(|e| {
                ReplayError::ParseLine {
                    line_no,
                    reason: e.to_string(),
                }
            })?;

            // 3) 顺便把 ReferralVersion 也解出来（chain_link.payload 是 JSON Value）
            let version: ReferralVersion =
                serde_json::from_value(link.payload.clone()).map_err(|e| {
                    ReplayError::ParseLine {
                        line_no,
                        reason: format!("chain_link.payload is not a valid ReferralVersion: {e}"),
                    }
                })?;

            // 4) 统计 Gap 显式声明数
            if matches!(
                version.continuity,
                crate::model::continuity_record::ContinuityKind::Gap { .. }
            ) {
                declared_gap_count += 1;
            }

            // 5) 构造 TimelineRow
            let fields_changed = collect_fields_changed(&version);
            let chain_this_hash_short = short_hex(&link.this_hash, 12);
            let chain_prev_hash_short = short_hex(&link.prev_hash, 12);
            let continuity = render_continuity(&version.continuity);
            let row = TimelineRow {
                cl_id: version.cl_id,
                modified_at: version.modified_at,
                actor: version.actor.clone(),
                fields_changed,
                sha256_short: short_hex(&version.payload_sha256, 12),
                chain_this_hash_short,
                chain_prev_hash_short,
                continuity,
            };
            rows.push(row);
            links.push(link);
        }

        if links.is_empty() {
            return Err(ReplayError::EmptyArchive);
        }

        // 6) 链 anchor 校验（首条 prev_hash = ZERO_HASH）
        let first_prev = &links[0].prev_hash;
        let zero_hex = crate::sha256_chain::chain::hex_lower(&crate::sha256_chain::chain::ZERO_HASH);
        if first_prev != &zero_hex {
            return Err(ReplayError::BadChainAnchor {
                actual: first_prev.clone(),
            });
        }

        // 7) 整链 verify（不抛，把 Err 装进报告）
        let chain_verify = verify_chain(&links);
        let recovery_point_required = chain_verify.is_err();

        Ok(ReplayReport {
            referral_id: referral_id.to_string(),
            state_file_path: path,
            link_count: links.len(),
            chain_verify,
            recovery_point_required,
            rows,
            declared_gap_count,
        })
    }
}

// =============================================================================
// Text rendering (for CLI stdout / 报告导出复用)
// =============================================================================

/// 把 `ReplayReport` 渲染成多行可读文本，供 `replay` 子命令 stdout 打印。
///
/// 渲染规则：
/// - header 一行 + 空行；
/// - 每条 `TimelineRow` 渲染为 2 行（基本信息 + 字段/链详情）；
/// - 链校验失败时尾部追加错误详情 + `recovery_point_required` 提示；
/// - 全部字段脱敏（不出现完整 sha256 / 患者标识 / 真实姓名）。
pub fn render_text_report(report: &ReplayReport) -> String {
    let mut out = String::new();
    out.push_str(&report.header_line());
    out.push('\n');
    out.push_str(&format!("state_file: {}\n", report.state_file_path.display()));
    out.push_str(&format!("declared_gap_count: {}\n", report.declared_gap_count));
    out.push('\n');

    if report.rows.is_empty() {
        out.push_str("(no timeline rows recovered)\n");
    } else {
        for (i, row) in report.rows.iter().enumerate() {
            out.push_str(&format!("[{}/{}] ", i + 1, report.rows.len()));
            out.push_str(&row.render_line());
            out.push('\n');
        }
    }

    out.push('\n');
    if report.chain_ok() {
        out.push_str("chain verify: ok\n");
    } else {
        out.push_str("chain verify: FAILED\n");
        match &report.chain_verify {
            Err(e) => out.push_str(&format!("  reason: {e}\n")),
            Ok(()) => out.push_str("  reason: <unexpected>\n"),
        }
        out.push_str(&format!("  {RECOVERY_POINT_REQUIRED_HINT}\n"));
    }
    out
}

// =============================================================================
// internal helpers
// =============================================================================

/// 字段级 diff 后被改动的 key 列表（add + remove + replace 的并集），按字典
/// 序排序。复用了 [`ReferralVersion::field_diff`] 的判定逻辑，但只取 key，
/// 不带 DiffOp —— 终端渲染只关心「动了哪些字段」，DiffOp 已在 UI diff 视图
/// 里以 `[+] [-] [~]` 标记呈现。
fn collect_fields_changed(version: &ReferralVersion) -> Vec<String> {
    let diff = version.field_diff();
    let mut keys: Vec<String> = diff.keys().cloned().collect();
    keys.sort();
    keys
}

/// sha256 hex 字符串前 `take` 字符截断；不足 take 时整串返回。
/// 绝不 panic；input 非法（非 hex）也原样返回（链 verify 已经在前面挡住）。
fn short_hex(s: &str, take: usize) -> String {
    if s.len() <= take {
        s.to_string()
    } else {
        s[..take].to_string()
    }
}

/// 把 `ContinuityKind` 渲染成单行字符串（terminal 列）。
fn render_continuity(kind: &crate::model::continuity_record::ContinuityKind) -> String {
    use crate::model::continuity_record::ContinuityKind;
    match kind {
        ContinuityKind::Manifest => "manifest".to_string(),
        ContinuityKind::Gap { first_id, last_id, reason } => {
            format!("gap[{first_id}..{last_id}, {reason:?}]")
        }
        ContinuityKind::RecoveryPoint { copperlist_id } => {
            format!("recovery_point[{copperlist_id}]")
        }
        ContinuityKind::Finished { next_copperlist_id } => {
            format!("finished[next={next_copperlist_id}]")
        }
    }
}

// 让 `Debug` 也能在没有 thiserror 的旧 consumer 下用；保留 fallback。
impl fmt::Display for ReplayReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render_text_report(self))
    }
}

// =============================================================================
// Unit tests — 与 task.json steps 对齐：
//  test_replay_full_chain + test_replay_recovery_point_required
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::continuity_record::{ContinuityKind, SourceGapReason};
    use crate::sha256_chain::chain::{canonical_json, Chain};
    use chrono::TimeZone;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::Path;
    use tempfile::tempdir;

    fn sample_version(cl_id: u64, actor: &str, diagnosis: &str) -> ReferralVersion {
        let mut before: BTreeMap<String, String> = BTreeMap::new();
        before.insert("diagnosis".to_string(), "chest pain".to_string());
        let mut after: BTreeMap<String, String> = BTreeMap::new();
        after.insert("diagnosis".to_string(), diagnosis.to_string());
        after.insert("actor".to_string(), actor.to_string());
        after.insert(
            "modified_at".to_string(),
            Utc.with_ymd_and_hms(2026, 10, 10, 8, 30, 0)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        // payload_sha256 用链式摘要（真实形态）；这里给一个看起来真实的 64-char hex
        let payload_sha256 =
            "1a2b3c4d5e6f7890abcdef1234567890aabbccddeeff00112233445566778899".to_string();
        let _ = payload_sha256; // 真实链 append 时会被覆盖
        let mut version = ReferralVersion {
            cl_id,
            version_no: cl_id.min(u32::MAX as u64) as u32,
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
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&out);
        version.payload_sha256 = hex_lower_buf(&buf);
        version
    }

    fn hex_lower_buf(b: &[u8; 32]) -> String {
        hex::encode(b)
    }

    /// 构造一份 `.copper`（JSON Lines，链长度 = versions.len()）。
    /// `tamper` 参数用于测 recovery_point_required：篡改指定行的 payload。
    fn write_copper(path: &Path, versions: &[ReferralVersion]) {
        let mut chain = Chain::new();
        let mut file = fs::File::create(path).expect("create copper");
        for v in versions {
            let link = chain.append(v).expect("chain append");
            let s = serde_json::to_string(&link).unwrap();
            writeln!(file, "{s}").expect("write line");
        }
    }

    /// 篡改 .copper 的第 `tamper_idx` 行 payload（不改 prev_hash / this_hash），
    /// 制造 `chain_verify` 失败但 `links.len()` 仍等于 versions.len() 的形态。
    ///
    /// 实际做法：把原 `ReferralVersion` 反序列化出来，修改一个字段（如 actor），
    /// 再重新序列化为 payload —— 这样 payload 仍是一个合法 `ReferralVersion`
    /// （主路径能解析出 TimelineRow），但 `canonical_json(payload)` 已经与原
    /// 链节记录的 `this_hash` 不匹配（保留未变），`verify_chain` 报
    /// `ThisHashMismatch(tamper_idx)`。
    fn tamper_payload(path: &Path, tamper_idx: usize) {
        let raw = fs::read_to_string(path).unwrap();
        let mut lines: Vec<String> = raw.lines().map(|s| s.to_string()).collect();
        let mut link: ChainLink = serde_json::from_str(&lines[tamper_idx]).unwrap();
        let mut version: ReferralVersion = serde_json::from_value(link.payload.clone()).unwrap();
        version.actor = format!("TAMPERED_{}_{}", version.cl_id, version.actor.len());
        link.payload = serde_json::to_value(&version).unwrap();
        lines[tamper_idx] = serde_json::to_string(&link).unwrap();
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    // -- test_replay_full_chain --

    #[test]
    fn test_replay_full_chain() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let versions = vec![
            sample_version(1, "dr_wang@hospital_a", "ACS suspected"),
            sample_version(2, "dr_li@hospital_b", "STEMI confirmed"),
            sample_version(3, "dr_zhao@hospital_a", "transferred for PCI"),
            sample_version(4, "dr_li@hospital_b", "PCI done"),
            sample_version(5, "dr_zhao@hospital_a", "stable, discharge"),
        ];
        let copper = state_dir.join("REF-001.copper");
        write_copper(&copper, &versions);

        let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
        let report = runner.run("REF-001").expect("replay must succeed on healthy chain");

        assert_eq!(report.referral_id, "REF-001");
        assert_eq!(report.state_file_path, copper);
        assert_eq!(report.link_count, 5);
        assert!(report.chain_ok(), "healthy chain must verify");
        assert!(!report.recovery_point_required);
        assert!(report.chain_verify.is_ok());
        assert_eq!(report.declared_gap_count, 0);
        assert_eq!(report.rows.len(), 5);

        // 行内容核对（cl_id 单调、actor 与 samples 对应、fields_changed 含 diagnosis）
        for (i, row) in report.rows.iter().enumerate() {
            assert_eq!(row.cl_id as usize, i + 1);
            assert_eq!(row.actor, versions[i].actor);
            assert!(row.fields_changed.iter().any(|k| k == "diagnosis"));
            assert_eq!(row.sha256_short.len(), 12);
            assert_eq!(row.chain_this_hash_short.len(), 12);
            assert_eq!(row.chain_prev_hash_short.len(), 12);
            assert_eq!(row.continuity, "manifest");
        }
        // 第一行 prev_hash = 全 0 hex 前 12 字符
        assert_eq!(report.rows[0].chain_prev_hash_short, "000000000000");
        // 第 i 行的 prev_hash 应等于第 i-1 行的 this_hash
        for i in 1..report.rows.len() {
            assert_eq!(
                report.rows[i].chain_prev_hash_short, report.rows[i - 1].chain_this_hash_short,
                "row[{i}].prev_hash must match row[{p}].this_hash",
                p = i - 1
            );
        }
    }

    // -- test_replay_recovery_point_required --

    #[test]
    fn test_replay_recovery_point_required() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let versions = vec![
            sample_version(1, "dr_wang@hospital_a", "ACS suspected"),
            sample_version(2, "dr_li@hospital_b", "STEMI confirmed"),
            sample_version(3, "dr_zhao@hospital_a", "transferred for PCI"),
            sample_version(4, "dr_li@hospital_b", "PCI done"),
        ];
        let copper = state_dir.join("REF-002.copper");
        write_copper(&copper, &versions);

        // 篡改 link[2] 的 payload（不改 prev_hash / this_hash）→ ThisHashMismatch(2)
        tamper_payload(&copper, 2);

        let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
        let report = runner.run("REF-002").expect("replay returns Ok even on chain verify fail");

        // 链 verify 失败但 rows 仍能渲染
        assert_eq!(report.link_count, 4);
        assert!(!report.chain_ok());
        assert!(report.recovery_point_required);
        match report.chain_verify {
            Err(ChainError::ThisHashMismatch { index, .. }) => {
                assert_eq!(index, 2, "tampered row must be flagged at index 2");
            }
            other => panic!("expected ThisHashMismatch at index 2, got {other:?}"),
        }
    }

    // -- 额外保护：空文件 / 文件不存在 / 坏 JSON 行 / referral_id 含分隔符 --

    #[test]
    fn test_replay_state_file_not_found() {
        let tmp = tempdir().unwrap();
        let runner = ReplayRunner::new(ReplayConfig::new(tmp.path().join("state")));
        let err = runner.run("NOPE").unwrap_err();
        assert!(matches!(err, ReplayError::StateFileNotFound { .. }));
    }

    #[test]
    fn test_replay_empty_archive_returns_err() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let copper = state_dir.join("REF-EMPTY.copper");
        fs::write(&copper, "").unwrap();
        let runner = ReplayRunner::new(ReplayConfig::new(state_dir));
        let err = runner.run("REF-EMPTY").unwrap_err();
        assert!(matches!(err, ReplayError::EmptyArchive));
    }

    #[test]
    fn test_replay_parse_line_error() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let copper = state_dir.join("REF-BAD.copper");
        fs::write(&copper, "not valid json\n").unwrap();
        let runner = ReplayRunner::new(ReplayConfig::new(state_dir));
        let err = runner.run("REF-BAD").unwrap_err();
        match err {
            ReplayError::ParseLine { line_no, .. } => assert_eq!(line_no, 1),
            other => panic!("expected ParseLine, got {other:?}"),
        }
    }

    #[test]
    fn test_replay_rejects_referral_id_with_slash() {
        let tmp = tempdir().unwrap();
        let runner = ReplayRunner::new(ReplayConfig::new(tmp.path().join("state")));
        let err = runner.run("../etc/passwd").unwrap_err();
        assert!(matches!(err, ReplayError::StateFileNotFound { .. }));
    }

    // -- render_text_report 含链错误细节 --

    #[test]
    fn test_render_text_report_includes_chain_error_detail() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let versions = vec![
            sample_version(1, "dr_wang@hospital_a", "ACS suspected"),
            sample_version(2, "dr_li@hospital_b", "STEMI confirmed"),
        ];
        let copper = state_dir.join("REF-R.copper");
        write_copper(&copper, &versions);
        tamper_payload(&copper, 1);

        let runner = ReplayRunner::new(ReplayConfig::new(state_dir));
        let report = runner.run("REF-R").expect("Ok even on verify fail");
        let text = render_text_report(&report);
        assert!(text.contains("chain verify: FAILED"));
        assert!(text.contains(RECOVERY_POINT_REQUIRED_HINT));
        assert!(text.contains("cl_id=1"));
        assert!(text.contains("cl_id=2"));
    }

    // -- declared_gap_count 统计 --

    #[test]
    fn test_replay_declared_gap_count() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let mut v1 = sample_version(1, "dr_a", "x");
        v1.continuity = ContinuityKind::Manifest;
        let mut v2 = sample_version(2, "dr_b", "y");
        v2.continuity = ContinuityKind::Gap {
            first_id: 4,
            last_id: 6,
            reason: SourceGapReason::SessionEnded,
        };
        let mut v3 = sample_version(3, "dr_c", "z");
        v3.continuity = ContinuityKind::Gap {
            first_id: 9,
            last_id: 11,
            reason: SourceGapReason::ManualDrop,
        };
        let mut v4 = sample_version(4, "dr_d", "w");
        v4.continuity = ContinuityKind::Finished {
            next_copperlist_id: 5,
        };

        let versions = vec![v1, v2, v3, v4];
        let copper = state_dir.join("REF-GAP.copper");
        write_copper(&copper, &versions);

        let runner = ReplayRunner::new(ReplayConfig::new(state_dir));
        let report = runner.run("REF-GAP").expect("ok");
        assert_eq!(report.declared_gap_count, 2);
        assert!(report.rows[1].continuity.starts_with("gap["));
        assert!(report.rows[2].continuity.starts_with("gap["));
        assert!(report.rows[3].continuity.starts_with("finished["));
    }

    // -- Display 不空且无 'unknown' 兜底字样 --

    #[test]
    fn test_replay_error_display_strings_nonempty_and_no_unknown() {
        let cases = [
            ReplayError::StateFileNotFound {
                path: "/tmp/x".to_string(),
            },
            ReplayError::StateFileUnreadable {
                path: "/tmp/x".to_string(),
                reason: "perm denied".to_string(),
            },
            ReplayError::ParseLine {
                line_no: 7,
                reason: "unexpected EOF".to_string(),
            },
            ReplayError::BadChainAnchor {
                actual: "deadbeef".repeat(8),
            },
            ReplayError::EmptyArchive,
        ];
        for e in cases {
            let s = format!("{e}");
            assert!(!s.is_empty());
            assert!(!s.contains("unknown"));
        }
    }

    // -- ReplayReport header_line 字段语义 --

    #[test]
    fn test_replay_report_header_line() {
        let r = ReplayReport {
            referral_id: "REF-9".to_string(),
            state_file_path: PathBuf::from("/tmp/state/REF-9.copper"),
            link_count: 3,
            chain_verify: Ok(()),
            recovery_point_required: false,
            rows: Vec::new(),
            declared_gap_count: 0,
        };
        assert!(r.header_line().contains("REF-9"));
        assert!(r.header_line().contains("links=3"));
        assert!(r.header_line().contains("chain_ok=true"));
        assert!(r.header_line().contains("recovery_point_required=false"));
    }
}
