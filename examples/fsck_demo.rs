//! fsck 序列空洞检测 demo — 用铜版 copper-rs `CopperListSequence::observe()` 跑 referral_003
//! 的 6 个版本（其中 cl_id=4 缺失），把 FsckReport 渲染到 stdout。
//!
//! 用法：`cargo run --example fsck_demo -- --referral referral_003 --state-dir <path>`
//!
//! 这只是示例程序（不是 CLI 子命令），用于驱动文档截图采集。

use std::path::PathBuf;

use clap::Parser;
use referral_trace_station::copperlist::{CopperListSequence, FsckReport};
use referral_trace_station::sha256_chain::ChainLink;

#[derive(Parser, Debug)]
#[command(about = "fsck demo — 读 .copper → 跑序列空洞检测 → 渲染人读报告")]
struct Args {
    #[arg(long, default_value = "referral_003")]
    referral: String,
    #[arg(long, default_value = "./state")]
    state_dir: PathBuf,
}

fn load_links(state_dir: &PathBuf, referral_id: &str) -> Vec<ChainLink> {
    let path = state_dir.join(format!("{referral_id}.copper"));
    let raw = std::fs::read_to_string(&path).expect("read copper archive");
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<ChainLink>(l).expect("parse chain link"))
        .collect()
}

fn render_report(referral_id: &str, links: &[ChainLink], report: &FsckReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let cl_ids: Vec<u64> = links
        .iter()
        .filter_map(|l| l.payload.get("cl_id").and_then(|v| v.as_u64()))
        .collect();
    let observed_ids_label = if cl_ids.is_empty() {
        "<none>".to_string()
    } else {
        let seq = cl_ids
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!("[{seq}]")
    };
    writeln!(s, "# fsck report for {referral_id}").unwrap();
    writeln!(s, "copper archive: <state_dir>/{referral_id}.copper").unwrap();
    writeln!(s, "links observed: {}", links.len()).unwrap();
    writeln!(s, "cl_ids observed: {observed_ids_label}").unwrap();
    writeln!(s).unwrap();
    writeln!(s, "summary: missing={} holes={} truncated={}",
        report.missing_count(),
        report.holes.len(),
        report.truncated).unwrap();
    writeln!(s, "healthy: {}", report.missing_count() == 0).unwrap();
    writeln!(s).unwrap();
    if report.holes.is_empty() {
        writeln!(s, "fsck ok — no holes detected").unwrap();
    } else {
        writeln!(s, "holes (first_id / last_id / reason):").unwrap();
        for (i, h) in report.holes.iter().enumerate() {
            writeln!(s, "  [{}] first_id={} last_id={}  reason={:?}  span={}",
                i + 1, h.first_id, h.last_id, h.reason, h.span).unwrap();
        }
    }
    writeln!(s).unwrap();
    writeln!(s, "Note: 故意在 fixture 链中删去 cl_id=4，fsck 据此报 SessionEnded 空洞。").unwrap();
    s
}

fn main() {
    let args = Args::parse();
    let links = load_links(&args.state_dir, &args.referral);
    let mut seq = CopperListSequence::new();
    for link in &links {
        if let Some(cl_id) = link.payload.get("cl_id").and_then(|v| v.as_u64()) {
            let ts = link
                .payload
                .get("modified_at")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp())
                .unwrap_or(0);
            seq.observe_with_time(cl_id, ts);
        }
    }
    let report = seq.fsck_report();
    print!("{}", render_report(&args.referral, &links, &report));
}
