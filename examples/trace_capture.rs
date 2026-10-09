//! ratatui 三视图真实渲染截图采集（TestBackend buffer dump）。
//!
//! 用铜版 copper-rs 的 ratatui 渲染管线（task graph `trace` 子命令的同款实现）
//! 跑 N tick 后把 buffer 的每格 (symbol + fg/bg) 输出为：
//!   1. `--out <path>`  JSON Lines（每行 `x,y,symbol,fg_r,fg_g,fg_b,bg_r,bg_g,bg_b`），
//!      便于外部脚本（Python+PIL 等）转换成 PNG；本工具不做图渲染，仅产出 cells 真值。
//!   2. `--text-out <path>` 纯文本帧（trim 末尾空白），便于 diff / grep / 文档引用。
//!
//! 用法：
//!   cargo run --example trace_capture -- --referral referral_001 \
//!       --state-dir ./state --ticks 3 --out /tmp/frame.jsonl --text-out /tmp/frame.txt
//!
//! **禁止**：本 example 不内置任何字符图渲染，只产出真实 buffer cells；PNG 由外部脚本
//! 按需生成。

use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use clap::Parser;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use referral_trace_station::event_bus::EventBus;
use referral_trace_station::ui::app::{App, AppState};
use referral_trace_station::ui::status_bar::AiState;

#[derive(Parser, Debug)]
#[command(about = "捕获 ratatui 三视图 TestBackend buffer 真实 cells")]
struct Args {
    #[arg(long, default_value = "referral_001")]
    referral: String,
    #[arg(long, default_value = "./state")]
    state_dir: PathBuf,
    #[arg(long, default_value_t = 3)]
    ticks: u64,
    #[arg(long, default_value_t = 120)]
    width: u16,
    #[arg(long, default_value_t = 20)]
    height: u16,
    #[arg(long, default_value_t = 0)]
    fsck_holes: u64,
    #[arg(long, default_value_t = false)]
    fsck_truncated: bool,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    text_out: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _g = runtime.enter();

    // 加载 .copper → ReferralVersion
    let state_dir_canon = args.state_dir.canonicalize().unwrap_or(args.state_dir.clone());
    let copper_path = state_dir_canon.join(format!("{}.copper", args.referral));
    let raw = std::fs::read_to_string(&copper_path).expect("read copper");
    let mut versions: Vec<referral_trace_station::model::referral_version::ReferralVersion> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let link: referral_trace_station::sha256_chain::ChainLink =
            serde_json::from_str(line).expect("parse link");
        let v: referral_trace_station::model::referral_version::ReferralVersion =
            serde_json::from_value(link.payload.clone()).expect("payload is ReferralVersion");
        versions.push(v);
    }
    let total_versions = versions.len() as u64;

    // 构建 AppState
    let mut app_state = AppState::empty();
    app_state.versions = versions.clone();
    app_state.selected_cl_id = versions.first().map(|v| v.cl_id);
    app_state.ai_state = AiState::On;
    app_state.fsck_holes = args.fsck_holes;
    app_state.fsck_truncated = args.fsck_truncated;
    app_state.parent_diagnosis.insert(
        "cl_id=1".to_string(),
        versions
            .first()
            .map(|v| v.after.get("diagnosis").cloned().unwrap_or_default())
            .unwrap_or_default(),
    );
    // 时间戳归零以便 demo 截图可控
    for v in &mut versions {
        v.modified_at = Utc.with_ymd_and_hms(2026, 10, 10, 8, 30, v.version_no).unwrap();
    }
    app_state.versions = versions;
    let bus = EventBus::new();
    let mut app = App::new(app_state, bus);

    // 跑 N tick
    let backend = TestBackend::new(args.width, args.height);
    let mut terminal = Terminal::new(backend).expect("Terminal::new");
    let draw_interval = std::time::Duration::from_millis(20);
    let result = futures::executor::block_on(app.run_for_ticks(
        &mut terminal,
        args.ticks,
        draw_interval,
    ));
    result.expect("run_for_ticks ok");

    // 捕获最终 buffer
    let buffer = terminal.backend().buffer().clone();

    // 文本帧
    let text: String = (0..buffer.area.height)
        .map(|y| {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer.get(x, y).symbol());
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(p) = &args.text_out {
        std::fs::write(p, format!("{text}\n")).expect("write text");
        eprintln!("[trace_capture] text frame written: {}", p.display());
    }

    // JSONL cells（每行 {x,y,symbol,fg,rgba,bg,rgba}，由外部脚本渲染 PNG）
    if let Some(p) = &args.out {
        let mut s = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = buffer.get(x, y);
                let sym = cell.symbol();
                let esc = |c: ratatui::style::Color| match c {
                    ratatui::style::Color::Reset => "#000000".to_string(),
                    ratatui::style::Color::Black => "#000000".to_string(),
                    ratatui::style::Color::Red => "#cc2222".to_string(),
                    ratatui::style::Color::Green => "#22aa22".to_string(),
                    ratatui::style::Color::Yellow => "#d4a017".to_string(),
                    ratatui::style::Color::Blue => "#2255cc".to_string(),
                    ratatui::style::Color::Magenta => "#aa22aa".to_string(),
                    ratatui::style::Color::Cyan => "#22aaaa".to_string(),
                    ratatui::style::Color::Gray => "#888888".to_string(),
                    ratatui::style::Color::DarkGray => "#444444".to_string(),
                    ratatui::style::Color::LightRed => "#ff5555".to_string(),
                    ratatui::style::Color::LightGreen => "#55ff55".to_string(),
                    ratatui::style::Color::LightYellow => "#ffff55".to_string(),
                    ratatui::style::Color::LightBlue => "#5555ff".to_string(),
                    ratatui::style::Color::LightMagenta => "#ff55ff".to_string(),
                    ratatui::style::Color::LightCyan => "#55ffff".to_string(),
                    ratatui::style::Color::White => "#dddddd".to_string(),
                    ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                    ratatui::style::Color::Indexed(i) => format!("idx:{i}"),
                };
                let fg = esc(cell.fg);
                let bg = esc(cell.bg);
                use std::fmt::Write as _;
                let _ = writeln!(
                    s,
                    "{{\"x\":{x},\"y\":{y},\"sym\":{:?},\"fg\":{:?},\"bg\":{:?}}}",
                    sym,
                    fg,
                    bg
                );
            }
        }
        std::fs::write(p, s).expect("write jsonl");
        eprintln!("[trace_capture] cell jsonl written: {}", p.display());
    }

    eprintln!(
        "[trace_capture] frame captured: {}x{} cells, total_versions={}",
        buffer.area.width, buffer.area.height, total_versions
    );
}
