//! 目录扫描器。
//!
//! 使用 `std::fs::read_dir` 扫描目录，列出子目录和视频文件（含文件大小）。
//! 对应 Python 版本的 `os.listdir` + 分类逻辑。
//!
//! 本模块是同步阻塞的，调用方应放在 `tokio::task::spawn_blocking` 中执行。

use std::path::Path;

use anyhow::Result;
use tracing::warn;

use crate::config::VideoExtensions;
use crate::errors::AppError;

/// 目录中的单个条目。
#[derive(Debug, Clone)]
pub struct DirItem {
    /// 条目名（不含父目录）
    pub name: String,
    /// 是否为目录（符号链接按目标类型判断）
    pub is_dir: bool,
    /// 文件大小（字节；目录为 0）
    pub size: u64,
}

/// 目录扫描结果（目录在前、文件在后，各自按名称升序）。
#[derive(Debug, Default)]
pub struct DirListing {
    /// 扫描到的条目
    pub items: Vec<DirItem>,
}

impl DirListing {
    /// 返回所有条目名（目录在前，文件在后）。
    #[must_use]
    pub fn all_items(&self) -> Vec<String> {
        self.items.iter().map(|item| item.name.clone()).collect()
    }

    /// 返回总条目数。
    #[must_use]
    pub fn total(&self) -> usize {
        self.items.len()
    }

    /// 是否为空目录。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 扫描目录，返回子目录和视频文件列表（含大小）。
///
/// 进入子目录时的路径越界校验由 [`crate::storage::path::PathGuard`] 负责，
/// 因此这里只枚举条目本身，避免对每个条目都做一次 `canonicalize` 系统调用。
///
/// # Arguments
///
/// * `dir` - 要扫描的目录（调用方需保证其在 BASE_DIR 之内）
///
/// # Errors
///
/// 目录不存在或无法读取时返回错误。
pub fn scan_directory(dir: &Path) -> Result<DirListing> {
    if !dir.is_dir() {
        return Err(AppError::directory_not_found(dir).into());
    }

    let mut dirs: Vec<DirItem> = Vec::new();
    let mut files: Vec<DirItem> = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warn!(error = %e, "Failed to read directory entry");
                continue;
            }
        };

        let name = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Linux/Windows 上 `file_type()` 通常直接来自目录项，无需额外系统调用；
        // 符号链接回退到 `metadata()`（跟随目标），保持「可进入符号链接目录」的既有行为。
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                warn!(name = %name, error = %e, "Failed to read directory entry type");
                continue;
            }
        };

        let (is_dir, is_file, size) = if file_type.is_symlink() {
            match std::fs::metadata(entry.path()) {
                Ok(meta) => (meta.is_dir(), meta.is_file(), meta.len()),
                Err(e) => {
                    warn!(name = %name, error = %e, "Failed to follow symlink");
                    continue;
                }
            }
        } else if file_type.is_dir() {
            (true, false, 0)
        } else if file_type.is_file() {
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            (false, true, size)
        } else {
            (false, false, 0)
        };

        if is_dir {
            dirs.push(DirItem {
                name,
                is_dir: true,
                size: 0,
            });
        } else if is_file && VideoExtensions::is_video(&name) {
            files.push(DirItem {
                name,
                is_dir: false,
                size,
            });
        }
    }

    dirs.sort_by_key(|item| item.name.clone());
    files.sort_by_key(|item| item.name.clone());

    let mut items = dirs;
    items.extend(files);

    Ok(DirListing { items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_scan_directory() {
        let tmp = TempDir::new().unwrap();

        // 创建测试文件和目录
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("video.mp4"), b"1234").unwrap();
        std::fs::write(tmp.path().join("video.mkv"), b"").unwrap();
        std::fs::write(tmp.path().join("document.pdf"), b"").unwrap();

        let listing = scan_directory(tmp.path()).unwrap();
        assert_eq!(listing.total(), 3);
        assert!(!listing.is_empty());
        assert_eq!(
            listing.all_items(),
            vec!["subdir", "video.mkv", "video.mp4"]
        );

        let subdir = listing
            .items
            .iter()
            .find(|item| item.name == "subdir")
            .unwrap();
        assert!(subdir.is_dir);
        assert_eq!(subdir.size, 0);

        let mp4 = listing
            .items
            .iter()
            .find(|item| item.name == "video.mp4")
            .unwrap();
        assert!(!mp4.is_dir);
        assert_eq!(mp4.size, 4);
    }

    #[test]
    fn test_scan_directory_missing() {
        assert!(scan_directory(Path::new("/definitely/not/here")).is_err());
    }

    #[test]
    fn test_dir_listing_all_items_order() {
        let listing = DirListing {
            items: vec![
                DirItem {
                    name: "a_dir".to_string(),
                    is_dir: true,
                    size: 0,
                },
                DirItem {
                    name: "b_video.mp4".to_string(),
                    is_dir: false,
                    size: 10,
                },
            ],
        };
        let all = listing.all_items();
        assert_eq!(all[0], "a_dir");
        assert_eq!(all[1], "b_video.mp4");
    }
}
