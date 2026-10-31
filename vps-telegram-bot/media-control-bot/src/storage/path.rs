//! 路径安全校验与原子发布工具函数。
//!
//! 提供严格的 `BASE_DIR` 边界防护（防止目录遍历与符号链接越界）、
//! 新建输出文件父目录校验，以及绝不覆盖已有文件的硬链接发布机制。

use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::errors::AppError;

/// 检查路径在文件系统上是否已占用（包括悬空符号链接）。
///
/// 相比 `Path::exists()`，`symlink_metadata` 不会跟随符号链接，
/// 因此即使目标是悬空符号链接也会返回 `true`，防止通过悬空软链越界写入。
#[must_use]
pub fn path_occupied(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// 路径安全守卫。
///
/// 封装 `BASE_DIR` 边界校验逻辑，确保所有文件读取、写入、删除均严格限制在媒体根目录内。
#[derive(Debug, Clone)]
pub struct PathGuard {
    /// 配置的媒体根目录原始路径（每次校验时动态规范化，防止挂载变化后沿用失效路径）
    base_dir: PathBuf,
}

impl PathGuard {
    /// 创建路径守卫实例。
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// 获取规范化后的 `BASE_DIR`；若根目录不存在或不可访问则立即报错，绝不退回未规范化路径。
    pub fn canonical_base(&self) -> Result<PathBuf> {
        self.base_dir
            .canonicalize()
            .map_err(|_| AppError::directory_not_found(&self.base_dir).into())
    }

    /// 校验已存在的路径是否位于 `BASE_DIR` 之内，并返回规范化后的真实路径。
    ///
    /// 若目标不存在或无法规范化，直接返回错误，**绝不退回原始未规范化路径**。
    pub fn assert_inside(&self, path: &Path) -> Result<PathBuf> {
        let base = self.canonical_base()?;
        let target = path.canonicalize().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::file_not_found(path)
            } else {
                AppError::FileIo {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                }
            }
        })?;

        if target == base || target.starts_with(&base) {
            Ok(target)
        } else {
            Err(AppError::PathTraversal { path: target }.into())
        }
    }

    /// 校验目标路径是位于 `BASE_DIR` 内的普通文件，且条目本身不是符号链接。
    ///
    /// 用于删除等高危操作，防止跟随符号链接越界或误删软链/目录。
    pub fn assert_regular_file_no_symlink(&self, path: &Path) -> Result<PathBuf> {
        let file_name = validate_single_filename(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| AppError::path_traversal(path))?;

        let canonical_parent = self.assert_inside(parent)?;
        let exact_path = canonical_parent.join(file_name);

        let sym_meta = std::fs::symlink_metadata(&exact_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::file_not_found(&exact_path)
            } else {
                AppError::FileIo {
                    path: exact_path.clone(),
                    message: e.to_string(),
                }
            }
        })?;

        if sym_meta.file_type().is_symlink() {
            return Err(AppError::PathTraversal { path: exact_path }.into());
        }
        if !sym_meta.file_type().is_file() {
            return Err(AppError::invalid_argument("Target path is not a regular file").into());
        }

        // 再次确认规范路径仍在 BASE_DIR 内
        let verified = self.assert_inside(&exact_path)?;
        Ok(verified)
    }

    /// 校验拟新建的输出文件或临时目录路径：
    /// 1. 文件名必须是单一普通名称（不含 `..`、`/`、`\`）；
    /// 2. 父目录必须真实存在且规范路径位于 `BASE_DIR` 内；
    /// 3. 目标路径当前不得存在（含悬空符号链接）。
    pub fn validate_new_output_path(&self, desired_path: &Path) -> Result<PathBuf> {
        let file_name = validate_single_filename(desired_path)?;
        let parent = desired_path
            .parent()
            .ok_or_else(|| AppError::path_traversal(desired_path))?;

        let canonical_parent = self.assert_inside(parent)?;
        let target = canonical_parent.join(file_name);

        if path_occupied(&target) {
            return Err(AppError::FileIo {
                path: target,
                message: "Target output path already exists".to_string(),
            }
            .into());
        }

        Ok(target)
    }

    /// 安全拼接单层子目录或文件名，并校验已存在的拼接结果位于 `BASE_DIR` 内。
    pub fn safe_join(&self, parent: &Path, child: &str) -> Result<PathBuf> {
        if child.is_empty() || child == "." || child == ".." || child.contains(['/', '\\', '\0']) {
            return Err(AppError::path_traversal(parent.join(child)).into());
        }
        let mut comp_iter = Path::new(child).components();
        match (comp_iter.next(), comp_iter.next()) {
            (Some(Component::Normal(_)), None) => {}
            _ => return Err(AppError::path_traversal(parent.join(child)).into()),
        }

        let canonical_parent = self.assert_inside(parent)?;
        let joined = canonical_parent.join(child);
        self.assert_inside(&joined)
    }
}

/// 提取并验证路径末尾是合法的单一普通文件名组件。
fn validate_single_filename(path: &Path) -> Result<&std::ffi::OsStr> {
    let file_name = path
        .file_name()
        .ok_or_else(|| AppError::path_traversal(path))?;

    let mut comps = Path::new(file_name).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(name)), None) if !name.is_empty() => Ok(file_name),
        _ => Err(AppError::path_traversal(path).into()),
    }
}

/// 生成不与现有文件（含悬空符号链接）冲突的唯一路径。
///
/// 若目标文件已存在，自动生成 `xxx_1.ext`、`xxx_2.ext` 等。
#[must_use]
pub fn unique_path(path: &Path) -> PathBuf {
    if !path_occupied(path) {
        return path.to_path_buf();
    }

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(Path::new("."));

    let mut index = 1u32;
    loop {
        let candidate = parent.join(format!("{stem}_{index}{ext}"));
        if !path_occupied(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

/// 将已通过完整性校验的临时产物文件安全发布为最终输出文件（**绝不覆盖已有同名文件**）。
///
/// 使用内核原子的 `hard_link`（若目标在检查后被其他进程抢先创建会立即返回 `AlreadyExists`，
/// 绝不会像 `rename` 那样静默覆盖已有文件），若冲突则自动递增选名重试，成功后移除临时路径。
pub fn publish_temp_file_no_overwrite(
    guard: &PathGuard,
    temp_file: &Path,
    desired_output: &Path,
) -> Result<PathBuf> {
    let verified_temp = guard.assert_regular_file_no_symlink(temp_file)?;

    for _ in 0..200 {
        let candidate = unique_path(desired_output);
        let validated_target = match guard.validate_new_output_path(&candidate) {
            Ok(p) => p,
            Err(_) => {
                // 若恰好在 unique_path 之后被创建，继续下一轮选名
                if path_occupied(&candidate) {
                    continue;
                }
                return guard.validate_new_output_path(&candidate);
            }
        };

        match std::fs::hard_link(&verified_temp, &validated_target) {
            Ok(()) => {
                let _ = std::fs::remove_file(&verified_temp);
                return Ok(validated_target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 其他进程刚刚创建了同名文件，绝不覆盖，继续尝试下一个唯一后缀
                continue;
            }
            Err(e) => {
                return Err(AppError::FileIo {
                    path: validated_target,
                    message: e.to_string(),
                }
                .into());
            }
        }
    }

    anyhow::bail!("Failed to allocate a non-conflicting output filename after 200 attempts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_assert_inside_existing_subdir() {
        let tmp = TempDir::new().unwrap();
        let guard = PathGuard::new(tmp.path().to_path_buf());
        let subdir = tmp.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        assert!(guard.assert_inside(&subdir).is_ok());
    }

    #[test]
    fn test_assert_inside_rejects_nonexistent_and_dotdot() {
        let tmp = TempDir::new().unwrap();
        let guard = PathGuard::new(tmp.path().to_path_buf());

        // 不存在的文件不得退回原始路径放行
        let missing = tmp.path().join("missing.mp4");
        assert!(guard.assert_inside(&missing).is_err());

        // 包含 .. 逃逸的不存在路径必须拒绝
        let traversal = tmp.path().join("..").join("..").join("etc").join("shadow");
        assert!(guard.assert_inside(&traversal).is_err());
    }

    #[test]
    fn test_safe_join_rejects_traversal_names() {
        let tmp = TempDir::new().unwrap();
        let guard = PathGuard::new(tmp.path().to_path_buf());
        assert!(guard.safe_join(tmp.path(), "..").is_err());
        assert!(guard.safe_join(tmp.path(), "../outside").is_err());
    }

    #[test]
    fn test_unique_path_and_atomic_publish_never_overwrites() {
        let tmp = TempDir::new().unwrap();
        let guard = PathGuard::new(tmp.path().to_path_buf());

        let existing = tmp.path().join("video.mp4");
        std::fs::write(&existing, b"original-content").unwrap();

        let temp_out = tmp.path().join(".tmp_merged.mp4");
        std::fs::write(&temp_out, b"new-merged-content").unwrap();

        let published = publish_temp_file_no_overwrite(&guard, &temp_out, &existing).unwrap();
        assert_eq!(
            published.file_name().unwrap().to_str().unwrap(),
            "video_1.mp4"
        );
        assert!(!temp_out.exists());
        // 原同名文件内容完好无损
        assert_eq!(std::fs::read(&existing).unwrap(), b"original-content");
        assert_eq!(std::fs::read(&published).unwrap(), b"new-merged-content");
    }
}
