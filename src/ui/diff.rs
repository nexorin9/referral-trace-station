//! ratatui 中央 DiffView widget
//!
//! 两段布局：
//! - **上半**：field-level diff（来自 `similar`-驱动到的 [`DiffEntry`] 列表）
//!   - `Add` → 绿色 `[+] field: after`
//!   - `Remove` → 红色 `[-] field: before`
//!   - `Replace` → 黄色 `[~] field: before → after`
//! - **下半**：AI 摘要 tab，tab 键切换显示
//!   - `show_ai=true` 且 `ai_summary=Some(...)`：结构化渲染
//!     `summary` + `fields_added` + `fields_removed` + `semantic_class`
//!   - `show_ai=false`：直接隐藏下半（按 q 不退；按 tab 显回）
//!   - `ai_summary=None`（AI 不可达 / 未配置）：下半显示「AI: off」占位
//!
//! 与 [`crate::ui::status_bar`] 一致：配色按 `user-perceivable` 严重度而定，
//! 不把内部状态机术语（如 `Manifest→Active`）丢到面向用户的标签里。
//!
//! 与源 copper-rs 的 diff 视角差异：源是按 CopperList 序列输出 raw bytes；本模块
//! 按字段名自然语言渲染，保留人类可读的「为何改、改了什么」。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::ai::openai_compat::AiSummary;

/// Diff 操作类型（UI 视角，无数据载荷——比 `crate::model::referral_version::DiffOp` 简单）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    Add,
    Remove,
    Replace,
}

/// UI 视角的 diff 一条。
///
/// 与 `crate::his::fhir_map::DiffEntry` 字段名一致但 **结构简化**：
/// - 多了 `field` 字段（fhir_map 版本用 `BTreeMap<field, ...>` 的 key）；
/// - `op` 单独抽出，未携带 `unified_diff` 全文（UI 上仅以 `→` 一行示意）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    /// 字段名（如 `diagnosis` / `bp_systolic`）
    pub field: String,
    /// 操作类型：Add / Remove / Replace
    pub op: DiffOp,
    /// before 端的值（Add 时为空）
    pub before: String,
    /// after 端的值（Remove 时为空）
    pub after: String,
}

impl DiffEntry {
    /// 从 fhir_map 的 [`DiffEntry`](crate::his::fhir_map::DiffEntry) + field 名
    /// 转 UI 视角的 [`DiffEntry`]。
    pub fn from_fhir(field: impl Into<String>, e: &crate::his::fhir_map::DiffEntry) -> Self {
        let op = match e {
            crate::his::fhir_map::DiffEntry::Add(_) => DiffOp::Add,
            crate::his::fhir_map::DiffEntry::Remove(_) => DiffOp::Remove,
            crate::his::fhir_map::DiffEntry::Replace { .. } => DiffOp::Replace,
        };
        Self {
            field: field.into(),
            op,
            before: e.before_value().to_string(),
            after: e.after_value().to_string(),
        }
    }

    /// Add (仅 after)
    pub fn add(field: impl Into<String>, after: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            op: DiffOp::Add,
            before: String::new(),
            after: after.into(),
        }
    }

    /// Remove (仅 before)
    pub fn remove(field: impl Into<String>, before: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            op: DiffOp::Remove,
            before: before.into(),
            after: String::new(),
        }
    }

    /// Replace (两端都有)
    pub fn replace(
        field: impl Into<String>,
        before: impl Into<String>,
        after: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            op: DiffOp::Replace,
            before: before.into(),
            after: after.into(),
        }
    }

    /// 单行 UI 渲染：带 `[+]/[-]/[~]` 前缀
    pub fn render_line(&self) -> Line<'static> {
        let (marker, color, body) = match self.op {
            DiffOp::Add => ("[+]", Color::Green, format!("{}: {}", self.field, self.after)),
            DiffOp::Remove => (
                "[-]",
                Color::Red,
                format!("{}: {}", self.field, self.before),
            ),
            DiffOp::Replace => (
                "[~]",
                Color::Yellow,
                format!("{}: {} → {}", self.field, self.before, self.after),
            ),
        };
        Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(body, Style::default().fg(color)),
        ])
    }
}

/// DiffView 渲染所需的状态快照。
///
/// 由主循环（`App::run`）在每次 draw 前用 listener / 选中变更填充。
#[derive(Debug, Clone, PartialEq)]
pub struct DiffView {
    /// 字段级 diff 列表（按 BTreeMap key 升序，与 `compute_diff` 输出一致）
    pub entries: Vec<DiffEntry>,
    /// AI 摘要（`None` 表示「AI 不可达 / 未配置」）
    pub ai_summary: Option<AiSummary>,
    /// 是否展开下半 AI tab（tab 键切换）
    pub show_ai: bool,
}

impl Default for DiffView {
    fn default() -> Self {
        Self::empty()
    }
}

impl DiffView {
    /// 构造空 DiffView（首屏用：上半空 + AI 关闭 + tab 隐藏）
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            ai_summary: None,
            show_ai: false,
        }
    }

    /// tab 键切换下半 AI tab 显隐
    pub fn toggle_ai_tab(&mut self) {
        self.show_ai = !self.show_ai;
    }

    /// 设置 AI 摘要（用于主循环在 ModelGateway 调用后回填）
    pub fn set_ai_summary(&mut self, summary: Option<AiSummary>) {
        self.ai_summary = summary;
    }

    /// 是否有 AI 摘要（无论显隐）
    pub fn has_ai_summary(&self) -> bool {
        self.ai_summary.is_some()
    }

    /// 是否下半 AI tab 实际可见：开关开 + 摘要存在
    pub fn ai_tab_visible(&self) -> bool {
        self.show_ai && self.ai_summary.is_some()
    }
}

/// 渲染 DiffView 到指定 area。
///
/// 两段垂直布局：
/// - 上 60% → field-level diff；
/// - 下 40% → AI tab（开关 + 摘要）；AI 不可达时显示「AI: off」。
pub fn render(frame: &mut Frame, area: Rect, view: &DiffView) {
    if area.height < 3 {
        // 太小到连两段都画不下 → 仅画上半提示
        let para = Paragraph::new(Line::from(Span::styled(
            "(terminal too small for diff view)",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(para, area);
        return;
    }

    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Percentage(60),
            Constraint::Percentage(40),
        ],
    )
    .split(area);

    render_diff_pane(frame, chunks[0], &view.entries);
    render_ai_pane(frame, chunks[1], view);
}

/// 渲染上半 field-level diff 列表
fn render_diff_pane(frame: &mut Frame, area: Rect, entries: &[DiffEntry]) {
    if entries.is_empty() {
        let para = Paragraph::new(Line::from(Span::styled(
            "(no field-level diff to display)",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(para, area);
        return;
    }

    let lines: Vec<Line<'static>> = entries.iter().map(|e| e.render_line()).collect();
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// 渲染下半 AI tab（按 show_ai + ai_summary 分支）
fn render_ai_pane(frame: &mut Frame, area: Rect, view: &DiffView) {
    // 用一个 Box 边框区分上下半（边框样式：tab 未展开时灰、展开时亮）
    let border_style = if view.ai_tab_visible() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // 标题（tab 是否显示）
    let title_style = if view.ai_tab_visible() {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title_text = if view.show_ai {
        " AI summary (tab to hide) "
    } else {
        " AI summary (tab to show) "
    };
    let title = ratatui::widgets::Block::default()
        .title(Span::styled(title_text, title_style))
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(border_style);

    let inner = title.inner(area);
    frame.render_widget(title, area);

    // 实际内容
    if !view.show_ai {
        // 用户按了 tab 隐藏 → 显示「(hidden)」
        let para = Paragraph::new(Line::from(Span::styled(
            "(hidden — press tab to show)",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(para, inner);
        return;
    }

    match &view.ai_summary {
        None => {
            // AI 不可达 / 未配置
            let para = Paragraph::new(Line::from(Span::styled(
                "AI: off",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            frame.render_widget(para, inner);
        }
        Some(summary) => {
            // 结构化渲染：summary + fields_added + fields_removed + semantic_class
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(vec![
                Span::styled("summary: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(summary.summary.clone(), Style::default().fg(Color::White)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("semantic_class: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(summary.semantic_class.clone(), Style::default().fg(Color::White)),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("fields_added ({}): ", summary.fields_added.len()),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if summary.fields_added.is_empty() {
                        "(none)".to_string()
                    } else {
                        summary.fields_added.join(", ")
                    },
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("fields_removed ({}): ", summary.fields_removed.len()),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if summary.fields_removed.is_empty() {
                        "(none)".to_string()
                    } else {
                        summary.fields_removed.join(", ")
                    },
                    Style::default().fg(Color::White),
                ),
            ]));
            let para = Paragraph::new(lines).wrap(Wrap { trim: false });
            frame.render_widget(para, inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 渲染并返回终端缓冲（Buffer dump）的简化字符串（逐行去除尾随空格）
    fn render_to_string(view: &DiffView, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal new");
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), view);
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

    fn sample_entries() -> Vec<DiffEntry> {
        vec![
            DiffEntry::replace("diagnosis", "chest pain", "ACS suspected"),
            DiffEntry::add("bp_systolic", "162"),
            DiffEntry::remove("old_field", "to be removed"),
        ]
    }

    fn sample_summary() -> AiSummary {
        AiSummary {
            summary: "诊断从 chest pain 更新到 ACS suspected".to_string(),
            fields_added: vec!["bp_systolic".to_string()],
            fields_removed: vec!["old_field".to_string()],
            semantic_class: "diagnosis_update".to_string(),
        }
    }

    #[test]
    fn test_diff_view_render_with_summary() {
        let view = DiffView {
            entries: sample_entries(),
            ai_summary: Some(sample_summary()),
            show_ai: true,
        };
        let s = render_to_string(&view, 80, 20);
        // 上半：三条 diff
        assert!(s.contains("[+]"), "Add marker missing: {}", s);
        assert!(s.contains("[-]"), "Remove marker missing: {}", s);
        assert!(s.contains("[~]"), "Replace marker missing: {}", s);
        assert!(s.contains("diagnosis"), "field name diagnosis missing: {}", s);
        assert!(s.contains("ACS suspected"), "after value missing: {}", s);
        assert!(s.contains("chest pain"), "before value missing: {}", s);
        // 下半：AI tab 显示
        assert!(s.contains("AI summary"), "AI pane title missing: {}", s);
        assert!(s.contains("summary:"), "AI summary block missing: {}", s);
        // AI 摘要中包含原 before/after 锚点；CJK 在 TestBackend 中被按 1 cell 渲染且
        // 可能被 padding split，故只校 ASCII 锚点。
        assert!(s.contains("chest pain"), "AI summary text should reference before value: {}", s);
        assert!(s.contains("ACS suspected"), "AI summary text should reference after value: {}", s);
        assert!(s.contains("fields_added"), "fields_added header missing: {}", s);
        assert!(s.contains("fields_removed"), "fields_removed header missing: {}", s);
        assert!(s.contains("semantic_class"), "semantic_class missing: {}", s);
        assert!(s.contains("diagnosis_update"), "semantic_class value missing: {}", s);
    }

    #[test]
    fn test_diff_view_ai_unreachable_shows_off() {
        let view = DiffView {
            entries: sample_entries(),
            ai_summary: None, // AI 不可达
            show_ai: true,
        };
        let s = render_to_string(&view, 80, 20);
        // 上半照常显示
        assert!(s.contains("[+]"), "top half should still render: {}", s);
        assert!(s.contains("diagnosis"), "top half diagnosis: {}", s);
        // 下半显示 "AI: off"
        assert!(s.contains("AI: off"), "AI off placeholder missing: {}", s);
    }

    #[test]
    fn test_diff_view_ai_tab_toggle_hides_pane() {
        let mut view = DiffView {
            entries: sample_entries(),
            ai_summary: Some(sample_summary()),
            show_ai: true,
        };
        assert!(view.ai_tab_visible(), "should be visible before toggle");
        view.toggle_ai_tab();
        assert!(!view.show_ai, "toggle should flip show_ai");
        assert!(!view.ai_tab_visible(), "should be hidden after toggle");
        let s = render_to_string(&view, 80, 20);
        // 切换后下半显示 (hidden — press tab to show)
        assert!(s.contains("hidden"), "tab-hidden placeholder missing: {}", s);
        // toggle 回來
        view.toggle_ai_tab();
        assert!(view.show_ai, "second toggle should flip back");
        assert!(view.ai_tab_visible(), "visible again after second toggle");
        let s2 = render_to_string(&view, 80, 20);
        assert!(s2.contains("ACS suspected") || s2.contains("AI summary"), "should show summary again: {}", s2);
    }

    #[test]
    fn test_diff_view_empty_default() {
        let v = DiffView::empty();
        assert!(v.entries.is_empty());
        assert!(v.ai_summary.is_none());
        assert!(!v.show_ai);
        assert!(!v.ai_tab_visible());
        assert!(!v.has_ai_summary());
        // 同等于 Default
        assert_eq!(v, DiffView::default());
    }

    #[test]
    fn test_diff_view_render_empty_top_half() {
        let view = DiffView {
            entries: Vec::new(),
            ai_summary: None,
            show_ai: false,
        };
        let s = render_to_string(&view, 80, 10);
        // 上半空 → "no field-level diff"
        assert!(
            s.contains("no field-level diff") || s.contains("("),
            "empty top half should show placeholder: {}",
            s
        );
    }

    #[test]
    fn test_diff_entry_from_fhir_three_ops() {
        use crate::his::fhir_map::DiffEntry as FhirDiff;
        let add = FhirDiff::Add("v".to_string());
        let ui_add = DiffEntry::from_fhir("field_a", &add);
        assert_eq!(ui_add.op, DiffOp::Add);
        assert_eq!(ui_add.field, "field_a");
        assert_eq!(ui_add.before, "");
        assert_eq!(ui_add.after, "v");

        let rm = FhirDiff::Remove("v".to_string());
        let ui_rm = DiffEntry::from_fhir("field_b", &rm);
        assert_eq!(ui_rm.op, DiffOp::Remove);
        assert_eq!(ui_rm.field, "field_b");
        assert_eq!(ui_rm.before, "v");
        assert_eq!(ui_rm.after, "");

        let rp = FhirDiff::Replace {
            before: "a".to_string(),
            after: "b".to_string(),
            unified_diff: String::new(),
        };
        let ui_rp = DiffEntry::from_fhir("field_c", &rp);
        assert_eq!(ui_rp.op, DiffOp::Replace);
        assert_eq!(ui_rp.field, "field_c");
        assert_eq!(ui_rp.before, "a");
        assert_eq!(ui_rp.after, "b");
    }

    #[test]
    fn test_diff_entry_constructors() {
        let a = DiffEntry::add("k", "v");
        assert_eq!(a.op, DiffOp::Add);
        assert_eq!(a.field, "k");
        assert_eq!(a.after, "v");
        assert_eq!(a.before, "");

        let r = DiffEntry::remove("k", "v");
        assert_eq!(r.op, DiffOp::Remove);
        assert_eq!(r.field, "k");
        assert_eq!(r.before, "v");
        assert_eq!(r.after, "");

        let p = DiffEntry::replace("k", "a", "b");
        assert_eq!(p.op, DiffOp::Replace);
        assert_eq!(p.field, "k");
        assert_eq!(p.before, "a");
        assert_eq!(p.after, "b");
    }

    #[test]
    fn test_diff_entry_render_line_markers() {
        let a = DiffEntry::add("k", "v");
        let l = a.render_line();
        let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(s.starts_with("[+]"), "Add marker prefix: {}", s);

        let r = DiffEntry::remove("k", "v");
        let s: String = r.render_line().spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(s.starts_with("[-]"), "Remove marker prefix: {}", s);

        let p = DiffEntry::replace("k", "a", "b");
        let s: String = p.render_line().spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(s.starts_with("[~]"), "Replace marker prefix: {}", s);
        assert!(s.contains("→"), "Replace should show arrow: {}", s);
    }

    #[test]
    fn test_diff_view_set_ai_summary() {
        let mut v = DiffView::empty();
        assert!(!v.has_ai_summary());
        v.set_ai_summary(Some(sample_summary()));
        assert!(v.has_ai_summary());
        v.set_ai_summary(None);
        assert!(!v.has_ai_summary());
    }
}
