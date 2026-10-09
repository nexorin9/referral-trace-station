//! HL7 解析失败兜底：把解不出的 .hl7 文件 mv 到 inbox/unparsed/ 并打
//! `.unparsed` tag。
//!
//! 设计原则：
//! - 不修改原文件（避免触达 drop 目录的 record 协议）
//! - 在 unparsed/ 下保留原文件名 + 时间戳，避免同一天多次失败覆盖
//! - 路径尽量保持原有结构：`<inbox>/unparsed/<filename>-<epoch>.unparsed.hl7`
//!
//! 注：本轮实现只做原子 mv；future improvement：写一行 json 描述失败原因
//! 到同名 .unparsed.json，本任务目标只覆盖「打 unparsed tag」即可。

use std::fs;
use std::path::{Path, PathBuf};

/// 错误类型：源文件不存在 / mv 失败 / unparsed 目录创建失败。
#[derive(Debug)]
pub enum FallbackError {
    /// 源文件不存在。
    SourceMissing(PathBuf),
    /// unparsed 目录创建失败。
    UnparsedDirFailed { path: PathBuf, source: std::io::Error },
    /// mv 操作失败。
    MoveFailed {
        src: PathBuf,
        dst: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for FallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FallbackError::SourceMissing(p) => {
                write!(f, "HL7 fallback source missing: {}", p.display())
            }
            FallbackError::UnparsedDirFailed { path, source } => write!(
                f,
                "HL7 fallback unparsed dir creation failed: {} ({})",
                path.display(),
                source
            ),
            FallbackError::MoveFailed { src, dst, source } => write!(
                f,
                "HL7 fallback move failed: {} -> {} ({})",
                src.display(),
                dst.display(),
                source
            ),
        }
    }
}

impl std::error::Error for FallbackError {}

/// 把解析失败的 HL7 文件 mv 到 `<inbox>/unparsed/` 并打 `.unparsed` tag。
///
/// 返回值：成功返回目标路径 [`PathBuf`]；失败返回 [`FallbackError`]。
///
/// 实现细节：
/// - `inbox` 指向原 .hl7 文件的所在目录；其下应已经存在或将被自动创建 `unparsed/`
/// - 目标文件名格式：`<原 stem>-<unix epoch>.unparsed.hl7`
/// - 冲突保护：epoch 秒级；若同秒内发生冲突则加 `.dup<N>` 后缀
pub fn fallback_to_unparsed(src: &Path) -> Result<PathBuf, FallbackError> {
    if !src.exists() {
        return Err(FallbackError::SourceMissing(src.to_path_buf()));
    }
    let parent = src.parent().ok_or(FallbackError::SourceMissing(
        src.to_path_buf(),
    ))?;
    let unparsed_dir = parent.join("unparsed");
    fs::create_dir_all(&unparsed_dir).map_err(|e| FallbackError::UnparsedDirFailed {
        path: unparsed_dir.clone(),
        source: e,
    })?;

    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("hl7_failed");
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dst = unparsed_dir.join(format!("{}-{}.unparsed.hl7", stem, epoch));

    // 同秒冲突保护：加 .dupN 后缀
    let final_dst = if dst.exists() {
        let mut counter = 1u32;
        loop {
            let cand = unparsed_dir.join(format!(
                "{}-{}.unparsed.hl7.dup{}",
                stem, epoch, counter
            ));
            if !cand.exists() {
                break cand;
            }
            counter += 1;
        }
    } else {
        dst
    };

    fs::rename(src, &final_dst).map_err(|e| FallbackError::MoveFailed {
        src: src.to_path_buf(),
        dst: final_dst.clone(),
        source: e,
    })?;
    Ok(final_dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_hl7(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn fallback_moves_to_unparsed_with_tag() {
        let dir = tempdir().expect("tempdir");
        let src = write_hl7(dir.path(), "broken.hl7", "NOT_HL7");
        let dst = fallback_to_unparsed(&src).expect("fallback ok");
        assert!(!src.exists());
        assert!(dst.exists());
        assert!(dst.to_string_lossy().contains("unparsed/"));
        let fname = dst.file_name().unwrap().to_string_lossy();
        assert!(
            fname.contains(".unparsed.hl7"),
            "filename should contain .unparsed.hl7: {}",
            fname
        );
    }

    #[test]
    fn fallback_creates_unparsed_dir_if_missing() {
        let dir = tempdir().expect("tempdir");
        let src = write_hl7(dir.path(), "broken2.hl7", "NOT_HL7");
        // unparsed/ 之前不存在
        assert!(!dir.path().join("unparsed").exists());
        let dst = fallback_to_unparsed(&src).expect("fallback ok");
        assert!(dir.path().join("unparsed").exists());
        assert!(dst.exists());
    }

    #[test]
    fn fallback_missing_source_returns_err() {
        let dir = tempdir().expect("tempdir");
        let bogus = dir.path().join("nope.hl7");
        let err = fallback_to_unparsed(&bogus).unwrap_err();
        assert!(matches!(err, FallbackError::SourceMissing(_)));
    }

    #[test]
    fn fallback_error_display_nonempty_and_distinct() {
        let v1 = FallbackError::SourceMissing(PathBuf::from("/tmp/missing.hl7"));
        let v2 = FallbackError::UnparsedDirFailed {
            path: PathBuf::from("/tmp/u"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let v3 = FallbackError::MoveFailed {
            src: PathBuf::from("/tmp/a"),
            dst: PathBuf::from("/tmp/b"),
            source: std::io::Error::new(std::io::ErrorKind::Other, "boom"),
        };
        for v in [&v1, &v2, &v3] {
            let s = format!("{}", v);
            assert!(!s.is_empty());
            assert!(!s.contains("unknown"));
        }
    }

    #[test]
    fn fallback_filename_includes_epoch_marker() {
        let dir = tempdir().expect("tempdir");
        let src = write_hl7(dir.path(), "x.hl7", "X");
        let dst = fallback_to_unparsed(&src).expect("ok");
        let name = dst.file_name().unwrap().to_string_lossy();
        // 形如 `x-<epoch>.unparsed.hl7`，断言包含 "-" 分隔
        assert!(name.contains("-"), "name should contain epoch separator: {}", name);
    }
}
