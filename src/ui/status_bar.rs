//! ratatui 顶部 StatusBar widget
//!
//! 三段水平布局：
//! - 左：当前 `referral_id` + 该 referral 的版本总数
//! - 中：`fsck` 空洞数（含 `truncated` 标记）
//! - 右：`AI: on/off` + 当前选中的 `cl_id`
//!
//! 配色约定（参考 spec.md §6 视觉目标卡）：
//! - 健康状态（fsck_holes=0, AI on, 有选中 cl_id）→ 绿色
//! - 警告状态（fsck_holes>0 或 truncated）→ 黄色
//! - 故障状态（AI off 或 selected_cl_id=None）→ 红色
//!
//! 与源 copper-rs 的 LiveLogListener ID 单调契约一致：被 UI 订阅的
//! `selected_cl_id` 必须落在 `cl_seq` 已观察集合内。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// AI 状态枚举 — 三态：未配置 / 关闭 / 开启
///
/// 对应 ModelGateway 的可观测状态：
/// - `Off`：`is_configured() == false`（.env 未配 OPENAI_* 凭据）
/// - `Closed`：配置过但最近一次调用 Network / Unavailable / ParseFailed
/// - `On`：最近一次调用成功且未超时
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiState {
    Off,
    Closed,
    On,
}

impl AiState {
    /// 三态机读文本（与 README 视觉目标卡一致）
    pub fn label(self) -> &'static str {
        match self {
            AiState::Off => "AI: off",
            AiState::Closed => "AI: closed",
            AiState::On => "AI: on",
        }
    }
}

/// StatusBar 渲染所需的状态快照
///
/// UI 层不持有业务状态；本结构由主循环（`App::run`）在每次 draw 前
/// 用 listener 回调填充后传给 `render`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBar {
    /// 当前选中的 referral_id（"-" 表示未选）
    pub referral_id: String,
    /// 当前 referral 的总版本数（来自 fsck observe 数）
    pub total_versions: u64,
    /// fsck 检测到的空洞数
    pub fsck_holes: u64,
    /// 是否因超过 MAX_HOLE 触发 truncated
    pub fsck_truncated: bool,
    /// ModelGateway 当前状态
    pub ai_state: AiState,
    /// 当前选中的 cl_id（None 表示尚未选中）
    pub selected_cl_id: Option<u64>,
}

impl StatusBar {
    /// 构造空状态 StatusBar（首屏用）
    pub fn empty() -> Self {
        Self {
            referral_id: "-".to_string(),
            total_versions: 0,
            fsck_holes: 0,
            fsck_truncated: false,
            ai_state: AiState::Off,
            selected_cl_id: None,
        }
    }

    /// 计算整体配色基调（用于背景渲染）
    ///
    /// 优先级：truncated（被限流）> fsck holes > healthy > fault
    /// - truncated / 空洞 → Warning（黄色）
    /// - AI on + 有选中 + 无空洞 → Healthy（绿色）
    /// - 其余（AI off / closed / 未选中）→ Fault（红色）
    pub fn severity(&self) -> StatusBarSeverity {
        if self.fsck_truncated {
            return StatusBarSeverity::Warning;
        }
        if self.fsck_holes > 0 {
            return StatusBarSeverity::Warning;
        }
        if matches!(self.ai_state, AiState::On) && self.selected_cl_id.is_some() {
            return StatusBarSeverity::Healthy;
        }
        StatusBarSeverity::Fault
    }
}

/// StatusBar 配色档位
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBarSeverity {
    Healthy,
    Warning,
    Fault,
}

impl StatusBarSeverity {
    /// 严重度 → 配色（健康/警告/故障）
    pub fn fg_color(self) -> Color {
        match self {
            StatusBarSeverity::Healthy => Color::Green,
            StatusBarSeverity::Warning => Color::Yellow,
            StatusBarSeverity::Fault => Color::Red,
        }
    }

    /// 严重度 → 背景色（深色调）
    pub fn bg_color(self) -> Color {
        match self {
            StatusBarSeverity::Healthy => Color::Reset,
            StatusBarSeverity::Warning => Color::Reset,
            StatusBarSeverity::Fault => Color::Reset,
        }
    }
}

/// 渲染 StatusBar 三段布局到指定 area
///
/// 左段：referral_id + 版本数
/// 中段：fsck 空洞数 + truncated 标记
/// 右段：AI 状态 + 选中 cl_id
///
/// 配色按 severity 应用；中段在 truncated 时加红色 span。
pub fn render(frame: &mut Frame, area: Rect, state: &StatusBar) {
    let fg = state.severity().fg_color();
    let bg = state.severity().bg_color();
    let base_style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);

    let chunks = Layout::new(
        Direction::Horizontal,
        [
            Constraint::Percentage(35),
            Constraint::Percentage(35),
            Constraint::Percentage(30),
        ],
    )
    .split(area);

    // ---- 左段：referral_id + 总版本数 ----
    let left_text = format!("ref: {} | versions: {}", state.referral_id, state.total_versions);
    let left = Paragraph::new(Line::from(Span::styled(left_text, base_style)));
    frame.render_widget(left, chunks[0]);

    // ---- 中段：fsck 空洞数 + truncated 标记 ----
    let holes_label = if state.fsck_truncated {
        format!("fsck holes: {} (truncated)", state.fsck_holes)
    } else {
        format!("fsck holes: {}", state.fsck_holes)
    };
    let middle_style = if state.fsck_truncated {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        base_style
    };
    let middle = Paragraph::new(Line::from(Span::styled(holes_label, middle_style)));
    frame.render_widget(middle, chunks[1]);

    // ---- 右段：AI 状态 + 选中 cl_id ----
    let cl_label = state
        .selected_cl_id
        .map(|c| format!("cl_id: {}", c))
        .unwrap_or_else(|| "cl_id: -".to_string());
    let ai_style = match state.ai_state {
        AiState::On => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        AiState::Closed => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        AiState::Off => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    };
    let right = Paragraph::new(Line::from(vec![
        Span::styled(format!("{} | ", state.ai_state.label()), ai_style),
        Span::styled(cl_label, base_style),
    ]));
    frame.render_widget(right, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 渲染并返回终端缓冲（Buffer dump）的简化字符串（逐行去除尾随空格）
    fn render_to_string(state: &StatusBar, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal new");
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), state);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut lines: Vec<String> = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.get(x, y).symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        lines.retain(|l| !l.is_empty());
        lines.join("\n")
    }

    #[test]
    fn test_status_bar_render_healthy_snapshot() {
        let state = StatusBar {
            referral_id: "REF-001".to_string(),
            total_versions: 5,
            fsck_holes: 0,
            fsck_truncated: false,
            ai_state: AiState::On,
            selected_cl_id: Some(3),
        };
        let s = render_to_string(&state, 80, 1);
        // 三段布局中，左段含 "ref: REF-001 | versions: 5"
        assert!(s.contains("ref: REF-001"), "left segment missing: {}", s);
        assert!(s.contains("versions: 5"), "left segment missing versions: {}", s);
        // 中段含 "fsck holes: 0"（无 truncated 后缀）
        assert!(s.contains("fsck holes: 0"), "middle segment missing: {}", s);
        assert!(!s.contains("truncated"), "healthy should not show truncated: {}", s);
        // 右段含 "AI: on" + "cl_id: 3"
        assert!(s.contains("AI: on"), "right segment missing AI label: {}", s);
        assert!(s.contains("cl_id: 3"), "right segment missing cl_id: {}", s);
    }

    #[test]
    fn test_status_bar_render_warning_with_truncated_snapshot() {
        let state = StatusBar {
            referral_id: "REF-002".to_string(),
            total_versions: 257,
            fsck_holes: 256,
            fsck_truncated: true,
            ai_state: AiState::On,
            selected_cl_id: Some(2),
        };
        let s = render_to_string(&state, 80, 1);
        assert!(s.contains("ref: REF-002"), "{}", s);
        assert!(s.contains("versions: 257"), "{}", s);
        assert!(s.contains("fsck holes: 256"), "middle missing: {}", s);
        assert!(s.contains("truncated"), "warning must show truncated: {}", s);
        assert!(s.contains("AI: on"), "{}", s);
        assert!(s.contains("cl_id: 2"), "{}", s);
    }

    #[test]
    fn test_status_bar_render_fault_ai_off_snapshot() {
        let state = StatusBar {
            referral_id: "-".to_string(),
            total_versions: 0,
            fsck_holes: 0,
            fsck_truncated: false,
            ai_state: AiState::Off,
            selected_cl_id: None,
        };
        let s = render_to_string(&state, 80, 1);
        assert!(s.contains("ref: -"), "{}", s);
        assert!(s.contains("versions: 0"), "{}", s);
        assert!(s.contains("fsck holes: 0"), "{}", s);
        assert!(s.contains("AI: off"), "{}", s);
        assert!(s.contains("cl_id: -"), "{}", s);
    }

    #[test]
    fn test_status_bar_render_warning_ai_closed_snapshot() {
        let state = StatusBar {
            referral_id: "REF-003".to_string(),
            total_versions: 8,
            fsck_holes: 3,
            fsck_truncated: false,
            ai_state: AiState::Closed,
            selected_cl_id: Some(5),
        };
        let s = render_to_string(&state, 80, 1);
        assert!(s.contains("AI: closed"), "AI closed label missing: {}", s);
        assert!(s.contains("fsck holes: 3"), "{}", s);
        assert!(s.contains("cl_id: 5"), "{}", s);
    }

    #[test]
    fn test_status_bar_severity_classification() {
        let mut s = StatusBar::empty();
        // empty 默认 Fault（AI off + selected_cl_id=None）
        assert_eq!(s.severity(), StatusBarSeverity::Fault);

        s.ai_state = AiState::On;
        s.selected_cl_id = Some(1);
        s.fsck_holes = 0;
        assert_eq!(s.severity(), StatusBarSeverity::Healthy);

        s.fsck_holes = 1;
        assert_eq!(s.severity(), StatusBarSeverity::Warning);

        s.fsck_holes = 0;
        s.fsck_truncated = true;
        assert_eq!(s.severity(), StatusBarSeverity::Warning);

        s.fsck_truncated = false;
        s.ai_state = AiState::Closed;
        s.selected_cl_id = None;
        assert_eq!(s.severity(), StatusBarSeverity::Fault);
    }

    #[test]
    fn test_ai_state_label_three_variants() {
        assert_eq!(AiState::Off.label(), "AI: off");
        assert_eq!(AiState::Closed.label(), "AI: closed");
        assert_eq!(AiState::On.label(), "AI: on");
    }

    #[test]
    fn test_status_bar_empty_helper() {
        let s = StatusBar::empty();
        assert_eq!(s.referral_id, "-");
        assert_eq!(s.total_versions, 0);
        assert_eq!(s.fsck_holes, 0);
        assert!(!s.fsck_truncated);
        assert_eq!(s.ai_state, AiState::Off);
        assert_eq!(s.selected_cl_id, None);
    }

    #[test]
    fn test_status_bar_severity_fg_color_three_variants() {
        assert_eq!(StatusBarSeverity::Healthy.fg_color(), Color::Green);
        assert_eq!(StatusBarSeverity::Warning.fg_color(), Color::Yellow);
        assert_eq!(StatusBarSeverity::Fault.fg_color(), Color::Red);
    }
}