//! ratatui 右侧 MetaView widget — 当前选中版本的元数据面板
//!
//! 字段展示：
//! - referral_id
//! - actor（修改者：姓名 + 机构）
//! - modified_at（ISO8601 形式）
//! - payload_sha256 前 12 字符（避免屏占过长）
//!
//! 与 [`crate::ui::timeline`] / [`crate::ui::diff`] 一致：UI 仅持有渲染快照，
//! 不持有 storage —— 主循环在每帧 draw 前用 timeline 选中行的 ReferralVersion
//! 投影元数据进来。
//!
//! 与源 copper-rs 差异：源铜版机器人在 runtime UI 上不展示「修改者」这种业务
//! 字段（铜版是机器人运行时，主体是算子 graph 不是「谁改的」）；本模块展示
//! 转诊单留痕回放所需的「人读可核对」字段（医务科对账场景必填）。

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::referral_version::ReferralVersion;

/// MetaView 渲染所需的状态快照
///
/// 由主循环从当前选中的 `ReferralVersion` 投影而成；selected=false
/// 时整块显示「(no version selected)」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaView {
    /// 当前是否选中了某行版本
    pub selected: bool,
    /// 当前选中的 referral_id（与 [`crate::ui::timeline`] 选中行一致）
    pub referral_id: String,
    /// 修改者
    pub actor: String,
    /// 修改时间
    pub modified_at: DateTime<Utc>,
    /// sha256 摘要前 12 字符（避免屏占过长）
    pub payload_sha256_short: String,
    /// continuity 标记（如 `manifest` / `gap[3..5]/late_join` / `recovery_point@7` / `finished@9`）
    pub continuity_label: String,
    /// 该版本对应的 Referral / Referral 的初始诊断（如有）
    pub diagnosis_initial: String,
}

impl Default for MetaView {
    fn default() -> Self {
        Self::empty()
    }
}

impl MetaView {
    /// 空 MetaView（首屏 / 未选中时用）
    pub fn empty() -> Self {
        Self {
            selected: false,
            referral_id: "-".to_string(),
            actor: "-".to_string(),
            modified_at: DateTime::parse_from_rfc3339("1970-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            payload_sha256_short: "------------".to_string(),
            continuity_label: "-".to_string(),
            diagnosis_initial: "-".to_string(),
        }
    }

    /// 从 `ReferralVersion` + 父 `Referral` 投影成 MetaView
    ///
    /// - `referral.diagnosis_initial` 用于展示「上转时初步诊断」（留痕回放常用）
    /// - 若传 `parent_referral=None` 则 `diagnosis_initial="-"`（测试 / 不带父表场景）
    pub fn from_versions(v: &ReferralVersion, parent_diagnosis_initial: Option<&str>) -> Self {
        let payload_sha256_short = if v.payload_sha256.len() >= 12 {
            v.payload_sha256[..12].to_string()
        } else {
            v.payload_sha256.clone()
        };
        let continuity_label = match &v.continuity {
            crate::model::continuity_record::ContinuityKind::Manifest => "manifest".to_string(),
            crate::model::continuity_record::ContinuityKind::Gap {
                first_id,
                last_id,
                reason,
            } => format!(
                "gap[{}..{}]/{}",
                first_id,
                last_id,
                match reason {
                    crate::model::continuity_record::SourceGapReason::LateJoin => "late_join",
                    crate::model::continuity_record::SourceGapReason::SessionEnded => "session_ended",
                    crate::model::continuity_record::SourceGapReason::ManualDrop => "manual_drop",
                }
            ),
            crate::model::continuity_record::ContinuityKind::RecoveryPoint {
                copperlist_id,
            } => format!("recovery_point@{}", copperlist_id),
            crate::model::continuity_record::ContinuityKind::Finished {
                next_copperlist_id,
            } => format!("finished@{}", next_copperlist_id),
        };
        Self {
            selected: true,
            referral_id: format!("REF (cl_id={})", v.cl_id),
            actor: v.actor.clone(),
            modified_at: v.modified_at,
            payload_sha256_short,
            continuity_label,
            diagnosis_initial: parent_diagnosis_initial
                .unwrap_or("-")
                .to_string(),
        }
    }
}

/// 渲染 MetaView 到指定 area
///
/// 带边框面板，四行内容 + 底部 padding：
/// 1. `referral_id: REF (cl_id=K)`
/// 2. `actor: NAME / INSTITUTION`
/// 3. `modified_at: ISO8601`
/// 4. `payload_sha256: 0x1234567890ab...`
/// 5. `continuity: manifest|gap[...]/late_join|...`
/// 6. `diagnosis_initial: ...`
pub fn render(frame: &mut Frame, area: Rect, view: &MetaView) {
    let block = Block::default()
        .title(Span::styled(
            " Metadata ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if !view.selected {
        let para = Paragraph::new(Line::from(Span::styled(
            "(no version selected — press down/up or wait for first version)",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(para, inner);
        return;
    }

    let key_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(Color::White);
    let hash_style = Style::default().fg(Color::Magenta);

    let lines = vec![
        Line::from(vec![
            Span::styled("referral_id: ", key_style),
            Span::styled(view.referral_id.clone(), value_style),
        ]),
        Line::from(vec![
            Span::styled("actor: ", key_style),
            Span::styled(view.actor.clone(), value_style),
        ]),
        Line::from(vec![
            Span::styled("modified_at: ", key_style),
            Span::styled(
                view.modified_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                value_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("payload_sha256: ", key_style),
            Span::styled(format!("0x{}...", view.payload_sha256_short), hash_style),
        ]),
        Line::from(vec![
            Span::styled("continuity: ", key_style),
            Span::styled(view.continuity_label.clone(), value_style),
        ]),
        Line::from(vec![
            Span::styled("diagnosis_initial: ", key_style),
            Span::styled(view.diagnosis_initial.clone(), value_style),
        ]),
    ];

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::continuity_record::{ContinuityKind, SourceGapReason};
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeMap;

    fn sample_version(cl_id: u64, version_no: u32, actor: &str, sha: &str) -> ReferralVersion {
        ReferralVersion {
            cl_id,
            version_no,
            actor: actor.to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 8, 30, 0).unwrap(),
            before: BTreeMap::new(),
            after: BTreeMap::new(),
            continuity: ContinuityKind::Manifest,
            payload_sha256: sha.to_string(),
        }
    }

    fn render_to_string(view: &MetaView, width: u16, height: u16) -> String {
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
    fn test_meta_view_empty_renders_placeholder() {
        let view = MetaView::empty();
        let s = render_to_string(&view, 40, 8);
        assert!(s.contains("no version selected"), "empty placeholder: {}", s);
    }

    #[test]
    fn test_meta_view_from_versions_shows_six_fields() {
        let v = sample_version(5, 3, "dr_li@hosp_b", "abcdef1234567890abcdef");
        let view = MetaView::from_versions(&v, Some("chest pain"));
        let s = render_to_string(&view, 50, 12);

        // 1) referral_id 行（含 cl_id=5）
        assert!(s.contains("referral_id"), "missing key: {}", s);
        assert!(s.contains("cl_id=5"), "missing cl_id value: {}", s);

        // 2) actor
        assert!(s.contains("actor"), "missing actor key: {}", s);
        assert!(s.contains("dr_li@hosp_b"), "missing actor value: {}", s);

        // 3) modified_at
        assert!(s.contains("modified_at"), "missing modified_at key: {}", s);
        assert!(
            s.contains("2026-10-10T08:30:00Z"),
            "missing ISO8601 value: {}",
            s
        );

        // 4) payload_sha256 前 12 字符（"abcdef123456"）+ 0x 前缀 + "..." 尾缀
        assert!(s.contains("payload_sha256"), "missing hash key: {}", s);
        assert!(
            s.contains("0xabcdef123456..."),
            "missing 12-char hash prefix: {}",
            s
        );

        // 5) continuity
        assert!(s.contains("continuity"), "missing continuity key: {}", s);
        assert!(s.contains("manifest"), "missing continuity value: {}", s);

        // 6) diagnosis_initial
        assert!(
            s.contains("diagnosis_initial"),
            "missing diagnosis_initial key: {}",
            s
        );
        assert!(s.contains("chest pain"), "missing diagnosis value: {}", s);
    }

    #[test]
    fn test_meta_view_payload_sha256_short_truncates() {
        let v = sample_version(1, 1, "a", "abc");
        let view = MetaView::from_versions(&v, None);
        // 长度 < 12 → 不截断，全展示
        assert_eq!(view.payload_sha256_short, "abc");

        let v2 = sample_version(1, 1, "a", &"a".repeat(64));
        let view2 = MetaView::from_versions(&v2, None);
        assert_eq!(view2.payload_sha256_short.len(), 12);
        assert_eq!(view2.payload_sha256_short, "a".repeat(12));
    }

    #[test]
    fn test_meta_view_continuity_label_four_variants() {
        // Manifest
        let mut v = sample_version(1, 1, "a", &"0".repeat(64));
        v.continuity = ContinuityKind::Manifest;
        let view = MetaView::from_versions(&v, None);
        assert_eq!(view.continuity_label, "manifest");

        // Gap
        v.continuity = ContinuityKind::Gap {
            first_id: 3,
            last_id: 5,
            reason: SourceGapReason::LateJoin,
        };
        let view = MetaView::from_versions(&v, None);
        assert_eq!(view.continuity_label, "gap[3..5]/late_join");

        v.continuity = ContinuityKind::Gap {
            first_id: 10,
            last_id: 12,
            reason: SourceGapReason::ManualDrop,
        };
        let view = MetaView::from_versions(&v, None);
        assert_eq!(view.continuity_label, "gap[10..12]/manual_drop");

        // RecoveryPoint
        v.continuity = ContinuityKind::RecoveryPoint { copperlist_id: 7 };
        let view = MetaView::from_versions(&v, None);
        assert_eq!(view.continuity_label, "recovery_point@7");

        // Finished
        v.continuity = ContinuityKind::Finished {
            next_copperlist_id: 9,
        };
        let view = MetaView::from_versions(&v, None);
        assert_eq!(view.continuity_label, "finished@9");
    }

    #[test]
    fn test_meta_view_empty_default() {
        let v = MetaView::empty();
        assert!(!v.selected);
        assert_eq!(v.referral_id, "-");
        assert_eq!(v.actor, "-");
        assert_eq!(v.payload_sha256_short, "------------");
        assert_eq!(v.continuity_label, "-");
        assert_eq!(v.diagnosis_initial, "-");
        // 默认等于 Default
        assert_eq!(v, MetaView::default());
    }

    #[test]
    fn test_meta_view_parent_diagnosis_initial_optional() {
        let v = sample_version(1, 1, "a", &"0".repeat(64));
        let view = MetaView::from_versions(&v, None);
        assert_eq!(view.diagnosis_initial, "-");

        let view2 = MetaView::from_versions(&v, Some("ACS suspected"));
        assert_eq!(view2.diagnosis_initial, "ACS suspected");
    }

    #[test]
    fn test_meta_view_selected_flag_set_after_from_versions() {
        let v = sample_version(1, 1, "a", &"0".repeat(64));
        let view = MetaView::from_versions(&v, None);
        assert!(view.selected);
    }

    #[test]
    fn test_meta_view_render_iso8601_modified_at_field() {
        let v = sample_version(1, 1, "dr_wang", &"0".repeat(64));
        let view = MetaView::from_versions(&v, None);
        let s = render_to_string(&view, 40, 12);
        // ISO8601 字段
        assert!(s.contains("2026-10-10T08:30:00Z"), "ISO8601: {}", s);
    }
}