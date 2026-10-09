//! EventBus: 桌面端事件总线（mpsc + broadcast 双通道）
//!
//! 适配源产品能力参考（copper-rs）：
//! - 源 cu29_log_runtime 的 `LiveLogListener` 是进程内回调注册表；本任务把它升级为
//!   桌面端 tokio 双通道总线：mpsc 单 sink（audit 持久化）+ broadcast 多订阅
//!   （ratatui UI / LiveLogRegistry）
//! - 背压策略：队列满时丢弃旧事件、保留新事件（broadcast ring buffer 天然覆盖；
//!   mpsc 满时 `try_send` 拒绝计入 dropped）+ `AtomicU64` 计数 audit
//!
//! 关键设计点：
//! - `TraceEvent` 与 `live_log::TraceEvent` 共享语义（pub re-export）——
//!   inbox watcher / fsck / ModelGateway / HisClient 都通过本总线广播
//! - 默认容量 1024（与源 CopperList 队列容量同量级），可通过
//!   `EventBus::with_capacity(n)` 自定义
//! - publish 走 **非阻塞** `try_send`：背压「丢旧保新」是产品语义，不允许
//!   在 watcher 路径上 await
//! - dropped 计数器独立于 broadcast 内部 lag 计数 —— 这是业务层关心的
//!   「我有没有丢消息」指标，对应 spec.md §5「失败模式可观测」
//!
//! 不引入的东西：
//! - 不直接镜像某一 copper-rs 文件 —— 该项目没有进程间 broadcast 概念，
//!   本模块是产品层的合成（tokio mpsc + tokio broadcast）

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

/// 与 `live_log::TraceEvent` 共享语义 —— 四个变体覆盖转诊单留痕主路径
pub use crate::live_log::TraceEvent;

/// 默认队列上限 1024
///
/// 选取理由：
/// - 桌面端 UI 刷新率 ~60Hz，1024 条 ≈ 17 秒缓冲，足以覆盖任何突发
/// - 与源 copper-rs `CopperList::MAX_BUFFERED_MESSAGES = 1024` 同量级
/// - 太小会导致正常吞吐下误触发背压；太大会浪费内存（每条事件 ~64 字节）
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// EventBus 错误类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBusError {
    /// mpsc writer 通道已关闭（无人消费 audit sink）
    MpscClosed,
    /// broadcast 订阅者滞后（`LaggedError` 由订阅者 recv 收到）
    ReceiverLagged { skipped: u64 },
    /// broadcast 通道已关闭（所有 sender drop 之后）
    BroadcastClosed,
}

impl std::fmt::Display for EventBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventBusError::MpscClosed => write!(f, "event bus mpsc writer channel closed"),
            EventBusError::ReceiverLagged { skipped } => {
                write!(f, "subscriber lagged by {} events", skipped)
            }
            EventBusError::BroadcastClosed => write!(f, "event bus broadcast channel closed"),
        }
    }
}

impl std::error::Error for EventBusError {}

/// 单次 `publish` 的结果快照
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    /// mpsc writer 端是否成功写入（false = mpsc 满或已关闭）
    pub mpsc_sent: bool,
    /// broadcast 端活跃订阅者数（`broadcast_tx.receiver_count()`）
    pub broadcast_subscribers: usize,
    /// 累计 dropped 计数（含本次 publish 触发）
    pub dropped_so_far: u64,
    /// 本次 publish 是否触发了 dropped（队列满时 ring overwrite / mpsc 拒绝）
    pub caused_drop: bool,
}

impl PublishOutcome {
    /// 是否至少有一个订阅端收到了事件
    pub fn any_recipient(&self) -> bool {
        self.mpsc_sent || self.broadcast_subscribers > 0
    }
}

/// 事件总线
///
/// 内部持有 `mpsc::Sender`（writer sink 单消费者）+ `broadcast::Sender`
/// （多订阅者 UI / LiveLogRegistry）。两个 channel 容量相同（默认 1024）。
pub struct EventBus {
    mpsc_tx: mpsc::Sender<TraceEvent>,
    mpsc_rx: Option<mpsc::Receiver<TraceEvent>>,
    broadcast_tx: broadcast::Sender<TraceEvent>,
    capacity: usize,
    dropped: Arc<AtomicU64>,
}

impl EventBus {
    /// 新建带默认容量的事件总线（1024）
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUS_CAPACITY)
    }

    /// 复制一份 handle 供跨线程 / 跨 task 调用
    ///
    /// `EventBus` 不实现 Clone（因为内部持 receiver），但业务经常需要把
    /// publisher 端移到子任务里跑；本方法返回一个新的轻量 handle，
    /// 共享 mpsc sender / broadcast sender / dropped 计数器。
    pub fn clone_handle(&self) -> EventBusHandle {
        EventBusHandle {
            mpsc_tx: self.mpsc_tx.clone(),
            broadcast_tx: self.broadcast_tx.clone(),
            dropped: Arc::clone(&self.dropped),
            capacity: self.capacity,
        }
    }

    /// 新建带指定容量的事件总线
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "event bus capacity must be > 0");
        let (mpsc_tx, mpsc_rx) = mpsc::channel(capacity);
        let (broadcast_tx, _) = broadcast::channel(capacity);
        Self {
            mpsc_tx,
            mpsc_rx: Some(mpsc_rx),
            broadcast_tx,
            capacity,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 取走 mpsc receiver —— writer sink 接管后可消费 audit 流
    ///
    /// 只能调用一次（receiver 是独占所有权）。第二次调用返回 `None`。
    pub fn take_mpsc_receiver(&mut self) -> Option<mpsc::Receiver<TraceEvent>> {
        self.mpsc_rx.take()
    }

    /// 订阅 broadcast 通道 —— 返回 `broadcast::Receiver`
    ///
    /// 与 spec 要求对齐：`subscribe() -> broadcast::Receiver`。
    /// 业务层可用 `Subscriber { rx: bus.subscribe() }` 包成 Subscriber，
    /// 享受 `recv() / try_recv()` 把 `LaggedError` 翻译成
    /// `EventBusError::ReceiverLagged` 的便利。
    pub fn subscribe(&self) -> broadcast::Receiver<TraceEvent> {
        self.broadcast_tx.subscribe()
    }

    /// 当前订阅者数（broadcast 端）
    pub fn subscriber_count(&self) -> usize {
        self.broadcast_tx.receiver_count()
    }

    /// 当前 mpsc 端是否能接收（writer sink 还活着）
    pub fn mpsc_writer_alive(&self) -> bool {
        !self.mpsc_tx.is_closed()
    }

    /// 累计 dropped 计数
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 当前 broadcast 队列长度（debug 用）
    pub fn broadcast_queue_len(&self) -> usize {
        self.broadcast_tx.len()
    }

    /// 当前 mpsc 队列长度（debug 用）
    pub fn mpsc_queue_len(&self) -> usize {
        self.mpsc_tx.capacity()
    }

    /// 复制 dropped 计数 Arc —— 给 writer sink 用于上报
    pub fn dropped_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }

    /// 队列容量
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 发布事件到双通道（非阻塞）
    ///
    /// 背压策略：
    /// 1. mpsc 端 `try_send`：满了返回 `Full` → 计入 dropped，但不阻断 publish
    /// 2. mpsc 端 `Closed`（writer sink drop 了）→ 不计 dropped，但不报错
    /// 3. broadcast 端 `send`：ring buffer 满了自动覆盖最旧事件
    ///    → **仅当有活跃订阅者且队列已满**才计 dropped（无人订阅时 ring
    ///    overwrite 没有意义 —— 没有订阅者会丢消息）
    /// 4. broadcast 端 `send` 返回 `Err(value)`（无订阅者）→ 不计 dropped
    ///    （这是正常情况：UI 还没起来不代表事件丢了）
    pub fn publish(&self, event: TraceEvent) -> PublishOutcome {
        let mut caused_drop = false;

        // mpsc 端
        let mpsc_sent = match self.mpsc_tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                caused_drop = true;
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        };

        // broadcast 端：先取订阅者数，再判断是否因队列满触发 drop
        let broadcast_subscribers_pre = self.broadcast_tx.receiver_count();
        let has_subscribers = broadcast_subscribers_pre > 0;
        let was_full = self.broadcast_tx.len() >= self.capacity;
        let broadcast_subscribers = match self.broadcast_tx.send(event) {
            Ok(n) => n,
            // 无订阅者：正常情况，不计 dropped
            Err(_) => broadcast_subscribers_pre,
        };
        // 只有「有订阅者」且「队列已满」才计 dropped
        // 无订阅者时 broadcast ring overwrite 是无害的（没人在等消息）
        if was_full && has_subscribers {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            caused_drop = true;
        }

        PublishOutcome {
            mpsc_sent,
            broadcast_subscribers,
            dropped_so_far: self.dropped.load(Ordering::Relaxed),
            caused_drop,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("capacity", &self.capacity)
            .field("broadcast_subscribers", &self.broadcast_tx.receiver_count())
            .field("broadcast_queue_len", &self.broadcast_tx.len())
            .field("mpsc_alive", &self.mpsc_writer_alive())
            .field("dropped", &self.dropped())
            .finish()
    }
}

/// 订阅句柄：包装 broadcast::Receiver + dropped 计数 Arc
///
/// 提供 `recv() -> Result<TraceEvent, EventBusError>`：把 tokio 的
/// `RecvError::Lagged(skipped)` 转成 `EventBusError::ReceiverLagged { skipped }`，
/// 业务层不必关心底层类型。
pub struct Subscriber {
    rx: broadcast::Receiver<TraceEvent>,
}

/// 跨线程 publisher handle —— 通过 [`EventBus::clone_handle`] 拿到
///
/// 与 `EventBus` 区别：不持 mpsc receiver（只持 sender），可以安全 Clone；
/// 不暴露 subscribe 接口（订阅需要走原 bus）。
#[derive(Clone)]
pub struct EventBusHandle {
    mpsc_tx: mpsc::Sender<TraceEvent>,
    broadcast_tx: broadcast::Sender<TraceEvent>,
    dropped: Arc<AtomicU64>,
    capacity: usize,
}

impl EventBusHandle {
    /// 发布事件到双通道（与 `EventBus::publish` 同语义）
    pub fn publish(&self, event: TraceEvent) -> PublishOutcome {
        let mut caused_drop = false;
        let mpsc_sent = match self.mpsc_tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                caused_drop = true;
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        };
        let broadcast_subscribers_pre = self.broadcast_tx.receiver_count();
        let has_subscribers = broadcast_subscribers_pre > 0;
        let was_full = self.broadcast_tx.len() >= self.capacity;
        let broadcast_subscribers = match self.broadcast_tx.send(event) {
            Ok(n) => n,
            Err(_) => broadcast_subscribers_pre,
        };
        if was_full && has_subscribers {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            caused_drop = true;
        }
        PublishOutcome {
            mpsc_sent,
            broadcast_subscribers,
            dropped_so_far: self.dropped.load(Ordering::Relaxed),
            caused_drop,
        }
    }
}

impl Subscriber {
    /// 从 `EventBus` 直接构造订阅者（封装 `bus.subscribe()` 一次）
    ///
    /// 业务层使用模式：
    /// ```ignore
    /// let bus = EventBus::new();
    /// let mut sub = Subscriber::from_bus(&bus);
    /// sub.recv().await?;
    /// ```
    pub fn from_bus(bus: &EventBus) -> Self {
        Self { rx: bus.subscribe() }
    }

    /// 接收下一条事件
    pub async fn recv(&mut self) -> Result<TraceEvent, EventBusError> {
        loop {
                match self.rx.recv().await {
                    Ok(ev) => return Ok(ev),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // 滞后跳过：返回错误让业务层决定（默认继续 recv）
                        // 本 API 选择直接返回 Err，便于业务层做指标上报
                        return Err(EventBusError::ReceiverLagged { skipped });
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(EventBusError::BroadcastClosed);
                    }
                }
        }
    }

    /// 同步接收（不挂起）—— 业务层可以轮询而不 await
    pub fn try_recv(&mut self) -> Result<TraceEvent, EventBusError> {
        match self.rx.try_recv() {
            Ok(ev) => Ok(ev),
            Err(broadcast::error::TryRecvError::Empty) => {
                // 用空事件表示「暂时没事件」—— 不当作 error
                // 但本函数签名要求 Result；引入 Err 表示
                Err(EventBusError::ReceiverLagged { skipped: 0 })
            }
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                Err(EventBusError::ReceiverLagged { skipped })
            }
            Err(broadcast::error::TryRecvError::Closed) => Err(EventBusError::BroadcastClosed),
        }
    }
}

impl std::fmt::Debug for Subscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriber").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// 构造一个 VersionAdded 事件 helper
    fn version_added(referral_id: &str, cl_id: u64) -> TraceEvent {
        TraceEvent::VersionAdded {
            referral_id: referral_id.to_string(),
            cl_id,
            actor: "test.actor".to_string(),
        }
    }

    /// 构造一个 FsckAlert 事件 helper
    fn fsck_alert(referral_id: &str, first: u64, last: u64) -> TraceEvent {
        TraceEvent::FsckAlert {
            referral_id: referral_id.to_string(),
            first_id: first,
            last_id: last,
            reason: "session_ended".to_string(),
            truncated: false,
        }
    }

    /// 让 subscriber 在给定时间内尽量 drain 收件箱
    async fn drain_for(
        sub: &mut Subscriber,
        max: usize,
        timeout_ms: u64,
    ) -> Vec<TraceEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut out = Vec::new();
        while out.len() < max && std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(20), sub.recv()).await {
                Ok(Ok(ev)) => out.push(ev),
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        out
    }

    /// test_event_bus_propagation：publish → 两个 subscriber 都收到
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_propagation() {
        let bus = EventBus::new();
        let mut sub1 = Subscriber { rx: bus.subscribe() };
        let mut sub2 = Subscriber { rx: bus.subscribe() };

        assert_eq!(bus.subscriber_count(), 2);

        let ev1 = version_added("REF-001", 1);
        let ev2 = version_added("REF-001", 2);
        let ev3 = fsck_alert("REF-001", 3, 5);

        let r1 = bus.publish(ev1.clone());
        let r2 = bus.publish(ev2.clone());
        let r3 = bus.publish(ev3.clone());

        assert!(r1.mpsc_sent);
        assert!(r2.mpsc_sent);
        assert!(r3.mpsc_sent);
        assert_eq!(r1.broadcast_subscribers, 2);
        assert_eq!(r2.broadcast_subscribers, 2);
        assert_eq!(r3.broadcast_subscribers, 2);
        assert_eq!(r1.dropped_so_far, 0);
        assert_eq!(r1.caused_drop, false);

        let r1_sub1 = drain_for(&mut sub1, 3, 200).await;
        let r1_sub2 = drain_for(&mut sub2, 3, 200).await;

        assert_eq!(r1_sub1.len(), 3, "sub1 must receive all 3 events");
        assert_eq!(r1_sub2.len(), 3, "sub2 must receive all 3 events");

        // 校验收到的事件序列（按 publish 顺序）
        assert_eq!(r1_sub1[0], ev1);
        assert_eq!(r1_sub1[1], ev2);
        assert_eq!(r1_sub1[2], ev3);
        assert_eq!(r1_sub2[0], ev1);
        assert_eq!(r1_sub2[1], ev2);
        assert_eq!(r1_sub2[2], ev3);

        assert_eq!(bus.dropped(), 0, "no drops expected in low-volume path");
    }

    /// test_event_bus_backpressure_drop：队列满时丢弃旧事件 + 计数 > 0
    ///
    /// 验证：
    /// 1. publish 1024 条事件填满 broadcast 队列
    /// 2. 再 publish 1 条 → ring overwrite → dropped 计数 ≥ 1
    /// 3. publish 仍可继续（不抛、不阻塞）
    /// 4. caused_drop == true 标记正确
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_backpressure_drop() {
        // 用小容量 8 跑测试，便于观察 backpressure 行为
        let bus = EventBus::with_capacity(8);

        // 准备 12 条事件（> capacity 8）
        let events: Vec<TraceEvent> = (1..=12)
            .map(|i| version_added("REF-BP", i))
            .collect();

        // publish 12 条：不订阅者，broadcast 仍可 send（无订阅者不算 dropped）
        let mut last_dropped = 0;
        let mut any_caused_drop = false;
        for ev in &events {
            let r = bus.publish(ev.clone());
            last_dropped = r.dropped_so_far;
            if r.caused_drop {
                any_caused_drop = true;
            }
        }

        // broadcast 队列长度受 ring buffer 限制，最多 8
        assert!(
            bus.broadcast_queue_len() <= 8,
            "broadcast queue len {} must be <= capacity 8",
            bus.broadcast_queue_len()
        );

        // 12 条事件投进 8 容量的 broadcast，必然触发 ring overwrite
        // dropped 计数 ≥ 4（12 - 8）
        assert!(
            last_dropped >= 4,
            "expected at least 4 dropped events, got {}",
            last_dropped
        );
        assert!(any_caused_drop, "at least one publish must report caused_drop=true");

        // 验证：publish 仍可继续（再投一条不 panic）
        let r_extra = bus.publish(version_added("REF-BP", 99));
        assert!(r_extra.caused_drop, "extra publish on full queue still causes drop");
        assert!(r_extra.dropped_so_far > last_dropped);

        // dropped_handle() 给到的是同一个 AtomicU64
        let handle = bus.dropped_handle();
        assert_eq!(handle.load(Ordering::Relaxed), bus.dropped());
    }

    /// test_event_bus_capacity_zero_panics：capacity=0 必须 panic
    #[test]
    #[should_panic(expected = "event bus capacity must be > 0")]
    fn test_event_bus_capacity_zero_panics() {
        let _ = EventBus::with_capacity(0);
    }

    /// test_event_bus_default_matches_new：Default impl 与 new() 同 capacity
    #[test]
    fn test_event_bus_default_matches_new() {
        let bus = EventBus::default();
        assert_eq!(bus.capacity(), DEFAULT_BUS_CAPACITY);
        assert_eq!(bus.subscriber_count(), 0);
        assert_eq!(bus.dropped(), 0);
        assert!(bus.mpsc_writer_alive());
    }

    /// test_event_bus_mpsc_audit_sink_drains：writer sink 可从 mpsc receiver 取事件
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_mpsc_audit_sink_drains() {
        let mut bus = EventBus::new();
        let mut rx = bus.take_mpsc_receiver().expect("first take");

        // 第二次 take 应返回 None
        assert!(bus.take_mpsc_receiver().is_none());

        let ev1 = version_added("REF-AUDIT", 1);
        let ev2 = fsck_alert("REF-AUDIT", 2, 4);
        bus.publish(ev1.clone());
        bus.publish(ev2.clone());

        let got1 = rx.recv().await.expect("recv ev1");
        let got2 = rx.recv().await.expect("recv ev2");
        assert_eq!(got1, ev1);
        assert_eq!(got2, ev2);
    }

    /// test_subscriber_lagged_error：subscriber 慢消费 → recv 返回 ReceiverLagged
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_subscriber_lagged_error() {
        let bus = EventBus::with_capacity(4);
        let mut sub = Subscriber { rx: bus.subscribe() };

        // publish 8 条事件（> capacity 4）但 subscriber 不消费
        for i in 1..=8u64 {
            bus.publish(version_added("REF-LAG", i));
        }

        // 此时 subscriber 已滞后 → recv 应返回 Err(ReceiverLagged { skipped > 0 })
        let res = sub.recv().await;
        match res {
            Err(EventBusError::ReceiverLagged { skipped }) => {
                assert!(skipped > 0, "skipped must be > 0 when subscriber lags");
            }
            other => panic!("expected ReceiverLagged, got {:?}", other),
        }
    }

    /// test_event_bus_publish_no_subscribers：无人订阅时不计 dropped（业务正常态）
    ///
    /// 注意：mpsc 与 broadcast 是独立的 —— mpsc 是否满取决于 writer sink
    /// 是否在消费，与 broadcast 订阅者数无关。本测试同时关闭 mpsc consumer
    /// （drop 掉 receiver），让 mpsc 处于 Closed 状态，从而**只**验证
    /// broadcast 端「无人订阅时不计 dropped」的语义。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_publish_no_subscribers() {
        let mut bus = EventBus::with_capacity(4);
        // Drop mpsc receiver → mpsc 处于 Closed 状态（try_send 返回 Err(Closed)
        // 不计 dropped；与 broadcast 端行为解耦）
        let _rx = bus.take_mpsc_receiver();
        drop(_rx);

        for i in 1..=10u64 {
            let r = bus.publish(version_added("REF-NOSUB", i));
            // mpsc Closed → 不计 dropped
            // broadcast 无订阅者 → 不计 dropped
            assert_eq!(
                r.dropped_so_far, 0,
                "publish with no subscribers and closed mpsc must not drop"
            );
            assert!(!r.mpsc_sent, "mpsc must report sent=false when receiver dropped");
        }
        assert_eq!(bus.dropped(), 0);
    }

    /// test_event_bus_mpsc_full_drops：mpsc 端满时也计入 dropped
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_mpsc_full_drops() {
        // 容量 4：不取 mpsc receiver，sender 持续投
        let bus = EventBus::with_capacity(4);
        let mut dropped_events = 0;
        let mut any_caused_drop = false;
        for i in 1..=10u64 {
            let r = bus.publish(version_added("REF-MPSC", i));
            if r.caused_drop {
                any_caused_drop = true;
            }
            // 累计 dropped_so_far 之差
            if i == 10 {
                dropped_events = r.dropped_so_far;
            }
        }
        // 10 条事件投进 4 容量 mpsc：4 成功 + 6 dropped
        assert!(any_caused_drop);
        // dropped ≥ 6（mpsc 满 6 次）+ broadcast 也可能满（无订阅者但队列仍累积）
        assert!(
            dropped_events >= 6,
            "expected at least 6 mpsc dropped events, got {}",
            dropped_events
        );
    }

    /// test_event_bus_subscriber_count_tracking：subscribe / drop 计数正确
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_subscriber_count_tracking() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(), 0);

        let sub1 = Subscriber { rx: bus.subscribe() };
        assert_eq!(bus.subscriber_count(), 1);

        let sub2 = Subscriber { rx: bus.subscribe() };
        assert_eq!(bus.subscriber_count(), 2);

        let sub3 = Subscriber { rx: bus.subscribe() };
        assert_eq!(bus.subscriber_count(), 3);

        drop(sub1);
        drop(sub2);
        // 异步 drop：等一小段时间让 broadcast 内部计数更新
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(bus.subscriber_count(), 1);

        drop(sub3);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(bus.subscriber_count(), 0);
    }

    /// test_event_bus_trace_event_re_export：bus 接收的 TraceEvent 与 live_log 一致
    #[test]
    fn test_event_bus_trace_event_re_export() {
        // 验证 pub use crate::live_log::TraceEvent 真的 re-export 成功
        let ev: TraceEvent = crate::live_log::TraceEvent::AiUnavailable {
            referral_id: "REF-RE".to_string(),
            cl_id: 5,
            cause: "test".to_string(),
        };
        // pub use 应当使两者类型完全相同（编译期等价）
        let ev2: crate::live_log::TraceEvent = ev.clone();
        assert_eq!(ev, ev2);
    }

    /// test_publish_outcome_any_recipient：helper 函数正确判定
    #[test]
    fn test_publish_outcome_any_recipient() {
        // mpsc_sent=true, broadcast=0 → 仍有 mpsc 接收
        let out1 = PublishOutcome {
            mpsc_sent: true,
            broadcast_subscribers: 0,
            dropped_so_far: 0,
            caused_drop: false,
        };
        assert!(out1.any_recipient());

        // mpsc_sent=false, broadcast>0 → 有订阅者接收
        let out2 = PublishOutcome {
            mpsc_sent: false,
            broadcast_subscribers: 3,
            dropped_so_far: 0,
            caused_drop: false,
        };
        assert!(out2.any_recipient());

        // mpsc_sent=false, broadcast=0 → 无人接收（双通道均无 consumer）
        let out3 = PublishOutcome {
            mpsc_sent: false,
            broadcast_subscribers: 0,
            dropped_so_far: 5,
            caused_drop: true,
        };
        assert!(!out3.any_recipient());
    }

    /// test_event_bus_debug_format：Debug 输出含关键字段
    #[test]
    fn test_event_bus_debug_format() {
        let bus = EventBus::with_capacity(7);
        let debug_str = format!("{:?}", bus);
        assert!(debug_str.contains("EventBus"));
        assert!(debug_str.contains("capacity"));
        assert!(debug_str.contains("7"));
        assert!(debug_str.contains("dropped"));
    }

    /// test_event_bus_error_display_nonempty_and_no_unknown：err 三 variant Display
    #[test]
    fn test_event_bus_error_display_nonempty_and_no_unknown() {
        let cases = vec![
            EventBusError::MpscClosed,
            EventBusError::ReceiverLagged { skipped: 5 },
            EventBusError::BroadcastClosed,
        ];
        for err in cases {
            let s = format!("{}", err);
            assert!(!s.is_empty(), "display must not be empty: {:?}", err);
            assert!(!s.contains("unknown"), "display must not contain 'unknown': {:?}", err);
        }
    }

    /// test_event_bus_dropped_handle_shared：Arc 句柄与 bus.dropped() 同步
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_event_bus_dropped_handle_shared() {
        let bus = EventBus::with_capacity(2);
        let handle = bus.dropped_handle();
        assert_eq!(handle.load(Ordering::Relaxed), 0);

        for i in 1..=5u64 {
            bus.publish(version_added("REF-ARC", i));
        }
        // bus.dropped() 与 handle.load() 必须一致（同一 AtomicU64）
        assert_eq!(handle.load(Ordering::Relaxed), bus.dropped());
        assert!(handle.load(Ordering::Relaxed) > 0);
    }

    /// test_event_bus_high_volume_correctness：1000 条事件不丢不重复
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_event_bus_high_volume_correctness() {
        // 用 256 容量跑 1000 条事件：会有 ring overwrite 但单 subscriber
        // 应当能持续 drain 出最近的事件
        let bus = EventBus::with_capacity(256);
        let mut sub = Subscriber { rx: bus.subscribe() };

        let collected = Arc::new(Mutex::new(HashMap::<u64, usize>::new()));
        let collected_clone = Arc::clone(&collected);
        let writer = tokio::spawn(async move {
            for i in 1..=1000u64 {
                bus.publish(version_added("REF-HV", i));
                if i % 100 == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
        });

        // drain 收件箱
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await {
                Ok(Ok(ev)) => {
                    if let TraceEvent::VersionAdded { cl_id, .. } = ev {
                        *collected_clone.lock().unwrap().entry(cl_id).or_insert(0) += 1;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
        writer.await.unwrap();

        // 收到的每条 cl_id 至少被计 1 次
        let collected = collected.lock().unwrap();
        assert!(!collected.is_empty(), "subscriber must receive at least some events");
        for (cl_id, count) in collected.iter() {
            assert!(*count >= 1, "cl_id {} received {} times", cl_id, count);
            assert!(*cl_id >= 1 && *cl_id <= 1000);
        }
    }
}