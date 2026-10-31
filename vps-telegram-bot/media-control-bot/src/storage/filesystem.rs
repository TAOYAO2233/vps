//! 文件系统工具函数。
//!
//! 提供文件大小格式化、安全删除、concat list 写入、任务级临时工作目录守卫，
//! 以及权限受限（Unix `0600`）的原子文件写入工具。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::storage::path::{path_occupied, PathGuard};
use crate::errors::AppError;

/// Reject files that were recently modified or change during a short observation window.
pub async fn ensure_stable_files(paths: &[PathBuf], quiet_secs: u64) -> Result<()> {
    if quiet_secs == 0 || paths.is_empty() {
        return Ok(());
    }
    let mut before = Vec::with_capacity(paths.len());
    for path in paths {
        let meta = std::fs::metadata(path)?;
        let modified = meta.modified()?;
        if meta.len() == 0
            || std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                .as_secs() < quiet_secs
        {
            return Err(AppError::invalid_argument(format!(
                "文件仍在写入或刚结束录制: {}",
                path.display()
            ))
            .into());
        }
        before.push((meta.len(), modified));
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    for (path, (size, modified)) in paths.iter().zip(before) {
        let meta = std::fs::metadata(path)?;
        if meta.len() != size || meta.modified()? != modified {
            return Err(AppError::SourceFileChanged {
                filename: path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string(),
            }
            .into());
        }
    }
    Ok(())
}

/// 格式化文件大小为人类可读字符串（小于 1GB 显示 MB，否则显示 GB）。
#[must_use]
pub fn format_file_size(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => format_file_size_bytes(m.len()),
        Err(_) => "0.00MB".to_string(),
    }
}

/// 格式化字节数为人类可读字符串（小于 1GB 显示 MB，否则显示 GB）。
#[must_use]
pub fn format_file_size_bytes(size_bytes: u64) -> String {
    let size_mb = size_bytes as f64 / (1024.0 * 1024.0);
    if size_mb >= 1024.0 {
        let size_gb = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        format!("{size_gb:.2}GB")
    } else {
        format!("{size_mb:.2}MB")
    }
}

/// 安全删除由本任务创建的单个临时文件（若不存在则忽略；绝不删除目录或跟随软链删目标）。
pub fn remove_if_exists(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_file() || meta.file_type().is_symlink() {
            if let Err(e) = std::fs::remove_file(path) {
                warn!(path = ?path, error = %e, "Failed to remove temporary file");
            }
        }
    }
}

/// 写入 FFmpeg concat list 文件（按 ffconcat 语法转义单引号）。
///
/// # Errors
///
/// 若文件创建或写入失败，返回错误。
pub fn write_concat_list(list_file: &Path, file_paths: &[PathBuf]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(list_file)
        .with_context(|| format!("Failed to create concat list: {}", list_file.display()))?;

    for path in file_paths {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Concat input path is not valid UTF-8: {}", path.display()))?;
        if path_str.contains('\n') || path_str.contains('\r') {
            anyhow::bail!("Concat input path contains a line break: {}", path.display());
        }
        // In ffconcat, backslashes are literal inside quotes. Close the quote,
        // escape the apostrophe outside it, then reopen the quote.
        let escaped = path_str.replace('\'', "'\\''");
        writeln!(file, "file '{escaped}'")?;
    }
    file.flush()?;
    Ok(())
}

/// 任务独占临时目录守卫（RAII）。
///
/// 在目标媒体目录（位于 `/storage512`）下创建独立的隐藏临时子目录，
/// 避免占用根分区 `/tmp`；无论任务正常结束、报错返回、被 `/stop` 取消或发生 panic，
/// [`Drop`] 时都会自动清理该临时子目录及其内部的本任务临时文件，绝不触碰外部已有文件。
#[derive(Debug)]
pub struct TempWorkDir {
    path: PathBuf,
    cleaned: bool,
}

impl TempWorkDir {
    /// 在已校验位于 `BASE_DIR` 内的 `parent_dir` 下创建唯一临时子目录。
    pub fn create_in(guard: &PathGuard, parent_dir: &Path, prefix: &str) -> Result<Self> {
        let canonical_parent = guard.assert_inside(parent_dir)?;
        let pid = std::process::id();
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for seq in 0..100u32 {
            let dir_name = format!(".mcb_tmp_{prefix}_{pid}_{now_ns}_{seq}");
            let candidate = canonical_parent.join(&dir_name);
            if path_occupied(&candidate) {
                continue;
            }
            match std::fs::create_dir(&candidate) {
                Ok(()) => {
                    return Ok(Self {
                        path: candidate,
                        cleaned: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "Failed to create task temporary directory in {}",
                            canonical_parent.display()
                        )
                    });
                }
            }
        }

        anyhow::bail!("Failed to allocate unique temporary work directory")
    }

    /// 返回临时目录路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 主动清理临时目录。
    pub fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&self.path) {
                    warn!(temp_dir = ?self.path, error = %e, "Failed to clean up temporary directory");
                }
            }
        }
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// 以原子方式写入仅服务账号可读（Unix 下 `0600`）的状态文件。
///
/// 先在同一父目录下以 `create_new` 创建权限受限的临时文件，写完 `sync_all` 后原子 `rename` 到目标路径。
pub fn write_private_file_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() && !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    let pid = std::process::id();
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut temp_path = None;
    let mut file_opt = None;

    for attempt in 0..50u32 {
        let candidate = parent.join(format!(".{file_name}.tmp.{pid}.{now_ns}.{attempt}"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        match opts.open(&candidate) {
            Ok(f) => {
                temp_path = Some(candidate);
                file_opt = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("Failed to create temporary state file"),
        }
    }

    let tmp = temp_path.ok_or_else(|| anyhow::anyhow!("Failed to allocate temp state filename"))?;
    let mut file = file_opt.expect("file opened");

    let write_res = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, target)?;
        #[cfg(unix)]
        if let Ok(dir_file) = std::fs::File::open(parent) {
            let _ = dir_file.sync_all();
        }
        Ok(())
    })();

    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }

    write_res
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_format_file_size_small() {
        let tmp = NamedTempFile::new().unwrap();
        let data = vec![0u8; 1024 * 1024];
        std::fs::write(tmp.path(), &data).unwrap();
        let size = format_file_size(tmp.path());
        assert!(size.ends_with("MB"), "Expected MB, got: {size}");
    }

    #[test]
    fn test_format_file_size_nonexistent() {
        let size = format_file_size(Path::new("/nonexistent/file.mp4"));
        assert_eq!(size, "0.00MB");
    }

    #[test]
    fn test_format_file_size_bytes() {
        assert_eq!(format_file_size_bytes(0), "0.00MB");
        assert_eq!(format_file_size_bytes(1024 * 1024), "1.00MB");
        assert_eq!(format_file_size_bytes(1024 * 1024 * 1024), "1.00GB");
    }

    #[test]
    fn test_write_concat_list_and_temp_workdir_cleanup() {
        let root = TempDir::new().unwrap();
        let guard = PathGuard::new(root.path().to_path_buf());
        let temp_dir_path;
        {
            let tmp_dir = TempWorkDir::create_in(&guard, root.path(), "merge").unwrap();
            temp_dir_path = tmp_dir.path().to_path_buf();
            assert!(temp_dir_path.is_dir());

            let list_path = temp_dir_path.join("concat.txt");
            let files = vec![
                PathBuf::from("/storage512/video1.mp4"),
                PathBuf::from("/storage512/video's 2.mp4"),
            ];
            write_concat_list(&list_path, &files).unwrap();
            let content = std::fs::read_to_string(&list_path).unwrap();
            assert!(content.contains("file '/storage512/video1.mp4'"));
            assert!(content.contains("file '/storage512/video'\\''s 2.mp4'"));
        }
        // Drop 后临时目录已自动清理
        assert!(!temp_dir_path.exists());
    }

    #[test]
    fn test_write_private_file_atomic() {
        let root = TempDir::new().unwrap();
        let state_file = root.path().join("alert_state.json");
        write_private_file_atomic(&state_file, br#"{"ok":true}"#).unwrap();
        assert_eq!(
            std::fs::read_to_string(&state_file).unwrap(),
            r#"{"ok":true}"#
        );
    }
}
