//! 集成测试（3 份转诊单 end-to-end：load → fsck → replay → export → verify）
//!
//! ## 范围
//!
//! 验证 3 份 referral fixtures 在「**fsck 空洞检测 → replay 链校验 → export 打 zip
//! → verify 反向核对**」全流程上的端到端行为：
//!
//! - `referral_001`：健康 5 步链（cl_ids 1..=5，chain 完整）；
//! - `referral_002`：健康 8 步链（cl_ids 1..=8，chain 完整）；
//! - `referral_003`：6 步链后删除中间 cl_id=4 制造空洞（cl_ids 1,2,3,5,6），
//!   同时 link[2] (cl_id=3) 的 payload 被改动以模拟「gap 场景下投递链路常伴
//!   随的 in-transit corruption」—— fsck 检测 cl_id 空洞、replay 检测 payload
//!   篡改、verify 检测 zip 内篡改，三层防御独立触发并互相印证。
//!
//! ## 与各模块内嵌单测的区别
//!
//! - `src/copperlist/fsck.rs`、`src/sha256_chain/chain.rs`、`src/replay/runner.rs`、
//!   `src/export/bundle.rs`、`src/verify/verifier.rs` 内嵌单测只覆盖**单一模块**
//!   的入口与边界；
//! - 本文件作为**集成测试**链接到 `referral_trace_station::*`，从外部视角跑完
//!   「构造 → 写入 → fsck → replay → export → verify」全流程，**串起来**检查
//!   模块间数据契约（ChainLink JSON 序列化、BTreeMap 字段顺序、canonical_json
//!   跨环境一致性、zip 内条目命名规范）。
//!
//! ## Fixtures 组织
//!
//! 静态 fixtures 写在 `tests/fixtures/referral_XXX/{state,inbox}/`：
//!
//! - `state/referral_XXX.copper`：JSON Lines 格式的链节存档（每行一条
//!   `ChainLink`），由 `setup_fixtures()` 在测试启动时**幂等生成**（每次都
//!   重写以保证与源码同步，避免 fixture 漂移）；
//! - `inbox/.gitkeep`：占位空文件，确保 `inbox/` 目录存在（spec.md §5「drop
//!   入站」目录约定）。
//!
//! ## 断言结构（与 task.json steps 对齐）
//!
//! 1. **fsck 三类断言**：
//!    - referral_001/002：`healthy=true`、`missing=0`、无 hole；
//!    - referral_003：`healthy=false`、`missing ≥ 1`、第一个 hole `first_id=4,
//!      last_id=4`、reason 为 `SessionEnded`（单条小跨度默认归因）。
//! 2. **replay 链路一致**：
//!    - referral_001/002：`chain_ok=true`、`recovery_point_required=false`、
//!      `rows.len()` 等于版本数；
//!    - referral_003：`chain_ok=false`（payload tamper）、`recovery_point_required=true`、
//!      `chain_error` 含 `ThisHashMismatch`。
//! 3. **export 三方条目**：
//!    - 三份 bundle.zip 均含 `manifest.json` / `chain.txt` / `versions/<cl_id>.json`
//!      三类条目；unzip -l 可列；`manifest.version_count` 与 `versions/` 内条目数一致；
//!      `chain.txt` 末行 = `end of chain` 哨兵。
//! 4. **verify 三向比对**：
//!    - referral_001/002：`chain_ok=true`、`is_clean()=true`、`tamper_locations.is_empty()`；
//!    - referral_003：`chain_ok=false`、`recovery_point_required=true`、
//!      `tamper_locations` 非空且至少一条 `ChainTxtThisMismatch`（来自 cl_id=3 的
//!      payload tamper 在 verify 重算时被识破）。

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use sha2::{Digest, Sha256};

use referral_trace_station::copperlist::fsck::{CopperListSequence, FsckReport};
use referral_trace_station::export::{BundleConfig, BundleWriter};
use referral_trace_station::model::continuity_record::{ContinuityKind, SourceGapReason};
use referral_trace_station::model::referral_version::ReferralVersion;
use referral_trace_station::replay::{ReplayConfig, ReplayRunner};
use referral_trace_station::sha256_chain::chain::{canonical_json, Chain, ChainLink};
use referral_trace_station::verify::{TamperKind, Verifier};

// =============================================================================
// Fixtures 路径与版本规约
// =============================================================================

/// 测试 fixtures 根目录（与 `integration_inbox_zip.rs` 等既有 integration 测试
/// 一致——一律走 `env!("CARGO_MANIFEST_DIR")` 拼绝对路径，避免 working dir
/// 切换影响路径推断）。
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// 单份 referral 的 `state/` 目录：放 `<referral_id>.copper`。
fn referral_state_dir(referral_id: &str) -> PathBuf {
    fixtures_root().join(referral_id).join("state")
}

/// 单份 referral 的 `inbox/` 目录：放 drop 入站 zip（占位即可）。
fn referral_inbox_dir(referral_id: &str) -> PathBuf {
    fixtures_root().join(referral_id).join("inbox")
}

// =============================================================================
// 数据构造 helpers
// =============================================================================

/// 构造一份 `ReferralVersion`，并把 `payload_sha256` 字段填上
/// `sha256(canonical_json(after))`（与 `src/replay/runner.rs` 内嵌测试
/// `sample_version` 一致；这是 audit 端的链式 hash，而 `Chain::append` 计算的
/// 是 `prev_hash || canonical_json(payload)` 链式 hash，两者用途不同）。
fn make_referral_version(cl_id: u64, actor: &str, diagnosis: &str) -> ReferralVersion {
    let mut before: BTreeMap<String, String> = BTreeMap::new();
    before.insert("diagnosis".to_string(), "chest pain".to_string());
    let mut after: BTreeMap<String, String> = BTreeMap::new();
    after.insert("diagnosis".to_string(), diagnosis.to_string());
    after.insert("actor".to_string(), actor.to_string());

    // modified_at 沿 cl_id 递增（每步 1 秒，便于 fsck observe_with_time 推断
    // SessionEnded 而非 ManualDrop；ManualDrop 需要 cl_id=4 前后 ≥10 秒时差，
    // 而我们用 1 秒步长 → SessionEnded 路径）。
    let mut version = ReferralVersion {
        cl_id,
        version_no: cl_id.min(u32::MAX as u64) as u32,
        actor: actor.to_string(),
        modified_at: Utc
            .with_ymd_and_hms(2026, 10, 10, 8, 30, 0)
            .unwrap()
            + chrono::Duration::seconds(cl_id as i64 - 1),
        before,
        after,
        continuity: ContinuityKind::Manifest,
        payload_sha256: String::new(),
    };

    let bytes = canonical_json(&version.after).expect("canonical_json after");
    let mut h = Sha256::new();
    h.update(&bytes);
    let out = h.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    version.payload_sha256 = hex::encode(buf);
    version
}

/// 把一组 `ReferralVersion` 用 `Chain::append` 串成 JSON Lines 写入 `.copper`。
/// `Chain::append` 内部已 canonical_json + sha256(prev || payload)，所以产物
/// 的 `this_hash` 一律正确。
fn write_copper(path: &Path, versions: &[ReferralVersion]) {
    let mut chain = Chain::new();
    let mut file = fs::File::create(path).expect("create .copper");
    for v in versions {
        let link = chain.append(v).expect("chain.append");
        let s = serde_json::to_string(&link).expect("serialize ChainLink");
        writeln!(file, "{s}").expect("writeln ChainLink");
    }
}

/// 从 `.copper` 重新打开并按行解析 `Vec<ChainLink>`。与 `src/main.rs` 的
/// `load_links_for_export` 行为一致，但放在这里避免引入 main 模块。
fn load_links(state_dir: &Path, referral_id: &str) -> Vec<ChainLink> {
    let path = state_dir.join(format!("{referral_id}.copper"));
    let file = fs::File::open(&path).expect("open .copper for load_links");
    let reader = BufReader::new(file);
    let mut links = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.expect("read line");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let link: ChainLink =
            serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("line {idx} parse: {e}"));
        links.push(link);
    }
    links
}

/// 在一组 `ReferralVersion` 上跑 `CopperListSequence::observe_with_time` 并
/// 汇总 `FsckReport`——与 `src/copperlist/fsck.rs` 内嵌单测同源。
fn run_fsck(versions: &[ReferralVersion]) -> FsckReport {
    let mut seq = CopperListSequence::new();
    for v in versions {
        seq.observe_with_time(v.cl_id, v.modified_at.timestamp());
    }
    seq.fsck_report()
}

/// 修改 `.copper` 中第 `tamper_idx` 行的 `ReferralVersion.actor`（不改
/// `prev_hash` / `this_hash`），制造「`chain_link.prev_hash` 仍正确衔接，但
/// 重算 `sha256(prev || canonical_json(payload))` 与链上 `this_hash` 不一致」
/// 的形态——`replay` 路径的 `verify_chain` 报 `ThisHashMismatch(tamper_idx)`，
/// `verify` 路径的 `verify_zip` 报 `ChainTxtThisMismatch`。
fn tamper_copper_payload(path: &Path, tamper_idx: usize) {
    let raw = fs::read_to_string(path).expect("read copper for tamper");
    let mut lines: Vec<String> = raw.lines().map(|s| s.to_string()).collect();
    assert!(
        tamper_idx < lines.len(),
        "tamper_idx {tamper_idx} out of range (lines={})",
        lines.len()
    );
    let mut link: ChainLink = serde_json::from_str(&lines[tamper_idx]).expect("parse link for tamper");
    let mut version: ReferralVersion =
        serde_json::from_value(link.payload.clone()).expect("payload is ReferralVersion");
    version.actor = format!("TAMPERED_actor_{}", version.cl_id);
    // 重算 payload_sha256 与 modified_at 一致性（仅替换 actor，前者不必修改：
    // payload_sha256 是 `canonical_json(after)` 的 hash，actor 字段改了之后 hash
    // 也变了，但 verify 不读 payload_sha256，它重算的是 `chain_link.this_hash`）。
    link.payload = serde_json::to_value(&version).expect("serialize tampered ReferralVersion");
    lines[tamper_idx] = serde_json::to_string(&link).expect("serialize tampered ChainLink");
    fs::write(path, lines.join("\n") + "\n").expect("write tampered copper");
}

// =============================================================================
// setup_fixtures —— 三份 referral fixtures 的幂等生成
// =============================================================================

/// 在测试启动时**幂等**生成三份 fixtures。`inbox/` 用 `.gitkeep` 占位（确保
/// 目录存在 + 不污染仓库），`state/` 写入真实 `.copper` JSON Lines 文件。
fn setup_fixtures() {
    // 三份 fixture 各自的 cl_id 序列：
    //   referral_001 → [1, 2, 3, 4, 5]    健康 5 步
    //   referral_002 → [1, 2, 3, 4, 5, 6, 7, 8]    健康 8 步
    //   referral_003 → [1, 2, 3, 5, 6]   健康 6 步后删除中间 cl_id=4
    let spec: &[(&str, &[u64])] = &[
        ("referral_001", &[1, 2, 3, 4, 5]),
        ("referral_002", &[1, 2, 3, 4, 5, 6, 7, 8]),
        ("referral_003", &[1, 2, 3, 5, 6]),
    ];

    for (referral_id, cl_ids) in spec {
        // 1) inbox/ 占位（spec.md §5 路径约定）
        let inbox_dir = referral_inbox_dir(referral_id);
        fs::create_dir_all(&inbox_dir).expect("create inbox dir");
        let gitkeep = inbox_dir.join(".gitkeep");
        if !gitkeep.exists() {
            fs::write(&gitkeep, b"# placeholder so the inbox dir exists in fixtures\n")
                .expect("write .gitkeep");
        }

        // 2) state/referral_XXX.copper（每次都重写——保证与源码同步）
        let state_dir = referral_state_dir(referral_id);
        fs::create_dir_all(&state_dir).expect("create state dir");
        let copper_path = state_dir.join(format!("{referral_id}.copper"));

        let versions: Vec<ReferralVersion> = cl_ids
            .iter()
            .map(|&cl_id| {
                let hospital = if cl_id % 2 == 0 { "b" } else { "a" };
                make_referral_version(
                    cl_id,
                    &format!("dr_step{cl_id}@hospital_{hospital}"),
                    &format!("diagnosis_step{cl_id}"),
                )
            })
            .collect();

        write_copper(&copper_path, &versions);
    }

    // 3) referral_003 额外 tamper：在 link[2]（cl_id=3）的 actor 字段写入
    //    「TAMPERED_actor_3」—— 不改 prev_hash / this_hash。fsck 因 cl_id 序列
    //    不连续仍报 gap；replay.verify_chain 因 canonical_json 变化报
    //    ThisHashMismatch(index=2)；verify 因 chain.txt this_hash[..12] 与
    //    重算不一致报 ChainTxtThisMismatch(index=3)。
    let copper_003 = referral_state_dir("referral_003").join("referral_003.copper");
    tamper_copper_payload(&copper_003, 2);
}

// =============================================================================
// 主测试 —— referral_001 / referral_002 / referral_003 端到端
// =============================================================================

#[test]
fn test_end_to_end_three_referrals() {
    // 0) Fixtures 幂等生成
    setup_fixtures();

    let tmp_bundle_dir = tempfile::tempdir().expect("tempdir for bundles");

    // ---------------------------------------------------------------------
    // 1) referral_001 —— 健康 5 步链：fsck OK / replay OK / export OK / verify OK
    // ---------------------------------------------------------------------
    {
        let versions: Vec<ReferralVersion> = [1u64, 2, 3, 4, 5]
            .iter()
            .map(|&cl_id| make_referral_version(cl_id, &format!("dr_a{cl_id}@hospital_a"), &format!("d{cl_id}")))
            .collect();

        // fsck
        let fsck = run_fsck(&versions);
        assert!(
            fsck.healthy,
            "referral_001 fsck must be healthy (got holes={:?} missing={})",
            fsck.holes,
            fsck.missing
        );
        assert_eq!(fsck.missing, 0, "referral_001 missing must be 0");
        assert!(fsck.holes.is_empty(), "referral_001 must have no holes");

        // replay
        let state_dir = referral_state_dir("referral_001");
        let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
        let report = runner
            .run("referral_001")
            .expect("referral_001 replay must succeed");
        assert!(
            report.chain_ok(),
            "referral_001 chain verify must pass (got {:?})",
            report.chain_verify
        );
        assert_eq!(report.rows.len(), 5, "referral_001 must have 5 rows");
        assert!(
            !report.recovery_point_required,
            "referral_001 must not require recovery_point"
        );

        // export
        let bundle_path = tmp_bundle_dir.path().join("bundle_referral_001.zip");
        let links = load_links(&state_dir, "referral_001");
        BundleWriter::new(BundleConfig::new("referral_001", bundle_path.clone()))
            .write(&links)
            .expect("referral_001 export must succeed");
        assert!(
            bundle_path.exists(),
            "referral_001 bundle must exist at {}",
            bundle_path.display()
        );

        // verify
        let verify = Verifier::new()
            .verify_zip(&bundle_path)
            .expect("referral_001 verify must succeed (zip exists)");
        assert!(
            verify.chain_ok,
            "referral_001 verify chain_ok must be true (tamper_locations={:?})",
            verify.tamper_locations
        );
        assert!(
            !verify.recovery_point_required,
            "referral_001 must not require recovery_point"
        );
        assert!(
            verify.tamper_locations.is_empty(),
            "referral_001 must have no tamper_locations"
        );
        assert!(
            verify.is_clean(),
            "referral_001 is_clean() must be true"
        );
        assert_eq!(
            verify.recomputed_version_count, 5,
            "referral_001 recomputed_version_count must equal 5"
        );
        assert_eq!(
            verify.manifest_version_count, 5,
            "referral_001 manifest_version_count must equal 5"
        );
    }

    // ---------------------------------------------------------------------
    // 2) referral_002 —— 健康 8 步链：fsck OK / replay OK / export OK / verify OK
    // ---------------------------------------------------------------------
    {
        let versions: Vec<ReferralVersion> = [1u64, 2, 3, 4, 5, 6, 7, 8]
            .iter()
            .map(|&cl_id| make_referral_version(cl_id, &format!("dr_b{cl_id}@hospital_b"), &format!("d{cl_id}")))
            .collect();

        // fsck
        let fsck = run_fsck(&versions);
        assert!(
            fsck.healthy,
            "referral_002 fsck must be healthy (got holes={:?} missing={})",
            fsck.holes,
            fsck.missing
        );
        assert_eq!(fsck.missing, 0);
        assert!(fsck.holes.is_empty());

        // replay
        let state_dir = referral_state_dir("referral_002");
        let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
        let report = runner
            .run("referral_002")
            .expect("referral_002 replay must succeed");
        assert!(report.chain_ok(), "referral_002 chain verify must pass");
        assert_eq!(report.rows.len(), 8);
        assert!(!report.recovery_point_required);

        // export
        let bundle_path = tmp_bundle_dir.path().join("bundle_referral_002.zip");
        let links = load_links(&state_dir, "referral_002");
        BundleWriter::new(BundleConfig::new("referral_002", bundle_path.clone()))
            .write(&links)
            .expect("referral_002 export must succeed");
        assert!(bundle_path.exists());

        // verify
        let verify = Verifier::new()
            .verify_zip(&bundle_path)
            .expect("referral_002 verify must succeed");
        assert!(
            verify.chain_ok,
            "referral_002 chain_ok must be true (tamper_locations={:?})",
            verify.tamper_locations
        );
        assert!(!verify.recovery_point_required);
        assert!(verify.tamper_locations.is_empty());
        assert!(verify.is_clean());
        assert_eq!(verify.recomputed_version_count, 8);
        assert_eq!(verify.manifest_version_count, 8);
    }

    // ---------------------------------------------------------------------
    // 3) referral_003 —— 6 步链删除中间 cl_id=4 + link[2] tamper
    //    → fsck 报 [4..4] 空洞 + SessionEnded reason
    //    → replay 报 ThisHashMismatch(index=2)（payload tamper）
    //    → verify 报 ChainTxtThisMismatch（chain.txt vs 重算不一致）
    // ---------------------------------------------------------------------
    {
        // 重建 versions 用作 fsck 输入（cl_ids 1,2,3,5,6 触发 [4..4] 空洞）
        let versions: Vec<ReferralVersion> = [1u64, 2, 3, 5, 6]
            .iter()
            .map(|&cl_id| make_referral_version(cl_id, &format!("dr_c{cl_id}@hospital_c"), &format!("d{cl_id}")))
            .collect();

        // fsck —— 必须报 [4..4] 空洞 + SessionEnded
        let fsck = run_fsck(&versions);
        assert!(
            !fsck.healthy,
            "referral_003 fsck must NOT be healthy (cl_id=4 missing) (got healthy=true)"
        );
        assert!(
            fsck.missing >= 1,
            "referral_003 missing must be ≥ 1 (got {})",
            fsck.missing
        );
        assert!(
            !fsck.holes.is_empty(),
            "referral_003 must report at least one hole"
        );
        let first_hole = &fsck.holes[0];
        assert_eq!(
            first_hole.first_id, 4,
            "hole first_id must be 4 (got {})",
            first_hole.first_id
        );
        assert_eq!(
            first_hole.last_id, 4,
            "hole last_id must be 4 (single-ID gap) (got {})",
            first_hole.last_id
        );
        assert_eq!(
            first_hole.reason,
            SourceGapReason::SessionEnded,
            "hole reason must be SessionEnded for single-ID gap with no large dt"
        );
        assert!(!fsck.truncated, "5-link fixture must not be truncated");

        // replay —— 必须报 chain_verify=Err(ThisHashMismatch) 因为 link[2] 的
        // payload 被 setup_fixtures 阶段 tamper 过（actor 改为 TAMPERED_actor_3）
        let state_dir = referral_state_dir("referral_003");
        let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
        let report = runner
            .run("referral_003")
            .expect("referral_003 replay must succeed (returns ReplayReport even when chain broken)");
        assert!(
            !report.chain_ok(),
            "referral_003 chain_ok must be false (link[2] tampered)"
        );
        assert!(
            report.recovery_point_required,
            "referral_003 must require recovery_point"
        );
        let chain_err = report
            .chain_verify
            .as_ref()
            .err()
            .expect("referral_003 chain_verify must be Err");
        let chain_err_str = format!("{chain_err}");
        assert!(
            chain_err_str.contains("this_hash mismatch"),
            "referral_003 chain error must be this_hash mismatch (got: {chain_err_str})"
        );
        assert!(
            chain_err_str.contains("link[2]"),
            "referral_003 chain error must reference link[2] (got: {chain_err_str})"
        );
        assert_eq!(report.rows.len(), 5, "referral_003 must have 5 rows (cl_ids 1,2,3,5,6)");

        // export —— BundleWriter 不做 chain verify，写出仍成功（spec.md §6 分层
        // 设计：export = 写盘；verify = 子命令）
        let bundle_path = tmp_bundle_dir.path().join("bundle_referral_003.zip");
        let links = load_links(&state_dir, "referral_003");
        BundleWriter::new(BundleConfig::new("referral_003", bundle_path.clone()))
            .write(&links)
            .expect("referral_003 export must succeed (BundleWriter does not chain-verify)");
        assert!(bundle_path.exists());

        // verify —— 必须报 chain_ok=false + ChainTxtThisMismatch
        let verify = Verifier::new()
            .verify_zip(&bundle_path)
            .expect("referral_003 verify must succeed (zip exists, payload tamper detected)");
        assert!(
            !verify.chain_ok,
            "referral_003 chain_ok must be false (tamper detected) (tamper_locations={:?})",
            verify.tamper_locations
        );
        assert!(
            verify.recovery_point_required,
            "referral_003 must require recovery_point (chain tamper detected)"
        );
        assert!(
            !verify.tamper_locations.is_empty(),
            "referral_003 must report at least one tamper_location"
        );
        let has_chain_txt_this_mismatch = verify
            .tamper_locations
            .iter()
            .any(|t| t.kind == TamperKind::ChainTxtThisMismatch);
        assert!(
            has_chain_txt_this_mismatch,
            "referral_003 must report ChainTxtThisMismatch (got tamper_locations={:?})",
            verify.tamper_locations
        );
        assert_eq!(
            verify.recomputed_version_count, 5,
            "referral_003 recomputed_version_count must equal 5"
        );
    }
}

// =============================================================================
// 单 referral 拆解测试 —— 与主测试并行提供更细粒度的诊断信息
// =============================================================================

/// referral_001 单独跑：覆盖主测试的 fsck / replay / export / verify 全部断言，
/// 失败时报告更聚焦。
#[test]
fn test_referral_001_healthy_5_step_e2e() {
    setup_fixtures();

    let versions: Vec<ReferralVersion> = [1u64, 2, 3, 4, 5]
        .iter()
        .map(|&cl_id| {
            make_referral_version(
                cl_id,
                &format!("dr_a{cl_id}@hospital_a"),
                &format!("d{cl_id}"),
            )
        })
        .collect();

    let fsck = run_fsck(&versions);
    assert!(fsck.healthy);
    assert_eq!(fsck.missing, 0);
    assert!(fsck.holes.is_empty());

    let state_dir = referral_state_dir("referral_001");
    let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
    let report = runner.run("referral_001").expect("replay");
    assert!(report.chain_ok());
    assert_eq!(report.rows.len(), 5);

    let tmp = tempfile::tempdir().expect("tempdir");
    let bundle_path = tmp.path().join("bundle_001.zip");
    let links = load_links(&state_dir, "referral_001");
    BundleWriter::new(BundleConfig::new("referral_001", bundle_path.clone()))
        .write(&links)
        .expect("export");

    // Spot-check：解压 bundle 校验三类条目齐
    let bundle_file = fs::File::open(&bundle_path).expect("open bundle");
    let mut archive = zip::ZipArchive::new(bundle_file).expect("zip open");
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names.sort();
    assert!(
        names.iter().any(|n| n == "manifest.json"),
        "bundle must contain manifest.json (got: {names:?})"
    );
    assert!(
        names.iter().any(|n| n == "chain.txt"),
        "bundle must contain chain.txt (got: {names:?})"
    );
    let versions_count = names.iter().filter(|n| n.starts_with("versions/") && n.ends_with(".json")).count();
    assert_eq!(
        versions_count, 5,
        "bundle must contain 5 versions/*.json entries (got {versions_count})"
    );

    // chain.txt 末行 = "end of chain" 哨兵
    let mut chain_txt_entry = archive.by_name("chain.txt").expect("chain.txt entry");
    let mut chain_txt = String::new();
    chain_txt_entry
        .read_to_string(&mut chain_txt)
        .expect("read chain.txt");
    let last_line = chain_txt.lines().filter(|l| !l.trim().is_empty()).last();
    assert_eq!(
        last_line,
        Some("end of chain"),
        "chain.txt last non-empty line must be 'end of chain' (got: {:?})",
        last_line
    );

    // verify 应通过
    let verify = Verifier::new().verify_zip(&bundle_path).expect("verify");
    assert!(verify.is_clean(), "referral_001 must verify clean");
    assert_eq!(verify.recomputed_chain_head_hash, verify.manifest_chain_head_hash);
}

/// referral_002 单独跑：8 步健康链的独立验证。
#[test]
fn test_referral_002_healthy_8_step_e2e() {
    setup_fixtures();

    let versions: Vec<ReferralVersion> = [1u64, 2, 3, 4, 5, 6, 7, 8]
        .iter()
        .map(|&cl_id| {
            make_referral_version(
                cl_id,
                &format!("dr_b{cl_id}@hospital_b"),
                &format!("d{cl_id}"),
            )
        })
        .collect();

    let fsck = run_fsck(&versions);
    assert!(fsck.healthy);
    assert_eq!(fsck.missing, 0);
    assert_eq!(fsck.first_id, Some(1));
    assert_eq!(fsck.last_id, Some(8));
    assert_eq!(fsck.entries, 8);

    let state_dir = referral_state_dir("referral_002");
    let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
    let report = runner.run("referral_002").expect("replay");
    assert!(report.chain_ok());
    assert_eq!(report.rows.len(), 8);

    let tmp = tempfile::tempdir().expect("tempdir");
    let bundle_path = tmp.path().join("bundle_002.zip");
    let links = load_links(&state_dir, "referral_002");
    BundleWriter::new(BundleConfig::new("referral_002", bundle_path.clone()))
        .write(&links)
        .expect("export");

    let verify = Verifier::new().verify_zip(&bundle_path).expect("verify");
    assert!(verify.is_clean());
    assert_eq!(verify.recomputed_version_count, 8);
}

/// referral_003 单独跑：gap + payload tamper 的独立验证。
#[test]
fn test_referral_003_gap_with_tamper_e2e() {
    setup_fixtures();

    // fsck 输入用 cl_ids 1,2,3,5,6
    let versions: Vec<ReferralVersion> = [1u64, 2, 3, 5, 6]
        .iter()
        .map(|&cl_id| {
            make_referral_version(
                cl_id,
                &format!("dr_c{cl_id}@hospital_c"),
                &format!("d{cl_id}"),
            )
        })
        .collect();

    let fsck = run_fsck(&versions);
    assert!(!fsck.healthy);
    assert!(fsck.missing >= 1);
    assert!(!fsck.holes.is_empty());
    let hole = &fsck.holes[0];
    assert_eq!(hole.first_id, 4);
    assert_eq!(hole.last_id, 4);
    assert_eq!(hole.reason, SourceGapReason::SessionEnded);

    // replay 检测 tamper at link[2]
    let state_dir = referral_state_dir("referral_003");
    let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
    let report = runner.run("referral_003").expect("replay");
    assert!(!report.chain_ok());
    assert!(report.recovery_point_required);
    let chain_err_str = format!(
        "{}",
        report.chain_verify.as_ref().err().expect("chain_verify must be Err")
    );
    assert!(
        chain_err_str.contains("this_hash mismatch"),
        "replay must report this_hash mismatch (got: {chain_err_str})"
    );
    assert!(
        chain_err_str.contains("link[2]"),
        "replay must reference link[2] (got: {chain_err_str})"
    );

    // export
    let tmp = tempfile::tempdir().expect("tempdir");
    let bundle_path = tmp.path().join("bundle_003.zip");
    let links = load_links(&state_dir, "referral_003");
    BundleWriter::new(BundleConfig::new("referral_003", bundle_path.clone()))
        .write(&links)
        .expect("export");

    // verify
    let verify = Verifier::new().verify_zip(&bundle_path).expect("verify");
    assert!(!verify.chain_ok);
    assert!(verify.recovery_point_required);
    assert!(!verify.tamper_locations.is_empty());
    assert!(
        verify
            .tamper_locations
            .iter()
            .any(|t| t.kind == TamperKind::ChainTxtThisMismatch),
        "verify must report ChainTxtThisMismatch (got: {:?})",
        verify.tamper_locations
    );
}