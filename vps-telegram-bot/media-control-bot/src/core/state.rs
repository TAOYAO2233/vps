//! 全局应用状态定义。
//!
//! [`AppState`] 是整个 Bot 的核心状态容器，通过 [`SharedState`]（即
//! `Arc<RwLock<AppState>>`）在所有异步任务之间安全共享。
//!
//! 与 Python 版本的 `context.user_data` Dict 相比，Rust 版本通过强类型结构体
//! 消除了运行时类型错误，并通过 `RwLock` 保证并发安全。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinHandle;

/// 全局共享状态的类型别名。
///
/// 使用 `Arc<RwLock<AppState>>` 实现：
/// - `Arc`：跨线程引用计数，允许多个所有者
/// - `RwLock`：允许多读单写，读操作不互斥，写操作独占
pub type SharedState = Arc<RwLock<AppState>>;

/// 操作类型枚举，对应文件选择器的各种操作模式。
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum ActionType {
    /// 浏览文件详情
    Browse,
    /// RTMP 推流
    Stream,
    /// YouTube 上传
    Youtube,
    /// 视频合并
    Concat,
    /// 批量转码
    Convert,
    /// 批量删除
    Delete,
}

impl ActionType {
    /// 从字符串解析操作类型。
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "browse" => Some(Self::Browse),
            "stream" => Some(Self::Stream),
            "youtube" => Some(Self::Youtube),
            "concat" => Some(Self::Concat),
            "convert" => Some(Self::Convert),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    /// 转换为字符串表示。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Browse => "browse",
            Self::Stream => "stream",
            Self::Youtube => "youtube",
            Self::Concat => "concat",
            Self::Convert => "convert",
            Self::Delete => "delete",
        }
    }

    /// 返回操作的中文显示名称。
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Browse => "浏览与查看详情",
            Self::Stream => "推流",
            Self::Youtube => "上传 YT",
            Self::Concat => "合并",
            Self::Convert => "转码 MP4",
            Self::Delete => "删除",
        }
    }

    /// 判断该操作是否为多选模式。
    #[must_use]
    pub fn is_multi_select(&self) -> bool {
        matches!(
            self,
            Self::Youtube | Self::Concat | Self::Convert | Self::Delete
        )
    }
}

/// 独占任务信息。
///
/// 同一时间只允许一个独占任务运行（推流、合并、转码、删除），
/// YouTube 上传使用独立的上传池，通过 Semaphore 控制并发数。
pub struct TaskInfo {
    /// 任务名称（用于显示）
    pub name: String,
    /// 任务的 JoinHandle，用于取消或等待
    pub handle: JoinHandle<()>,
    /// 任务启动时间
    pub started_at: Instant,
}

impl std::fmt::Debug for TaskInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskInfo")
            .field("name", &self.name)
            .field("started_at", &self.started_at)
            .finish()
    }
}

/// YouTube 上传任务信息。
#[derive(Debug)]
pub struct UploadTask {
    /// 文件名（用于显示）
    pub filename: String,
    /// 文件完整路径
    #[allow(dead_code)]
    pub path: PathBuf,
    /// 当前状态描述
    pub status: String,
    /// 上传进度（0.0 ~ 100.0）
    pub progress: f64,
    /// 任务创建时间
    pub created_at: Instant,
    /// 取消信号发送端
    pub cancel_tx: tokio::sync::watch::Sender<bool>,
    /// 任务的 JoinHandle
    pub handle: Option<JoinHandle<()>>,
}

impl UploadTask {
    /// 创建新的上传任务。
    #[must_use]
    pub fn new(filename: String, path: PathBuf) -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let task = Self {
            filename,
            path,
            status: "排队中".to_string(),
            progress: 0.0,
            created_at: Instant::now(),
            cancel_tx,
            handle: None,
        };
        (task, cancel_rx)
    }

    /// 发送取消信号。
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// 返回已运行时间（秒）。
    #[must_use]
    pub fn elapsed_secs(&self) -> f64 {
        self.created_at.elapsed().as_secs_f64()
    }
}

/// 全局应用状态。
///
/// 所有字段均通过 `Arc<RwLock<AppState>>` 访问，保证线程安全。
#[derive(Debug)]
pub struct AppState {
    /// 当前浏览目录
    pub current_dir: PathBuf,

    /// 当前运行中的独占任务（推流/合并/转码/删除）
    pub active_task: Option<TaskInfo>,

    /// 取消标志：设置为 true 时，所有正在运行的独占任务应尽快退出
    pub cancel_flag: bool,

    /// YouTube 上传任务池（key: task_id）
    pub youtube_pool: HashMap<String, UploadTask>,

    /// YouTube 上传并发控制信号量（共享，不随状态重置）
    pub youtube_semaphore: Arc<Semaphore>,

    /// 各操作类型的文件选择集合（key: ActionType，value: 选中的文件路径集合）
    pub selections: HashMap<ActionType, HashSet<PathBuf>>,

    /// 当前目录下的文件/目录列表缓存（用于通过索引查找文件名）
    pub current_files: Vec<String>,

    /// 待确认删除的文件列表
    pub pending_delete_files: Vec<PathBuf>,

    /// 当前正在运行的子进程 PID（用于强制终止）
    pub current_process_pid: Option<u32>,
}

impl AppState {
    /// 创建初始状态。
    ///
    /// # Arguments
    ///
    /// * `base_dir` - 媒体文件根目录
    /// * `youtube_max_concurrent` - YouTube 最大并发上传数
    pub fn new(base_dir: PathBuf, youtube_max_concurrent: usize) -> Self {
        Self {
            current_dir: base_dir,
            active_task: None,
            cancel_flag: false,
            youtube_pool: HashMap::new(),
            youtube_semaphore: Arc::new(Semaphore::new(youtube_max_concurrent)),
            selections: HashMap::new(),
            current_files: Vec::new(),
            pending_delete_files: Vec::new(),
            current_process_pid: None,
        }
    }

    /// 创建共享状态（包装为 `Arc<RwLock<AppState>>`）。
    #[must_use]
    pub fn into_shared(self) -> SharedState {
        Arc::new(RwLock::new(self))
    }

    /// 判断当前是否有独占任务正在运行。
    #[must_use]
    pub fn has_active_task(&self) -> bool {
        self.active_task
            .as_ref()
            .map(|t| !t.handle.is_finished())
            .unwrap_or(false)
    }

    /// 获取当前独占任务名称。
    #[must_use]
    pub fn active_task_name(&self) -> Option<&str> {
        if self.has_active_task() {
            self.active_task.as_ref().map(|t| t.name.as_str())
        } else {
            None
        }
    }

    /// 清理已完成的独占任务。
    pub fn cleanup_active_task(&mut self) {
        if let Some(task) = &self.active_task {
            if task.handle.is_finished() {
                self.active_task = None;
            }
        }
    }

    /// 清理已完成的 YouTube 上传任务。
    pub fn cleanup_youtube_pool(&mut self) {
        self.youtube_pool.retain(|_, task| {
            task.handle
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false)
        });
    }

    /// 获取活跃的 YouTube 上传任务数量。
    #[must_use]
    pub fn active_youtube_count(&self) -> usize {
        self.youtube_pool
            .values()
            .filter(|t| t.handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false))
            .count()
    }

    /// 获取指定操作类型的选中文件集合（可变引用）。
    pub fn selections_mut(&mut self, action: &ActionType) -> &mut HashSet<PathBuf> {
        self.selections.entry(action.clone()).or_default()
    }

    /// 获取指定操作类型的选中文件集合（只读引用）。
    #[must_use]
    pub fn selections_ref(&self, action: &ActionType) -> Option<&HashSet<PathBuf>> {
        self.selections.get(action)
    }

    /// 清空指定操作类型的选中文件集合。
    pub fn clear_selections(&mut self, action: &ActionType) {
        self.selections.remove(action);
    }

    /// 取消所有正在运行的任务（设置 cancel_flag，终止子进程，通知 YouTube 上传）。
    pub fn cancel_all(&mut self) {
        self.cancel_flag = true;

        // 终止子进程（通过 kill 命令调用，避免 unsafe 块）
        if let Some(pid) = self.current_process_pid {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .spawn();
            }
            #[cfg(windows)]
            {
                // Windows 下通过 taskkill 终止
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .spawn();
            }
        }

        // 通知所有 YouTube 上传任务取消
        for task in self.youtube_pool.values() {
            task.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AppState {
        AppState::new(PathBuf::from("/tmp"), 2)
    }

    #[test]
    fn test_action_type_roundtrip() {
        for action in [
            ActionType::Browse,
            ActionType::Stream,
            ActionType::Youtube,
            ActionType::Concat,
            ActionType::Convert,
            ActionType::Delete,
        ] {
            let s = action.as_str();
            let parsed = ActionType::from_str(s).unwrap();
            assert_eq!(action, parsed);
        }
    }

    #[test]
    fn test_action_type_multi_select() {
        assert!(!ActionType::Browse.is_multi_select());
        assert!(!ActionType::Stream.is_multi_select());
        assert!(ActionType::Youtube.is_multi_select());
        assert!(ActionType::Concat.is_multi_select());
        assert!(ActionType::Convert.is_multi_select());
        assert!(ActionType::Delete.is_multi_select());
    }

    #[test]
    fn test_initial_state() {
        let state = make_state();
        assert!(!state.has_active_task());
        assert_eq!(state.active_youtube_count(), 0);
        assert!(!state.cancel_flag);
    }

    #[test]
    fn test_selections() {
        let mut state = make_state();
        let action = ActionType::Convert;
        let path = PathBuf::from("/tmp/video.mp4");

        state.selections_mut(&action).insert(path.clone());
        assert!(state.selections_ref(&action).unwrap().contains(&path));

        state.clear_selections(&action);
        assert!(state.selections_ref(&action).is_none());
    }
}
