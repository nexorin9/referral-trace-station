//! live_log: 实时事件广播注册表
//!
//! 镜像 copper-rs `LiveLogListener = Box<dyn Fn(&CuLogEntry, &str, &[&str])>` 主路径，
//! 适配到桌面端 UI 即时刷新场景：
//! - [`listener::TraceEvent`] 四态枚举：version_added / fsck_alert /
//!   ai_unavailable / his_unavailable（与 event_bus TraceEvent 共享）
//! - [`listener::LiveLogRegistry`] 注册表：register → ListenerId 单调 +
//!   unregister + broadcast
//! - [`listener::ListenerId`] u64 单调（首分配 = 1，0 留给「未观察」语义）

pub mod listener;

pub use listener::{
    LiveLogError, LiveLogRegistry, TraceEvent, ListenerId, LISTENER_ID_FIRST,
};
