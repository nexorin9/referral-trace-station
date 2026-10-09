//! 性能验收（深度证据 c · plan.md §4 + spec.md §4 真实规模输入）。
//!
//! ## 目标
//!
//! 验证 100 份转诊单 × 5 版本在 **fsck 空洞检测 + replay 链校验 + render_text_report 渲染**
//! 全流程下的总耗时 ≤ 30 秒。这是 spec.md §4「真实规模输入 + 可度量验收」的深度证据 (c)。
//!
//! ## 范围
//!
//! - **fsck**：对每份 referral 的 5 个 cl_id 调用 `CopperListSequence::observe_with_time`，
//!   然后 `fsck_report()` 汇总 —— 与 `src/copperlist/fsck.rs::observe_with_time` 的实参语义
//!   一致（id + unix_ts）。
//! - **replay**：对每份 referral 的 `.copper` 跑 `ReplayRunner::run` —— 包括
//!   `fs::read_to_string` + JSON Lines 解析 + `verify_chain` 链校验 + TimelineRow 构造。
//! - **render**：对每个 `ReplayReport` 跑 `render_text_report` —— 多行人读文本
//!   拼接、链错误细节格式化。
//!
//! ## 数据规模
//!
//! 100 份 fixture：
//! - 99 份健康链（cl_ids 1..=5 连续）；
//! - 1 份 `referral_050` 故意制造空洞（cl_ids = [1, 2, 3, 5, 6]，缺 cl_id=4），
//!   fsck 应在 referral_050 检出 1 个 SessionEnded 空洞。
//!
//! 每份 .copper = 5 条 ChainLink（JSON Lines），合计 500 条链节、500 次
//! `sha256(canonical_json(...))` 链式 hash 计算；100 次 fsck + 100 次 verify_chain
//! + 100 次 render_text_report。
//!
//! ## 测量策略
//!
//! 用 `std::time::Instant` 分别测：
//! - `t_fsck_total`：所有 100 份 fsck 累计；
//! - `t_replay_total`：所有 100 份 replay 累计；
//! - `t_render_total`：所有 100 份 render_text_report 累计；
//! - `t_overall` = fsck + replay + render（去 fixture 生成开销）。
//!
//! 失败策略：t_overall > 30s 时 `panic!` 并打印拆解时长；否则输出 `--nocapture`
//! 友好的阶段性时长供诊断。
//!
//! ## 命令
//!
//! ```bash
//! cargo test --manifest-path referral-trace-station/Cargo.toml \
//!     --release --test perf_100_referrals -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use chrono::{TimeZone, Utc};
use sha2::{Digest, Sha256};

use referral_trace_station::copperlist::fsck::CopperListSequence;
use referral_trace_station::model::continuity_record::ContinuityKind;
use referral_trace_station::model::referral_version::ReferralVersion;
use referral_trace_station::replay::{render_text_report, ReplayConfig, ReplayRunner};
use referral_trace_station::sha256_chain::chain::{canonical_json, Chain};

// =============================================================================
// 规模与阈值常量（与 task.json 对齐）
// =============================================================================

/// 总转诊单数（与 task.json 描述「100 条转诊单」一致）。
const REFERRAL_COUNT: usize = 100;

/// 健康 fixture 的版本数（每份 referral 的 cl_ids 长度）。
const HEALTHY_VERSIONS_PER_REFERRAL: usize = 5;

/// 故意制造空洞的 referral_id（参考 task.json 的「1 份故意空洞」语义）。
/// 取第 50 份（中间位置，便于手工验证）；cl_ids = [1, 2, 3, 5, 6]，
/// 缺 cl_id=4 → fsck 检出 1 个 SessionEnded 空洞。
const GAP_REFERRAL_ID: &str = "referral_050";

/// 性能阈值：总耗时 ≤ 30 秒。
/// 该阈值与 spec.md §4「100 转诊单 × 5 版本 fsck + render ≤ 30s」一致。
const MAX_OVERALL_SECS: f64 = 30.0;

// =============================================================================
// 数据构造 helpers（与 tests/integration_end_to_end.rs 共享语义）
// =============================================================================

/// 构造一份 `ReferralVersion`，把 `payload_sha256 = sha256(canonical_json(after))`
/// 填好（与 `Chain::append` 用的 hash 不同 —— audit 端的链式 hash 由
/// `Chain::append` 自行计算）。
fn make_referral_version(cl_id: u64, actor: &str, diagnosis: &str) -> ReferralVersion {
    let mut before: BTreeMap<String, String> = BTreeMap::new();
    before.insert("diagnosis".to_string(), "chest pain".to_string());
    let mut after: BTreeMap<String, String> = BTreeMap::new();
    after.insert("diagnosis".to_string(), diagnosis.to_string());
    after.insert("actor".to_string(), actor.to_string());

    // modified_at 沿 cl_id 递增 1 秒（与 integration_end_toend 一致）
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
fn write_copper(path: &Path, versions: &[ReferralVersion]) {
    let mut chain = Chain::new();
    let mut file = fs::File::create(path).expect("create .copper");
    for v in versions {
        let link = chain.append(v).expect("chain.append");
        let s = serde_json::to_string(&link).expect("serialize ChainLink");
        writeln!(file, "{s}").expect("writeln ChainLink");
    }
}

/// 在一组 `ReferralVersion` 上跑 `CopperListSequence::observe_with_time` 并
/// 汇总 `FsckReport` —— 与 `src/copperlist/fsck.rs` 内嵌单测同源。
fn run_fsck(versions: &[ReferralVersion]) -> referral_trace_station::copperlist::fsck::FsckReport {
    let mut seq = CopperListSequence::new();
    for v in versions {
        seq.observe_with_time(v.cl_id, v.modified_at.timestamp());
    }
    seq.fsck_report()
}

// =============================================================================
// 100 份 fixtures 生成（在 tempdir，幂等）
// =============================================================================

/// 生成 100 份 referral fixtures 到 `state_dir`。
///
/// - 99 份健康（cl_ids = [1, 2, 3, 4, 5]）；
/// - 1 份 `referral_050`（cl_ids = [1, 2, 3, 5, 6]）故意制造空洞。
fn setup_100_fixtures(state_dir: &Path) {
    fs::create_dir_all(state_dir).expect("create state_dir");

    for i in 1..=REFERRAL_COUNT {
        let n = format!("{:03}", i);
        let referral_id = format!("referral_{n}");

        // 决定 cl_ids 序列：referral_050 制造空洞
        let cl_ids: Vec<u64> = if referral_id == GAP_REFERRAL_ID {
            vec![1, 2, 3, 5, 6]
        } else {
            (1..=HEALTHY_VERSIONS_PER_REFERRAL as u64).collect()
        };

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

        let copper_path = state_dir.join(format!("{referral_id}.copper"));
        write_copper(&copper_path, &versions);
    }
}

// =============================================================================
// 主测试 — fsck + replay + render 全流程 ≤ 30s
// =============================================================================

#[test]
fn test_perf_fsck_replay_render_100_referrals_under_30s() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_dir = tmp.path().join("state");
    setup_100_fixtures(&state_dir);

    // ---- 阶段 1: fsck 100 份 ----
    let t_fsck_start = Instant::now();
    let mut healthy_count: usize = 0;
    let mut gap_count: usize = 0;
    for i in 1..=REFERRAL_COUNT {
        let n = format!("{:03}", i);
        let referral_id = format!("referral_{n}");
        let copper_path = state_dir.join(format!("{referral_id}.copper"));
        // 仅解析 .copper 拿 cl_ids + modified_at，喂 fsck
        let raw = fs::read_to_string(&copper_path).expect("read .copper for fsck");
        let mut cl_ids: Vec<u64> = Vec::new();
        let mut timestamps: BTreeMap<u64, i64> = BTreeMap::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let link: referral_trace_station::sha256_chain::chain::ChainLink =
                serde_json::from_str(trimmed).expect("parse ChainLink for fsck");
            let version: ReferralVersion =
                serde_json::from_value(link.payload.clone()).expect("parse ReferralVersion");
            cl_ids.push(version.cl_id);
            timestamps.insert(version.cl_id, version.modified_at.timestamp());
        }
        let versions_for_fsck: Vec<ReferralVersion> = cl_ids
            .iter()
            .map(|&id| ReferralVersion {
                cl_id: id,
                version_no: id.min(u32::MAX as u64) as u32,
                actor: format!("dr_step{id}@hospital_a"),
                modified_at: Utc.timestamp_opt(*timestamps.get(&id).unwrap(), 0).unwrap(),
                before: BTreeMap::new(),
                after: BTreeMap::new(),
                continuity: ContinuityKind::Manifest,
                payload_sha256: String::new(),
            })
            .collect();
        let fsck = run_fsck(&versions_for_fsck);
        if fsck.healthy {
            healthy_count += 1;
        } else {
            gap_count += 1;
        }
    }
    let t_fsck_elapsed = t_fsck_start.elapsed();

    // fsck 断言：100 份里 99 份健康 + 1 份 referral_050 应报空洞
    assert_eq!(healthy_count, REFERRAL_COUNT - 1, "99 healthy referrals expected");
    assert_eq!(gap_count, 1, "exactly 1 referral with gaps expected (referral_050)");

    // ---- 阶段 2: replay 100 份 ----
    let runner = ReplayRunner::new(ReplayConfig::new(state_dir.clone()));
    let t_replay_start = Instant::now();
    let mut reports = Vec::with_capacity(REFERRAL_COUNT);
    for i in 1..=REFERRAL_COUNT {
        let n = format!("{:03}", i);
        let referral_id = format!("referral_{n}");
        let report = runner
            .run(&referral_id)
            .unwrap_or_else(|e| panic!("replay {referral_id} failed: {e}"));
        reports.push(report);
    }
    let t_replay_elapsed = t_replay_start.elapsed();

    // ---- 阶段 3: render 100 份 ----
    let t_render_start = Instant::now();
    let mut rendered_bytes: usize = 0;
    for report in &reports {
        let text = render_text_report(report);
        rendered_bytes += text.len();
    }
    let t_render_elapsed = t_render_start.elapsed();

    let t_overall = t_fsck_elapsed + t_replay_elapsed + t_render_elapsed;

    // ---- 报告 ----
    println!("\n=== perf_100_referrals benchmark ===");
    println!("referrals         : {REFERRAL_COUNT}");
    println!("versions/ref      : {HEALTHY_VERSIONS_PER_REFERRAL} (referral_050 has 5 with gap)");
    println!("fsck total        : {:?}", t_fsck_elapsed);
    println!("replay total      : {:?}", t_replay_elapsed);
    println!("render total      : {:?}", t_render_elapsed);
    println!("overall (sum)     : {t_overall:?}");
    println!("rendered bytes    : {rendered_bytes}");
    println!("healthy fsck count: {healthy_count}");
    println!("gap fsck count    : {gap_count}");
    println!("======================================\n");

    // ---- 验收：总耗时 ≤ 30 秒 ----
    let overall_secs = t_overall.as_secs_f64();
    if overall_secs > MAX_OVERALL_SECS {
        panic!(
            "perf_100_referrals: overall elapsed {overall_secs:.3}s exceeds {MAX_OVERALL_SECS:.1}s budget\n\
             breakdown: fsck={:?}, replay={:?}, render={:?}",
            t_fsck_elapsed, t_replay_elapsed, t_render_elapsed
        );
    }
}

// =============================================================================
// 子测试 — 拆解每一阶段，让 diagnose 报告更细
// =============================================================================

#[test]
fn test_perf_fsck_only_100_referrals_under_10s() {
    // fsck 单阶段上限 10s（总体 30s 预算的 1/3，余下给 replay + render）
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_dir = tmp.path().join("state");
    setup_100_fixtures(&state_dir);

    let start = Instant::now();
    let mut last_fsck_report: Option<referral_trace_station::copperlist::fsck::FsckReport> = None;
    for i in 1..=REFERRAL_COUNT {
        let n = format!("{:03}", i);
        let referral_id = format!("referral_{n}");
        let copper_path = state_dir.join(format!("{referral_id}.copper"));
        let raw = fs::read_to_string(&copper_path).expect("read");
        let mut versions = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let link: referral_trace_station::sha256_chain::chain::ChainLink =
                serde_json::from_str(trimmed).expect("parse");
            let version: ReferralVersion =
                serde_json::from_value(link.payload.clone()).expect("parse version");
            versions.push(version);
        }
        let fsck = run_fsck(&versions);
        if referral_id == GAP_REFERRAL_ID {
            last_fsck_report = Some(fsck);
        }
    }
    let elapsed = start.elapsed();
    println!("fsck-only 100 referrals: {elapsed:?}");
    assert!(
        elapsed.as_secs_f64() <= 10.0,
        "fsck-only too slow: {elapsed:?}"
    );

    // referral_050 fsck 报告应展示 1 个 SessionEnded 空洞
    let r = last_fsck_report.expect("referral_050 fsck report");
    assert!(!r.healthy);
    assert_eq!(r.missing, 1);
    assert_eq!(r.holes.len(), 1);
    assert_eq!(r.holes[0].first_id, 4);
    assert_eq!(r.holes[0].last_id, 4);
    assert_eq!(
        r.holes[0].reason,
        referral_trace_station::model::continuity_record::SourceGapReason::SessionEnded
    );
}

#[test]
fn test_perf_replay_only_100_referrals_under_15s() {
    // replay 单阶段上限 15s（包含 read + parse + verify_chain + TimelineRow 构造）
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_dir = tmp.path().join("state");
    setup_100_fixtures(&state_dir);

    let runner = ReplayRunner::new(ReplayConfig::new(state_dir));
    let start = Instant::now();
    let mut link_count_total: usize = 0;
    for i in 1..=REFERRAL_COUNT {
        let n = format!("{:03}", i);
        let referral_id = format!("referral_{n}");
        let report = runner.run(&referral_id).expect("replay");
        assert!(report.chain_ok(), "{referral_id} chain verify failed");
        link_count_total += report.link_count;
    }
    let elapsed = start.elapsed();
    println!("replay-only 100 referrals: {elapsed:?}, total links: {link_count_total}");
    assert!(
        elapsed.as_secs_f64() <= 15.0,
        "replay-only too slow: {elapsed:?}"
    );
    assert_eq!(link_count_total, 99 * 5 + 5); // 99 healthy (5 links each) + referral_050 (5 links with gap)
}

#[test]
fn test_perf_render_only_100_referrals_under_10s() {
    // render 单阶段上限 10s（render_text_report 纯字符串拼接）
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_dir = tmp.path().join("state");
    setup_100_fixtures(&state_dir);

    let runner = ReplayRunner::new(ReplayConfig::new(state_dir));
    let reports: Vec<_> = (1..=REFERRAL_COUNT)
        .map(|i| {
            let n = format!("{:03}", i);
            let referral_id = format!("referral_{n}");
            runner.run(&referral_id).expect("replay")
        })
        .collect();

    let start = Instant::now();
    let mut total_bytes: usize = 0;
    for r in &reports {
        let s = render_text_report(r);
        total_bytes += s.len();
    }
    let elapsed = start.elapsed();
    println!("render-only 100 referrals: {elapsed:?}, rendered bytes: {total_bytes}");
    assert!(
        elapsed.as_secs_f64() <= 10.0,
        "render-only too slow: {elapsed:?}"
    );
    assert!(total_bytes > 100 * 200, "rendered output too small: {total_bytes} bytes");
}
