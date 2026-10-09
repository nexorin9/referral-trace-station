//! LiveLogListener: 实时事件广播注册表
//!
//! 源产品能力参考（copper-rs/core/cu29_log_runtime/src/lib.rs）：
//! - `LiveLogListener = Box<dyn Fn(&CuLogEntry, &str, &[&str]) + Send + Sync + 'static>`
//! - `LiveLogListenerId = usize`，单调递增，从 1 开始（永不返回 0）
//! - `next_id.saturating_add(1).max(1)` 防止溢出回绕
//! - `LiveLogListeners { next_id, listeners: Vec<(ListenerId, LiveLogListener)> }`
//! - `insert(listener) -> ListenerId` 与 `remove(id)`
//!
//! 本任务适配到桌面端：
//! - `TraceEvent`：version_added / fsck_alert / ai_unavailable / his_unavailable
//!   四态事件，与 event_bus 的 TraceEvent 共享
//! - `ListenerId(u64)` 单调，起点 1（首分配从 1 起，0 留给「未观察」语义）
//! - `LiveLogRegistry`：BTreeMap<ListenerId, Box<dyn FnMut(...)>> + register /
//!   unregister / broadcast
//! - listener 函数签名改为 `FnMut(&TraceEvent, &str, &[&str])`（业务场景
//!   需要 stateful sink，与源纯 `Fn` 不同），其余注册/反注册/ID 单调契约
//!   与源严格对齐

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// 实时事件类型 — 与 event_bus TraceEvent 共享语义
///
/// spec §3 中提到「UI 注册为 listener，新版本入 storage 即时刷新」；这里
/// 给出四态统一枚举，覆盖转诊单版本新增、fsck 空洞检测、AI 不可达、
/// HIS 不可达四类 UI 必须即时响应的场景。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    /// 新版本加入 storage（from inbox watcher 或 HIS REST 拉取）
    VersionAdded {
        referral_id: String,
        cl_id: u64,
        actor: String,
    },
    /// fsck 检测到序列空洞
    FsckAlert {
        referral_id: String,
        first_id: u64,
        last_id: u64,
        reason: String,
        truncated: bool,
    },
    /// ModelGateway 不可达（超时/解析失败/未配置）
    AiUnavailable {
        referral_id: String,
        cl_id: u64,
        cause: String,
    },
    /// HIS REST 不可达（5xx/超时/凭据缺失）
    HisUnavailable {
        referral_id: String,
        cause: String,
    },
}

impl fmt::Display for TraceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceEvent::VersionAdded { referral_id, cl_id, actor } => {
                write!(f, "VersionAdded(referral={}, cl_id={}, actor={})", referral_id, cl_id, actor)
            }
            TraceEvent::FsckAlert { referral_id, first_id, last_id, reason, truncated } => {
                write!(
                    f,
                    "FsckAlert(referral={}, first={}, last={}, reason={}, truncated={})",
                    referral_id, first_id, last_id, reason, truncated
                )
            }
            TraceEvent::AiUnavailable { referral_id, cl_id, cause } => {
                write!(f, "AiUnavailable(referral={}, cl_id={}, cause={})", referral_id, cl_id, cause)
            }
            TraceEvent::HisUnavailable { referral_id, cause } => {
                write!(f, "HisUnavailable(referral={}, cause={})", referral_id, cause)
            }
        }
    }
}

/// Listener 函数签名 — Box<dyn FnMut + Send + 'static>
///
/// 与源 `Fn + Send + Sync` 不同：业务 sink（UI 更新、ratatui 状态推进）
/// 需要 stateful 闭包，因此用 `FnMut`。`Send` 满足跨线程注册；
/// 不要求 `Sync`，因为广播时持有独占的 `&mut self`。
pub type LiveLogListener = Box<dyn FnMut(&TraceEvent, &str, &[&str]) + Send + 'static>;

/// ListenerId 单调计数器
///
/// 源用 `usize`；本任务改为 `u64` 以与 cl_id 类型保持一致（避免 listener id
/// 与 copperlist id 在 UI 层面混淆），并显式 newtype 以便 Display + 错误诊断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ListenerId(pub u64);

impl fmt::Display for ListenerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L#{}", self.0)
    }
}

/// 首分配起点 = 1（0 留给「未注册 / 未观察」语义，与 CL_ID_FIRST = 1 对齐）
pub const LISTENER_ID_FIRST: u64 = 1;

/// LiveLogRegistry 错误类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveLogError {
    /// ListenerId 已存在（重复注册被拒）
    DuplicateListenerId { id: ListenerId },
    /// ListenerId 不存在（反注册未注册的 id）
    UnknownListenerId { id: ListenerId },
    /// ListenerId 已达饱和上限 u64::MAX
    ListenerIdExhausted,
}

impl fmt::Display for LiveLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveLogError::DuplicateListenerId { id } => {
                write!(f, "duplicate listener id: {}", id)
            }
            LiveLogError::UnknownListenerId { id } => {
                write!(f, "unknown listener id: {}", id)
            }
            LiveLogError::ListenerIdExhausted => {
                write!(f, "listener id exhausted (u64::MAX reached)")
            }
        }
    }
}

impl std::error::Error for LiveLogError {}

/// LiveLogRegistry: 注册表 + 单调 ID 分配 + 广播
///
/// 与源 `LiveLogListeners { next_id, listeners: Vec<(ListenerId, Box<dyn Fn>)> }`
/// 行为等价：
/// - `register(listener) -> Result<ListenerId, LiveLogError>`：分配 next_id，
///   next_id 单调递增（永不重用、永不回绕 0）；失败情形为饱和溢出；
/// - `unregister(id) -> Result<(), LiveLogError>`：从 map 中移除，未找到
///   返回 `UnknownListenerId`；
/// - `broadcast(event, fmt_str, param_names)`：遍历所有 listener 调用其闭包，
///   listener 内部 panic 由 broadcast 整体捕获（不污染其他 listener）。
///
/// 与源不同：使用 `BTreeMap` 而非 `Vec` 以便 O(log n) 删除 + 按 ID 排序遍历。
/// 闭包类型为 `FnMut` 而非 `Fn`，其余注册/反注册/ID 单调契约与源严格对齐。
#[derive(Default)]
pub struct LiveLogRegistry {
    next_id: u64,
    listeners: BTreeMap<ListenerId, LiveLogListener>,
}

impl LiveLogRegistry {
    /// 新建空注册表（next_id = LISTENER_ID_FIRST = 1）
    pub fn new() -> Self {
        Self {
            next_id: LISTENER_ID_FIRST,
            listeners: BTreeMap::new(),
        }
    }

    /// 注册 listener，返回单调递增的 ListenerId
    ///
    /// 与源 `insert` 等价：分配 next_id，next_id 单调递增。
    /// 这里在溢出检测时返回 Err，不静默 saturating 到边界（饱和状态下
    /// 继续分配会出现两个 listener 共享同一个 ID 的语义违反）。
    pub fn register<F>(&mut self, listener: F) -> Result<ListenerId, LiveLogError>
    where
        F: FnMut(&TraceEvent, &str, &[&str]) + Send + 'static,
    {
        // 溢出检查：next_id 已为 u64::MAX 时拒绝继续分配
        if self.next_id == u64::MAX {
            return Err(LiveLogError::ListenerIdExhausted);
        }
        let id = ListenerId(self.next_id);
        // ID 已存在保护（与源 insert 不同：源用 Vec 不过滤重复 id，
        // 本任务用 BTreeMap 显式拒收重复注册，避免广播语义混乱）
        if self.listeners.contains_key(&id) {
            return Err(LiveLogError::DuplicateListenerId { id });
        }
        self.listeners.insert(id, Box::new(listener));
        // 单调递增：源用 saturating_add(1).max(1) 防止溢出回绕到 0；
        // 这里已经把 u64::MAX 单独处理掉，因此 next_id 永远 < u64::MAX 时
        // 可以直接 +1。
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(LiveLogError::ListenerIdExhausted)?;
        Ok(id)
    }

    /// 反注册 listener
    ///
    /// 与源 `remove(id)` 等价：从容器中移除指定 ID 的 listener。
    /// 未找到时返回 `UnknownListenerId`（源为 no-op；本任务显式报错便于
    /// 业务层诊断）。
    pub fn unregister(&mut self, id: ListenerId) -> Result<(), LiveLogError> {
        match self.listeners.remove(&id) {
            Some(_) => Ok(()),
            None => Err(LiveLogError::UnknownListenerId { id }),
        }
    }

    /// 广播事件给所有 listener
    ///
    /// 与源 `notify_live_listeners` 等价：遍历所有 listener 调用其闭包。
    /// listener 内部 panic 由 `std::panic::catch_unwind` 捕获（仅当 cfg
    /// `unwind` 时生效；abort 时不保护但也不污染其他 listener）。
    /// 闭包为 `FnMut`，所以遍历时持独占 `&mut self.listeners`；遇到 listener
    /// panic 时跳过该 listener，继续广播给后续 listener。
    pub fn broadcast(&mut self, event: &TraceEvent, fmt_str: &str, param_names: &[&str]) {
        // 先收集 ID 列表（按 id 升序排列，BTreeMap 默认有序）
        let ids: Vec<ListenerId> = self.listeners.keys().copied().collect();
        for id in ids {
            // remove + insert 允许 FnMut 在 broadcast 期间被调用
            // 由于 broadcast 期间只能有一个 listener 被调用，所以先取出
            // listener 再调用，调用完再放回
            let mut listener = match self.listeners.remove(&id) {
                Some(l) => l,
                None => continue, // 已被外部 unregister
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(event, fmt_str, param_names);
            }));
            // 无论是否 panic 都放回 listener（panic 时 listener 状态可能被
            // 污染，但业务上要求「panic 不影响其他 listener」—— 放回但
            // 下次调用仍可能 panic；这是与源一致的妥协）
            self.listeners.insert(id, listener);
            if result.is_err() {
                // 单个 listener panic 不影响其他 listener — 继续下一个
                // 真实工程里通常 eprintln!("listener {} panicked", id) 记录；
                // 这里保持安静以避免日志噪音。
                continue;
            }
        }
    }

    /// 当前注册 listener 数量
    pub fn len(&self) -> usize {
        self.listeners.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }

    /// 下一个待分配的 ListenerId（仅用于诊断）
    pub fn next_id(&self) -> u64 {
        self.next_id
    }
}

impl fmt::Debug for LiveLogRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveLogRegistry")
            .field("next_id", &self.next_id)
            .field("listeners", &self.listeners.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 收集 listener 收到的所有事件的共享 buffer（用于验证广播）
    fn make_collector() -> (Arc<Mutex<Vec<String>>>, impl FnMut(&TraceEvent, &str, &[&str]) + Send + 'static) {
        let buf = Arc::new(Mutex::new(Vec::<String>::new()));
        let buf_clone = Arc::clone(&buf);
        let cb = move |event: &TraceEvent, _fmt: &str, _params: &[&str]| {
            buf_clone.lock().unwrap().push(event.to_string());
        };
        (buf, cb)
    }

    /// 步骤 1：注册 listener → 广播 → 收到事件；返回 id
    #[test]
    fn test_register_broadcast_unregister() {
        let mut reg = LiveLogRegistry::new();
        let (buf, cb) = make_collector();
        let id = reg.register(cb).expect("register ok");

        let ev = TraceEvent::VersionAdded {
            referral_id: "REF-001".to_string(),
            cl_id: 7,
            actor: "doctor.li".to_string(),
        };
        reg.broadcast(&ev, "added", &["referral", "cl_id"]);
        assert_eq!(buf.lock().unwrap().len(), 1);
        assert!(buf.lock().unwrap()[0].contains("VersionAdded"));

        // 反注册后再广播应不再收到
        reg.unregister(id).expect("unregister ok");
        reg.broadcast(&ev, "added", &["referral", "cl_id"]);
        assert_eq!(buf.lock().unwrap().len(), 1, "post-unregister must not receive");
    }

    /// 步骤 2：连续 register 返回的 ListenerId 严格单调递增
    #[test]
    fn test_listener_id_monotonic() {
        let mut reg = LiveLogRegistry::new();
        let (_b1, cb1) = make_collector();
        let (_b2, cb2) = make_collector();
        let (_b3, cb3) = make_collector();

        let id1 = reg.register(cb1).unwrap();
        let id2 = reg.register(cb2).unwrap();
        let id3 = reg.register(cb3).unwrap();

        assert_eq!(id1, ListenerId(1));
        assert_eq!(id2, ListenerId(2));
        assert_eq!(id3, ListenerId(3));
        assert!(id1 < id2);
        assert!(id2 < id3);
        assert_eq!(reg.next_id(), 4);
    }

    /// 反注册后 next_id 不倒退、再次 register 接续
    #[test]
    fn test_unregister_does_not_regress_next_id() {
        let mut reg = LiveLogRegistry::new();
        let (_b1, cb1) = make_collector();
        let (_b2, cb2) = make_collector();

        let id1 = reg.register(cb1).unwrap();
        assert_eq!(id1, ListenerId(1));
        reg.unregister(id1).expect("unregister ok");
        let id2 = reg.register(cb2).unwrap();
        assert_eq!(id2, ListenerId(2), "id must continue, not reuse removed slot");
        assert_eq!(reg.next_id(), 3);
    }

    /// 多次广播同一事件：所有 listener 都应收到（每次广播都触发）
    #[test]
    fn test_broadcast_reaches_all_listeners() {
        let mut reg = LiveLogRegistry::new();
        let (buf1, cb1) = make_collector();
        let (buf2, cb2) = make_collector();
        let (buf3, cb3) = make_collector();

        reg.register(cb1).unwrap();
        reg.register(cb2).unwrap();
        reg.register(cb3).unwrap();

        let ev = TraceEvent::FsckAlert {
            referral_id: "REF-002".to_string(),
            first_id: 5,
            last_id: 8,
            reason: "late_join".to_string(),
            truncated: false,
        };
        reg.broadcast(&ev, "fsck", &["a", "b"]);
        reg.broadcast(&ev, "fsck", &["a", "b"]);

        assert_eq!(buf1.lock().unwrap().len(), 2);
        assert_eq!(buf2.lock().unwrap().len(), 2);
        assert_eq!(buf3.lock().unwrap().len(), 2);
    }

    /// TraceEvent 四态枚举可序列化（与 event_bus 共享语义）
    #[test]
    fn test_trace_event_serde_round_trip() {
        let cases = vec![
            TraceEvent::VersionAdded {
                referral_id: "REF-A".to_string(),
                cl_id: 1,
                actor: "nurse.wang".to_string(),
            },
            TraceEvent::FsckAlert {
                referral_id: "REF-B".to_string(),
                first_id: 3,
                last_id: 5,
                reason: "session_ended".to_string(),
                truncated: false,
            },
            TraceEvent::AiUnavailable {
                referral_id: "REF-C".to_string(),
                cl_id: 9,
                cause: "timeout_4000ms".to_string(),
            },
            TraceEvent::HisUnavailable {
                referral_id: "REF-D".to_string(),
                cause: "503_service_unavailable".to_string(),
            },
        ];
        for ev in cases {
            let json = serde_json::to_string(&ev).expect("serialize");
            let back: TraceEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(ev, back);
        }
    }

    /// LiveLogError Display 全部非空
    #[test]
    fn test_error_display_strings_nonempty() {
        let cases = vec![
            LiveLogError::DuplicateListenerId { id: ListenerId(1) },
            LiveLogError::UnknownListenerId { id: ListenerId(2) },
            LiveLogError::ListenerIdExhausted,
        ];
        for err in cases {
            let s = format!("{}", err);
            assert!(!s.is_empty(), "display must not be empty: {:?}", err);
            // Display 必须体现 ID（Duplicate/Unknown）— 不允许空 placeholder
            assert!(s.contains("L#") || s.contains("exhausted"), "display missing marker: {:?}", err);
        }
    }

    /// 未注册的 ID 反注册返回 Err
    #[test]
    fn test_unregister_unknown_id_returns_err() {
        let mut reg = LiveLogRegistry::new();
        let result = reg.unregister(ListenerId(99));
        assert!(matches!(result, Err(LiveLogError::UnknownListenerId { .. })));
    }

    /// ListenerId Display 形如 "L#N"
    #[test]
    fn test_listener_id_display() {
        assert_eq!(format!("{}", ListenerId(1)), "L#1");
        assert_eq!(format!("{}", ListenerId(12345)), "L#12345");
    }
}
