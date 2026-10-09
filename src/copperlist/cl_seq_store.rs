//! `ClIdStoreOnDisk`：把 [`ClIdStore`] 落盘到 `state/cl_seq.json`，重启可续读。
//!
//! 落盘策略：
//! - **原子写**：先写 `<path>.tmp` 再 rename 到 `<path>`，避免进程中途崩溃留下残缺文件。
//! - **每次 mutation 后落盘**：`alloc_next` / `observe` 走 `&mut self`，落盘由
//!   [`record_alloc_next`](Self::record_alloc_next) 与
//!   [`record_observe`](Self::record_observe) 触发；调用方拿不到「未落盘」的中间状态。
//!
//! 重排保护：
//! - 启动时调用 [`open_or_create`](Self::open_or_create) 加载盘上数据；
//! - 一旦进程内 [`ClIdStore`] 把某个 referral 推到 `last_allocated = N`，
//!   重启后从盘上读到相同 referral 但 `last_allocated = M`（M ≥ N）时，
//!   直接以盘上值为准（merge 时严格大者赢）；
//! - 重启后**仍能拒绝**「声称 id ≤ 盘上 last_allocated」的事件（观察端
//!   [`ClIdStore::observe`] 守门，与 [`crate::copperlist::cl_seq`] 同款契约）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::cl_seq::{ClIdError, ClIdStore};

/// 单调计数器的落盘包装。
///
/// `path` 通常指向 `state/cl_seq.json`（与 .env.example 的 `RTS_STATE_DIR` 一致）。
#[derive(Debug)]
pub struct ClIdStoreOnDisk {
    path: PathBuf,
    store: ClIdStore,
}

impl ClIdStoreOnDisk {
    /// 打开一个既存的 `cl_seq.json`；文件不存在返回 [`io::ErrorKind::NotFound`]。
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let bytes = fs::read(&path)?;
        let store: ClIdStore = serde_json::from_slice(&bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cl_seq.json 反序列化失败：{e}"),
            )
        })?;
        Ok(Self { path, store })
    }

    /// 打开或创建：文件不存在则新建空 store；存在则加载。
    ///
    /// 注意：「新建空 store」**不会**主动写文件 —— 等到首次 `record_*` 调用才会落盘。
    pub fn open_or_create(path: PathBuf) -> io::Result<Self> {
        if path.exists() {
            Self::open(path)
        } else {
            Ok(Self {
                path,
                store: ClIdStore::new(),
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn store(&self) -> &ClIdStore {
        &self.store
    }

    /// 可变借用内部 store；不负责落盘（落盘请走 [`record_alloc_next`] /
    /// [`record_observe`] 或显式 [`save`]）。
    pub fn store_mut(&mut self) -> &mut ClIdStore {
        &mut self.store
    }

    /// 把当前 store 内容写到 `<path>.tmp` 然后 rename 到 `<path>`。
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.store).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("cl_seq 序列化失败：{e}"))
        })?;
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 出站分配 + 立即落盘；任何 I/O 错误都会冒泡。
    pub fn record_alloc_next(&mut self, referral_id: &str) -> io::Result<Result<u64, ClIdError>> {
        let outcome = self.store.alloc_next(referral_id);
        if outcome.is_ok() {
            self.save()?;
        }
        Ok(outcome)
    }

    /// 入站观察 + 立即落盘；任何 I/O 错误都会冒泡。
    pub fn record_observe(
        &mut self,
        referral_id: &str,
        proposed: u64,
    ) -> io::Result<Result<u64, ClIdError>> {
        let outcome = self.store.observe(referral_id, proposed);
        if outcome.is_ok() {
            self.save()?;
        }
        Ok(outcome)
    }

    /// 消费 self，回收内部 store。
    pub fn into_inner(self) -> ClIdStore {
        self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persistence_save_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cl_seq.json");

        let mut disk = ClIdStoreOnDisk::open_or_create(path.clone()).unwrap();
        assert_eq!(disk.record_alloc_next("REF-001").unwrap().unwrap(), 1);
        assert_eq!(disk.record_alloc_next("REF-001").unwrap().unwrap(), 2);
        assert_eq!(disk.record_alloc_next("REF-002").unwrap().unwrap(), 1);
        assert_eq!(disk.record_observe("REF-001", 10).unwrap().unwrap(), 10);

        // 重启：另一个实例打开同一文件，能续读到相同状态
        let reloaded = ClIdStoreOnDisk::open(path).unwrap();
        assert_eq!(reloaded.store().last_allocated("REF-001"), 10);
        assert_eq!(reloaded.store().last_allocated("REF-002"), 1);
    }

    #[test]
    fn test_persistence_reorder_after_reload() {
        // 模拟进程崩溃：盘上 last_allocated(REF-A) = 7；重启后 inbound event 声称 5 → 拒收。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cl_seq.json");

        let mut first = ClIdStoreOnDisk::open_or_create(path.clone()).unwrap();
        for _ in 0..7 {
            first.record_alloc_next("REF-A").unwrap().unwrap();
        }
        drop(first);

        let mut second = ClIdStoreOnDisk::open(path).unwrap();
        assert_eq!(second.store().last_allocated("REF-A"), 7);
        let err = second.record_observe("REF-A", 5).unwrap().unwrap_err();
        match err {
            ClIdError::ReorderOrDuplicate {
                referral_id,
                last_allocated,
                proposed,
            } => {
                assert_eq!(referral_id, "REF-A");
                assert_eq!(last_allocated, 7);
                assert_eq!(proposed, 5);
            }
        }
        // 拒收后落盘不应发生；下次仍能从盘上读到 last_allocated = 7
        let third = ClIdStoreOnDisk::open(second.path().to_path_buf()).unwrap();
        assert_eq!(third.store().last_allocated("REF-A"), 7);
    }

    #[test]
    fn test_persistence_open_or_create_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not_exist_yet.json");
        let disk = ClIdStoreOnDisk::open_or_create(path.clone()).unwrap();
        assert!(disk.store().is_empty());
        // 尚未 save()，文件不应存在
        assert!(!path.exists());

        // 触发首次 mutation 应建文件
        let mut disk = disk;
        disk.record_alloc_next("REF-X").unwrap().unwrap();
        assert!(path.exists());
    }
}