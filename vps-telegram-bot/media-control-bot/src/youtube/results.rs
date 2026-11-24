//! Durable confirmed upload results, independent of automatic recovery settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::persistence::lock_for_queue_file;
use crate::storage::filesystem::write_private_file_atomic;
use crate::utils::redact::redact_secrets;

/// Delivery state of the separate Telegram notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    /// No acknowledged delivery yet, or an attempt was interrupted.
    Pending,
    /// Telegram acknowledged a separate result message.
    Sent,
    /// The bounded delivery attempts ended without acknowledgement.
    Failed,
}

/// A confirmed video survives task cleanup and process restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadResultRecord {
    /// Stable task identity shared with recovery entries.
    pub task_id: String,
    /// Original display filename.
    pub filename: String,
    /// Intended Telegram chat.
    pub chat_id: i64,
    /// Server-confirmed YouTube video identifier.
    pub video_id: String,
    /// First confirmed completion time in RFC 3339 with timezone.
    pub completed_at: String,
    /// Independent notification delivery state.
    pub notification_status: NotificationStatus,
    /// Cumulative attempts, including explicit manual resends.
    pub notification_attempts: u32,
    /// Last delivery error after secret redaction.
    pub notification_error: Option<String>,
}

impl UploadResultRecord {
    /// Create a confirmed result before attempting notification delivery.
    pub fn confirmed(task_id: &str, filename: &str, chat_id: i64, video_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            filename: filename.to_string(),
            chat_id,
            video_id: video_id.to_string(),
            completed_at: chrono::Local::now().to_rfc3339(),
            notification_status: NotificationStatus::Pending,
            notification_attempts: 0,
            notification_error: None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ResultFile {
    entries: Vec<UploadResultRecord>,
}

/// Shared per-path locking and atomic private-file writes protect result updates.
pub struct YoutubeResultStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl YoutubeResultStore {
    /// `queue.json` uses the distinct sibling `queue.json.results.json`.
    pub fn beside_queue(queue: &Path) -> Self {
        let mut name = queue.file_name().unwrap_or_default().to_os_string();
        name.push(".results.json");
        let path = queue.with_file_name(name);
        let lock = lock_for_queue_file(&path);
        Self { path, lock }
    }

    fn read_path(path: &Path) -> Result<ResultFile> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid YouTube result history"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ResultFile::default()),
            Err(e) => Err(e).context("Cannot read YouTube result history"),
        }
    }

    fn write_path(path: &Path, file: &ResultFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(file)?;
        write_private_file_atomic(path, &bytes)
            .context("Cannot persist YouTube result history")
    }

    /// Newest results first. No recovery/upload is triggered by reading history.
    pub async fn load(&self) -> Result<Vec<UploadResultRecord>> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        let mut entries = tokio::task::spawn_blocking(move || Self::read_path(&path))
            .await
            .context("YouTube result load worker panicked")??
            .entries;
        entries.sort_by(|a, b| {
            let ta = chrono::DateTime::parse_from_rfc3339(&a.completed_at)
                .ok()
                .map(|dt| dt.timestamp_nanos_opt());
            let tb = chrono::DateTime::parse_from_rfc3339(&b.completed_at)
                .ok()
                .map(|dt| dt.timestamp_nanos_opt());
            match (ta, tb) {
                (Some(na), Some(nb)) => nb.cmp(&na),
                _ => b.completed_at.cmp(&a.completed_at),
            }
        });
        Ok(entries)
    }

    /// Idempotently store completion; retain the first timestamp and delivery state.
    pub async fn save_confirmed(&self, record: UploadResultRecord) -> Result<UploadResultRecord> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = Self::read_path(&path)?;
            if let Some(existing) = file.entries.iter().find(|e| e.task_id == record.task_id) {
                anyhow::ensure!(
                    existing.video_id == record.video_id,
                    "Conflicting confirmed video IDs"
                );
                return Ok(existing.clone());
            }
            file.entries.push(record.clone());
            Self::write_path(&path, &file)?;
            Ok(record)
        })
        .await
        .context("YouTube result save worker panicked")?
    }

    /// Persist each attempt before sending, including across crashes or manual resends.
    pub async fn begin_notification_attempt(&self, task_id: &str) -> Result<()> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        let task_id = task_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut file = Self::read_path(&path)?;
            let record = file
                .entries
                .iter_mut()
                .find(|e| e.task_id == task_id)
                .context("Confirmed upload result missing")?;
            record.notification_status = NotificationStatus::Pending;
            record.notification_attempts = record.notification_attempts.saturating_add(1);
            Self::write_path(&path, &file)
        })
        .await
        .context("YouTube result begin_notification worker panicked")?
    }

    /// Store notification delivery without changing the confirmed upload result.
    pub async fn finish_notification(&self, task_id: &str, delivered: bool, error: Option<&str>) -> Result<()> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        let task_id = task_id.to_string();
        let redacted_err = error.map(|e| redact_secrets(e).chars().take(500).collect());
        tokio::task::spawn_blocking(move || {
            let mut file = Self::read_path(&path)?;
            let record = file
                .entries
                .iter_mut()
                .find(|e| e.task_id == task_id)
                .context("Confirmed upload result missing")?;
            record.notification_status = if delivered {
                NotificationStatus::Sent
            } else {
                NotificationStatus::Failed
            };
            record.notification_error = redacted_err;
            Self::write_path(&path, &file)
        })
        .await
        .context("YouTube result finish_notification worker panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completion_and_delivery_survive_reopening_store() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue.json");
        let store = YoutubeResultStore::beside_queue(&queue);
        let record = UploadResultRecord::confirmed("task", "video.mp4", 1, "video123");
        let first = store.save_confirmed(record.clone()).await.unwrap();
        store.begin_notification_attempt("task").await.unwrap();
        store.finish_notification("task", false, Some("network failed")).await.unwrap();
        // Saving the same completion again must not erase failed notification state.
        store.save_confirmed(record).await.unwrap();
        let entries = YoutubeResultStore::beside_queue(&queue).load().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].video_id, "video123");
        assert_eq!(entries[0].completed_at, first.completed_at);
        assert_eq!(entries[0].notification_status, NotificationStatus::Failed);
        assert_eq!(entries[0].notification_attempts, 1);
        assert!(!queue.exists());
    }

    #[tokio::test]
    async fn corrupted_result_history_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = YoutubeResultStore::beside_queue(&dir.path().join("queue.json"));
        std::fs::write(&store.path, b"broken").unwrap();
        assert!(store.save_confirmed(UploadResultRecord::confirmed("t", "v", 1, "id")).await.is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), b"broken");
    }

    #[tokio::test]
    async fn success_history_is_enabled_even_when_automatic_recovery_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");
        let queue = crate::youtube::persistence::YoutubeQueueStore::new(
            crate::config::YoutubeRecoveryMode::None, queue_path.clone(),
        );
        queue.results().save_confirmed(UploadResultRecord::confirmed("t", "v", 1, "id")).await.unwrap();
        assert_eq!(queue.results().load().await.unwrap().len(), 1);
        assert!(!queue_path.exists());
        assert!(queue.load_recoverable_entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_store_instances_do_not_lose_results_or_delivery_updates() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue.json");
        let mut jobs = Vec::new();
        for index in 0..8 {
            let store = YoutubeResultStore::beside_queue(&queue);
            jobs.push(tokio::spawn(async move {
                let id = format!("task{index}");
                store.save_confirmed(UploadResultRecord::confirmed(&id, "v", 1, &format!("video{index}"))).await.unwrap();
                store.begin_notification_attempt(&id).await.unwrap();
                store.finish_notification(&id, true, None).await.unwrap();
            }));
        }
        for job in jobs { job.await.unwrap(); }
        let records = YoutubeResultStore::beside_queue(&queue).load().await.unwrap();
        assert_eq!(records.len(), 8);
        assert!(records.iter().all(|r| r.notification_status == NotificationStatus::Sent && r.notification_attempts == 1));
    }
}
