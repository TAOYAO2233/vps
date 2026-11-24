//! Reversible media deletion on the same mounted filesystem.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::warn;

use crate::errors::AppError;
use crate::storage::filesystem::write_private_file_atomic;
use crate::storage::path::{path_occupied, PathGuard};

/// 通过文件头魔数嗅探孤立 `.media` 的视频容器扩展名，确保恢复后可在媒体菜单中识别。
fn detect_media_extension(path: &Path) -> &'static str {
    let mut header = [0u8; 376];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut header))
        .unwrap_or(0);
    if n >= 8 && &header[4..8] == b"ftyp" {
        "mp4"
    } else if n >= 4 && header[0..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        "mkv"
    } else if n >= 3 && &header[0..3] == b"FLV" {
        "flv"
    } else if n >= 189 && header[0] == 0x47 && header[188] == 0x47 {
        "ts"
    } else {
        "mp4"
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashEntry {
    pub id: String,
    pub original_path: PathBuf,
    pub size: u64,
    pub deleted_at_secs: u64,
}

#[derive(Debug, Default)]
pub struct TrashClearReport {
    pub removed: usize,
    pub freed_bytes: u64,
    pub failed: usize,
    pub cancelled: bool,
    pub remaining: usize,
}

fn prepare_restore_parent(guard: &PathGuard, parent: &Path) -> Result<()> {
    let base = guard.canonical_base()?;
    let relative = parent.strip_prefix(&base).map_err(|_| AppError::path_traversal(parent))?;
    if relative.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(AppError::path_traversal(parent).into());
    }

    #[cfg(unix)]
    {
        use rustix::fs::{mkdirat, open, openat, Mode, OFlags};
        use rustix::io::Errno;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut current = open(&base, flags, Mode::empty())?;
        for part in relative.components() {
            let name = part.as_os_str();
            current = match openat(&current, name, flags, Mode::empty()) {
                Ok(dir) => dir,
                Err(Errno::NOENT) => {
                    match mkdirat(&current, name, Mode::from_bits_truncate(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(err) => return Err(err.into()),
                    }
                    openat(&current, name, flags, Mode::empty())?
                }
                Err(err) => return Err(err.into()),
            };
        }
    }
    #[cfg(not(unix))]
    {
        let mut current = base.clone();
        for part in relative.components() {
            current.push(part.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => return Err(AppError::path_traversal(&current).into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&current)?,
                Err(e) => return Err(e.into()),
            }
            current = guard.assert_inside(&current)?;
        }
    }
    Ok(())
}

pub struct TrashStore {
    base_dir: PathBuf,
    retention_days: u64,
}

impl TrashStore {
    pub fn new(base_dir: PathBuf, retention_days: u64) -> Self {
        Self { base_dir, retention_days }
    }

    fn trash_dir(&self) -> Result<PathBuf> {
        let guard = PathGuard::new(self.base_dir.clone());
        let base = guard.canonical_base()?;
        let dir = base.join(".mcb_trash");
        if !path_occupied(&dir) {
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.file_type().is_dir() {
            anyhow::bail!("Trash path is not a regular directory");
        }
        guard.assert_inside(&dir)
    }

    fn checked_id(id: &str) -> Result<()> {
        if id.is_empty() || id.len() > 48 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'_') {
            anyhow::bail!("Invalid trash item ID");
        }
        Ok(())
    }

    /// 绑定用户二次确认的文件身份快照执行移入：在 `.json` 落盘后、实际 `rename` 前再次复核身份，最大限度收窄替换窗口。
    pub fn move_to_trash_verified(
        &self,
        target: &crate::core::state::ConfirmedFileTarget,
    ) -> Result<TrashEntry> {
        let guard = PathGuard::new(self.base_dir.clone());
        let source = target.verify_unchanged(&guard)?;
        let dir = self.trash_dir()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let id = format!(
            "{now:x}_{:x}_{:x}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let media_path = dir.join(format!("{id}.media"));
        let record_path = dir.join(format!("{id}.json"));
        let entry = TrashEntry {
            id,
            original_path: source.clone(),
            size: target.size,
            deleted_at_secs: now,
        };
        if path_occupied(&media_path) || path_occupied(&record_path) {
            anyhow::bail!("Trash item ID collision");
        }
        let bytes = serde_json::to_vec(&entry).context("Failed to serialize trash record")?;
        write_private_file_atomic(&record_path, &bytes)?;

        // 紧邻 rename 前再次核验文件身份，防止写盘 sync 窗口内原路径被外部替换
        let final_source = match target.verify_unchanged(&guard) {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_file(&record_path);
                return Err(e);
            }
        };
        if let Err(e) = std::fs::rename(&final_source, &media_path) {
            let _ = std::fs::remove_file(&record_path);
            return Err(e.into());
        }
        Ok(entry)
    }

    pub fn move_to_trash(&self, original: &Path) -> Result<TrashEntry> {
        let guard = PathGuard::new(self.base_dir.clone());
        let source = guard.assert_regular_file_no_symlink(original)?;
        let dir = self.trash_dir()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let id = format!("{now:x}_{:x}_{:x}", std::process::id(), NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let media_path = dir.join(format!("{id}.media"));
        let record_path = dir.join(format!("{id}.json"));
        let entry = TrashEntry {
            id,
            original_path: source.clone(),
            size: std::fs::metadata(&source)?.len(),
            deleted_at_secs: now,
        };
        if path_occupied(&media_path) || path_occupied(&record_path) {
            anyhow::bail!("Trash item ID collision");
        }
        // WAL 顺序：先原子落盘恢复元数据 .json，再 rename 移入 .media，
        // 确保任意时刻断电或崩溃都不会产生“缺少 .json 的不可恢复孤立 .media”。
        let bytes = serde_json::to_vec(&entry).context("Failed to serialize trash record")?;
        write_private_file_atomic(&record_path, &bytes)?;
        if let Err(e) = std::fs::rename(&source, &media_path) {
            let _ = std::fs::remove_file(&record_path);
            return Err(e.into());
        }
        Ok(entry)
    }

    /// 启动对账：清理超过 60 秒且缺少对应 `.media` 的未完成事务记录，并为历史遗留孤立 `.media` 补建恢复记录。
    pub fn reconcile(&self) -> Result<usize> {
        let dir = self.trash_dir()?;
        let base = PathGuard::new(self.base_dir.clone()).canonical_base()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut reconciled = 0usize;

        for item in std::fs::read_dir(&dir)? {
            let Ok(item) = item else { continue };
            let path = item.path();
            if !item.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if Self::checked_id(stem).is_err() {
                continue;
            }

            if ext == "json" {
                let media_path = dir.join(format!("{stem}.media"));
                if !path_occupied(&media_path) {
                    let age = item
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
                        .map_or(0, |d| d.as_secs());
                    if age >= 60 && std::fs::remove_file(&path).is_ok() {
                        reconciled += 1;
                    }
                }
            } else if ext == "media" {
                let record_path = dir.join(format!("{stem}.json"));
                if !path_occupied(&record_path) {
                    let size = item.metadata().map_or(0, |m| m.len());
                    let detected_ext = detect_media_extension(&path);
                    let entry = TrashEntry {
                        id: stem.to_string(),
                        original_path: base.join(format!("recovered_{stem}.{detected_ext}")),
                        size,
                        deleted_at_secs: now,
                    };
                    if let Ok(bytes) = serde_json::to_vec(&entry) {
                        if write_private_file_atomic(&record_path, &bytes).is_ok() {
                            reconciled += 1;
                        }
                    }
                }
            }
        }
        Ok(reconciled)
    }

    pub fn list(&self) -> Result<Vec<TrashEntry>> {
        let dir = self.trash_dir()?;
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&dir)? {
            let item = match item {
                Ok(it) => it,
                Err(e) => {
                    warn!(error = %e, "Failed to read trash directory entry");
                    continue;
                }
            };
            let path = item.path();
            let is_file = item.file_type().map(|ft| ft.is_file()).unwrap_or(false);
            if path.extension().and_then(|s| s.to_str()) != Some("json") || !is_file {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = ?path.file_name(), error = %e, "Skipping unreadable trash record");
                    continue;
                }
            };
            let entry: TrashEntry = match serde_json::from_slice(&bytes) {
                Ok(e) => e,
                Err(e) => {
                    warn!(path = ?path.file_name(), error = %e, "Skipping corrupted trash record");
                    continue;
                }
            };
            if let Err(e) = Self::checked_id(&entry.id) {
                warn!(path = ?path.file_name(), error = %e, "Skipping trash record with invalid ID");
                continue;
            }
            // 跳过对应 .media 尚未落位或已缺失的记录
            if !path_occupied(&dir.join(format!("{}.media", entry.id))) {
                continue;
            }
            entries.push(entry);
        }
        entries.sort_by(|a, b| {
            b.deleted_at_secs.cmp(&a.deleted_at_secs).then_with(|| a.id.cmp(&b.id))
        });
        Ok(entries)
    }

    /// Binds a clear confirmation to the exact visible set of trash entries.
    pub fn snapshot_token(entries: &[TrashEntry]) -> String {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for entry in entries {
            entry.id.hash(&mut hash);
            entry.size.hash(&mut hash);
        }
        format!("{:016x}", hash.finish())
    }

    pub fn clear_all(&self, expected_token: &str, cancel_rx: &watch::Receiver<bool>) -> Result<TrashClearReport> {
        let entries = self.list()?;
        if entries.is_empty() || Self::snapshot_token(&entries) != expected_token {
            anyhow::bail!("回收站内容已变化，请重新打开并确认");
        }
        let dir = self.trash_dir()?;
        let guard = PathGuard::new(self.base_dir.clone());
        let mut report = TrashClearReport { remaining: entries.len(), ..TrashClearReport::default() };
        for entry in entries {
            if *cancel_rx.borrow() {
                report.cancelled = true;
                break;
            }
            let media_path = dir.join(format!("{}.media", entry.id));
            let record_path = dir.join(format!("{}.json", entry.id));
            let media_result = if path_occupied(&media_path) {
                guard.assert_regular_file_no_symlink(&media_path).and_then(|path| {
                    let size = std::fs::metadata(&path)?.len();
                    std::fs::remove_file(path)?;
                    Ok(size)
                })
            } else {
                Ok(0)
            };
            match media_result {
                Ok(freed) => {
                    report.freed_bytes = report.freed_bytes.saturating_add(freed);
                    if std::fs::remove_file(record_path).is_ok() {
                        report.removed += 1;
                    } else {
                        report.failed += 1;
                    }
                }
                Err(_) => report.failed += 1,
            }
            report.remaining -= 1;
        }
        Ok(report)
    }

    pub fn restore(&self, id: &str, cancel_rx: &watch::Receiver<bool>) -> Result<PathBuf> {
        Self::checked_id(id)?;
        let dir = self.trash_dir()?;
        let guard = PathGuard::new(self.base_dir.clone());
        let record_path = guard.assert_regular_file_no_symlink(&dir.join(format!("{id}.json")))?;
        let entry: TrashEntry = serde_json::from_slice(&std::fs::read(&record_path)?)?;
        if entry.id != id { anyhow::bail!("Trash record ID mismatch"); }
        let media_path = dir.join(format!("{id}.media"));
        let media = guard.assert_regular_file_no_symlink(&media_path)?;

        if *cancel_rx.borrow() {
            return Err(AppError::Cancelled.into());
        }

        // 候选恢复路径：先试原路径，遇 AlreadyExists 或占用则自动选新名，绝不覆盖任何已有文件
        let parent = entry
            .original_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid original restore path"))?;
        prepare_restore_parent(&guard, parent)?;
        let stem = entry
            .original_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("restored");
        let ext = entry
            .original_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();

        let mut restored_target = None;
        for attempt in 0..=999u32 {
            if *cancel_rx.borrow() {
                return Err(AppError::Cancelled.into());
            }
            let raw_candidate = if attempt == 0 {
                entry.original_path.clone()
            } else {
                parent.join(format!("{stem}_restored_{attempt}{ext}"))
            };
            let Ok(candidate) = guard.validate_new_output_path(&raw_candidate) else {
                continue;
            };

            match std::fs::hard_link(&media, &candidate) {
                Ok(()) => {
                    restored_target = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // 并发创建了同名文件，继续尝试下一个不冲突文件名，绝不覆盖
                    continue;
                }
                Err(link_err) => {
                    // 跨文件系统等不支持硬链接的场景：先原子写入隐藏临时文件，再无覆盖 hard_link 提交
                    let tmp_path = candidate.with_file_name(format!(
                        ".mcb_restore_tmp_{}_{}_{}",
                        id,
                        std::process::id(),
                        attempt
                    ));
                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut tmp_file = match options.open(&tmp_path) {
                        Ok(f) => f,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "恢复文件创建临时文件失败 (hard_link: {link_err}; open: {e})"
                            ));
                        }
                    };
                    let copy_res = (|| -> Result<()> {
                        let mut src = std::fs::File::open(&media)?;
                        let permissions = src.metadata()?.permissions();
                        let mut buffer = [0u8; 64 * 1024];
                        loop {
                            if *cancel_rx.borrow() {
                                return Err(AppError::Cancelled.into());
                            }
                            let bytes = src.read(&mut buffer)?;
                            if bytes == 0 {
                                break;
                            }
                            tmp_file.write_all(&buffer[..bytes])?;
                        }
                        tmp_file.set_permissions(permissions)?;
                        tmp_file.sync_all()?;
                        Ok(())
                    })();
                    drop(tmp_file);
                    if let Err(copy_err) = copy_res {
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(copy_err.context(format!("恢复文件复制失败 (hard_link: {link_err})")));
                    }
                    if *cancel_rx.borrow() {
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(AppError::Cancelled.into());
                    }
                    match std::fs::hard_link(&tmp_path, &candidate) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(&tmp_path);
                            restored_target = Some(candidate);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            let _ = std::fs::remove_file(&tmp_path);
                            continue;
                        }
                        Err(commit_err) => {
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err(anyhow::anyhow!(
                                "恢复文件原子提交失败 (hard_link: {link_err}; commit: {commit_err})"
                            ));
                        }
                    }
                }
            }
        }

        let target = restored_target
            .ok_or_else(|| anyhow::anyhow!("无法为恢复文件分配不冲突的目标路径"))?;
        std::fs::remove_file(&media)?;
        std::fs::remove_file(record_path)?;
        Ok(target)
    }

    pub fn cleanup_expired(&self) -> Result<usize> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let dir = self.trash_dir()?;
        let guard = PathGuard::new(self.base_dir.clone());
        let mut removed = 0;
        for entry in self.list()? {
            if now.saturating_sub(entry.deleted_at_secs) < self.retention_days.saturating_mul(86_400) {
                continue;
            }
            let media_path = dir.join(format!("{}.media", entry.id));
            if path_occupied(&media_path) {
                match guard.assert_regular_file_no_symlink(&media_path) {
                    Ok(media) => {
                        if let Err(e) = std::fs::remove_file(&media) {
                            warn!(id = %entry.id, error = %e, "Failed to remove expired trash media; keeping record");
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!(id = %entry.id, error = %e, "Expired trash media failed safety check; keeping record");
                        continue;
                    }
                }
            }
            if let Err(e) = std::fs::remove_file(dir.join(format!("{}.json", entry.id))) {
                warn!(id = %entry.id, error = %e, "Failed to remove expired trash record");
                continue;
            }
            removed += 1;
        }
        Ok(removed)
    }
}
