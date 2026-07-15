use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, watch};

#[derive(Clone)]
pub struct ActiveTaskInfo {
    pub name: String,
    pub cancel_tx: watch::Sender<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct YoutubeUploadInfo {
    pub filename: String,
    pub path: String,
    pub chat_id: i64,
    pub user_id: i64,
    pub created_at: f64,
    pub status: String,
    pub progress: f64,
    #[serde(skip)]
    pub cancel_tx: Option<watch::Sender<bool>>,
}

#[derive(Clone, Default)]
pub struct UserSession {
    pub current_dir: PathBuf,
    pub current_files: Vec<String>,
    pub selected: HashMap<String, std::collections::HashSet<usize>>,
    pub pending_delete: Vec<PathBuf>,
}

pub struct AppState {
    pub active_task: Mutex<Option<ActiveTaskInfo>>,
    pub youtube_uploads: Mutex<HashMap<String, YoutubeUploadInfo>>,
    pub youtube_semaphore: Arc<Semaphore>,
    pub user_sessions: Mutex<HashMap<i64, UserSession>>,
}

impl AppState {
    pub fn new(max_concurrent_uploads: usize) -> Arc<Self> {
        Arc::new(Self {
            active_task: Mutex::new(None),
            youtube_uploads: Mutex::new(HashMap::new()),
            youtube_semaphore: Arc::new(Semaphore::new(max_concurrent_uploads)),
            user_sessions: Mutex::new(HashMap::new()),
        })
    }

    pub async fn get_session(&self, user_id: i64, base_dir: &Path) -> UserSession {
        let mut sessions = self.user_sessions.lock().await;
        sessions.entry(user_id).or_insert_with(|| UserSession {
            current_dir: base_dir.to_path_buf(),
            ..Default::default()
        }).clone()
    }

    pub async fn update_session<F>(&self, user_id: i64, f: F)
    where
        F: FnOnce(&mut UserSession),
    {
        let mut sessions = self.user_sessions.lock().await;
        if let Some(session) = sessions.get_mut(&user_id) {
            f(session);
        }
    }

    pub async fn start_exclusive_task(&self, name: &str) -> Result<watch::Receiver<bool>, String> {
        let mut active = self.active_task.lock().await;
        if let Some(ref task) = *active {
            return Err(format!("已有独占任务正在运行：{}", task.name));
        }
        let uploads = self.youtube_uploads.lock().await;
        if !uploads.is_empty() {
            return Err(format!("当前有 {} 个 YouTube 上传任务，请等待完成或停用。", uploads.len()));
        }

        let (tx, rx) = watch::channel(false);
        *active = Some(ActiveTaskInfo {
            name: name.to_string(),
            cancel_tx: tx,
        });
        Ok(rx)
    }

    pub async fn stop_all_tasks(&self) -> Vec<String> {
        let mut stopped = Vec::new();
        let mut active = self.active_task.lock().await;
        if let Some(task) = active.take() {
            let _ = task.cancel_tx.send(true);
            stopped.push(format!("独占任务 [{}]", task.name));
        }
        let mut uploads = self.youtube_uploads.lock().await;
        for (_, info) in uploads.iter_mut() {
            if let Some(ref tx) = info.cancel_tx {
                let _ = tx.send(true);
                stopped.push(format!("上传任务 [{}]", info.filename));
            }
            info.status = "已手动终止".to_string();
        }
        stopped
    }
}

pub fn load_persisted_queue(path: &Path) -> HashMap<String, YoutubeUploadInfo> {
    if !path.exists() {
        return HashMap::new();
    }
    match fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn save_persisted_queue(path: &Path, queue: &HashMap<String, YoutubeUploadInfo>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(queue) {
        let temp_path = path.with_extension("tmp");
        if fs::write(&temp_path, data).is_ok() {
            let _ = fs::rename(&temp_path, path);
        }
    }
}