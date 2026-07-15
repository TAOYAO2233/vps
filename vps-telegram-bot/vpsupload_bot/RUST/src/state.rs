use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{Mutex, Semaphore, Notify};

pub struct ActiveTask {
    pub name: String,
    pub cancel_flag: Arc<AtomicBool>,
    pub cancel_notify: Arc<Notify>,
}

#[derive(Clone)]
pub struct UserSession {
    pub current_dir: PathBuf,
    pub current_files: Vec<String>,
    pub selected_youtube: HashSet<usize>,
    pub selected_concat: HashSet<usize>,
    pub selected_convert: HashSet<usize>,
    pub selected_delete: HashSet<usize>,
    pub pending_delete_files: Vec<PathBuf>,
    pub progress_bar_theme: usize, // <--- 新增：0=科幻方块, 1=彩色水果, 2=简约细线
}

impl UserSession {
    pub fn new(default_dir: PathBuf) -> Self {
        Self {
            current_dir: default_dir,
            current_files: Vec::new(),
            selected_youtube: HashSet::new(),
            selected_concat: HashSet::new(),
            selected_convert: HashSet::new(),
            selected_delete: HashSet::new(),
            pending_delete_files: Vec::new(),
            progress_bar_theme: 0, // 默认使用科幻方块
        }
    }
    
    pub fn get_selected(&mut self, action: &str) -> &mut HashSet<usize> {
        match action {
            "youtube" => &mut self.selected_youtube,
            "concat" => &mut self.selected_concat,
            "convert" => &mut self.selected_convert,
            "delete" => &mut self.selected_delete,
            _ => &mut self.selected_youtube,
        }
    }

    pub fn clear_all_selected(&mut self) {
        self.selected_youtube.clear();
        self.selected_concat.clear();
        self.selected_convert.clear();
        self.selected_delete.clear();
        self.pending_delete_files.clear();
    }
}

pub struct AppState {
    pub config: crate::config::AppConfig,
    pub active_task: Mutex<Option<ActiveTask>>,
    pub youtube_semaphore: Arc<Semaphore>,
    pub youtube_uploads: Mutex<HashMap<String, YoutubeUploadInfo>>,
    pub sessions: Mutex<HashMap<i64, UserSession>>,
}

#[derive(Clone)]
pub struct YoutubeUploadInfo {
    pub filename: String,
    pub filepath: PathBuf,
    pub status: String,
    pub progress: f64,
    pub cancel_flag: Arc<AtomicBool>,
    pub cancel_notify: Arc<Notify>,
    pub created_at: f64,
}

impl AppState {
    pub fn new(config: crate::config::AppConfig) -> Self {
        let max_uploads = config.youtube_max_concurrent_uploads;
        Self {
            config,
            active_task: Mutex::new(None),
            youtube_semaphore: Arc::new(Semaphore::new(max_uploads)),
            youtube_uploads: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_session(&self, user_id: i64) -> UserSession {
        let mut map = self.sessions.lock().await;
        map.entry(user_id)
            .or_insert_with(|| UserSession::new(self.config.base_dir.clone()))
            .clone()
    }

    pub async fn save_session(&self, user_id: i64, session: UserSession) {
        let mut map = self.sessions.lock().await;
        map.insert(user_id, session);
    }
}