//! 任务管理器。
//!
//! 负责独占任务的启动、互斥检查和 YouTube 上传池的管理。
//! 对应 Python 版本的 `start_long_task`、`get_active_task`、`get_youtube_uploads` 等函数。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info, warn};

use crate::core::state::{SharedState, TaskInfo, UploadTask};
use crate::errors::AppError;

/// 任务管理器，封装任务启动与状态检查逻辑。
pub struct TaskManager {
    state: SharedState,
}

impl TaskManager {
    /// 创建任务管理器实例。
    #[must_use]
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    /// 尝试启动一个独占任务。
    ///
    /// 独占任务规则：
    /// - 同一时间只允许一个独占任务运行
    /// - 若有 YouTube 上传任务正在运行，也不允许启动独占任务（避免文件冲突）
    ///
    /// # Arguments
    ///
    /// * `task_name` - 任务名称（用于显示和日志）
    /// * `task_factory` - 异步任务工厂函数
    ///
    /// # Errors
    ///
    /// 若已有独占任务或 YouTube 上传任务正在运行，返回对应错误。
    pub async fn start_exclusive<F, Fut>(&self, task_name: &str, task_factory: F) -> Result<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        // 检查互斥条件
        {
            let mut state = self.state.write().await;
            state.cleanup_active_task();
            state.cleanup_youtube_pool();

            if state.has_active_task() {
                let name = state.active_task_name().unwrap_or("运行中任务").to_string();
                return Err(AppError::TaskAlreadyRunning { task_name: name }.into());
            }

            let upload_count = state.active_youtube_count();
            if upload_count > 0 {
                return Err(AppError::YoutubeUploadBlocking {
                    count: upload_count,
                }
                .into());
            }

            state.cancel_flag = false;
        }

        let state_clone = Arc::clone(&self.state);
        let task_name_owned = task_name.to_string();
        let task_name_log = task_name.to_string();

        let handle = tokio::spawn(async move {
            info!(task = %task_name_owned, "Exclusive task started");
            match task_factory().await {
                Ok(()) => {
                    info!(task = %task_name_owned, "Exclusive task completed successfully");
                }
                Err(e) if e.downcast_ref::<AppError>() == Some(&AppError::Cancelled) => {
                    info!(task = %task_name_owned, "Exclusive task cancelled by user");
                }
                Err(e) => {
                    error!(task = %task_name_owned, error = %e, "Exclusive task failed");
                }
            }

            // 清理状态
            let mut state = state_clone.write().await;
            state.active_task = None;
            state.cancel_flag = false;
            state.current_process_pid = None;
        });

        // 写入任务信息
        {
            let mut state = self.state.write().await;
            state.active_task = Some(TaskInfo {
                name: task_name_log,
                handle,
                started_at: std::time::Instant::now(),
            });
        }

        Ok(())
    }

    /// 启动一个 YouTube 上传任务（加入上传池）。
    ///
    /// YouTube 上传任务与独占任务互斥：若有独占任务运行，不允许启动上传。
    /// 多个 YouTube 上传任务可并发，但受 Semaphore 限制最大并发数。
    ///
    /// # Arguments
    ///
    /// * `task_id` - 任务唯一 ID
    /// * `filename` - 文件名（用于显示）
    /// * `path` - 文件路径
    /// * `task_factory` - 接受取消信号接收端的异步任务工厂
    ///
    /// # Errors
    ///
    /// 若有独占任务正在运行，返回错误。
    pub async fn start_youtube_upload<F, Fut>(
        &self,
        task_id: String,
        filename: String,
        path: PathBuf,
        task_factory: F,
    ) -> Result<tokio::sync::watch::Receiver<bool>>
    where
        F: FnOnce(tokio::sync::watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        // 检查独占任务
        {
            let mut state = self.state.write().await;
            state.cleanup_active_task();

            if state.has_active_task() {
                let name = state.active_task_name().unwrap_or("运行中任务").to_string();
                return Err(AppError::TaskAlreadyRunning { task_name: name }.into());
            }
        }

        let (upload_task, cancel_rx) = UploadTask::new(filename.clone(), path);
        let cancel_rx_clone = cancel_rx.clone();

        let state_clone = Arc::clone(&self.state);
        let task_id_clone = task_id.clone();
        let filename_log = filename.clone();

        let handle = tokio::spawn(async move {
            info!(task_id = %task_id_clone, filename = %filename_log, "YouTube upload task started");
            match task_factory(cancel_rx_clone).await {
                Ok(()) => {
                    info!(task_id = %task_id_clone, "YouTube upload completed");
                }
                Err(e) => {
                    warn!(task_id = %task_id_clone, error = %e, "YouTube upload ended with error");
                }
            }

            // 从上传池移除
            let mut state = state_clone.write().await;
            state.youtube_pool.remove(&task_id_clone);
        });

        // 写入上传池
        {
            let mut state = self.state.write().await;
            let mut task = upload_task;
            task.handle = Some(handle);
            state.youtube_pool.insert(task_id, task);
        }

        Ok(cancel_rx)
    }

    /// 取消所有正在运行的任务。
    #[allow(dead_code)]
    pub async fn cancel_all(&self) {
        let mut state = self.state.write().await;
        state.cancel_all();
    }

    /// 检查是否有独占任务正在运行。
    #[allow(dead_code)]
    pub async fn has_active_task(&self) -> bool {
        let mut state = self.state.write().await;
        state.cleanup_active_task();
        state.has_active_task()
    }

    /// 获取当前独占任务名称。
    #[allow(dead_code)]
    pub async fn active_task_name(&self) -> Option<String> {
        let state = self.state.read().await;
        state.active_task_name().map(|s| s.to_string())
    }

    /// 获取活跃的 YouTube 上传任务数量。
    #[allow(dead_code)]
    pub async fn active_youtube_count(&self) -> usize {
        let mut state = self.state.write().await;
        state.cleanup_youtube_pool();
        state.active_youtube_count()
    }
}
