//! CLI dispatcher — clap 子命令 trace / replay / export / verify
//!
//! Phase 4 Task 18：trace 子命令接入 ratatui 三视图主循环 App::run()。
//! Phase 4 Task 19：replay 子命令接入 ReplayRunner（`.copper` 解析 + 链校验 +
//! 时间线渲染）。
//! Phase 4 Task 20：export 子命令接入 BundleWriter（manifest + versions +
//! chain.txt 三类条目打 zip）。
//! Phase 4 Task 21：verify 子命令接入 Verifier（解压 zip → 重算 sha256 → tamper 检测）。

use clap::{Parser, Subcommand};
use std::process::ExitCode;
use std::time::Duration;

use referral_trace_station::event_bus::EventBus;
use referral_trace_station::export::{
    BundleConfig, BundleError, BundleWriter,
};
use referral_trace_station::replay::{render_text_report, ReplayConfig, ReplayError, ReplayRunner};
use referral_trace_station::ui::app::{App, AppState};
use referral_trace_station::verify::{render_text_report as render_verify_report, Verifier, VerifyError};

#[derive(Parser, Debug)]
#[command(
    name = "referral-trace-station",
    version,
    about = "转诊单留痕回放工作台 — task graph + cl_id 单调 + fsck + replay + sha256 链",
    long_about = "跨机构转诊单 / 检查互认文档的版本时间线接缝：cl_id 单调 + 序列空洞检测 + 链式 sha256 + ratatui 三视图回放"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 启动桌面端 ratatui 三视图（顶部 status_bar + 左 timeline + 中 diff + 右 meta）
    Trace {
        /// Draw interval in milliseconds（默认 100ms ≈ 10Hz 重绘）
        #[arg(long, default_value_t = 100)]
        draw_interval_ms: u64,
        /// 运行 tick 数（0 = 无限；测试或调试用 N 值限定；默认 0）
        #[arg(long, default_value_t = 0)]
        max_ticks: u64,
    },
    /// 读取 state/<referral_id>.copper（JSON Lines 链节存档） → 校验 sha256 链 → 渲染时间线
    ///
    /// exit code：
    /// - 0：链无篡改；
    /// - 2：链校验失败（recovery_point_required，详见报告「chain verify: FAILED」段）；
    /// - 1：state 文件缺失 / 解析错 / 配置错。
    Replay {
        /// 转诊单 id（对应 state/<referral_id>.copper 的 stem）。
        #[arg(long)]
        referral: String,
        /// 状态存档根目录（默认读 env `RTS_STATE_DIR`，缺省 `./state`）
        #[arg(long)]
        state_dir: Option<String>,
    },
    /// 把 state/<referral_id>.copper 打成一个可脱敏审计的 zip 包（manifest.json +
    /// versions/<cl_id>.json + chain.txt 三类条目），供医务科 / 接收方医院人工签收。
    ///
    /// exit code：
    /// - 0：zip 写盘成功（含「end of chain」末行 + versions/ 完整 + manifest 三字段齐）；
    /// - 1：state 文件缺失 / 解析错 / 链空 / IO 错。
    Export {
        /// 转诊单 id（对应 state/<referral_id>.copper 的 stem）。
        #[arg(long)]
        referral: String,
        /// 输出 zip 路径（绝对路径或相对当前工作目录）。
        #[arg(long)]
        out: String,
        /// 状态存档根目录（默认读 env `RTS_STATE_DIR`，缺省 `./state`）
        #[arg(long)]
        state_dir: Option<String>,
    },
    /// 解压 bundle.zip → 重算 sha256 链 → 与 chain.txt / versions/*.json 三向比对。
    ///
    /// exit code：
    /// - 0：整链无篡改（chain_ok=true）；
    /// - 2：链校验失败 / tamper detected（recovery_point_required=true，详见报告 tamper_locations）；
    /// - 1：zip 缺失 / 解析错 / entry 缺失。
    Verify {
        /// bundle.zip 路径（绝对路径或相对当前工作目录）。
        #[arg(long)]
        zip: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Trace {
            draw_interval_ms,
            max_ticks,
        } => {
            if let Err(e) = run_trace(draw_interval_ms, max_ticks).await {
                eprintln!("referral-trace-station trace: app error: {}", e);
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Commands::Replay { referral, state_dir } => match run_replay(&referral, state_dir.as_deref()) {
            Ok(exit_code) => exit_code,
            Err(e) => {
                eprintln!("referral-trace-station replay: {}", e);
                ExitCode::from(1)
            }
        },
        Commands::Export { referral, out, state_dir } => match run_export(&referral, &out, state_dir.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("referral-trace-station export: {}", e);
                ExitCode::from(1)
            }
        },
        Commands::Verify { zip } => match run_verify(&zip) {
            Ok(exit_code) => exit_code,
            Err(e) => {
                eprintln!("referral-trace-station verify: {}", e);
                ExitCode::from(1)
            }
        },
    }
}

async fn run_trace(draw_interval_ms: u64, max_ticks: u64) -> Result<(), Box<dyn std::error::Error>> {
    let bus = EventBus::new();
    let state = AppState::empty();
    let mut app = App::new(state, bus);

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;

    let _ = ratatui::crossterm::terminal::enable_raw_mode();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen,
        ratatui::crossterm::event::EnableMouseCapture
    );

    let draw_interval = Duration::from_millis(draw_interval_ms);
    let result = if max_ticks == 0 {
        app.run_for_ticks(&mut terminal, u64::MAX, draw_interval).await
    } else {
        app.run_for_ticks(&mut terminal, max_ticks, draw_interval).await
    };

    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::LeaveAlternateScreen,
        ratatui::crossterm::event::DisableMouseCapture
    );
    let _ = ratatui::crossterm::terminal::disable_raw_mode();

    result.map(|_| ()).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

/// `replay` 子命令执行入口。
///
/// 返回 `ExitCode` 而非 `Result<(), Box<dyn Error>>` 的原因：CLI 需要把
/// `chain verify failed`（业务级）与 `state file missing`（I/O 级）映射到不
/// 同退出码（业务级 = 2；I/O 级 = 1）。这把决策放在 main 层而不是 ReplayRunner
/// 里，让 runner 模块保持纯函数语义。
fn run_replay(referral: &str, state_dir: Option<&str>) -> Result<ExitCode, ReplayError> {
    // 装配 ReplayConfig：命令行 --state-dir 优先；缺则走 env RTS_STATE_DIR；
    // 都没有则用默认 "./state"。
    let config = match state_dir {
        Some(d) => ReplayConfig::new(d),
        None => ReplayConfig::from_env_with_default("./state"),
    };

    let runner = ReplayRunner::new(config);
    match runner.run(referral) {
        Ok(report) => {
            print!("{}", render_text_report(&report));
            if report.recovery_point_required {
                Ok(ExitCode::from(2))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        Err(e) => Err(e),
    }
}

/// `export` 子命令执行入口。
///
/// 装配流程：
/// 1. 跑 `replay` 拿 `ReplayReport.rows`（已成功解析的 TimelineRow）+ 链节
///    集合（这里直接复用 `ReplayRunner.run` 内部的 `links` —— 但 report
///    不直接暴露 `links`，所以走「replay 一次再走 export 一次」的两次磁盘
///    读取，或在 export 入口内重新打开 `.copper` 解析）。
///
/// 为避免在 `ReplayReport` 上新增 `links` 字段（会破坏 Task 19 已稳定的
/// `ReplayReport` 形状），export 入口直接读 `.copper` → 解析
/// `Vec<ChainLink>` → 调 `BundleWriter::write`。
///
/// 退出码：
/// - 0：zip 写盘成功；
/// - 1：state 缺失 / 解析错 / 链空 / IO 错。
fn run_export(referral: &str, out: &str, state_dir: Option<&str>) -> Result<(), BundleError> {
    // 装配 ReplayConfig 同 replay 子命令（CLI --state-dir > env > ./state）
    let replay_config = match state_dir {
        Some(d) => ReplayConfig::new(d),
        None => ReplayConfig::from_env_with_default("./state"),
    };

    // 1) 跑一次 replay 拿时间线（让用户在终端也看到「导出的就是这些行」），
    //    且**不**失败 → 仅作为可视化信息（recovery_point_required 也照常导
    //    出，consumer 拿到 zip 后由 verify 子命令拒收；这里不强制 verify 通
    //    过，符合 spec §6「导出 = 写盘；verify = 子命令」分层）。
    let runner = ReplayRunner::new(replay_config);
    let report = runner.run(referral).map_err(|e| match e {
        ReplayError::StateFileNotFound { path } => BundleError::WriteZip {
            path,
            reason: "state file not found".to_string(),
        },
        ReplayError::StateFileUnreadable { path, reason } => BundleError::WriteZip { path, reason },
        ReplayError::ParseLine { line_no, reason } => BundleError::Serialize {
            reason: format!("parse line {line_no}: {reason}"),
        },
        ReplayError::BadChainAnchor { actual } => BundleError::Serialize {
            reason: format!("bad chain anchor: {actual}"),
        },
        ReplayError::EmptyArchive => BundleError::EmptyChain {
            referral_id: referral.to_string(),
        },
    })?;

    // 2) 重新打开 .copper 取 Vec<ChainLink>（export 需要 prev_hash/this_hash）
    //    —— 复用 ReplayRunner 的解析路径，避免在 export 里再写一遍 JSON
    //    Lines 解析。ReplayRunner 当前未暴露 `links`；这里通过 export 路径
    //    直接读 .copper 重做最小解析。
    let links = load_links_for_export(referral, &runner)?;

    // 3) 调 BundleWriter
    let cfg = BundleConfig::new(referral.to_string(), out);
    let writer = BundleWriter::new(cfg);
    writer.write(&links)?;

    // 4) stdout 一行 + 概要（含 recovery_point_required 提示，让 caller 知道）
    println!(
        "export {} : links={} out={} chain_ok={} recovery_point_required={}",
        report.referral_id,
        links.len(),
        out,
        report.chain_ok(),
        report.recovery_point_required,
    );
    if report.recovery_point_required {
        eprintln!(
            "referral-trace-station export: WARNING chain verify failed; consumer must run `verify` and reject"
        );
    }

    Ok(())
}

/// 重新打开 `state/<referral_id>.copper`，按 JSON Lines 解析为 `Vec<ChainLink>`。
///
/// 与 [`ReplayRunner::run`] 内部解析一致（同一文件、同一格式），但
/// `ReplayReport` 不暴露原始 `links`，故 export 路径在这里做最小重读。
/// 复用 ReplayRunner 的 `copper_path` 拼路径，绕开手工拼字符串。
fn load_links_for_export(
    referral: &str,
    runner: &ReplayRunner,
) -> Result<Vec<referral_trace_station::sha256_chain::chain::ChainLink>, BundleError> {
    use std::fs;
    use std::io::{BufRead, BufReader};

    use referral_trace_station::sha256_chain::chain::ChainLink;

    let path = runner.config().copper_path(referral);
    let file = fs::File::open(&path).map_err(|e| BundleError::WriteZip {
        path: path.display().to_string(),
        reason: format!("reopen state file: {e}"),
    })?;
    let reader = BufReader::new(file);
    let mut links = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| BundleError::WriteZip {
            path: path.display().to_string(),
            reason: format!("read line {idx}: {e}"),
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let link: ChainLink = serde_json::from_str(trimmed).map_err(|e| BundleError::Serialize {
            reason: format!("line {idx}: {e}"),
        })?;
        links.push(link);
    }
    if links.is_empty() {
        return Err(BundleError::EmptyChain {
            referral_id: referral.to_string(),
        });
    }
    Ok(links)
}

/// `verify` 子命令执行入口。
///
/// 返回 `ExitCode` 而非 `Result<(), Box<dyn Error>>` 的原因：CLI 需要把
/// 「整链无篡改」(0)、「链校验失败 / tamper detected」(2)、「zip 缺失 /
/// 解析错」(1) 映射到不同退出码。decision 放在 main 层而不是 Verifier 里，
/// 让 verifier 模块保持纯函数语义。
fn run_verify(zip: &str) -> Result<ExitCode, VerifyError> {
    let verifier = Verifier::new();
    let zip_path = std::path::Path::new(zip);
    let report = verifier.verify_zip(zip_path)?;

    // stdout 多行人读可审计报告
    print!("{}", render_verify_report(&report));
    // 一行摘要，便于 caller 解析
    println!("{}", report.summary_line());

    if report.chain_ok {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(2))
    }
}