//! YouTube 上传队列可选持久化存储（默认 `YOUTUBE_RECOVERY_MODE=none` 不启用）。
//!
//! 安全与并发一致性保证（v1.1 修正）：
//! - 保守默认模式下（`YoutubeRecoveryMode::None`），所有持久化操作均为空操作（no-op），绝不擅自在重启后自动上传；
//! - **跨批次全局串行互斥**：除由 [`crate::core::state::AppState`] 持有全进程共享实例外，底层按规范化队列路径通过
//!   全局锁表 `QUEUE_FILE_LOCKS` 共享同一把异步互斥锁，彻底杜绝多批次并发 `upsert` / `update_session_uri` / `remove` 后写覆盖先写；
//! - **显式错误传播**：所有落盘操作均返回 `Result<()>`（不静默吞掉写盘错误），确保 `requeue` / `resume` 模式可在发送首个视频分块前严格确认会话 URI 已安全落盘；
//! - **最小残余窗口说明**：若进程恰好在服务端已返回会话 `Location` 头、本地尚未完成原子 `rename` 的极窄时间窗内崩溃，服务端只会残留一个 `0` 字节未传数据的空会话（不会生成已发布视频）；所有载入或写入的 `session_uri` 均立即注册脱敏。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::{Config, YoutubeRecoveryMode};
use crate::storage::filesystem::write_private_file_atomic;
use crate::utils::redact::{redact_secrets, register_youtube_session_uri};

/// 按队列文件规范化路径共享进程级异步互斥锁，防止任何多实例并发读写同一文件导致丢任务。
static QUEUE_FILE_LOCKS: Lazy<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

fn lock_for_queue_file(path: &Path) -> Arc<Mutex<()>> {
    let key = normalize_queue_key(path);
    let mut map = QUEUE_FILE_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn normalize_queue_key(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    if let Some(parent) = path.parent() {
        let base_parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(canon_parent) = base_parent.canonicalize() {
            if let Some(name) = path.file_name() {
                return canon_parent.join(name);
            }
        }
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

/// 持久化的单个待完成 YouTube 上传任务条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedUploadEntry {
    /// 任务唯一标识
    pub task_id: String,
    /// 视频源文件规范路径
    pub file_path: PathBuf,
    /// 视频显示文件名
    pub filename: String,
    /// 创建任务时的源文件大小（字节）
    pub file_size: u64,
    /// 创建任务时的源文件修改时间（UNIX 纳秒）
    pub modified_nanos: Option<u128>,
    /// 触发任务的管理员聊天 ID
    pub chat_id: i64,
    /// 可恢复上传会话 URI（仅在 `requeue` / `resume` 防重核查与续传时记录，文件权限强制 `0600`）
    pub session_uri: Option<String>,
    /// 是否处于“全部字节已发送但服务端持续返回 308 未确认最终 video_id”的待核实状态
    #[serde(default)]
    pub unconfirmed_full_upload: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedQueueFile {
    entries: Vec<PersistedUploadEntry>,
}

/// YouTube 上传队列持久化管理器。
#[derive(Debug)]
pub struct YoutubeQueueStore {
    mode: YoutubeRecoveryMode,
    queue_file: PathBuf,
    file_lock: Arc<Mutex<()>>,
}

impl YoutubeQueueStore {
    /// 根据全局配置创建队列持久化管理器（同一文件路径共享全局串行锁）。
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.youtube_recovery_mode, config.youtube_queue_file.clone())
    }

    /// 按显式恢复模式与队列文件路径创建持久化管理器。
    #[must_use]
    pub fn new(mode: YoutubeRecoveryMode, queue_file: PathBuf) -> Self {
        let file_lock = lock_for_queue_file(&queue_file);
        Self {
            mode,
            queue_file,
            file_lock,
        }
    }

    /// 创建保守不启用持久化的默认实例（供测试初始化状态使用）。
    #[must_use]
    pub fn new_noop() -> Self {
        Self::new(
            YoutubeRecoveryMode::None,
            PathBuf::from("./youtube_upload_queue.json"),
        )
    }

    /// 当前配置的恢复策略。
    #[must_use]
    #[allow(dead_code)]
    pub fn mode(&self) -> YoutubeRecoveryMode {
        self.mode
    }

    /// 是否启用了队列持久化（`requeue` 或 `resume`）。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.mode != YoutubeRecoveryMode::None
    }

    /// 读取上次未完成的任务列表（若模式为 `None` 则直接返回空列表，不触发自动恢复）。
    pub async fn load_recoverable_entries(&self) -> Result<Vec<PersistedUploadEntry>> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }
        let _guard = self.file_lock.lock().await;
        let data = read_queue_file_strict(&self.queue_file)?;
        for entry in &data.entries {
            if let Some(uri) = &entry.session_uri {
                register_youtube_session_uri(uri);
            }
        }
        Ok(data.entries)
    }

    /// 新增或更新一条待完成上传记录（`None` 模式下为空操作；启用模式下写盘失败明确返回 `Err`）。
    pub async fn upsert(&self, entry: PersistedUploadEntry) -> Result<()> {
        if let Some(uri) = &entry.session_uri {
            register_youtube_session_uri(uri);
        }
        if !self.is_enabled() {
            return Ok(());
        }

        let _guard = self.file_lock.lock().await;
        let mut data = read_queue_file_strict(&self.queue_file)?;
        if let Some(existing) = data.entries.iter_mut().find(|e| e.task_id == entry.task_id) {
            *existing = entry;
        } else {
            data.entries.push(entry);
        }
        save_queue_file_strict(&self.queue_file, &data)
    }

    /// 为指定任务安全持久化已分配的 Resumable Session URI。
    ///
    /// 在启用 `requeue` / `resume` 模式下，调用方必须等待本方法返回 `Ok(())` 后才允许发送首个视频分块；
    /// 若条目不存在或原子写盘失败，返回 `Err` 阻止在不可恢复状态下盲目传数据。
    pub async fn update_session_uri(&self, task_id: &str, session_uri: &str) -> Result<()> {
        register_youtube_session_uri(session_uri);
        if !self.is_enabled() {
            return Ok(());
        }
        let _guard = self.file_lock.lock().await;
        let mut data = read_queue_file_strict(&self.queue_file)?;
        let existing = data
            .entries
            .iter_mut()
            .find(|e| e.task_id == task_id)
            .ok_or_else(|| {
                anyhow::anyhow!("Persisted upload task entry missing for task_id={task_id}")
            })?;
        existing.session_uri = Some(session_uri.to_string());
        existing.unconfirmed_full_upload = false;
        save_queue_file_strict(&self.queue_file, &data)
    }

    /// 标记任务处于“全部字节已发送但服务端持续返回 308 未确认 video_id”状态，保留会话信息供后续核实。
    pub async fn mark_state_unconfirmed(
        &self,
        task_id: &str,
        session_uri: Option<&str>,
    ) -> Result<()> {
        if let Some(uri) = session_uri {
            register_youtube_session_uri(uri);
        }
        if !self.is_enabled() {
            return Ok(());
        }
        let _guard = self.file_lock.lock().await;
        let mut data = read_queue_file_strict(&self.queue_file)?;
        if let Some(existing) = data.entries.iter_mut().find(|e| e.task_id == task_id) {
            if let Some(uri) = session_uri {
                existing.session_uri = Some(uri.to_string());
            }
            existing.unconfirmed_full_upload = true;
            save_queue_file_strict(&self.queue_file, &data)?;
        }
        Ok(())
    }

    /// 在任务成功完成、被手动 `/stop` 取消或确定性永久失败后移除持久化记录。
    pub async fn remove(&self, task_id: &str) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let _guard = self.file_lock.lock().await;
        let mut data = read_queue_file_strict(&self.queue_file)?;
        let before = data.entries.len();
        data.entries.retain(|e| e.task_id != task_id);
        if data.entries.len() != before {
            save_queue_file_strict(&self.queue_file, &data)?;
        }
        Ok(())
    }
}

fn read_queue_file_strict(path: &Path) -> Result<PersistedQueueFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            if text.trim().is_empty() {
                return Ok(PersistedQueueFile::default());
            }
            serde_json::from_str::<PersistedQueueFile>(&text).map_err(|e| {
                anyhow::anyhow!(
                    "Corrupt YouTube queue file ({}): {}",
                    path.display(),
                    redact_secrets(&e.to_string())
                )
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PersistedQueueFile::default()),
        Err(e) => Err(anyhow::anyhow!(
            "Failed to read YouTube queue file ({}): {}",
            path.display(),
            redact_secrets(&e.to_string())
        )),
    }
}

fn save_queue_file_strict(path: &Path, data: &PersistedQueueFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(data)
        .context("Failed to serialize YouTube upload queue")?;
    write_private_file_atomic(path, &bytes).map_err(|e| {
        anyhow::anyhow!(
            "Failed to persist YouTube upload queue ({}): {}",
            path.display(),
            redact_secrets(&e.to_string())
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_conservative_none_mode_never_persists_or_recovers() {
        let tmp = TempDir::new().unwrap();
        let qfile = tmp.path().join("yt_queue.json");
        let store = YoutubeQueueStore::new(YoutubeRecoveryMode::None, qfile.clone());

        store
            .upsert(PersistedUploadEntry {
                task_id: "t1".into(),
                file_path: PathBuf::from("/storage512/v1.mp4"),
                filename: "v1.mp4".into(),
                file_size: 1024,
                modified_nanos: Some(1),
                chat_id: 1,
                session_uri: None,
                unconfirmed_full_upload: false,
            })
            .await
            .unwrap();

        assert!(!qfile.exists());
        assert!(store.load_recoverable_entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_resume_mode_persists_and_removes_cleanly() {
        let tmp = TempDir::new().unwrap();
        let qfile = tmp.path().join("yt_queue.json");
        let store = YoutubeQueueStore::new(YoutubeRecoveryMode::Resume, qfile);

        store
            .upsert(PersistedUploadEntry {
                task_id: "t2".into(),
                file_path: PathBuf::from("/storage512/v2.mp4"),
                filename: "v2.mp4".into(),
                file_size: 2048,
                modified_nanos: Some(99),
                chat_id: 1,
                session_uri: None,
                unconfirmed_full_upload: false,
            })
            .await
            .unwrap();
        store
            .update_session_uri(
                "t2",
                "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&upload_id=fake_session_123",
            )
            .await
            .unwrap();

        let loaded = store.load_recoverable_entries().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].session_uri.is_some());

        store.remove("t2").await.unwrap();
        assert!(store.load_recoverable_entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_concurrent_cross_batch_mutations_never_lose_entries() {
        let tmp = TempDir::new().unwrap();
        let qfile = tmp.path().join("concurrent_queue.json");

        // 故意构造两个指向同一队列文件的 YoutubeQueueStore 实例，模拟跨批次并发访问
        let store_batch_a = Arc::new(YoutubeQueueStore::new(
            YoutubeRecoveryMode::Resume,
            qfile.clone(),
        ));
        let store_batch_b = Arc::new(YoutubeQueueStore::new(
            YoutubeRecoveryMode::Resume,
            qfile.clone(),
        ));

        let mut handles = Vec::new();
        for idx in 0..16usize {
            let store = if idx % 2 == 0 {
                Arc::clone(&store_batch_a)
            } else {
                Arc::clone(&store_batch_b)
            };
            handles.push(tokio::spawn(async move {
                let tid = format!("task_{idx}");
                store
                    .upsert(PersistedUploadEntry {
                        task_id: tid.clone(),
                        file_path: PathBuf::from(format!("/storage512/v_{idx}.mp4")),
                        filename: format!("v_{idx}.mp4"),
                        file_size: 4096 + idx as u64,
                        modified_nanos: Some(idx as u128),
                        chat_id: 1,
                        session_uri: None,
                        unconfirmed_full_upload: false,
                    })
                    .await
                    .unwrap();

                let fake_uri = format!(
                    "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&upload_id=fake_sess_{idx}"
                );
                store.update_session_uri(&tid, &fake_uri).await.unwrap();

                // 移除所有被 4 整除的任务，其余 12 个任务必须全部完好保留且带各自的 session_uri
                if idx % 4 == 0 {
                    store.remove(&tid).await.unwrap();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let remaining = store_batch_a.load_recoverable_entries().await.unwrap();
        assert_eq!(remaining.len(), 12);
        for idx in (0..16usize).filter(|i| i % 4 != 0) {
            let tid = format!("task_{idx}");
            let found = remaining.iter().find(|e| e.task_id == tid).unwrap();
            assert_eq!(
                found.session_uri.as_deref(),
                Some(
                    format!(
                        "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&upload_id=fake_sess_{idx}"
                    )
                    .as_str()
                )
            );
        }
    }
}
