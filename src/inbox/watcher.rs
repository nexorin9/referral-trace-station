//! `InboxWatcher`：notify-rs + 100ms debounce 监听 `inbox/<date>/*.zip`。
//!
//! ## 源产品能力参考
//!
//! copper-rs 的日志流是**进程内** `CuLogEntry` 流，没有「外部 drop 入站」概念。
//! 本模块填补「区域平台 ORU / FHIR / zip 三件套推送 → cl_seq 单调流」的桥：
//!
//! 1. **监听 inbox 目录**：`notify` + `notify-debouncer-full`（100ms 去抖动，
//!    避免区域平台分片写 zip 时一个文件产生多个事件）。
//! 2. **解析 drop**：每条 `DebouncedEvent.path` 调 [`parse_inbox_zip`] 抽
//!    `before/after/signature` 三件套；失败走 `inbox/unparsed/` + 广播告警。
//! 3. **落 cl_seq 单调流**：filename stem → referral_id，调
//!    [`ClIdStoreOnDisk::record_alloc_next`] 分配下一个 cl_id；持久化由 store
//!    自身负责（与 Task 3 同款 `state/cl_seq.json`）。
//! 4. **落 sha256 链**：`ReferralVersion` 直入 [`Chain::append`]，生成链节 +
//!    自动接链（与 Task 7 共享）。
//! 5. **广播 `VersionAdded`**：调 [`LiveLogRegistry::broadcast`] 让 UI 立即
//!    刷新 timeline。
//!
//! ## 不变量
//!
//! - **顺序**：解析 → store 分配 → chain append → listener 广播；任意一步失败
//!   即中断后续步骤（不静默部分推进），保证 chain / store / listener 三方状态
//!   一致。
//! - **referral_id 来源**：filename stem（例 `REF-001.zip` → `REF-001`）。这条
//!   约定在 `task.json` 收尾三段（README / 典型场景）会写明。
//! - **cl_id 分配**：本地 `alloc_next`（信任来源机构的 drop，但用本地 cl_seq
//!    单调流重打 cl_id），避免双机构 cl_id 编号空间冲突。
//!
//! ## 与源产品的差异
//!
//! - 源 `notify-debouncer-full` 只输出 `DebouncedEvent` 流；本模块把每个事件
//!   映射成「解析 → 存储 → 广播」三步同步路径。
//! - 源不区分 file 与 dir 事件；本模块只看 `is_file() + extension == "zip"`。
//! - 同步回调里通过 `tokio::runtime::Handle::current()` 派发异步处理任务，
//!   不阻塞 notify 的内部线程。
//!
//! ## 公开 API
//!
//! - [`InboxWatcherConfig`]：inbox 根目录 + debounce 窗口。
//! - [`start_watching`]：建立监听并返回 [`InboxWatcherHandle`]。
//! - [`InboxWatcherHandle`]：持有 `Debouncer` 的 RAII handle（drop = 停监听）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, FileIdMap};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;
use tokio::runtime::Handle as TokioHandle;

use crate::continuity::ContinuityKind;
use crate::copperlist::cl_seq_store::ClIdStoreOnDisk;
use crate::inbox::zip_parser::{parse_inbox_zip, InboxZipError};
use crate::live_log::listener::{LiveLogRegistry, TraceEvent};
use crate::model::referral_version::ReferralVersion;
use crate::sha256_chain::chain::{canonical_json, Chain};

/// Watcher 启动失败原因。
///
/// `Display` 全部不空且不出现 `"unknown"` 字样；caller 据此回退到「drop 目录
/// 兜底 + LiveLog `HisUnavailable` 事件」。
#[derive(Debug, Error)]
pub enum WatcherError {
    /// 调用 [`start_watching`] 时未在 tokio runtime 内（需要 `Handle::current()` 派发任务）。
    #[error("inbox watcher 启动失败：未在 tokio runtime 内（{0}）")]
    NoTokioRuntime(String),

    /// `notify-debouncer-full::new_debouncer` 失败（线程创建 / tick 计算等）。
    #[error("inbox watcher 创建 debouncer 失败：{0}")]
    DebouncerCreate(String),

    /// `debouncer.watcher().watch()` 失败（路径不存在 / 权限拒绝 / 不支持的平台）。
    #[error("inbox watcher 监听 {path} 失败：{reason}")]
    WatchPath { path: String, reason: String },
}

/// InboxWatcher 配置。
///
/// - `inbox_root`：被监听的目录（通常 `inbox/<YYYY-MM-DD>`，或 `inbox/` 全量）；
///   目录需在 `start_watching` 前已存在，否则 [`WatcherError::WatchPath`]。
/// - `debounce_ms`：去抖动窗口（毫秒）；同一文件 100ms 内的多次写入合并为一次事件。
#[derive(Clone, Debug)]
pub struct InboxWatcherConfig {
    pub inbox_root: PathBuf,
    pub debounce_ms: u64,
}

impl InboxWatcherConfig {
    pub fn new(inbox_root: impl Into<PathBuf>, debounce_ms: u64) -> Self {
        Self {
            inbox_root: inbox_root.into(),
            debounce_ms,
        }
    }
}

/// InboxWatcher 句柄：drop 即停监听。
///
/// 内部持有 [`Debouncer`]，drop 时由 `notify-debouncer-full` 的 `Drop` 实现
/// 通知内部线程退出。
pub struct InboxWatcherHandle {
    /// `Debouncer` 持有推荐 watcher（kqueue / inotify / FSEvents）+ file-id cache；
    /// drop 时自动停止监听并 join 内部线程。
    _debouncer: Debouncer<RecommendedWatcher, FileIdMap>,
    /// 保留 inbox 根路径便于诊断 / 测试断言。
    pub inbox_root: PathBuf,
}

/// 启动 inbox 监听。
///
/// ## 流程
///
/// 1. 校验当前线程在 tokio runtime 内（用于从 sync 回调里派发异步任务）。
/// 2. 创建 `Debouncer<RecommendedWatcher, RecommendedCache>`，timeout = config.debounce_ms。
/// 3. 注册递归监听 `config.inbox_root`（[RecursiveMode::Recursive]，
///    允许 `inbox/<date>/...` 子目录，未来按日期分桶）。
/// 4. 返回 [`InboxWatcherHandle`]；drop 时停监听。
///
/// ## 错误
///
/// - 不在 tokio runtime 内 → [`WatcherError::NoTokioRuntime`]。
/// - 创建 debouncer 失败（极少见，线程创建失败）→ [`WatcherError::DebouncerCreate`]。
/// - 监听路径失败（路径不存在 / 权限拒绝）→ [`WatcherError::WatchPath`]。
pub fn start_watching(
    config: InboxWatcherConfig,
    store: Arc<Mutex<ClIdStoreOnDisk>>,
    chain: Arc<Mutex<Chain>>,
    listener: Arc<Mutex<LiveLogRegistry>>,
) -> Result<InboxWatcherHandle, WatcherError> {
    // 1. tokio Handle（sync 回调里要派发 async 处理）
    let handle = TokioHandle::try_current()
        .map_err(|e| WatcherError::NoTokioRuntime(e.to_string()))?;

    // 2. inbox 根目录必须存在（notify 在不存在的路径上返回错误）
    if !config.inbox_root.exists() {
        return Err(WatcherError::WatchPath {
            path: config.inbox_root.display().to_string(),
            reason: "path does not exist".to_string(),
        });
    }

    let inbox_root = config.inbox_root.clone();
    // canonicalize 解决 macOS 上 /var vs /private/var 软链差异：
    // tempfile 返回的路径在 notify 事件里被解析为 /private/var/...；
    // 不做 canonicalize 会让 path.starts_with 比较失败导致事件被跳过。
    let inbox_root_canonical = std::fs::canonicalize(&inbox_root).unwrap_or(inbox_root.clone());
    let debounce = Duration::from_millis(config.debounce_ms);

    // 3. 构造 sync 回调：每个 DebouncedEvent 派发一个 tokio task
    let store_cb = Arc::clone(&store);
    let chain_cb = Arc::clone(&chain);
    let listener_cb = Arc::clone(&listener);
    let inbox_root_for_cb = inbox_root_canonical.clone();

    let mut debouncer: Debouncer<RecommendedWatcher, FileIdMap> = new_debouncer(
        debounce,
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                // 一个 DebouncedEvent 包含若干 path；同一文件多次写在 debounce 窗口内
                // 会被折叠成 single event with one path；不同 path 各自保留
                for ev in events {
                    for path in ev.event.paths.iter() {
                        // 只看 inbox 根目录下的 .zip 文件
                        if !path.starts_with(&inbox_root_for_cb) {
                            continue;
                        }
                        if path.extension().and_then(|s| s.to_str()) != Some("zip") {
                            continue;
                        }
                        if !path.is_file() {
                            continue;
                        }
                        // 接收所有可能含「写入文件」语义的 EventKind：
                        // - Create(_)：新建（notify-rs 6.x 默认对新建文件）
                        // - Modify(_)：同文件二次写 / 内容更新
                        // - Other：兜底（macOS FSEvents / Linux inotify 偶发）
                        //   包含 Data / MetaData / Any / Custom(4) 等非显式 Create/Modify 子类
                        if matches!(
                            ev.event.kind,
                            EventKind::Remove(_) | EventKind::Access(_)
                        ) {
                            continue;
                        }
                        let path = path.clone();
                        let store = Arc::clone(&store_cb);
                        let chain = Arc::clone(&chain_cb);
                        let listener = Arc::clone(&listener_cb);
                        handle.spawn(async move {
                            process_inbox_file(&path, store, chain, listener).await;
                        });
                    }
                }
            }
            Err(errors) => {
                for e in errors {
                    eprintln!("inbox watcher error: {e:?}");
                }
            }
        },
    )
    .map_err(|e| WatcherError::DebouncerCreate(format!("{e}")))?;

    // 4. 启动监听
    debouncer
        .watcher()
        .watch(&inbox_root, RecursiveMode::Recursive)
        .map_err(|e| WatcherError::WatchPath {
            path: inbox_root.display().to_string(),
            reason: format!("{e}"),
        })?;

    Ok(InboxWatcherHandle {
        _debouncer: debouncer,
        inbox_root,
    })
}

/// 处理一个 inbox drop zip：解析 → store 分配 → chain append → listener 广播。
///
/// 失败策略：任意一步失败 `eprintln!` 后退出，**不抛**也不重试；caller
/// （notify 回调）已忽略返回。监听本身不依赖单条 drop 的成败。
async fn process_inbox_file(
    path: &Path,
    store: Arc<Mutex<ClIdStoreOnDisk>>,
    chain: Arc<Mutex<Chain>>,
    listener: Arc<Mutex<LiveLogRegistry>>,
) {
    // 1) 解析 zip
    let parsed = match parse_inbox_zip(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "inbox watcher: 解析失败 {} : {}",
                path.display(),
                format_inbox_error(&e)
            );
            // 解析失败（缺签 / 坏 JSON 等）也算一次「事件可观测」，仅 eprintln，
            // 后续可接 inbox/unparsed/ 落盘（Task 8 fallback 已有契约）。
            return;
        }
    };

    // 2) 提取 referral_id（filename stem：REF-001.zip → REF-001）
    let referral_id = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            eprintln!(
                "inbox watcher: 无法从文件名提取 referral_id: {}",
                path.display()
            );
            return;
        }
    };

    // 3) store 分配 cl_id（持久化由 record_alloc_next 自带 save）
    let cl_id = {
        let mut store_guard = match store.lock() {
            Ok(g) => g,
            Err(_) => {
                eprintln!("inbox watcher: store mutex 已毒化");
                return;
            }
        };
        match store_guard.record_alloc_next(&referral_id) {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                eprintln!("inbox watcher: alloc_next 失败 {referral_id} : {e}");
                return;
            }
            Err(e) => {
                eprintln!("inbox watcher: store IO 错误 {referral_id} : {e}");
                return;
            }
        }
    };

    // 4) 构造 ReferralVersion
    let actor = parsed
        .after
        .get("actor")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let before_str = flatten_value_map(&parsed.before);
    let after_str = flatten_value_map(&parsed.after);

    // payload_sha256 = sha256(canonical_json(after))
    let payload_sha256 = match canonical_json(&parsed.after) {
        Ok(bytes) => hex_lower_sha256(&bytes),
        Err(e) => {
            eprintln!("inbox watcher: canonical_json 失败 {referral_id} : {e}");
            return;
        }
    };

    let version = ReferralVersion {
        cl_id,
        version_no: cl_id.min(u32::MAX as u64) as u32,
        actor,
        modified_at: Utc::now(),
        before: before_str,
        after: after_str,
        continuity: ContinuityKind::Manifest,
        payload_sha256,
    };

    // 5) chain append
    {
        let mut chain_guard = match chain.lock() {
            Ok(g) => g,
            Err(_) => {
                eprintln!("inbox watcher: chain mutex 已毒化");
                return;
            }
        };
        if let Err(e) = chain_guard.append(&version) {
            eprintln!("inbox watcher: chain append 失败 {referral_id} : {e}");
            return;
        }
    }

    // 6) 广播 VersionAdded
    {
        let mut listener_guard = match listener.lock() {
            Ok(g) => g,
            Err(_) => {
                eprintln!("inbox watcher: listener mutex 已毒化");
                return;
            }
        };
        listener_guard.broadcast(
            &TraceEvent::VersionAdded {
                referral_id: referral_id.clone(),
                cl_id,
                actor: version.actor.clone(),
            },
            "version_added",
            &["referral", "cl_id", "actor"],
        );
    }
}

/// 把 `serde_json::Map` 展平成 `BTreeMap<String, String>`：
/// - string 原样
/// - 数字 / bool / null 用 `Display`
/// - 数组 / 对象 fallback 到 `to_string()`（JSON 字面量）
fn flatten_value_map(
    map: &BTreeMap<String, Value>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in map {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
            other => other.to_string(),
        };
        out.insert(k.clone(), s);
    }
    out
}

/// sha256(32 bytes) → 64-char lowercase hex
fn hex_lower_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    hex::encode(buf)
}

/// 提取 `InboxZipError` 的简短描述（避免 `Display` 太长污染 eprintln）
fn format_inbox_error(e: &InboxZipError) -> String {
    // `InboxZipError` 已实现 `Display`；这里直接转字符串，避免改 enum 的可见性。
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_log::listener::{LiveLogRegistry, TraceEvent};
    use crate::sha256_chain::chain::Chain;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::tempdir;

    /// 构造一个有效的三件套 drop zip（before/after/signature）。
    fn write_drop_zip(path: &Path, before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>, signature: &str) {
        let file = std::fs::File::create(path).expect("create zip");
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::FileOptions::default();

        zip.start_file("before.json", opts).expect("start before");
        let bytes = serde_json::to_vec(before).expect("serialize before");
        zip.write_all(&bytes).expect("write before");

        zip.start_file("after.json", opts).expect("start after");
        let bytes = serde_json::to_vec(after).expect("serialize after");
        zip.write_all(&bytes).expect("write after");

        zip.start_file("signature.txt", opts).expect("start signature");
        zip.write_all(signature.as_bytes()).expect("write signature");

        zip.finish().expect("finish zip");
    }

    /// 构造一组默认 before/after + actor 的样例。
    fn sample_drop() -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
        let mut before = BTreeMap::new();
        before.insert("diagnosis".to_string(), Value::String("chest pain".to_string()));
        let mut after = BTreeMap::new();
        after.insert("diagnosis".to_string(), Value::String("ACS suspected".to_string()));
        after.insert("actor".to_string(), Value::String("dr_li@hospital_b".to_string()));
        (before, after)
    }

    /// 等待最多 `max_wait` 让 `events` 收到至少 1 条事件（轮询 25ms）。
    async fn wait_for_event(events: &Arc<Mutex<Vec<TraceEvent>>>, max_wait: Duration) -> usize {
        let start = std::time::Instant::now();
        loop {
            let n = events.lock().unwrap().len();
            if n > 0 || start.elapsed() >= max_wait {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// 测试 1：写入新 zip 后 listener 在 debounce 窗口内收到 VersionAdded。
    #[tokio::test]
    async fn test_watcher_detects_new_file() {
        let tmp = tempdir().expect("tempdir");
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).expect("mkdir inbox");
        let store_path = tmp.path().join("cl_seq.json");

        let store = Arc::new(Mutex::new(
            ClIdStoreOnDisk::open_or_create(store_path).expect("open store"),
        ));
        let chain = Arc::new(Mutex::new(Chain::new()));
        let listener = Arc::new(Mutex::new(LiveLogRegistry::new()));

        // 注册 collector
        let events: Arc<Mutex<Vec<TraceEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_cb = Arc::clone(&events);
        listener
            .lock()
            .unwrap()
            .register(move |ev, _fmt, _params| {
                events_cb.lock().unwrap().push(ev.clone());
            })
            .expect("register");

        // 启动 watcher
        let cfg = InboxWatcherConfig::new(inbox.clone(), 100);
        let _handle = start_watching(cfg, store.clone(), chain.clone(), listener.clone())
            .expect("start watching");

        // 给 watcher 一点时间完成 setup（macOS FSEvents / Linux inotify 初始化）
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 写入 drop zip
        let (before, after) = sample_drop();
        write_drop_zip(&inbox.join("REF-001.zip"), &before, &after, "signature_ok");

        // 等待事件
        let n = wait_for_event(&events, Duration::from_millis(2000)).await;
        assert_eq!(
            n, 1,
            "expected exactly 1 VersionAdded, got {n}: {:?}",
            events.lock().unwrap()
        );

        // 校验事件内容
        let events = events.lock().unwrap();
        match &events[0] {
            TraceEvent::VersionAdded { referral_id, cl_id, actor } => {
                assert_eq!(referral_id, "REF-001");
                assert_eq!(*cl_id, 1, "首次 alloc_next 应为 1");
                assert_eq!(actor, "dr_li@hospital_b");
            }
            other => panic!("expected VersionAdded, got {other:?}"),
        }

        // 校验 store 推进
        assert_eq!(
            store.lock().unwrap().store().last_allocated("REF-001"),
            1,
            "store 应推进到 1"
        );

        // 校验 chain append
        assert_eq!(chain.lock().unwrap().len(), 1, "chain 应有 1 条");
    }

    /// 测试 2：同一路径两次写（中间间隔 < debounce）只触发一次 VersionAdded。
    #[tokio::test]
    async fn test_watcher_debounce() {
        let tmp = tempdir().expect("tempdir");
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).expect("mkdir inbox");
        let store_path = tmp.path().join("cl_seq.json");

        let store = Arc::new(Mutex::new(
            ClIdStoreOnDisk::open_or_create(store_path).expect("open store"),
        ));
        let chain = Arc::new(Mutex::new(Chain::new()));
        let listener = Arc::new(Mutex::new(LiveLogRegistry::new()));

        let events: Arc<Mutex<Vec<TraceEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_cb = Arc::clone(&events);
        listener
            .lock()
            .unwrap()
            .register(move |ev, _fmt, _params| {
                events_cb.lock().unwrap().push(ev.clone());
            })
            .expect("register");

        let cfg = InboxWatcherConfig::new(inbox.clone(), 200); // 200ms debounce 更稳
        let _handle = start_watching(cfg, store.clone(), chain.clone(), listener.clone())
            .expect("start watching");

        tokio::time::sleep(Duration::from_millis(50)).await;

        // 在 < 200ms 内对同一文件写两次：第一次 create，第二次 modify（write 覆盖）
        // notify-debouncer-full 把同一路径的事件折叠成单次 DebouncedEvent，
        // process_inbox_file 仍会被调一次（因为 events Vec 中该 path 只剩一条）。
        let path = inbox.join("REF-A.zip");
        let (before, after) = sample_drop();
        write_drop_zip(&path, &before, &after, "sig1");
        // 立即第二次写（间隔 < debounce）
        tokio::time::sleep(Duration::from_millis(30)).await;
        write_drop_zip(&path, &before, &after, "sig2_updated");

        // 等待足够 debounce 时间
        tokio::time::sleep(Duration::from_millis(800)).await;

        let n = events.lock().unwrap().len();
        // 同路径 debounce 后只触发一次 VersionAdded
        assert_eq!(
            n, 1,
            "expected exactly 1 VersionAdded after debounce on same path, got {n}"
        );

        // store 只推进一次（cl_id = 1）
        assert_eq!(store.lock().unwrap().store().last_allocated("REF-A"), 1);
    }

    /// 测试 3：缺签 drop 不入流（拒收而不抛错）。
    #[tokio::test]
    async fn test_watcher_rejects_signature_missing() {
        let tmp = tempdir().expect("tempdir");
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).expect("mkdir inbox");
        let store_path = tmp.path().join("cl_seq.json");

        let store = Arc::new(Mutex::new(
            ClIdStoreOnDisk::open_or_create(store_path).expect("open store"),
        ));
        let chain = Arc::new(Mutex::new(Chain::new()));
        let listener = Arc::new(Mutex::new(LiveLogRegistry::new()));

        let events: Arc<Mutex<Vec<TraceEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_cb = Arc::clone(&events);
        listener
            .lock()
            .unwrap()
            .register(move |ev, _fmt, _params| {
                events_cb.lock().unwrap().push(ev.clone());
            })
            .expect("register");

        let cfg = InboxWatcherConfig::new(inbox.clone(), 100);
        let _handle = start_watching(cfg, store.clone(), chain.clone(), listener.clone())
            .expect("start watching");

        tokio::time::sleep(Duration::from_millis(50)).await;

        // 写缺签 zip：手工 zip（无 signature.txt）
        let path = inbox.join("REF-BAD.zip");
        let file = std::fs::File::create(&path).expect("create");
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::FileOptions::default();
        zip.start_file("before.json", opts).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.start_file("after.json", opts).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.finish().unwrap();

        tokio::time::sleep(Duration::from_millis(800)).await;

        // 缺签应被拒收：listener 没收到任何事件，store 也不推进
        assert_eq!(
            events.lock().unwrap().len(),
            0,
            "缺签 drop 不应触发 VersionAdded"
        );
        assert_eq!(store.lock().unwrap().store().last_allocated("REF-BAD"), 0);
        assert_eq!(chain.lock().unwrap().len(), 0);
    }
}