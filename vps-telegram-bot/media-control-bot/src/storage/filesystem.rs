//! 文件系统工具函数。
//!
//! 提供文件大小格式化、安全删除、concat list 写入等工具函数。
//! 对应 Python 版本的 `get_formatted_file_size`、`remove_if_exists`、`write_concat_list` 等函数。

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use tracing::warn;

/// 格式化文件大小为人类可读字符串。
///
/// 小于 1GB 显示 MB，否则显示 GB。
///
/// # Arguments
///
/// * `path` - 文件路径
///
/// # Returns
///
/// 格式化后的大小字符串，如 `"123.45MB"` 或 `"1.23GB"`。
#[must_use]
pub fn format_file_size(path: &Path) -> String {
    let size_bytes = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return "0.00MB".to_string(),
    };

    let size_mb = size_bytes as f64 / (1024.0 * 1024.0);
    if size_mb >= 1024.0 {
        let size_gb = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        format!("{size_gb:.2}GB")
    } else {
        format!("{size_mb:.2}MB")
    }
}

/// 安全删除文件（若文件不存在则忽略）。
///
/// 对应 Python 版本的 `remove_if_exists` 函数。
///
/// # Arguments
///
/// * `path` - 要删除的文件路径
pub fn remove_if_exists(path: &Path) {
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(path = ?path, error = %e, "Failed to remove file");
        }
    }
}

/// 写入 FFmpeg concat list 文件。
///
/// 对应 Python 版本的 `write_concat_list` 函数。
/// 自动转义文件路径中的单引号和反斜杠。
///
/// # Arguments
///
/// * `list_file` - 输出 list 文件路径
/// * `file_paths` - 要合并的文件路径列表
///
/// # Errors
///
/// 若文件写入失败，返回错误。
pub fn write_concat_list(list_file: &Path, file_paths: &[std::path::PathBuf]) -> Result<()> {
    let mut file = std::fs::File::create(list_file)?;
    for path in file_paths {
        let path_str = path.to_string_lossy();
        // 转义单引号和反斜杠（FFmpeg concat list 格式要求）
        let escaped = path_str.replace('\\', "\\\\").replace('\'', "\\'");
        writeln!(file, "file '{escaped}'")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_format_file_size_small() {
        let tmp = NamedTempFile::new().unwrap();
        // 写入 1MB 数据
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
    fn test_write_concat_list() {
        let tmp = NamedTempFile::new().unwrap();
        let files = vec![
            std::path::PathBuf::from("/tmp/video1.mp4"),
            std::path::PathBuf::from("/tmp/video's 2.mp4"),
        ];
        write_concat_list(tmp.path(), &files).unwrap();
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(content.contains("file '/tmp/video1.mp4'"));
        assert!(content.contains("file '/tmp/video\\'s 2.mp4'"));
    }

    #[test]
    fn test_remove_if_exists() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        assert!(path.exists());
        remove_if_exists(&path);
        // 再次调用不应 panic
        remove_if_exists(&path);
    }
}
