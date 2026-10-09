//! 桌面端 ratatui UI 模块
//!
//! 子模块：
//! - [`status_bar`]：顶部状态条（referral_id / 版本数 / fsck 空洞 / AI 状态 / 选中 cl_id）
//! - [`diff`]：中央 DiffView（field-level diff + AI 摘要 tab）
//! - [`timeline`]：左侧版本列表（按 cl_id 倒序排序 + ListState 选中状态）
//! - [`meta`]：右侧元数据面板（当前选中版本的 referral_id / actor / modified_at / sha256 / continuity）
//! - [`app`]：三视图主循环（App::run_for_ticks + tokio::select! 事件循环）

pub mod app;
pub mod diff;
pub mod meta;
pub mod status_bar;
pub mod timeline;