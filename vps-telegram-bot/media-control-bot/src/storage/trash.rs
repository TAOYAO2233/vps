//! Reversible media deletion on the same mounted filesystem.

use std::path::{Path, PathBuf};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::storage::filesystem::write_private_file_atomic;
use crate::storage::path::{path_occupied, PathGuard};

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
            std::fs::create_dir(&dir)?;
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
        std::fs::rename(&source, &media_path)?;
        let save = serde_json::to_vec(&entry)
            .context("Failed to serialize trash record")
            .and_then(|bytes| write_private_file_atomic(&record_path, &bytes));
        if let Err(e) = save {
            let _ = std::fs::rename(&media_path, &source);
            return Err(e);
        }
        Ok(entry)
    }

    pub fn list(&self) -> Result<Vec<TrashEntry>> {
        let dir = self.trash_dir()?;
        let mut entries = Vec::new();
        for item in std::fs::read_dir(dir)? {
            let item = item?;
            let path = item.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json")
                || !item.file_type()?.is_file()
            {
                continue;
            }
            let entry: TrashEntry = serde_json::from_slice(&std::fs::read(&path)?)?;
            Self::checked_id(&entry.id)?;
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

    pub fn clear_all(&self, expected_token: &str) -> Result<TrashClearReport> {
        let entries = self.list()?;
        if entries.is_empty() || Self::snapshot_token(&entries) != expected_token {
            anyhow::bail!("回收站内容已变化，请重新打开并确认");
        }
        let dir = self.trash_dir()?;
        let guard = PathGuard::new(self.base_dir.clone());
        let mut report = TrashClearReport::default();
        for entry in entries {
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
        }
        Ok(report)
    }

    pub fn restore(&self, id: &str) -> Result<PathBuf> {
        Self::checked_id(id)?;
        let dir = self.trash_dir()?;
        let record_path = dir.join(format!("{id}.json"));
        let entry: TrashEntry = serde_json::from_slice(&std::fs::read(&record_path)?)?;
        if entry.id != id { anyhow::bail!("Trash record ID mismatch"); }
        let media_path = dir.join(format!("{id}.media"));
        let guard = PathGuard::new(self.base_dir.clone());
        let media = guard.assert_regular_file_no_symlink(&media_path)?;
        let target = guard.validate_new_output_path(&entry.original_path)?;
        std::fs::hard_link(&media, &target)?;
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
            if let Ok(media) = guard.assert_regular_file_no_symlink(&media_path) {
                std::fs::remove_file(media)?;
            }
            std::fs::remove_file(dir.join(format!("{}.json", entry.id)))?;
            removed += 1;
        }
        Ok(removed)
    }
}
