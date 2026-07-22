//! 目录扫描器。
//!
//! 使用 `std::fs::read_dir` 扫描目录，列出子目录和视频文件。
//! 对应 Python 版本的 `os.listdir` + 分类逻辑。

use std::path::Path;

use anyhow::Result;
use tracing::warn;

use crate::config::VideoExtensions;
use crate::storage::path::PathGuard;

/// 目录扫描结果。
#[derive(Debug, Default)]
pub struct DirListing {
    /// 子目录列表（已排序）
    pub dirs: Vec<String>,
    /// 视频文件列表（已排序）
    pub files: Vec<String>,
}

impl DirListing {
    /// 返回所有条目（目录在前，文件在后）。
    #[must_use]
    pub fn all_items(&self) -> Vec<String> {
        let mut items = self.dirs.clone();
        items.extend(self.files.clone());
        items
    }

    /// 返回总条目数。
    #[must_use]
    #[allow(dead_code)]
    pub fn total(&self) -> usize {
        self.dirs.len() + self.files.len()
    }
}

/// 扫描目录，返回子目录和视频文件列表。
///
/// # Arguments
///
/// * `dir` - 要扫描的目录
/// * `path_guard` - 路径安全守卫（用于过滤越界路径）
///
/// # Errors
///
/// 若目录不存在或无法读取，返回错误。
pub fn scan_directory(dir: &Path, path_guard: &PathGuard) -> Result<DirListing> {
    if !dir.is_dir() {
        return Err(crate::errors::AppError::directory_not_found(dir).into());
    }

    let mut listing = DirListing::default();

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "Failed to read directory entry");
                continue;
            }
        };

        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        let item_path = entry.path();

        // 路径安全校验
        if path_guard.assert_inside(&item_path).is_err() {
            warn!(path = ?item_path, "Skipping path outside BASE_DIR");
            continue;
        }

        if item_path.is_dir() {
            listing.dirs.push(name_str);
        } else if item_path.is_file() && VideoExtensions::is_video(&name_str) {
            listing.files.push(name_str);
        }
    }

    listing.dirs.sort();
    listing.files.sort();

    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_scan_directory() {
        let tmp = TempDir::new().unwrap();
        let guard = PathGuard::new(tmp.path().to_path_buf());

        // 创建测试文件和目录
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("video.mp4"), b"").unwrap();
        std::fs::write(tmp.path().join("video.mkv"), b"").unwrap();
        std::fs::write(tmp.path().join("document.pdf"), b"").unwrap();

        let listing = scan_directory(tmp.path(), &guard).unwrap();
        assert_eq!(listing.dirs, vec!["subdir"]);
        assert_eq!(listing.files, vec!["video.mkv", "video.mp4"]);
        assert_eq!(listing.total(), 3);
    }

    #[test]
    fn test_dir_listing_all_items() {
        let listing = DirListing {
            dirs: vec!["a_dir".to_string()],
            files: vec!["b_video.mp4".to_string()],
        };
        let all = listing.all_items();
        assert_eq!(all[0], "a_dir");
        assert_eq!(all[1], "b_video.mp4");
    }
}
