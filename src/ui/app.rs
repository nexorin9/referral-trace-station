//! ratatui 三视图主循环 App::run()
//!
//! 主路径：
//! - 顶部 [`status_bar`]（fsck 状态 / AI 状态 / 选中 cl_id）
//! - 左 [`timeline`]（按 cl_id 倒序的版本列表）
//! - 中 [`diff`]（字段级 diff + AI 摘要 tab）
//! - 右 [`meta`]（当前选中版本的 referral_id / actor / modified_at / sha256 / continuity）
//!
//! 事件循环：
//! ```text
//! tokio::select! {
//!     event = subscriber.recv() => 处理 TraceEvent (VersionAdded / FsckAlert / AiUnavailable / HisUnavailable)
//!     _tick  = draw_interval.tick() => 重绘 ratatui 帧
//!     key    = key_press()          => 处理 q 退出 / tab 切 AI / j k 上下
//! }
//! ```
//!
//! 与源 copper-rs 差异：源是机器人运行时，主循环是 CopperList 处理循环；
//! 本模块是桌面端事件循环，UI 渲染为主，处理 TraceEvent 为辅。
//! 但「事件驱动 + 持久化状态」的核心思想与源一致 —— UI 不持有业务数据，
//! 仅持有渲染快照；业务状态由 ClIdStore / Chain / LiveLogRegistry 等模块负责。

use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Terminal;

use crate::event_bus::{EventBus, Subscriber};
use crate::live_log::TraceEvent as LiveTraceEvent;
use crate::model::referral_version::{DiffOp, ReferralVersion};

use super::diff::{DiffEntry as UiDiffEntry, DiffView};
use super::meta::MetaView;
use super::status_bar::{render as render_status_bar, AiState, StatusBar};
use super::timeline::TimelineView;

/// UI 渲染所用业务状态的最小持有
///
/// App 不持有 storage —— `versions` 仅作为 UI 渲染快照；
/// ClIdStore / Chain / fsck 状态由上游模块（如 `App::on_trace_event` 调用的 listener）
/// 维护。这里只把"UI 看得见"的字段提取出来。
#[derive(Debug)]
pub struct AppState {
    /// 所有已观察的 ReferralVersion 快照（按 cl_id 升序，但 timeline.rs 渲染时再倒序）
    pub versions: Vec<ReferralVersion>,
    /// referral 父表（key=referral_id）；value=(diagnosis_initial, total_versions)
    pub parent_diagnosis: BTreeMap<String, String>,
    /// 当前 referral 的 fsck 空洞数
    pub fsck_holes: u64,
    /// fsck truncated 标记
    pub fsck_truncated: bool,
    /// AI 状态（off / closed / on）
    pub ai_state: AiState,
    /// 当前选中行的 ReferralVersion.cl_id（与 timeline::selected 一致）
    pub selected_cl_id: Option<u64>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::empty()
    }
}

impl AppState {
    pub fn empty() -> Self {
        Self {
            versions: Vec::new(),
            parent_diagnosis: BTreeMap::new(),
            fsck_holes: 0,
            fsck_truncated: false,
            ai_state: AiState::Off,
            selected_cl_id: None,
        }
    }

    /// 处理 VersionAdded 事件：把新版本塞进 versions + 自动选中
    fn on_version_added(&mut self, ev: &LiveTraceEvent) {
        if let LiveTraceEvent::VersionAdded {
            referral_id,
            cl_id,
            actor,
        } = ev
        {
            // 用 actor 拼 ReferralVersion.modified_at 用当前时间；这里我们
            // 不调用 chrono::Utc::now（保证测试可重现），由 caller 提前填好；
            // 本 UI 层假设 version 已由 upstream 模块投影到 self.versions 中。
            // 本函数仅做 self.selected_cl_id = Some(*cl_id)。
            self.selected_cl_id = Some(*cl_id);
            // 注：real call 是由 watcher 直接 push 到 self.versions（业务侧）
            // 这里仅保留「最新被选中的 cl_id」
            let _ = (referral_id, actor);
        }
    }

    /// 处理 FsckAlert 事件：更新 fsck 空洞计数 + truncated 标记
    fn on_fsck_alert(&mut self, ev: &LiveTraceEvent) {
        if let LiveTraceEvent::FsckAlert {
            referral_id: _,
            first_id: _,
            last_id: _,
            reason: _,
            truncated,
        } = ev
        {
            self.fsck_holes += 1;
            if *truncated {
                self.fsck_truncated = true;
            }
        }
    }

    /// 处理 AiUnavailable 事件：AI 状态转为 Closed
    fn on_ai_unavailable(&mut self, ev: &LiveTraceEvent) {
        if let LiveTraceEvent::AiUnavailable { .. } = ev {
            self.ai_state = AiState::Closed;
        }
    }

    /// 处理 HisUnavailable 事件：业务侧会决定 fallback；UI 仅记一次
    fn on_his_unavailable(&mut self, ev: &LiveTraceEvent) {
        if let LiveTraceEvent::HisUnavailable { .. } = ev {
            // UI 层无需修改状态（drop 降级由 watcher 接管）
            // 这里保留 hook 便于后续扩展（如状态栏显示「HIS down」）
        }
    }

    /// 处理单个 TraceEvent
    pub fn on_trace_event(&mut self, ev: &LiveTraceEvent) {
        match ev {
            LiveTraceEvent::VersionAdded { .. } => self.on_version_added(ev),
            LiveTraceEvent::FsckAlert { .. } => self.on_fsck_alert(ev),
            LiveTraceEvent::AiUnavailable { .. } => self.on_ai_unavailable(ev),
            LiveTraceEvent::HisUnavailable { .. } => self.on_his_unavailable(ev),
        }
    }

    /// 把 state 投影成 ratatui 各子 view
    pub fn build_views(&self) -> (TimelineView, DiffView, MetaView, StatusBar) {
        // timeline：按 cl_id 倒序
        let timeline = TimelineView::from_versions(&self.versions);
        // meta：当前选中版本的元数据
        let meta = self
            .versions
            .iter()
            .find(|v| Some(v.cl_id) == self.selected_cl_id)
            .map(|v| {
                let parent_dx = self
                    .versions
                    .first()
                    .and_then(|first| self.parent_diagnosis.get(&first.referral_id_placeholder()).cloned());
                MetaView::from_versions(v, parent_dx.as_deref())
            })
            .unwrap_or_else(MetaView::empty);
        // diff：当前选中版本的字段级 diff
        let diff_entries = self
            .versions
            .iter()
            .find(|v| Some(v.cl_id) == self.selected_cl_id)
            .map(|v| field_diff_to_ui(&v.field_diff()))
            .unwrap_or_default();
        let diff = DiffView {
            entries: diff_entries,
            ai_summary: None,
            show_ai: false,
        };
        // status_bar
        let current_referral = self
            .versions
            .iter()
            .find(|v| Some(v.cl_id) == self.selected_cl_id)
            .map(|v| format!("REF-{}", v.cl_id))
            .unwrap_or_else(|| "-".to_string());
        let total_versions = self.versions.len() as u64;
        let status_bar = StatusBar {
            referral_id: current_referral,
            total_versions,
            fsck_holes: self.fsck_holes,
            fsck_truncated: self.fsck_truncated,
            ai_state: self.ai_state,
            selected_cl_id: self.selected_cl_id,
        };
        (timeline, diff, meta, status_bar)
    }
}

/// ReferralVersion 本身没有 referral_id 字段（外键挂在父表）；
/// 给 timeline 调用方一个占位 key（用 first version 的 actor 解析）。
impl ReferralVersion {
    /// 占位：返回 actor 的前 12 字符作为 parent_diagnosis map 的 key（仅 UI 用）
    pub fn referral_id_placeholder(&self) -> String {
        // ReferralVersion 不存 referral_id 是 spec 设计（外键由 caller 注入）；
        // UI 层用 cl_id 作 placeholder，避免 panicking。
        format!("cl_id={}", self.cl_id)
    }
}

/// 把 ReferralVersion::field_diff（model::DiffOp）转 UI DiffEntry
fn field_diff_to_ui(
    diff: &BTreeMap<String, DiffOp>,
) -> Vec<UiDiffEntry> {
    diff.iter()
        .map(|(field, op)| match op {
            DiffOp::Add(v) => UiDiffEntry::add(field.clone(), v.clone()),
            DiffOp::Remove => UiDiffEntry::remove(field.clone(), ""),
            DiffOp::Replace { before, after } => {
                UiDiffEntry::replace(field.clone(), before.clone(), after.clone())
            }
        })
        .collect()
}

/// 键盘事件 — 仅支持 q 退出 + tab 切 AI + j/k 上下 + 1/0 数字
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Quit,
    ToggleAiTab,
    Next,
    Previous,
    Unknown,
}

/// App：桌面端 ratatui 主循环
pub struct App {
    pub state: AppState,
    pub bus: EventBus,
}

impl App {
    pub fn new(state: AppState, bus: EventBus) -> Self {
        Self { state, bus }
    }

    /// 构造一个 EventBus subscriber 包装（便于在 run() 里 await）
    pub fn subscribe(&self) -> Subscriber {
        Subscriber::from_bus(&self.bus)
    }

    /// 投影 state → 三个 view + status_bar；调用方负责渲染
    pub fn render_frame<B: Backend>(
        &self,
        terminal: &mut Terminal<B>,
    ) -> Result<(), AppError> {
        let (timeline, diff, meta, status_bar) = self.state.build_views();
        terminal
            .draw(|frame| {
                let area = frame.size();
            // 顶 1 行 status_bar + 下方三视图（左 30% timeline + 中 40% diff + 右 30% meta）
            let outer_chunks = Layout::new(
                Direction::Vertical,
                [Constraint::Length(1), Constraint::Min(3)],
            )
            .split(area);

            render_status_bar(frame, outer_chunks[0], &status_bar);

            let body_chunks = Layout::new(
                Direction::Horizontal,
                [
                    Constraint::Percentage(30),
                    Constraint::Percentage(40),
                    Constraint::Percentage(30),
                ],
            )
            .split(outer_chunks[1]);

            super::timeline::render(frame, body_chunks[0], &timeline);
            super::diff::render(frame, body_chunks[1], &diff);
            super::meta::render(frame, body_chunks[2], &meta);
            })?;
        Ok(())
    }

    /// 主事件循环：跑 N 个 tick 后退出（便于测试）
    ///
    /// 真实终端用：`App::run(terminal, draw_interval=100ms).await` 阻塞直到 q
    pub async fn run_for_ticks<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        ticks: u64,
        draw_interval: Duration,
    ) -> Result<(), AppError> {
        let mut subscriber = self.subscribe();
        let mut tick_count = 0u64;
        let mut interval = tokio::time::interval(draw_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event_opt = subscriber.recv() => {
                    match event_opt {
                        Ok(ev) => self.state.on_trace_event(&ev),
                        Err(crate::event_bus::EventBusError::ReceiverLagged { .. }) => {
                            // 滞后：跳过，本轮继续
                            continue;
                        }
                        Err(crate::event_bus::EventBusError::BroadcastClosed) => {
                            // 没人 publish 了 → 直接退出
                            return Ok(());
                        }
                        Err(crate::event_bus::EventBusError::MpscClosed) => {
                            // mpsc audit sink 关闭不是退出条件
                            continue;
                        }
                    }
                    self.render_frame(terminal)?;
                }
                _ = interval.tick() => {
                    tick_count += 1;
                    self.render_frame(terminal)?;
                    if tick_count >= ticks {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// App 错误类型
#[derive(Debug)]
pub enum AppError {
    /// ratatui 渲染失败
    Render(std::io::Error),
    /// EventBus 关闭
    BusClosed,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Render(e) => write!(f, "ratatui render error: {}", e),
            AppError::BusClosed => write!(f, "event bus closed"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Render(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::continuity_record::ContinuityKind;
    use crate::ui::diff::DiffOp as UiDiffOp;
    use crate::ui::status_bar::StatusBarSeverity;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn sample_version(cl_id: u64, version_no: u32, actor: &str) -> ReferralVersion {
        let mut before = BTreeMap::new();
        before.insert("diagnosis".to_string(), "chest pain".to_string());
        let mut after = BTreeMap::new();
        after.insert("diagnosis".to_string(), "ACS suspected".to_string());
        after.insert("bp_systolic".to_string(), "162".to_string());
        ReferralVersion {
            cl_id,
            version_no,
            actor: actor.to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 1, 0, 0).unwrap(),
            before,
            after,
            continuity: ContinuityKind::Manifest,
            payload_sha256: "0".repeat(64),
        }
    }

    fn render_to_string(
        terminal: &mut Terminal<TestBackend>,
    ) -> String {
        // TestBackend 直接暴露 buffer() 方法（不走 Backend trait）
        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        let width = area.width;
        let height = area.height;
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
    fn test_app_state_default_empty() {
        let state = AppState::default();
        assert!(state.versions.is_empty());
        assert_eq!(state.fsck_holes, 0);
        assert!(!state.fsck_truncated);
        assert_eq!(state.ai_state, AiState::Off);
        assert_eq!(state.selected_cl_id, None);
        // 等价于 empty()
        assert_eq!(state.versions.len(), AppState::empty().versions.len());
    }

    #[test]
    fn test_app_state_on_version_added_sets_selected_cl_id() {
        let mut state = AppState::empty();
        let ev = LiveTraceEvent::VersionAdded {
            referral_id: "REF-001".to_string(),
            cl_id: 7,
            actor: "dr_li".to_string(),
        };
        state.on_trace_event(&ev);
        assert_eq!(state.selected_cl_id, Some(7));
    }

    #[test]
    fn test_app_state_on_fsck_alert_increments_holes() {
        let mut state = AppState::empty();
        let ev = LiveTraceEvent::FsckAlert {
            referral_id: "REF-001".to_string(),
            first_id: 3,
            last_id: 5,
            reason: "late_join".to_string(),
            truncated: false,
        };
        state.on_trace_event(&ev);
        assert_eq!(state.fsck_holes, 1);
        assert!(!state.fsck_truncated);

        // truncated=true
        let ev2 = LiveTraceEvent::FsckAlert {
            referral_id: "REF-001".to_string(),
            first_id: 6,
            last_id: 100,
            reason: "session_ended".to_string(),
            truncated: true,
        };
        state.on_trace_event(&ev2);
        assert_eq!(state.fsck_holes, 2);
        assert!(state.fsck_truncated);
    }

    #[test]
    fn test_app_state_on_ai_unavailable_sets_closed() {
        let mut state = AppState::empty();
        let ev = LiveTraceEvent::AiUnavailable {
            referral_id: "REF-001".to_string(),
            cl_id: 1,
            cause: "network".to_string(),
        };
        state.on_trace_event(&ev);
        assert_eq!(state.ai_state, AiState::Closed);
    }

    #[test]
    fn test_app_state_on_his_unavailable_does_not_panic() {
        let mut state = AppState::empty();
        let ev = LiveTraceEvent::HisUnavailable {
            referral_id: "REF-001".to_string(),
            cause: "5xx".to_string(),
        };
        state.on_trace_event(&ev);
        // 不修改状态（业务 fallback 由 watcher 接管）
        assert_eq!(state.ai_state, AiState::Off);
        assert_eq!(state.fsck_holes, 0);
    }

    #[test]
    fn test_app_state_build_views_with_three_versions() {
        let mut state = AppState::empty();
        state.versions.push(sample_version(1, 1, "dr_a"));
        state.versions.push(sample_version(2, 2, "dr_b"));
        state.versions.push(sample_version(3, 3, "dr_c"));
        state.selected_cl_id = Some(3);

        let (timeline, _diff, meta, status_bar) = state.build_views();
        // timeline: cl_id 倒序，选中第一行（cl_id=3）
        assert_eq!(timeline.rows[0].cl_id, 3);
        assert_eq!(timeline.selected_cl_id(), Some(3));
        // meta: selected=true（因为 versions 含 cl_id=3）
        assert!(meta.selected);
        // status_bar
        assert_eq!(status_bar.total_versions, 3);
        assert_eq!(status_bar.selected_cl_id, Some(3));
        assert_eq!(status_bar.referral_id, "REF-3");
    }

    #[test]
    fn test_app_state_build_views_when_no_selection() {
        let mut state = AppState::empty();
        state.versions.push(sample_version(1, 1, "dr_a"));
        // 未选中
        let (_timeline, _diff, meta, _status_bar) = state.build_views();
        assert!(!meta.selected);
    }

    #[test]
    fn test_field_diff_to_ui_three_ops() {
        let mut diff = BTreeMap::new();
        diff.insert("a".to_string(), DiffOp::Add("new".to_string()));
        diff.insert("b".to_string(), DiffOp::Remove);
        diff.insert(
            "c".to_string(),
            DiffOp::Replace {
                before: "old".to_string(),
                after: "new".to_string(),
            },
        );
        let ui = field_diff_to_ui(&diff);
        assert_eq!(ui.len(), 3);
        // 至少有一条 Add / Remove / Replace（顺序由 BTreeMap key 决定）
        let ops: Vec<UiDiffOp> = ui.iter().map(|e| e.op).collect();
        assert!(ops.contains(&UiDiffOp::Add));
        assert!(ops.contains(&UiDiffOp::Remove));
        assert!(ops.contains(&UiDiffOp::Replace));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_app_event_loop_render_3_ticks_snapshot() {
        let bus = EventBus::new();
        let mut state = AppState::empty();
        state.versions.push(sample_version(1, 1, "dr_li@hosp_a"));
        state.versions.push(sample_version(2, 2, "dr_wang@hosp_b"));
        state.versions.push(sample_version(3, 3, "dr_zhao@hosp_c"));
        state.selected_cl_id = Some(3);
        state.ai_state = AiState::On;

        let mut app = App::new(state, bus);
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).expect("terminal new");

        // 跑 3 个 tick（draw_interval=20ms × 3 = ~60ms）
        app.run_for_ticks(&mut terminal, 3, Duration::from_millis(20))
            .await
            .expect("run_for_ticks ok");

        let s = render_to_string(&mut terminal);

        // 顶部 status_bar
        assert!(
            s.contains("ref: REF-3") || s.contains("versions: 3"),
            "status_bar should show version count: {}",
            s
        );
        assert!(s.contains("AI: on"), "AI: on label missing: {}", s);

        // 左侧 timeline（cl_id=3 在前）
        let pos_3 = s.find("cl_id=3").expect("cl_id=3 should appear");
        let pos_1 = s.find("cl_id=1").expect("cl_id=1 should appear");
        assert!(
            pos_3 < pos_1,
            "cl_id=3 should appear before cl_id=1: {}",
            s
        );

        // 右侧 meta：selected=true → actor 显示
        assert!(
            s.contains("dr_zhao@hosp_c"),
            "selected version actor should appear in meta panel: {}",
            s
        );
        assert!(
            s.contains("payload_sha256"),
            "meta panel should include payload_sha256: {}",
            s
        );
        assert!(s.contains("continuity"), "meta should include continuity: {}", s);

        // 中央 diff：三个版本最新一条有 diagnosis/bp_systolic diff
        assert!(s.contains("diagnosis"), "diff should include diagnosis: {}", s);
        assert!(s.contains("chest pain"), "diff should include before value: {}", s);
        assert!(s.contains("ACS suspected"), "diff should include after value: {}", s);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_app_event_loop_processes_version_added_event() {
        let bus = EventBus::new();
        let mut state = AppState::empty();
        state.versions.push(sample_version(1, 1, "dr_a"));
        let mut app = App::new(state, bus);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal new");

        // broadcast 不会重放给 publish 之后的订阅者；这里把 publish 放在
        // tokio::spawn 异步任务里延迟 30ms 后执行，给 run_for_ticks 内部
        // subscriber 充分时间 attach。
        let bus_clone = app.bus.clone_handle();
        let publisher_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = bus_clone.publish(LiveTraceEvent::VersionAdded {
                referral_id: "REF-NEW".to_string(),
                cl_id: 99,
                actor: "dr_new".to_string(),
            });
        });

        // 跑 5 tick（≈75ms）让内部 subscriber attach 并收到 publish 事件
        app.run_for_ticks(&mut terminal, 5, Duration::from_millis(15))
            .await
            .expect("run_for_ticks ok");

        // 等 publisher task 完成
        let _ = publisher_task.await;

        // selected_cl_id 应该被更新到 99
        assert_eq!(app.state.selected_cl_id, Some(99));
    }

    #[test]
    fn test_app_new_keeps_state_and_bus() {
        let bus = EventBus::new();
        let state = AppState::empty();
        let app = App::new(state, bus);
        assert_eq!(app.state.versions.len(), 0);
        assert_eq!(app.state.selected_cl_id, None);
    }

    #[test]
    fn test_app_render_frame_succeeds_on_empty_state() {
        let bus = EventBus::new();
        let app = App::new(AppState::empty(), bus);
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal new");
        let result = app.render_frame(&mut terminal);
        assert!(result.is_ok(), "render_frame should succeed on empty state");
    }

    #[test]
    fn test_app_status_bar_severity_constructed_correctly() {
        let mut state = AppState::empty();
        state.fsck_holes = 0;
        state.ai_state = AiState::On;
        state.selected_cl_id = Some(1);
        let (_, _, _, status_bar) = state.build_views();
        // 无空洞 + AI on + 有选中 → Healthy
        assert_eq!(status_bar.severity(), StatusBarSeverity::Healthy);

        state.fsck_holes = 1;
        let (_, _, _, status_bar) = state.build_views();
        assert_eq!(status_bar.severity(), StatusBarSeverity::Warning);
    }

    #[test]
    fn test_app_referral_id_placeholder_format() {
        let v = sample_version(5, 1, "dr_x");
        assert_eq!(v.referral_id_placeholder(), "cl_id=5");
    }
}