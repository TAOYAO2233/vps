//! 任务管理器。
//!
//! 负责独占任务的启动、异常退出/panic 安全回收、互斥检查、YouTube 有界上传池管理，
//! 以及 `/stop` 与 `SIGINT`/`SIGTERM` 共用的两阶段实际退出确认协调。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{error, info, warn};

use crate::core::state::{ExclusiveStopSummary, SharedState, TaskInfo, UploadTask};
use crate::errors::AppError;
use crate::utils::redact::redact_secrets;

/// 单次停止请求锁定的待退出目标快照（仅针对发起停止时尚在运行的任务，绝不误取消后续新任务）。
#[derive(Debug, Clone)]
pub struct StopTicket {
    /// 待停止的独占任务 `(task_id, task_name)`
    pub exclusive_target: Option<(u64, String)>,
    /// 待停止的 YouTube 任务列表 `(task_id, filename)`
    pub youtube_targets: Vec<(String, String)>,
}

/// 发起 `/stop` 时的第一阶段判定结果。
#[cfg(test)]
#[derive(Debug, Clone)]
pub enum StopInitiation {
    /// 当前没有任何运行中或排队中的任务
    NoActiveTasks,
    /// 已有正在等待任务退出的 `/stop` 流程（幂等返回当前等待信息，避免连发 `/stop` 重复报成功）
    AlreadyStopping {
        /// 当前仍在退出中的独占任务名
        exclusive_name: Option<String>,
        /// 当前仍在退出中的 YouTube 任务数
        youtube_count: usize,
    },
    /// 成功发出取消信号并获得唯一等待票据
    Started(StopTicket),
}

/// 第二阶段等待任务真正退出、回收子进程与清理临时目录后的核实报告。
#[derive(Debug, Clone, Default)]
pub struct StopCompletionReport {
    /// 发起时是否存在待停止任务
    pub had_tasks: bool,
    /// 已确认完全退出并完成清理的独占任务 `(任务名, 退出摘要)`
    pub stopped_exclusive: Option<(String, ExclusiveStopSummary)>,
    /// 已确认退出的 YouTube 上传/排队任务文件名列表
    pub stopped_youtube: Vec<String>,
    /// 超过等待预算仍未确认退出的独占任务 `(任务名, 当前子进程 PID, 当前摘要)`（保持互斥锁不提前释放）
    pub timed_out_exclusive: Option<(String, Option<u32>, ExclusiveStopSummary)>,
    /// 超过等待预算仍未确认退出的 YouTube 任务文件名列表
    pub timed_out_youtube: Vec<String>,
}

impl StopCompletionReport {
    /// 所有目标任务是否均已确认真正退出并完成收尾。
    #[must_use]
    pub fn all_exited(&self) -> bool {
        self.timed_out_exclusive.is_none() && self.timed_out_youtube.is_empty()
    }
}

/// 确保等待器在异常取消或 panic 时也会复位 `stop_in_progress` 标志的守卫。
struct StopInProgressGuard {
    state: SharedState,
    armed: bool,
}

impl StopInProgressGuard {
    fn new(state: SharedState) -> Self {
        Self { state, armed: true }
    }

    async fn finish(&mut self) {
        if self.armed {
            self.armed = false;
            self.state.write().await.stop_in_progress = false;
        }
    }
}

impl Drop for StopInProgressGuard {
    fn drop(&mut self) {
        if self.armed {
            let state = Arc::clone(&self.state);
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    state.write().await.stop_in_progress = false;
                });
            }
        }
    }
}

/// 任务管理器，封装任务启动、队列上限校验与实际完成确认停机逻辑。
pub struct TaskManager {
    state: SharedState,
}

impl TaskManager {
    /// 创建任务管理器实例。
    #[must_use]
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    /// Cancel exactly one YouTube task, leaving other uploads untouched.
    pub async fn request_stop_youtube(&self, task_id: &str) -> bool {
        let state = self.state.read().await;
        if let Some(task) = state.youtube_pool.get(task_id) {
            task.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel the currently active exclusive task only when its ID matches.
    pub async fn request_stop_exclusive(&self, task_id: u64) -> bool {
        let mut state = self.state.write().await;
        if let Some(task) = &state.active_task {
            if task.task_id == task_id && !task.handle.is_finished() {
                let _ = task.cancel_tx.send(true);
                state.cancel_flag = true;
                return true;
            }
        }
        false
    }

    /// 尝试启动一个独占任务。
    ///
    /// 独占任务规则：
    /// - 若正在等待旧任务停止退出（`stop_in_progress`），拒绝启动新任务；
    /// - 同一时间只允许一个独占任务运行；
    /// - 若有 YouTube 上传任务正在运行或排队，也不允许启动独占任务（避免文件读写冲突）；
    /// - 每个独占任务均分配独立的 `(task_id, cancel_rx)`，并通过双层 `tokio::spawn` 监护：
    ///   只有内部任务完成全部子进程回收与临时目录清理后，才会释放独占状态槽位。
    pub async fn start_exclusive<F, Fut>(&self, task_name: &str, task_factory: F) -> Result<()>
    where
        F: FnOnce(u64, tokio::sync::watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let state_clone = Arc::clone(&self.state);
        let mut state = self.state.write().await;

        state.cleanup_active_task();
        state.cleanup_youtube_pool();

        if state.stop_in_progress {
            return Err(AppError::TaskStopping.into());
        }

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

        let task_id = state.alloc_task_id();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        state.cancel_flag = false;

        let task_name_owned = task_name.to_string();
        let task_name_log = task_name.to_string();

        let handle = tokio::spawn(async move {
            info!(task_id = task_id, task = %task_name_owned, "Exclusive task started");
            let inner = tokio::spawn(task_factory(task_id, cancel_rx));
            let result_label = match inner.await {
                Ok(Ok(())) => {
                    info!(task_id = task_id, task = %task_name_owned, "Exclusive task completed");
                    "成功"
                }
                Ok(Err(e)) if e.downcast_ref::<AppError>() == Some(&AppError::Cancelled) => {
                    info!(task_id = task_id, task = %task_name_owned, "Exclusive task stopped cleanly");
                    "已取消"
                }
                Ok(Err(e)) => {
                    let safe_err = redact_secrets(&e.to_string());
                    error!(task_id = task_id, task = %task_name_owned, error = %safe_err, "Exclusive task failed");
                    "失败"
                }
                Err(join_err) => {
                    let safe_err = redact_secrets(&join_err.to_string());
                    error!(
                        task_id = task_id,
                        task = %task_name_owned,
                        error = %safe_err,
                        "Exclusive task terminated unexpectedly"
                    );
                    "异常退出"
                }
            };

            // 仅在内部逻辑、子进程回收、网络中断及临时目录 Drop 清理全部结束后，才归档摘要并释放互斥锁
            let mut state = state_clone.write().await;
            state.record_task(task_name_owned, result_label);
            state.finish_exclusive_task(task_id);
        });

        state.active_task = Some(TaskInfo {
            task_id,
            name: task_name_log,
            handle,
            cancel_tx,
            summary: ExclusiveStopSummary::default(),
            started_at: Instant::now(),
        });

        Ok(())
    }

    /// 预检批量 YouTube 上传请求是否满足停止状态互斥、独占任务互斥和队列总容量上限。
    pub async fn check_youtube_batch_capacity(&self, requested_count: usize) -> Result<()> {
        let mut state = self.state.write().await;
        state.cleanup_active_task();
        state.cleanup_youtube_pool();

        if state.stop_in_progress {
            return Err(AppError::TaskStopping.into());
        }

        if state.has_active_task() {
            let name = state.active_task_name().unwrap_or("运行中任务").to_string();
            return Err(AppError::TaskAlreadyRunning { task_name: name }.into());
        }

        let current = state.active_youtube_count();
        let max_total = state.youtube_max_total_tasks;
        if current.saturating_add(requested_count) > max_total {
            return Err(AppError::YoutubeQueueFull {
                current,
                requested: requested_count,
                max_total,
            }
            .into());
        }

        Ok(())
    }

    /// 启动一个 YouTube 上传任务（原子校验停止状态、独占互斥与队列上限后加入上传池）。
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
        let state_clone = Arc::clone(&self.state);
        let mut state = self.state.write().await;
        state.cleanup_active_task();
        state.cleanup_youtube_pool();

        if state.stop_in_progress {
            return Err(AppError::TaskStopping.into());
        }

        if state.has_active_task() {
            let name = state.active_task_name().unwrap_or("运行中任务").to_string();
            return Err(AppError::TaskAlreadyRunning { task_name: name }.into());
        }

        let current = state.active_youtube_count();
        let max_total = state.youtube_max_total_tasks;
        if state.youtube_pool.contains_key(&task_id) {
            return Err(AppError::TaskAlreadyRunning { task_name: filename }.into());
        }
        if current >= max_total {
            return Err(AppError::YoutubeQueueFull {
                current,
                requested: 1,
                max_total,
            }
            .into());
        }

        let (upload_task, cancel_rx) = UploadTask::new(filename.clone(), path);
        let cancel_rx_clone = cancel_rx.clone();

        let task_id_clone = task_id.clone();
        let filename_log = filename;

        let handle = tokio::spawn(async move {
            info!(task_id = %task_id_clone, filename = %filename_log, "YouTube upload task started");
            let inner = tokio::spawn(task_factory(cancel_rx_clone));
            let result_label = match inner.await {
                Ok(Ok(())) => {
                    info!(task_id = %task_id_clone, "YouTube upload completed");
                    "成功"
                }
                Ok(Err(e))
                    if matches!(
                        e.downcast_ref::<AppError>(),
                        Some(AppError::Cancelled | AppError::YoutubeUploadCancelled { .. })
                    ) =>
                {
                    info!(task_id = %task_id_clone, "YouTube upload cancelled by user");
                    "已取消"
                }
                Ok(Err(e)) => {
                    let safe_err = redact_secrets(&e.to_string());
                    warn!(task_id = %task_id_clone, error = %safe_err, "YouTube upload ended with error");
                    "失败"
                }
                Err(join_err) => {
                    let safe_err = redact_secrets(&join_err.to_string());
                    error!(task_id = %task_id_clone, error = %safe_err, "YouTube upload task panicked");
                    "异常退出"
                }
            };

            // 无论成败或异常均在网络连接与辅助进度协程收尾后从上传池移除
            let mut state = state_clone.write().await;
            state.record_task(format!("YouTube: {filename_log}"), result_label);
            state.youtube_pool.remove(&task_id_clone);
        });

        let mut task = upload_task;
        task.handle = Some(handle);
        state.youtube_pool.insert(task_id, task);

        Ok(cancel_rx)
    }

    /// 第一阶段：原子检查并向当前活跃任务发送取消信号（幂等保护连续 `/stop`）。
    #[cfg(test)]
    pub async fn initiate_stop(&self) -> StopInitiation {
        let mut state = self.state.write().await;
        state.cleanup_active_task();
        state.cleanup_youtube_pool();

        let has_active = state.has_active_task();
        let yt_count = state.active_youtube_count();

        if !has_active && yt_count == 0 {
            state.stop_in_progress = false;
            return StopInitiation::NoActiveTasks;
        }

        if state.stop_in_progress {
            // 幂等重申取消信号，不重复创建第二个等待报告协程
            state.cancel_all();
            return StopInitiation::AlreadyStopping {
                exclusive_name: state.active_task_name().map(str::to_string),
                youtube_count: yt_count,
            };
        }

        state.stop_in_progress = true;
        let ticket = collect_and_signal_targets(&mut state);
        StopInitiation::Started(ticket)
    }

    /// 第二阶段：等待票据锁定的任务真正退出、回收子进程与清理临时目录。
    ///
    /// 若超时仍有任务未退出，**绝不提前清空** `active_task` 或 `youtube_pool`，确保互斥保护持续生效，
    /// 并如实将未退出任务与 PID 填入 [`StopCompletionReport`]。
    pub async fn wait_for_stop_ticket(
        &self,
        ticket: StopTicket,
        timeout: Duration,
    ) -> StopCompletionReport {
        let mut stop_guard = StopInProgressGuard::new(Arc::clone(&self.state));
        let deadline = Instant::now() + timeout;

        loop {
            let any_remaining = {
                let mut state = self.state.write().await;
                state.cleanup_active_task();
                state.cleanup_youtube_pool();

                let excl_running = ticket.exclusive_target.as_ref().is_some_and(|(tid, _)| {
                    state
                        .active_task
                        .as_ref()
                        .is_some_and(|t| t.task_id == *tid && !t.handle.is_finished())
                });

                let yt_running = ticket.youtube_targets.iter().any(|(yt_id, _)| {
                    state.youtube_pool.get(yt_id).is_some_and(|t| {
                        t.handle.as_ref().is_some_and(|h| !h.is_finished())
                    })
                });

                excl_running || yt_running
            };

            if !any_remaining || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let report = {
            let mut state = self.state.write().await;
            state.cleanup_active_task();
            state.cleanup_youtube_pool();

            let mut stopped_exclusive = None;
            let mut timed_out_exclusive = None;
            if let Some((tid, name)) = ticket.exclusive_target {
                let still_active = state
                    .active_task
                    .as_ref()
                    .is_some_and(|t| t.task_id == tid && !t.handle.is_finished());
                if still_active {
                    let pid = state.current_process_pid;
                    let summary = state
                        .active_task
                        .as_ref()
                        .map(|t| t.summary.clone())
                        .unwrap_or_default();
                    timed_out_exclusive = Some((name, pid, summary));
                } else {
                    let summary = state.take_exclusive_summary(tid);
                    stopped_exclusive = Some((name, summary));
                }
            }

            let mut stopped_youtube = Vec::new();
            let mut timed_out_youtube = Vec::new();
            for (yt_id, fname) in ticket.youtube_targets {
                let still_active = state
                    .youtube_pool
                    .get(&yt_id)
                    .is_some_and(|t| t.handle.as_ref().is_some_and(|h| !h.is_finished()));
                if still_active {
                    timed_out_youtube.push(fname);
                } else {
                    stopped_youtube.push(fname);
                }
            }

            StopCompletionReport {
                had_tasks: true,
                stopped_exclusive,
                stopped_youtube,
                timed_out_exclusive,
                timed_out_youtube,
            }
        };

        stop_guard.finish().await;
        report
    }

    /// 供进程停机信号（`SIGINT` / `SIGTERM`）与测试调用：对全部活跃任务发出取消并在有限时间内核实真实退出状态。
    pub async fn cancel_and_wait_all(&self, timeout: Duration) -> StopCompletionReport {
        let ticket = {
            let mut state = self.state.write().await;
            state.cleanup_active_task();
            state.cleanup_youtube_pool();
            if !state.has_active_task() && state.active_youtube_count() == 0 {
                state.stop_in_progress = false;
                return StopCompletionReport::default();
            }
            state.stop_in_progress = true;
            collect_and_signal_targets(&mut state)
        };

        self.wait_for_stop_ticket(ticket, timeout).await
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
        state.active_task_name().map(str::to_string)
    }

    /// 获取活跃的 YouTube 上传任务数量。
    #[allow(dead_code)]
    pub async fn active_youtube_count(&self) -> usize {
        let mut state = self.state.write().await;
        state.cleanup_youtube_pool();
        state.active_youtube_count()
    }
}

fn collect_and_signal_targets(state: &mut crate::core::state::AppState) -> StopTicket {
    let exclusive_target = state
        .active_task
        .as_ref()
        .filter(|t| !t.handle.is_finished())
        .map(|t| {
            let _ = t.cancel_tx.send(true);
            (t.task_id, t.name.clone())
        });
    if exclusive_target.is_some() {
        state.cancel_flag = true;
    }

    let mut youtube_targets = Vec::new();
    for (yt_id, task) in &state.youtube_pool {
        if task.handle.as_ref().is_some_and(|h| !h.is_finished()) {
            task.cancel();
            youtube_targets.push((yt_id.clone(), task.filename.clone()));
        }
    }
    youtube_targets.sort_by(|a, b| a.0.cmp(&b.0));

    StopTicket {
        exclusive_target,
        youtube_targets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> TaskManager {
        let state = crate::core::state::AppState::new(PathBuf::from("."), 2).into_shared();
        TaskManager::new(state)
    }

    #[tokio::test]
    async fn test_start_exclusive_rejects_second_task() {
        let manager = make_manager();

        manager
            .start_exclusive("任务 A", |_, _| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok::<(), anyhow::Error>(())
            })
            .await
            .unwrap();

        let second = manager
            .start_exclusive("任务 B", |_, _| async { Ok::<(), anyhow::Error>(()) })
            .await;

        let err = second.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<AppError>(),
            Some(AppError::TaskAlreadyRunning { .. })
        ));
    }

    #[tokio::test]
    async fn test_start_exclusive_recovers_after_completion_and_panic() {
        let manager = make_manager();

        manager
            .start_exclusive("异常任务", |_, _| async {
                panic!("simulated task panic");
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(manager
            .start_exclusive("正常任务 B", |_, _| async { Ok::<(), anyhow::Error>(()) })
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_two_phase_stop_idempotency_timeout_lock_and_new_task_isolation() {
        let state = crate::core::state::AppState::new(PathBuf::from("."), 1).into_shared();
        let manager = TaskManager::new(Arc::clone(&state));
        let (release_hung_tx, mut release_hung_rx) = tokio::sync::watch::channel(false);

        // 模拟一个收到取消后仍短时阻塞清理的删除任务（记录已删除 3/5 个文件）
        let state_in_task = Arc::clone(&state);
        manager
            .start_exclusive("删除文件", move |tid, mut cancel_rx| async move {
                state_in_task
                    .write()
                    .await
                    .record_exclusive_deleted_progress(tid, 3, 5, 1024);
                let _ = cancel_rx.changed().await;
                // 等待外部放行，模拟清理耗时超过第一次短超时
                let _ = release_hung_rx.changed().await;
                Err(AppError::Cancelled.into())
            })
            .await
            .unwrap();

        // 第 1 次 /stop 获得 Started 票据；连续第 2 次 /stop 必须返回 AlreadyStopping 而非重复创建等待器
        let first = manager.initiate_stop().await;
        let StopInitiation::Started(ticket) = first else {
            panic!("Expected StopInitiation::Started");
        };

        // 停止等待进行中时尝试启动新独占任务必须被 TaskStopping 互斥拒绝
        let blocked = manager
            .start_exclusive("新任务", |_, _| async { Ok(()) })
            .await;
        assert!(matches!(
            blocked.unwrap_err().downcast_ref::<AppError>(),
            Some(AppError::TaskStopping)
        ));

        let second = manager.initiate_stop().await;
        assert!(matches!(second, StopInitiation::AlreadyStopping { .. }));

        // 若等待超时（60ms），必须如实报告未确认退出且保持互斥锁不提前释放
        let timed_out_report = manager
            .wait_for_stop_ticket(ticket, Duration::from_millis(60))
            .await;
        assert!(!timed_out_report.all_exited());
        assert!(timed_out_report.timed_out_exclusive.is_some());
        assert!(manager.has_active_task().await);

        // 放行旧任务完成收尾
        let _ = release_hung_tx.send(true);
        let final_report = manager.cancel_and_wait_all(Duration::from_secs(1)).await;
        assert!(final_report.all_exited());

        // 验证新启动的任务拥有全新独立的 cancel_rx，绝不会被此前的 /stop 误取消
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        manager
            .start_exclusive("后续正常任务", move |_, cancel_rx| async move {
                let was_cancelled = *cancel_rx.borrow();
                let _ = done_tx.send(was_cancelled);
                Ok(())
            })
            .await
            .unwrap();
        assert!(!done_rx.await.unwrap());
    }

    #[tokio::test]
    async fn test_youtube_queue_upper_bound_enforced() {
        let mut raw_state = crate::core::state::AppState::new(PathBuf::from("."), 1);
        raw_state.youtube_max_total_tasks = 2;
        let manager = TaskManager::new(raw_state.into_shared());

        for idx in 0..2 {
            manager
                .start_youtube_upload(
                    format!("yt_{idx}"),
                    format!("v{idx}.mp4"),
                    PathBuf::from(format!("v{idx}.mp4")),
                    |mut cancel_rx| async move {
                        let _ = cancel_rx.changed().await;
                        Ok(())
                    },
                )
                .await
                .unwrap();
        }

        let third = manager
            .start_youtube_upload(
                "yt_overflow".into(),
                "v3.mp4".into(),
                PathBuf::from("v3.mp4"),
                |_| async { Ok(()) },
            )
            .await;
        assert!(matches!(
            third.unwrap_err().downcast_ref::<AppError>(),
            Some(AppError::YoutubeQueueFull { .. })
        ));

        let report = manager.cancel_and_wait_all(Duration::from_secs(1)).await;
        assert!(report.all_exited());
        assert_eq!(report.stopped_youtube.len(), 2);
        assert_eq!(manager.active_youtube_count().await, 0);
    }
}
