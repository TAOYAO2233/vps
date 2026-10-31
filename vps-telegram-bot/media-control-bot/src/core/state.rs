//! 全局应用状态定义。
//!
//! [`AppState`] 是整个 Bot 的核心状态容器，通过 [`SharedState`]（即
//! `Arc<RwLock<AppState>>`）在所有异步任务之间安全共享。
//!
//! 关键安全与一致性约束：
//! - 多选文件列表使用有序 `Vec<PathBuf>`，严格保留用户的点击选择顺序（禁止 `HashSet` 乱序或字母序重排）；
//! - 每次渲染文件选择器均生成带目录内容哈希的 [`MenuSnapshot`]，防止旧菜单消息或目录变动导致索引错位；
//! - 删除确认绑定唯一令牌与逐文件身份快照 [`PendingDeleteBatch`]，防止状态串扰与误删。

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::errors::AppError;
use crate::storage::disk::DiskMonitor;
use crate::storage::path::PathGuard;
use crate::storage::scanner::DirItem;
use crate::youtube::persistence::YoutubeQueueStore;

/// 全局共享状态的类型别名。
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
    /// 批量转封装 MP4
    Convert,
    /// 批量删除
    Delete,
}

impl std::str::FromStr for ActionType {
    type Err = AppError;

    /// 从字符串解析操作类型。
    ///
    /// # Errors
    ///
    /// 传入未知操作名时返回 [`AppError::InvalidArgument`]。
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "browse" => Ok(Self::Browse),
            "stream" => Ok(Self::Stream),
            "youtube" => Ok(Self::Youtube),
            "concat" => Ok(Self::Concat),
            "convert" => Ok(Self::Convert),
            "delete" => Ok(Self::Delete),
            other => Err(AppError::invalid_argument(format!(
                "Unknown action: {other}"
            ))),
        }
    }
}

impl ActionType {
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
            Self::Concat => "无损合并",
            Self::Convert => "无损转封装 MP4",
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

/// 文件选择器快照中的单个条目摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// 条目名（单层目录名或文件名）
    pub name: String,
    /// 是否为目录
    pub is_dir: bool,
    /// 渲染时的文件字节数（目录为 0）
    #[allow(dead_code)]
    pub size: u64,
}

impl From<&DirItem> for SnapshotEntry {
    fn from(item: &DirItem) -> Self {
        Self {
            name: item.name.clone(),
            is_dir: item.is_dir,
            size: item.size,
        }
    }
}

/// 计算目录路径与排序条目列表的稳定 32 位摘要（用于检测目录增删改名导致的菜单索引漂移）。
#[must_use]
pub fn compute_listing_hash(dir: &Path, items: &[SnapshotEntry]) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dir.hash(&mut hasher);
    items.len().hash(&mut hasher);
    for item in items {
        item.name.hash(&mut hasher);
        item.is_dir.hash(&mut hasher);
        item.size.hash(&mut hasher);
    }
    let full = hasher.finish();
    ((full >> 32) as u32) ^ (full as u32)
}

/// 文件选择器菜单快照（绑定 `callback_data` 中的 8 位十六进制 `snap_id`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuSnapshot {
    /// 8 位十六进制快照令牌（总回调长度恒小于 36 字节，符合 Telegram 64 字节上限）
    pub snap_id: String,
    /// 目录结构摘要哈希
    pub listing_hash: u32,
    /// 渲染该菜单时的操作类型
    pub action: ActionType,
    /// 渲染该菜单时的规范化目录路径
    pub dir: PathBuf,
    /// 渲染时的完整条目快照
    pub entries: Vec<SnapshotEntry>,
}

/// 待确认删除的单个文件身份快照（用于删除确认绑定与删除前二次核验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedFileTarget {
    /// 规范化真实文件路径（非符号链接）
    pub path: PathBuf,
    /// 生成确认框时的文件大小（字节）
    pub size: u64,
    /// 生成确认框时的修改时间戳（UNIX 纳秒）
    pub modified_nanos: Option<u128>,
    /// Unix 文件身份标识 `(dev, ino)`
    #[cfg(unix)]
    pub dev_ino: Option<(u64, u64)>,
}

impl ConfirmedFileTarget {
    /// 校验目标为 `BASE_DIR` 内的普通非软链文件，并捕获其当前身份快照。
    pub fn capture(guard: &PathGuard, path: &Path) -> Result<Self> {
        let canonical_path = guard.assert_regular_file_no_symlink(path)?;
        let meta = std::fs::symlink_metadata(&canonical_path).map_err(|e| AppError::FileIo {
            path: canonical_path.clone(),
            message: e.to_string(),
        })?;

        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());

        #[cfg(unix)]
        let dev_ino = {
            use std::os::unix::fs::MetadataExt;
            Some((meta.dev(), meta.ino()))
        };

        Ok(Self {
            path: canonical_path,
            size: meta.len(),
            modified_nanos,
            #[cfg(unix)]
            dev_ino,
        })
    }

    /// 在真正执行删除前二次核验路径边界、非软链普通文件类型及文件身份未发生变化。
    pub fn verify_unchanged(&self, guard: &PathGuard) -> Result<PathBuf> {
        let filename = self.display_name();
        let canonical_path = guard.assert_regular_file_no_symlink(&self.path)?;
        if canonical_path != self.path {
            return Err(AppError::SourceFileChanged { filename }.into());
        }

        let meta = std::fs::symlink_metadata(&canonical_path).map_err(|e| AppError::FileIo {
            path: canonical_path.clone(),
            message: e.to_string(),
        })?;

        let current_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());

        if meta.len() != self.size || current_mtime != self.modified_nanos {
            return Err(AppError::SourceFileChanged { filename }.into());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if self.dev_ino != Some((meta.dev(), meta.ino())) {
                return Err(AppError::SourceFileChanged { filename }.into());
            }
        }

        Ok(canonical_path)
    }

    /// 返回用于展示的文件名。
    #[must_use]
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }
}

/// 待确认删除批次（绑定唯一确认令牌与目标文件快照集合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDeleteBatch {
    /// 8 位十六进制确认令牌
    pub token: String,
    /// 待删除文件身份快照列表
    pub targets: Vec<ConfirmedFileTarget>,
}

/// 独占任务退出或被停止时的诊断与不可逆进度摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExclusiveStopSummary {
    /// 若为回收站任务：记录 `(已移动数量, 批次总数量, 预留字段)`。
    pub deleted_progress: Option<(usize, usize, u64)>,
    /// 若为批量转封装任务：记录 `(已成功发布数量, 批次总数量)`
    pub converted_progress: Option<(usize, usize)>,
    /// 子进程是否因优雅退出（`q`）超时而触发了 `SIGKILL` 强制兜底
    pub forced_kill_used: bool,
}

/// 独占任务信息。
///
/// 同一时间只允许一个独占任务运行（推流、合并、转封装、删除），
/// 每个独占任务均拥有独立的 `task_id` 与取消通道 `cancel_tx`，确保 `/stop` 不会误伤后续新启动的任务。
pub struct TaskInfo {
    /// 单调递增任务唯一 ID
    pub task_id: u64,
    /// 任务名称（用于显示）
    pub name: String,
    /// 任务的 JoinHandle，仅当子进程回收与临时目录清理全部完成后才会结束
    pub handle: JoinHandle<()>,
    /// 仅作用于本独占任务的取消信号发送端
    pub cancel_tx: tokio::sync::watch::Sender<bool>,
    /// 本任务的实时不可逆进度与子进程回收摘要
    pub summary: ExclusiveStopSummary,
    /// 任务启动时间
    pub started_at: Instant,
}

impl std::fmt::Debug for TaskInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskInfo")
            .field("task_id", &self.task_id)
            .field("name", &self.name)
            .field("summary", &self.summary)
            .field("started_at", &self.started_at)
            .finish()
    }
}

/// YouTube 上传任务信息。
#[derive(Debug)]
#[allow(dead_code)]
pub struct UploadTask {
    /// 文件名（用于显示）
    pub filename: String,
    /// 文件完整路径
    pub path: PathBuf,
    /// 当前状态描述
    pub status: String,
    /// 主进度（0.0 ~ 100.0，与服务端已确认进度保持一致）
    pub progress: f64,
    /// 本地已读取并发送进度（0.0 ~ 100.0）
    pub local_sent_percent: f64,
    /// YouTube 服务端已确认接收（`308 Range`）进度（0.0 ~ 100.0）
    pub server_confirmed_percent: f64,
    /// 上传完成后服务端返回的 `video_id`
    pub video_id: Option<String>,
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
            status: "排队等待信号量".to_string(),
            progress: 0.0,
            local_sent_percent: 0.0,
            server_confirmed_percent: 0.0,
            video_id: None,
            created_at: Instant::now(),
            cancel_tx,
            handle: None,
        };
        (task, cancel_rx)
    }

    /// 更新本地发送进度与服务端确认进度。
    pub fn update_progress(&mut self, local_sent_pct: f64, server_confirmed_pct: f64) {
        self.local_sent_percent = local_sent_pct.clamp(0.0, 100.0);
        self.server_confirmed_percent = server_confirmed_pct.clamp(0.0, 100.0);
        self.progress = self.server_confirmed_percent;
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

/// Recently completed task, retained in memory for the administrator's history view.
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub name: String,
    pub result: String,
    pub finished_at: String,
}

#[derive(Debug, Clone)]
pub struct MenuSession {
    pub current_dir: PathBuf,
    pub search_query: String,
    pub selections: HashMap<ActionType, Vec<PathBuf>>,
    pub current_snapshot: Option<MenuSnapshot>,
    pub pending_delete: Option<PendingDeleteBatch>,
}

/// 全局应用状态。
///
/// 所有字段均通过 `Arc<RwLock<AppState>>` 访问，保证线程安全。
#[derive(Debug)]
pub struct AppState {
    /// 当前浏览目录
    pub current_dir: PathBuf,

    /// Saved selector state for each Telegram menu message.
    pub menu_sessions: HashMap<(i64, i32), MenuSession>,
    pub menu_lock: Arc<Mutex<()>>,

    /// Case-insensitive filter for the current file selector.
    pub search_query: String,

    /// 当前运行中的独占任务（推流/合并/转封装/删除）
    pub active_task: Option<TaskInfo>,

    /// 兼容诊断标志：是否存在正在执行停止等待流程的协调器（期间互斥阻止启动新任务，避免连发 `/stop` 重复报成功）
    pub stop_in_progress: bool,

    /// 兼容旧状态查询的取消标志（实际取消信号均通过任务级 `watch::Sender<bool>` 精确投递）
    pub cancel_flag: bool,

    /// 单调递增独占任务序号
    next_task_seq: u64,

    /// 最近结束的独占任务摘要缓存（供 `/stop` 第二阶段读取确切的已删除数量等结果）
    completed_exclusive_summaries: HashMap<u64, ExclusiveStopSummary>,

    /// YouTube 上传任务池（key: task_id）
    pub youtube_pool: HashMap<String, UploadTask>,

    /// Most recent completed tasks, newest first.
    pub recent_tasks: VecDeque<TaskRecord>,

    /// YouTube 上传并发控制信号量（共享，不随状态重置）
    pub youtube_semaphore: Arc<Semaphore>,

    /// Serializes whole-batch capacity checks and enqueueing.
    pub youtube_enqueue_lock: Arc<Mutex<()>>,

    /// YouTube 上传池允许的最大任务总数（运行中 + 排队中）
    pub youtube_max_total_tasks: usize,

    /// 全进程共享的 YouTube 队列持久化存储实例（消除多批次并发写覆盖）
    pub youtube_queue_store: Arc<YoutubeQueueStore>,

    /// 各操作类型的有序文件选择列表（严格保留用户点击顺序）
    pub selections: HashMap<ActionType, Vec<PathBuf>>,

    /// 当前活跃的文件选择器菜单快照
    pub current_snapshot: Option<MenuSnapshot>,

    /// 菜单快照与确认令牌递增序号
    snapshot_seq: u32,

    /// 待确认删除批次（绑定确认令牌与文件身份快照）
    pub pending_delete: Option<PendingDeleteBatch>,

    /// 当前正在运行的子进程 PID（诊断用：便于与 `ps` 输出对应）
    pub current_process_pid: Option<u32>,

    /// `/storage512` 磁盘实时监测与三级报警协调器
    pub disk_monitor: Arc<DiskMonitor>,
}

impl AppState {
    /// 根据完整 [`Config`] 创建生产运行态初始状态。
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            current_dir: config.base_dir.clone(),
            menu_sessions: HashMap::new(),
            menu_lock: Arc::new(Mutex::new(())),
            search_query: String::new(),
            active_task: None,
            stop_in_progress: false,
            cancel_flag: false,
            next_task_seq: 0,
            completed_exclusive_summaries: HashMap::new(),
            youtube_pool: HashMap::new(),
            recent_tasks: VecDeque::new(),
            youtube_semaphore: Arc::new(Semaphore::new(config.youtube_max_concurrent_uploads)),
            youtube_enqueue_lock: Arc::new(Mutex::new(())),
            youtube_max_total_tasks: config.youtube_max_total_tasks(),
            youtube_queue_store: Arc::new(YoutubeQueueStore::from_config(config)),
            selections: HashMap::new(),
            current_snapshot: None,
            snapshot_seq: 0,
            pending_delete: None,
            current_process_pid: None,
            disk_monitor: Arc::new(DiskMonitor::from_config(config)),
        }
    }

    /// 创建基础初始状态（供单元测试或默认参数场景使用）。
    #[must_use]
    #[allow(dead_code)]
    pub fn new(base_dir: PathBuf, youtube_max_concurrent: usize) -> Self {
        Self {
            current_dir: base_dir,
            menu_sessions: HashMap::new(),
            menu_lock: Arc::new(Mutex::new(())),
            search_query: String::new(),
            active_task: None,
            stop_in_progress: false,
            cancel_flag: false,
            next_task_seq: 0,
            completed_exclusive_summaries: HashMap::new(),
            youtube_pool: HashMap::new(),
            recent_tasks: VecDeque::new(),
            youtube_semaphore: Arc::new(Semaphore::new(youtube_max_concurrent)),
            youtube_enqueue_lock: Arc::new(Mutex::new(())),
            youtube_max_total_tasks: youtube_max_concurrent.saturating_add(10),
            youtube_queue_store: Arc::new(YoutubeQueueStore::new_noop()),
            selections: HashMap::new(),
            current_snapshot: None,
            snapshot_seq: 0,
            pending_delete: None,
            current_process_pid: None,
            disk_monitor: Arc::new(DiskMonitor::new_default(PathBuf::from("/storage512"), 1)),
        }
    }

    /// 分配下一个独占任务唯一 ID。
    pub fn alloc_task_id(&mut self) -> u64 {
        self.next_task_seq = self.next_task_seq.wrapping_add(1);
        self.next_task_seq
    }

    pub fn record_task(&mut self, name: String, result: &str) {
        self.recent_tasks.push_front(TaskRecord {
            name,
            result: result.to_string(),
            finished_at: chrono::Local::now().format("%m-%d %H:%M").to_string(),
        });
        self.recent_tasks.truncate(30);
    }

    pub fn load_menu_session(&mut self, key: (i64, i32), base_dir: &Path) {
        let session = self.menu_sessions.get(&key).cloned().unwrap_or_else(|| MenuSession {
            current_dir: base_dir.to_path_buf(),
            search_query: String::new(),
            selections: HashMap::new(),
            current_snapshot: None,
            pending_delete: None,
        });
        self.current_dir = session.current_dir;
        self.search_query = session.search_query;
        self.selections = session.selections;
        self.current_snapshot = session.current_snapshot;
        self.pending_delete = session.pending_delete;
    }

    pub fn save_menu_session(&mut self, key: (i64, i32)) {
        if self.menu_sessions.len() >= 64 && !self.menu_sessions.contains_key(&key) {
            if let Some(old_key) = self.menu_sessions.keys().next().copied() {
                self.menu_sessions.remove(&old_key);
            }
        }
        self.menu_sessions.insert(key, MenuSession {
            current_dir: self.current_dir.clone(),
            search_query: self.search_query.clone(),
            selections: self.selections.clone(),
            current_snapshot: self.current_snapshot.clone(),
            pending_delete: self.pending_delete.clone(),
        });
    }

    /// 更新指定独占任务的删除不可逆进度 `(已删除数, 总数, 已释放字节)`。
    pub fn record_exclusive_deleted_progress(
        &mut self,
        task_id: u64,
        deleted: usize,
        total: usize,
        freed_bytes: u64,
    ) {
        if let Some(task) = self.active_task.as_mut().filter(|t| t.task_id == task_id) {
            task.summary.deleted_progress = Some((deleted, total, freed_bytes));
        }
    }

    /// 更新指定独占任务的批量转封装完成进度 `(已完成数, 总数)`。
    pub fn record_exclusive_converted_progress(
        &mut self,
        task_id: u64,
        completed: usize,
        total: usize,
    ) {
        if let Some(task) = self.active_task.as_mut().filter(|t| t.task_id == task_id) {
            task.summary.converted_progress = Some((completed, total));
        }
    }

    /// 记录指定独占任务在停止子进程时触发了超时 `SIGKILL` 强制兜底。
    pub fn record_exclusive_forced_kill(&mut self, task_id: u64) {
        if let Some(task) = self.active_task.as_mut().filter(|t| t.task_id == task_id) {
            task.summary.forced_kill_used = true;
        }
    }

    /// 在独占任务真正收尾完成后归档其摘要并释放独占槽位。
    pub fn finish_exclusive_task(&mut self, task_id: u64) {
        if self.active_task.as_ref().map(|t| t.task_id) == Some(task_id) {
            if let Some(finished) = self.active_task.take() {
                if self.completed_exclusive_summaries.len() >= 16 {
                    if let Some(&oldest_id) = self.completed_exclusive_summaries.keys().min() {
                        self.completed_exclusive_summaries.remove(&oldest_id);
                    }
                }
                self.completed_exclusive_summaries
                    .insert(task_id, finished.summary);
            }
            self.cancel_flag = false;
            self.current_process_pid = None;
        }
    }

    /// 取出已归档或仍在运行中的指定独占任务摘要。
    pub fn take_exclusive_summary(&mut self, task_id: u64) -> ExclusiveStopSummary {
        if let Some(summary) = self.completed_exclusive_summaries.remove(&task_id) {
            return summary;
        }
        self.active_task
            .as_ref()
            .filter(|t| t.task_id == task_id)
            .map(|t| t.summary.clone())
            .unwrap_or_default()
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

    /// 清理已完成的独占任务（同步归档其退出摘要）。
    pub fn cleanup_active_task(&mut self) {
        let finished_id = self
            .active_task
            .as_ref()
            .filter(|t| t.handle.is_finished())
            .map(|t| t.task_id);
        if let Some(tid) = finished_id {
            self.finish_exclusive_task(tid);
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

    /// 获取活跃（运行中或排队中）的 YouTube 上传任务数量。
    #[must_use]
    pub fn active_youtube_count(&self) -> usize {
        self.youtube_pool
            .values()
            .filter(|t| t.handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false))
            .count()
    }

    /// 切换指定文件的选中状态（严格保持用户首次选中的先后顺序）。
    ///
    /// 返回切换后该文件是否处于选中状态（`true` = 已追加选中，`false` = 已取消选中）。
    pub fn toggle_selection(&mut self, action: &ActionType, path: PathBuf) -> bool {
        let list = self.selections.entry(action.clone()).or_default();
        if let Some(pos) = list.iter().position(|p| p == &path) {
            list.remove(pos);
            false
        } else {
            list.push(path);
            true
        }
    }

    /// 查询文件在指定操作列表中的 1-based 选择序号（未选中返回 `None`）。
    #[must_use]
    #[allow(dead_code)]
    pub fn selection_order_index(&self, action: &ActionType, path: &Path) -> Option<usize> {
        self.selections
            .get(action)
            .and_then(|list| list.iter().position(|p| p == path))
            .map(|idx| idx + 1)
    }

    /// 获取指定操作类型的有序选中文件切片。
    #[must_use]
    pub fn selections_slice(&self, action: &ActionType) -> &[PathBuf] {
        self.selections
            .get(action)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 清空指定操作类型的选中文件列表。
    pub fn clear_selections(&mut self, action: &ActionType) {
        self.selections.remove(action);
    }

    /// 更新并记录当前文件选择器菜单快照，返回快照引用。
    ///
    /// 若操作模式、目录和目录条目哈希均未变化，则复用同一 `snap_id`，否则生成新 `snap_id`。
    pub fn update_menu_snapshot(
        &mut self,
        action: ActionType,
        dir: PathBuf,
        entries: Vec<SnapshotEntry>,
    ) -> MenuSnapshot {
        let listing_hash = compute_listing_hash(&dir, &entries);
        if let Some(existing) = &self.current_snapshot {
            if existing.action == action
                && existing.dir == dir
                && existing.listing_hash == listing_hash
            {
                return existing.clone();
            }
        }

        self.snapshot_seq = self.snapshot_seq.wrapping_add(1);
        let seq_part = (self.snapshot_seq & 0xFFFF) as u16;
        let hash_part = (listing_hash & 0xFFFF) as u16;
        let snap_id = format!("{seq_part:04x}{hash_part:04x}");

        let snapshot = MenuSnapshot {
            snap_id,
            listing_hash,
            action,
            dir,
            entries,
        };
        self.current_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// 创建并保存新的待确认删除批次，返回分配的 8 位十六进制令牌。
    pub fn set_pending_delete(&mut self, targets: Vec<ConfirmedFileTarget>) -> String {
        self.snapshot_seq = self.snapshot_seq.wrapping_add(1);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.snapshot_seq.hash(&mut hasher);
        for t in &targets {
            t.path.hash(&mut hasher);
            t.size.hash(&mut hasher);
        }
        let digest = (hasher.finish() & 0xFFFF) as u16;
        let seq_part = (self.snapshot_seq & 0xFFFF) as u16;
        let token = format!("{seq_part:04x}{digest:04x}");

        self.pending_delete = Some(PendingDeleteBatch {
            token: token.clone(),
            targets,
        });
        token
    }

    /// 原子校验并取出匹配令牌的待删除批次（防止重复点击或旧确认框误触发）。
    pub fn take_pending_delete(&mut self, token: &str) -> Option<PendingDeleteBatch> {
        let matches = self
            .pending_delete
            .as_ref()
            .map(|b| b.token == token)
            .unwrap_or(false);
        if matches {
            self.pending_delete.take()
        } else {
            None
        }
    }

    /// 取消当前所有正在运行或排队中的任务（向当前独占任务和所有 YouTube 上传任务发送各自作用域的取消信号）。
    pub fn cancel_all(&mut self) {
        if let Some(active) = &self.active_task {
            if !active.handle.is_finished() {
                self.cancel_flag = true;
                let _ = active.cancel_tx.send(true);
            }
        }
        for task in self.youtube_pool.values() {
            task.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
            let parsed: ActionType = s.parse().unwrap();
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
    fn test_ordered_selections_preserve_exact_user_click_order() {
        let mut state = make_state();
        let action = ActionType::Concat;
        // 故意先选 z_part1 再选 a_part2，验证绝不被字母序重排
        let first = PathBuf::from("/storage512/z_part1.flv");
        let second = PathBuf::from("/storage512/a_part2.flv");
        let third = PathBuf::from("/storage512/m_part3.flv");

        assert!(state.toggle_selection(&action, first.clone()));
        assert!(state.toggle_selection(&action, second.clone()));
        assert!(state.toggle_selection(&action, third.clone()));

        assert_eq!(
            state.selections_slice(&action),
            &[first.clone(), second.clone(), third.clone()]
        );
        assert_eq!(state.selection_order_index(&action, &first), Some(1));
        assert_eq!(state.selection_order_index(&action, &second), Some(2));
        assert_eq!(state.selection_order_index(&action, &third), Some(3));

        // 取消中间项后剩余顺序依然严格保持
        assert!(!state.toggle_selection(&action, second));
        assert_eq!(state.selections_slice(&action), &[first, third]);

        state.clear_selections(&action);
        assert!(state.selections_slice(&action).is_empty());
    }

    #[test]
    fn test_menu_snapshot_detects_directory_changes() {
        let mut state = make_state();
        let dir = PathBuf::from("/storage512/room1");
        let entries_v1 = vec![SnapshotEntry {
            name: "a.mp4".into(),
            is_dir: false,
            size: 100,
        }];
        let snap1 = state.update_menu_snapshot(ActionType::Delete, dir.clone(), entries_v1.clone());
        assert_eq!(snap1.snap_id.len(), 8);

        // 同一目录无增删变化时复用同一 snap_id
        let snap1_same = state.update_menu_snapshot(ActionType::Delete, dir.clone(), entries_v1);
        assert_eq!(snap1.snap_id, snap1_same.snap_id);

        // 目录新增文件时生成新 snap_id
        let entries_v2 = vec![
            SnapshotEntry {
                name: "a.mp4".into(),
                is_dir: false,
                size: 100,
            },
            SnapshotEntry {
                name: "b.mp4".into(),
                is_dir: false,
                size: 200,
            },
        ];
        let snap2 = state.update_menu_snapshot(ActionType::Delete, dir, entries_v2);
        assert_ne!(snap1.snap_id, snap2.snap_id);
    }

    #[test]
    fn test_confirmed_file_target_detects_tampering() {
        let tmp = TempDir::new().unwrap();
        let guard = PathGuard::new(tmp.path().to_path_buf());
        let f = tmp.path().join("seg1.mp4");
        std::fs::write(&f, b"hello").unwrap();

        let target = ConfirmedFileTarget::capture(&guard, &f).unwrap();
        assert!(target.verify_unchanged(&guard).is_ok());

        // 修改文件内容/长度后二次校验必须拒绝删除
        std::fs::write(&f, b"hello-modified-length").unwrap();
        assert!(target.verify_unchanged(&guard).is_err());
    }
}
