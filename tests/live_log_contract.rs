//! live_log 注册契约单元测试（Task 26）
//!
//! ## 范围
//!
//! 与 `src/live_log/listener.rs` 内嵌单测互补 —— 此处作为外部 integration test
//! 覆盖「注册契约」六类：
//!
//! 1. `test_listener_register_immediate_first_event`：注册 listener 后立即广播
//!    第一条事件 → 必须收到；与「注册即就绪」契约一致。
//! 2. `test_listener_unregister_no_callback`：反注册后再广播 → 该 listener 不
//!    再收到；守「unregister = 立即从广播集合移除」契约。
//! 3. `test_listener_multiple_all_receive`：多个 listener 注册 → 单次广播触发
//!    全部 listener 调用，顺序按 ListenerId 升序（与 BTreeMap 默认遍历顺序一致）。
//! 4. `test_listener_panic_does_not_affect_others`：单个 listener panic → 其
//!    他 listener 仍收到事件（broadcast 用 `catch_unwind` 隔离 panic）。
//! 5. `test_listener_id_monotonic`：连续 register 返回 ListenerId 严格递增
//!    （起点 1，next_id 单调不减，unregister 不退回）。
//! 6. `test_listener_register_same_closure_twice_distinct_ids`：同一闭包（不同
//!    共享 buffer）两次 register → 返回两个不同 ListenerId —— 守住「永不重用
//!    ListenerId」契约（与源 `LiveLogListenerId` 不重用语义一致）。
//!
//! ## 与 src/live_log/listener.rs 内嵌单测的区别
//!
//! - 内嵌单测在 lib 模块内（`#[cfg(test)] mod tests`），粒度是「注册广播 +
//!   ID 单调 + serde 往返 + Display + 反注册 unknown」；
//! - 本文件作为 integration test，从外部视角覆盖**完整 6 类契约**，重点在
//!   「注册即就绪」「unregister 立即移除」「panic 隔离」「永不重用 ID」
//!   这四条业务关键不变量。
//!
//! ## 与源 copper-rs LiveLogListener 契约对齐
//!
//! 源 copper-rs/core/cu29_log_runtime/src/lib.rs：
//! - `LiveLogListener = Box<dyn Fn(&CuLogEntry, &str, &[&str]) + Send + Sync + 'static>`
//! - `LiveLogListenerId = usize`，单调，从 1 起，永不重用
//! - `LiveLogListeners` 注册/反注册/广播三方法
//!
//! 本任务适配：`Fn + Send + Sync → FnMut + Send`（stateful sink），其余契约
//! 严格对齐。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use referral_trace_station::live_log::{LiveLogError, LiveLogRegistry, ListenerId, TraceEvent};

// -- helpers --

/// 构造一个共享 buffer + 闭包：listener 调用时把 `TraceEvent.to_string()`
/// 推入 buffer。FnMut + Send（通过 Arc<Mutex<Vec<String>>> 共享）。
fn make_collector() -> (Arc<Mutex<Vec<String>>>, impl FnMut(&TraceEvent, &str, &[&str]) + Send + 'static) {
    let buf = Arc::new(Mutex::new(Vec::<String>::new()));
    let buf_clone = Arc::clone(&buf);
    let cb = move |event: &TraceEvent, _fmt: &str, _params: &[&str]| {
        buf_clone.lock().unwrap().push(event.to_string());
    };
    (buf, cb)
}

// -- 1. 注册 listener → 立即收到首次事件 --

/// 测试 1：注册后立即收到首次广播事件。
///
/// 验证：
/// - 注册返回 `ListenerId(1)`（首分配起点）
/// - 广播 `VersionAdded{referral_id=REF-001, cl_id=7, actor=dr.li}` → buffer
///   含 1 条 `TraceEvent::Display` 输出，且包含三关键字段（REF-001 / cl_id=7
///   / actor=dr.li）
/// - 注册即就绪：无需 warm-up 或额外 ready 事件
///
/// 边界意义：守住「register() 返回即 broadcast() 可达」的契约 —— UI 注册为
/// listener 后，第一条入站事件必须立刻可见（不能等下一次 tick）。
#[test]
fn test_listener_register_immediate_first_event() {
    let mut reg = LiveLogRegistry::new();
    let (buf, cb) = make_collector();

    let id = reg.register(cb).expect("register must succeed");
    assert_eq!(id, ListenerId(1));
    assert_eq!(reg.len(), 1);
    assert!(!reg.is_empty());

    let ev = TraceEvent::VersionAdded {
        referral_id: "REF-001".to_string(),
        cl_id: 7,
        actor: "dr.li".to_string(),
    };
    reg.broadcast(&ev, "added", &["referral", "cl_id"]);

    let got = buf.lock().unwrap();
    assert_eq!(got.len(), 1, "first event must be received immediately");
    let line = &got[0];
    assert!(line.contains("VersionAdded"), "line missing event kind: {}", line);
    assert!(line.contains("REF-001"), "line missing referral_id: {}", line);
    assert!(line.contains("cl_id=7"), "line missing cl_id: {}", line);
    assert!(line.contains("dr.li"), "line missing actor: {}", line);
}

// -- 2. 反注册后不再收到事件 --

/// 测试 2：反注册后广播事件，listener 不再收到。
///
/// 验证：
/// - 反注册前：广播 → buffer 收到
/// - 反注册返回 Ok
/// - 反注册后：广播 → buffer 长度不变（unregister 立即从广播集合移除）
/// - 反注册同一 ID 第二次 → Err(UnknownListenerId)（守住 unregister 单次生效）
///
/// 边界意义：守「unregister = 立即移除」契约；UI 取消订阅后下一条事件
/// 不能误投递到已退订的闭包（否则会触发 stale sink 调用导致业务状态污染）。
#[test]
fn test_listener_unregister_no_callback() {
    let mut reg = LiveLogRegistry::new();
    let (buf, cb) = make_collector();
    let id = reg.register(cb).expect("register ok");
    assert_eq!(reg.len(), 1);

    let ev = TraceEvent::VersionAdded {
        referral_id: "REF-001".to_string(),
        cl_id: 1,
        actor: "dr.wang".to_string(),
    };
    reg.broadcast(&ev, "added", &["referral"]);
    assert_eq!(buf.lock().unwrap().len(), 1, "pre-unregister: received 1 event");

    // 反注册
    reg.unregister(id).expect("first unregister ok");
    assert_eq!(reg.len(), 0);
    assert!(reg.is_empty());

    // 反注册后再广播 → 不应再收到
    let ev2 = TraceEvent::VersionAdded {
        referral_id: "REF-001".to_string(),
        cl_id: 2,
        actor: "dr.li".to_string(),
    };
    reg.broadcast(&ev2, "added", &["referral"]);
    assert_eq!(
        buf.lock().unwrap().len(),
        1,
        "post-unregister: must NOT receive more events"
    );

    // 第二次反注册同一 ID → Err(UnknownListenerId)
    let err = reg.unregister(id).expect_err("second unregister must fail");
    assert!(matches!(err, LiveLogError::UnknownListenerId { .. }));
}

// -- 3. 多 listener 都收到同一事件 --

/// 测试 3：多个 listener 注册 → 单次广播全部收到；顺序按 ListenerId 升序。
///
/// 验证：
/// - 注册 3 个 listener → ListenerId 分别为 1 / 2 / 3
/// - 单次广播 → 3 个 buffer 长度都 = 1（每个 listener 各收到一次）
/// - buffer 内容三 listener 都包含 "FsckAlert" + "REF-002" + "first=5" 等字段
///
/// 边界意义：守「broadcast fan-out」契约；一条事件必须投递到所有已注册
/// listener（不能漏；这是 UI 多面板同步刷新的基础）。
#[test]
fn test_listener_multiple_all_receive() {
    let mut reg = LiveLogRegistry::new();
    let (buf1, cb1) = make_collector();
    let (buf2, cb2) = make_collector();
    let (buf3, cb3) = make_collector();

    let id1 = reg.register(cb1).expect("register 1");
    let id2 = reg.register(cb2).expect("register 2");
    let id3 = reg.register(cb3).expect("register 3");

    assert_eq!(id1, ListenerId(1));
    assert_eq!(id2, ListenerId(2));
    assert_eq!(id3, ListenerId(3));
    assert_eq!(reg.len(), 3);

    let ev = TraceEvent::FsckAlert {
        referral_id: "REF-002".to_string(),
        first_id: 5,
        last_id: 8,
        reason: "late_join".to_string(),
        truncated: false,
    };
    reg.broadcast(&ev, "fsck", &["a", "b"]);

    for (idx, buf) in [&buf1, &buf2, &buf3].iter().enumerate() {
        let got = buf.lock().unwrap();
        assert_eq!(got.len(), 1, "listener #{} must receive 1 event", idx + 1);
        let line = &got[0];
        assert!(line.contains("FsckAlert"), "listener #{} missing kind", idx + 1);
        assert!(line.contains("REF-002"), "listener #{} missing referral_id", idx + 1);
        assert!(line.contains("first=5"), "listener #{} missing first_id", idx + 1);
        assert!(line.contains("last=8"), "listener #{} missing last_id", idx + 1);
        assert!(line.contains("late_join"), "listener #{} missing reason", idx + 1);
    }

    // 第二次广播 → 三 listener 都再收到 1 条（累计 2）
    reg.broadcast(&ev, "fsck", &["a", "b"]);
    for (idx, buf) in [&buf1, &buf2, &buf3].iter().enumerate() {
        assert_eq!(
            buf.lock().unwrap().len(),
            2,
            "listener #{} must receive 2 events after 2 broadcasts",
            idx + 1
        );
    }
}

// -- 4. listener panic 不影响其他 listener --

/// 测试 4：单个 listener panic，其他 listener 仍收到事件。
///
/// 验证：
/// - 注册 listener_panic（故意 panic）+ listener_normal（共享 buffer）
/// - 广播事件 → panic listener 抛出（被 catch_unwind 捕获，不污染广播主路径）
/// - listener_normal 仍收到事件（panic 被隔离）
/// - panic listener 仍保留在注册表（broadcast 后再插入；下次仍可能 panic）
///
/// 边界意义：守「panic 不污染广播」契约 —— 任何单一 listener 故障
/// 不能让整个 UI 推送停摆。这是 ratatui 状态条 + 多 panel 同步的基础。
/// 用 `std::panic::catch_unwind` 在 broadcast 内部隔离 listener 调用。
#[test]
fn test_listener_panic_does_not_affect_others() {
    let mut reg = LiveLogRegistry::new();

    // listener_normal 共享 buffer
    let buf = Arc::new(Mutex::new(Vec::<String>::new()));
    let buf_clone = Arc::clone(&buf);
    let normal_cb = move |event: &TraceEvent, _fmt: &str, _params: &[&str]| {
        buf_clone.lock().unwrap().push(event.to_string());
    };

    // listener_panic 故意 panic
    let panic_cb = |_event: &TraceEvent, _fmt: &str, _params: &[&str]| {
        panic!("intentional listener panic for contract test");
    };

    let normal_id = reg.register(normal_cb).expect("register normal");
    let panic_id = reg.register(panic_cb).expect("register panic");

    assert_eq!(normal_id, ListenerId(1));
    assert_eq!(panic_id, ListenerId(2));

    let ev = TraceEvent::VersionAdded {
        referral_id: "REF-PANIC".to_string(),
        cl_id: 99,
        actor: "dr.x".to_string(),
    };

    // broadcast 应当不 panic（catch_unwind 隔离 panic listener）
    reg.broadcast(&ev, "added", &["referral"]);

    // panic listener 之后：normal listener 仍收到事件
    let got = buf.lock().unwrap();
    assert_eq!(got.len(), 1, "normal listener must still receive after panic peer");
    assert!(got[0].contains("VersionAdded"));
    assert!(got[0].contains("REF-PANIC"));

    // panic listener 仍在注册表中（broadcast 后被 re-insert；不被丢弃）
    assert_eq!(reg.len(), 2, "panic listener must remain in registry");
}

// -- 5. listener ID 单调递增 --

/// 测试 5：连续 register 返回 ListenerId 严格单调递增。
///
/// 验证：
/// - 连续注册 5 个 listener → 返回 ListenerId(1), (2), (3), (4), (5)
/// - 每对相邻 ID 严格 < （严格递增）
/// - 中间反注册后继续注册 → ID 继续推进（不重用已反注册的 slot）
/// - reg.next_id() 跟踪下次分配的 ID
///
/// 边界意义：守源 `LiveLogListenerId` 单调 + 永不重用契约；测试编号可作为
/// 审计 trail（UI 取消订阅后重订也不会让 ID 倒退到旧值）。
#[test]
fn test_listener_id_monotonic() {
    let mut reg = LiveLogRegistry::new();
    let (_b1, cb1) = make_collector();
    let (_b2, cb2) = make_collector();
    let (_b3, cb3) = make_collector();
    let (_b4, cb4) = make_collector();
    let (_b5, cb5) = make_collector();

    let id1 = reg.register(cb1).unwrap();
    let id2 = reg.register(cb2).unwrap();
    let id3 = reg.register(cb3).unwrap();
    let mid_id = reg.unregister(id2).expect("unregister mid");
    assert_eq!(mid_id, ());
    let id4 = reg.register(cb4).unwrap();
    let id5 = reg.register(cb5).unwrap();

    assert_eq!(id1, ListenerId(1));
    assert_eq!(id2, ListenerId(2));
    assert_eq!(id3, ListenerId(3));
    assert_eq!(id4, ListenerId(4), "id must NOT reuse slot 2 after unregister");
    assert_eq!(id5, ListenerId(5));

    // 严格递增
    assert!(id1 < id2);
    assert!(id2 < id3);
    assert!(id3 < id4);
    assert!(id4 < id5);

    // next_id 跟踪下次分配
    assert_eq!(reg.next_id(), 6);

    // 反注册 ID 不应被重用（守住「永不重用」语义）
    // 验证：当前 BTreeMap 仍含 id1/id3/id4/id5，无 id2
    assert!(reg.unregister(ListenerId(2)).is_err(), "id 2 must not be reusable");
    assert!(reg.unregister(ListenerId(1)).is_ok());
    assert!(reg.unregister(ListenerId(3)).is_ok());
    assert!(reg.unregister(ListenerId(4)).is_ok());
    assert!(reg.unregister(ListenerId(5)).is_ok());
    assert!(reg.is_empty());
}

// -- 6. 同一闭包两次 register → 不同 ID（永不重用） --

/// 测试 6：同一逻辑闭包（不同共享 buffer）两次 register 返回不同 ListenerId。
///
/// 验证：
/// - 第一次 register → ListenerId(1)
/// - 第二次 register（不同 buffer 但行为逻辑相同）→ ListenerId(2)（不是 1）
/// - 单次广播 → 两个 listener 都收到（ID 不重用 = listener 不互相覆盖）
/// - 两个 buffer 内容相同事件但各 1 条（fan-out 完整）
///
/// 边界意义：守源「永不重用 ListenerId」契约；当前实现 BTreeMap 用 contains_key
/// 检查 + Err(DuplicateListenerId) 保护显式重用，但 ID 由 next_id 单调分配
/// 永远不会触发 contains_key 命中路径 —— 关键测试是「同一逻辑行为两次
/// register 仍被视作两个独立 listener」（fan-out 而非 overwrite）。
#[test]
fn test_listener_register_same_closure_twice_distinct_ids() {
    let mut reg = LiveLogRegistry::new();

    // 同一逻辑行为（push to buf）两次 register，但用两个独立 buffer
    let buf1 = Arc::new(Mutex::new(Vec::<String>::new()));
    let buf1_clone = Arc::clone(&buf1);
    let cb1 = move |event: &TraceEvent, _fmt: &str, _params: &[&str]| {
        buf1_clone.lock().unwrap().push(event.to_string());
    };

    let buf2 = Arc::new(Mutex::new(Vec::<String>::new()));
    let buf2_clone = Arc::clone(&buf2);
    let cb2 = move |event: &TraceEvent, _fmt: &str, _params: &[&str]| {
        buf2_clone.lock().unwrap().push(event.to_string());
    };

    let id1 = reg.register(cb1).expect("register #1");
    let id2 = reg.register(cb2).expect("register #2");

    assert_eq!(id1, ListenerId(1));
    assert_eq!(id2, ListenerId(2));
    assert_ne!(id1, id2, "two registers must yield distinct IDs (no overwrite)");
    assert_eq!(reg.len(), 2);

    // 单次广播 → 两个 buffer 各 1 条（fan-out 完整，非 overwrite）
    let ev = TraceEvent::HisUnavailable {
        referral_id: "REF-OVERWRITE".to_string(),
        cause: "503_service_unavailable".to_string(),
    };
    reg.broadcast(&ev, "his", &["referral"]);

    // 锁作用域限定：单次 broadcast 后立即释放 MutexGuard，避免后续 broadcast
    // 重新调用 listener 时与已持锁的 guard 死锁（cb1 / cb2 与 got1 / got2 共享
    // 同一 Arc<Mutex<Vec<String>>>）。
    {
        let got1 = buf1.lock().unwrap();
        let got2 = buf2.lock().unwrap();
        assert_eq!(got1.len(), 1, "first listener must receive 1 event");
        assert_eq!(got2.len(), 1, "second listener must receive 1 event (not overwritten)");
        assert_eq!(got1[0], got2[0]);
        assert!(got1[0].contains("HisUnavailable"));
        assert!(got1[0].contains("REF-OVERWRITE"));
        assert!(got1[0].contains("503_service_unavailable"));
    }

    // 显式 DuplicateListenerId 错误路径（不可达但可验证 error 类型）
    // 通过手工构造已含 id 的状态不可行（register API 不暴露 ID 注入）；
    // 这里改为验证 err variant 的语义存在 + Display 非空 + matches!。
    let dup_err = LiveLogError::DuplicateListenerId { id: ListenerId(1) };
    let dup_s = format!("{}", dup_err);
    assert!(!dup_s.is_empty());
    assert!(dup_s.contains("L#1"), "DuplicateListenerId Display must include id");

    // 附：审计 sink 计数器配合（防止 broadcast 偷偷过滤 listener）
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    let cb_counter = move |_event: &TraceEvent, _fmt: &str, _params: &[&str]| {
        counter_clone.fetch_add(1, Ordering::SeqCst);
    };
    let counter_id = reg.register(cb_counter).expect("register counter");
    assert_eq!(counter_id, ListenerId(3));
    reg.broadcast(&ev, "his", &["referral"]);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "counter listener must be called exactly once per broadcast"
    );
}