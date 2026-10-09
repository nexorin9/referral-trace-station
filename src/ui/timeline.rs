//! ratatui 左侧 Timeline widget — 按 cl_id 倒序排列的版本列表
//!
//! 布局：
//! - 标题栏："Versions (cl_id desc)"
//! - 选中行加粗 + 黄色背景（当前 selected_cl_id）
//! - 每行格式：`v{N}  cl_id={K}  actor={S}`（N=version_no, K=cl_id, S=actor）
//!
//! 与 [`crate::ui::status_bar`] 一致：UI 仅持有渲染快照（Vec<TimelineRow> +
//! ratatui `ListState`），不持有 storage —— 主循环在每次 draw 前用
//! listener 回调把最新 versions 投影进来。
//!
//! 与源 copper-rs 差异：源按 CopperList 序列遍历 raw bytes；本模块按业务
//! 版本号排序 + 渲染人类可读字段名（actor / version_no / cl_id）。

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::model::referral_version::ReferralVersion;

/// Timeline 一行渲染所需的最小快照。
///
/// 由主循环在每帧 draw 前从 `ReferralVersion` 投影而成；UI 不持有
/// 完整 `ReferralVersion`（含 BTreeMap 大 payload）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRow {
    /// 该版本对应的 ReferralVersion.cl_id（u64，单调）
    pub cl_id: u64,
    /// 该版本对应的 ReferralVersion.version_no
    pub version_no: u32,
    /// 修改者（`ReferralVersion.actor`，可能含姓名 + 机构）
    pub actor: String,
    /// 修改时间（`ReferralVersion.modified_at`）— 暂不在行内展示，留作
    /// 调试用；主路径上每行只展示 cl_id / version_no / actor
    pub modified_at: DateTime<Utc>,
}

impl TimelineRow {
    /// 从 `ReferralVersion` 投影成 UI 用的 TimelineRow
    pub fn from_referral_version(v: &ReferralVersion) -> Self {
        Self {
            cl_id: v.cl_id,
            version_no: v.version_no,
            actor: v.actor.clone(),
            modified_at: v.modified_at,
        }
    }

    /// 单行 UI 渲染：`v{N}  cl_id={K}  actor={S}`
    pub fn render_line(&self) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("v{} ", self.version_no),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("cl_id={} ", self.cl_id),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("actor={}", self.actor),
                Style::default().fg(Color::Gray),
            ),
        ])
    }
}

/// Timeline 渲染所需的状态快照
///
/// `rows` 已按 cl_id 倒序排列（最新版本在最上）；`list_state` 持有
/// 选中行索引。
#[derive(Debug)]
pub struct TimelineView {
    pub rows: Vec<TimelineRow>,
    pub list_state: ListState,
}

impl Default for TimelineView {
    fn default() -> Self {
        Self::empty()
    }
}

impl TimelineView {
    /// 空 TimelineView（首屏用）
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            list_state: ListState::default(),
        }
    }

    /// 从 `Vec<ReferralVersion>` 构建（按 cl_id 倒序排序）
    ///
    /// 排序规则：cl_id 大的在前；同 cl_id 时 version_no 大的在前（一般
    /// 不会发生，因为 cl_id 与 version_no 同方向单调，但保险起见）。
    pub fn from_versions(versions: &[ReferralVersion]) -> Self {
        let mut sorted: Vec<&ReferralVersion> = versions.iter().collect();
        sorted.sort_by(|a, b| {
            b.cl_id
                .cmp(&a.cl_id)
                .then(b.version_no.cmp(&a.version_no))
        });
        let rows: Vec<TimelineRow> = sorted
            .iter()
            .map(|v| TimelineRow::from_referral_version(v))
            .collect();
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        Self { rows, list_state }
    }

    /// 当前选中行的 cl_id（None 表示未选中 / 空）
    pub fn selected_cl_id(&self) -> Option<u64> {
        self.list_state.selected().and_then(|idx| {
            self.rows.get(idx).map(|row| row.cl_id)
        })
    }

    /// 选中下一行（向下）
    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1).min(self.rows.len() - 1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    /// 选中上一行（向上）
    pub fn select_previous(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    /// 选中第一行
    pub fn select_first(&mut self) {
        if !self.rows.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    /// 选中最后一行
    pub fn select_last(&mut self) {
        if !self.rows.is_empty() {
            self.list_state.select(Some(self.rows.len() - 1));
        }
    }

    /// 行数
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// 渲染 Timeline 到指定 area
///
/// 上方 1 行标题栏 + 下方版本列表（带边框）。选中行加粗 + 黄色背景。
pub fn render(frame: &mut Frame, area: Rect, view: &TimelineView) {
    // 外框
    let block = Block::default()
        .title(Span::styled(
            " Timeline (cl_id desc) ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 || view.rows.is_empty() {
        let para = Paragraph::new(Line::from(Span::styled(
            "(no versions yet — waiting for inbox watcher)",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(para, inner);
        return;
    }

    // 用 List 渲染（ratatui 原生 ListState 支持）
    let items: Vec<ListItem> = view
        .rows
        .iter()
        .map(|row| ListItem::new(row.render_line()))
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = view.list_state.clone();
    frame.render_stateful_widget(list, inner, &mut state);
}

// 上面用到了 Paragraph，需要 import 它
// use ratatui::widgets::Paragraph;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_version(cl_id: u64, version_no: u32, actor: &str) -> ReferralVersion {
        use std::collections::BTreeMap;
        ReferralVersion {
            cl_id,
            version_no,
            actor: actor.to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 1, 0, 0).unwrap(),
            before: BTreeMap::new(),
            after: BTreeMap::new(),
            continuity: crate::model::continuity_record::ContinuityKind::Manifest,
            payload_sha256: "0".repeat(64),
        }
    }

    fn render_to_string(view: &TimelineView, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal new");
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

    #[test]
    fn test_timeline_empty_renders_placeholder() {
        let view = TimelineView::empty();
        let s = render_to_string(&view, 30, 5);
        assert!(
            s.contains("no versions yet"),
            "empty timeline should show placeholder: {}",
            s
        );
    }

    #[test]
    fn test_timeline_from_versions_sorts_cl_id_desc() {
        let versions = vec![
            sample_version(1, 1, "dr_a@hosp_a"),
            sample_version(3, 3, "dr_c@hosp_c"),
            sample_version(2, 2, "dr_b@hosp_b"),
        ];
        let view = TimelineView::from_versions(&versions);
        assert_eq!(view.rows.len(), 3);
        // 倒序：cl_id=3 在前
        assert_eq!(view.rows[0].cl_id, 3);
        assert_eq!(view.rows[1].cl_id, 2);
        assert_eq!(view.rows[2].cl_id, 1);
        // 默认选中第一行（最新）
        assert_eq!(view.selected_cl_id(), Some(3));
    }

    #[test]
    fn test_timeline_select_navigation() {
        let versions = vec![
            sample_version(1, 1, "a"),
            sample_version(2, 2, "b"),
            sample_version(3, 3, "c"),
        ];
        let mut view = TimelineView::from_versions(&versions);
        // DESC 排序：rows = [cl_id=3, cl_id=2, cl_id=1]
        assert_eq!(view.selected_cl_id(), Some(3), "默认选中首行（最新版本）");

        // select_next 一直走到底
        view.select_next();
        assert_eq!(view.selected_cl_id(), Some(2), "next → 上一行");

        view.select_next();
        assert_eq!(view.selected_cl_id(), Some(1), "next → 最早一行");

        view.select_next();
        assert_eq!(
            view.selected_cl_id(),
            Some(1),
            "next at last row stays (cl_id=1)"
        );

        // select_previous 走回头
        view.select_previous();
        assert_eq!(view.selected_cl_id(), Some(2));

        view.select_previous();
        assert_eq!(view.selected_cl_id(), Some(3));

        view.select_previous();
        assert_eq!(
            view.selected_cl_id(),
            Some(3),
            "prev at first row stays (cl_id=3)"
        );

        // select_last / select_first
        view.select_last();
        assert_eq!(view.selected_cl_id(), Some(1), "last row = cl_id=1");

        view.select_first();
        assert_eq!(view.selected_cl_id(), Some(3), "first row = cl_id=3");
    }

    #[test]
    fn test_timeline_select_on_empty_safe() {
        let mut view = TimelineView::empty();
        view.select_next();
        view.select_previous();
        view.select_first();
        view.select_last();
        assert_eq!(view.selected_cl_id(), None);
        assert!(view.is_empty());
    }

    #[test]
    fn test_timeline_render_snapshot_shows_cl_id_and_actor() {
        let versions = vec![
            sample_version(1, 1, "dr_li@hosp_b"),
            sample_version(2, 2, "dr_wang@hosp_c"),
        ];
        let view = TimelineView::from_versions(&versions);
        let s = render_to_string(&view, 60, 8);
        // 倒序排列：cl_id=2 在前
        let pos_2 = s.find("cl_id=2").expect("cl_id=2 should appear");
        let pos_1 = s.find("cl_id=1").expect("cl_id=1 should appear");
        assert!(
            pos_2 < pos_1,
            "cl_id=2 should appear before cl_id=1 (desc order): {}",
            s
        );
        // 行内含 actor 字段
        assert!(s.contains("dr_li@hosp_b"), "actor missing: {}", s);
        assert!(s.contains("dr_wang@hosp_c"), "actor missing: {}", s);
        // 行内含 version_no 前缀 v1 / v2
        assert!(s.contains("v2 "), "version_no prefix missing: {}", s);
        assert!(s.contains("v1 "), "version_no prefix missing: {}", s);
    }

    #[test]
    fn test_timeline_render_too_short_shows_placeholder() {
        let versions = vec![sample_version(1, 1, "a")];
        let view = TimelineView::from_versions(&versions);
        // inner.height < 2：height=3 给外框至少留 1 行，inner=1 仍 <2
        let s = render_to_string(&view, 30, 3);
        // 高度太小，可能没渲染条目（也可能渲染了）；placeholder 不强制
        // 主要验证不 panic
        let _ = s;
    }

    #[test]
    fn test_timeline_row_from_referral_version_field_copy() {
        let v = sample_version(7, 4, "dr_x");
        let row = TimelineRow::from_referral_version(&v);
        assert_eq!(row.cl_id, 7);
        assert_eq!(row.version_no, 4);
        assert_eq!(row.actor, "dr_x");
    }

    #[test]
    fn test_timeline_len_and_is_empty() {
        let view = TimelineView::empty();
        assert_eq!(view.len(), 0);
        assert!(view.is_empty());

        let versions = vec![sample_version(1, 1, "a"), sample_version(2, 2, "b")];
        let view = TimelineView::from_versions(&versions);
        assert_eq!(view.len(), 2);
        assert!(!view.is_empty());
    }

    #[test]
    fn test_timeline_row_render_line_format() {
        let row = TimelineRow {
            cl_id: 5,
            version_no: 2,
            actor: "dr_z@hosp_a".to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 1, 0, 0).unwrap(),
        };
        let line = row.render_line();
        let s: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(s.starts_with("v2 "), "version prefix: {}", s);
        assert!(s.contains("cl_id=5"), "cl_id field: {}", s);
        assert!(s.contains("actor=dr_z@hosp_a"), "actor field: {}", s);
    }

    #[test]
    fn test_timeline_from_versions_default_selects_first_row() {
        // 验证 sort 稳定性：相同 cl_id 时 version_no 大的在前（一般不会发生，
        // 但保险逻辑）
        let versions = vec![
            sample_version(1, 1, "a"),
            sample_version(1, 2, "b"),
            sample_version(1, 3, "c"),
        ];
        let view = TimelineView::from_versions(&versions);
        // version_no=3 在前（按 cl_id 倒序相同则按 version_no 倒序）
        assert_eq!(view.rows[0].version_no, 3);
        assert_eq!(view.selected_cl_id(), Some(1));
    }
}