//! YouTube 批量与可恢复上传操作控制器。
//!
//! 关键工程特性：
//! - 入队前统一校验 `PathGuard::assert_regular_file_no_symlink`、OAuth `token.json` 与有界队列容量上限（`YOUTUBE_MAX_QUEUE_SIZE`）；
//! - 排队等待 `Semaphore` 许可期间使用 `tokio::select!` 监听取消通道，收到 `/stop` 时毫秒级立即退出；
//! - 实时区分并展示“本地已读取/发送进度”与“服务端 `308 Range` 已确认进度”，仅在拿到有效 `video_id` 后标记完成；
//! - 支持可选重启恢复钩子（保守默认 `YOUTUBE_RECOVERY_MODE=none` 下绝不擅自自动恢复上传）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode};
use tracing::{info, warn};

use crate::config::{Config, YoutubeRecoveryMode};
use crate::core::state::{ActionType, SharedState};
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::storage::filesystem::{ensure_stable_files, format_file_size_bytes};
use crate::storage::path::PathGuard;
use crate::utils::format::escape_html;
use crate::utils::redact::{redact_secrets, sanitize_user_error};
use crate::youtube::persistence::{PersistedUploadEntry, YoutubeQueueStore};
use crate::youtube::upload::{SourceFileIdentity, UploadProgress, YoutubeUploader};

const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const PROGRESS_UPDATE_THRESHOLD: f64 = 1.0;

/// Verify a retained session without sending another video byte.
pub async fn verify_pending_upload(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    task_id: &str,
) -> Result<()> {
    let enqueue_lock = Arc::clone(&state.read().await.youtube_enqueue_lock);
    let _enqueue_guard = enqueue_lock.lock().await;
    let store = {
        let s = state.read().await;
        if s.youtube_pool.contains_key(task_id) {
            return Err(AppError::invalid_argument("该任务正在上传，请等待完成或先停止任务").into());
        }
        Arc::clone(&s.youtube_queue_store)
    };
    if let Some(record) = store
        .results()
        .load()
        .await?
        .into_iter()
        .find(|e| e.task_id == task_id && e.chat_id == msg.chat.id.0)
    {
        complete_upload(bot, msg, state, &store, task_id, &record.filename, &record.video_id, false).await;
        return Ok(());
    }
    let entry = store
        .load_recoverable_entries()
        .await?
        .into_iter()
        .find(|entry| {
            entry.task_id == task_id
                && entry.unconfirmed_full_upload
                && (entry.chat_id == msg.chat.id.0
                    || (entry.chat_id == 0 && msg.chat.id.0 == config.admin_id))
        })
        .ok_or_else(|| AppError::invalid_argument("待核实任务已不存在或不属于当前聊天"))?;
    let session_uri = entry.session_uri.as_deref()
        .ok_or_else(|| AppError::invalid_argument("任务没有可核实的会话地址"))?;
    let uploader = YoutubeUploader::new(config.token_file.clone(), config.youtube_chunk_bytes());
    let result = uploader.verify_session(session_uri, entry.file_size).await;
    let text = match result {
        Ok(Some(video_id)) => {
            complete_upload(bot, msg, state, &store, task_id, &entry.filename, &video_id, false).await;
            return Ok(());
        }
        Ok(None) => format!("⏳ <b>服务端仍未确认完成</b>\n{}\n已保留会话记录；没有重新上传。",
            escape_html(&entry.filename)),
        Err(e) => format!("⚠️ <b>暂时无法核实</b>\n{}\n{}\n已保留会话记录。",
            escape_html(&entry.filename), escape_html(&sanitize_user_error(&e))),
    };
    bot.send_message(msg.chat.id, text).parse_mode(ParseMode::Html).await?;
    Ok(())
}

/// Explicitly resend a persisted result without creating or resuming an upload.
pub async fn resend_upload_result(
    bot: &Bot, msg: &Message, state: &SharedState, task_id: &str,
) -> Result<()> {
    let store = { Arc::clone(&state.read().await.youtube_queue_store) };
    if state.read().await.youtube_pool.contains_key(task_id) {
        return Err(AppError::invalid_argument("该任务仍在处理结果，请稍后补发").into());
    }
    let results = store.results();
    let record = results.load().await?.into_iter()
        .find(|e| e.task_id == task_id && e.chat_id == msg.chat.id.0)
        .ok_or_else(|| AppError::invalid_argument("没有找到该聊天中的上传结果"))?;
    let text = success_text(&record);
    let report = super::youtube_notify::send_result_only(bot, msg.chat.id, &text, Some((&results, task_id))).await;
    info!(task_id, delivered = report.delivered, attempts = report.attempts, "Manual YouTube result resend finished");
    Ok(())
}

/// Resume a selected retained session after the administrator explicitly chooses it.
pub async fn resume_pending_upload(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    task_id: &str,
) -> Result<()> {
    let enqueue_lock = Arc::clone(&state.read().await.youtube_enqueue_lock);
    let _enqueue_guard = enqueue_lock.lock().await;
    let (store, monitor) = {
        let s = state.read().await;
        if s.youtube_pool.contains_key(task_id) {
            return Err(AppError::invalid_argument("该任务正在上传，请等待完成或先停止任务").into());
        }
        (Arc::clone(&s.youtube_queue_store), Arc::clone(&s.disk_monitor))
    };
    if let Some(record) = store
        .results()
        .load()
        .await?
        .into_iter()
        .find(|e| e.task_id == task_id && e.chat_id == msg.chat.id.0)
    {
        complete_upload(bot, msg, &state, &store, task_id, &record.filename, &record.video_id, false).await;
        return Ok(());
    }
    monitor.ensure_space_available(0).await?;
    let entry = store.load_recoverable_entries().await?
        .into_iter()
        .find(|e| {
            e.task_id == task_id
                && (e.chat_id == msg.chat.id.0
                    || (e.chat_id == 0 && msg.chat.id.0 == config.admin_id))
        })
        .ok_or_else(|| AppError::invalid_argument("待核实或搁置任务已不存在或不属于当前聊天"))?;
    let session_uri = entry.session_uri.clone();
    if entry.unconfirmed_full_upload && session_uri.is_none() {
        return Err(AppError::invalid_argument("该任务结果待核实，但缺少原会话。请先在 YouTube Studio 核对。").into());
    }
    let path = PathGuard::new(config.base_dir.clone())
        .assert_regular_file_no_symlink(&entry.file_path)?;
    let expected_identity = SourceFileIdentity {
        size: entry.file_size,
        modified_nanos: entry.modified_nanos,
    };
    expected_identity.verify_unchanged(&path)?;
    let tm = TaskManager::new(Arc::clone(&state));
    // 先校验互斥与队列容量，再发送进度消息，避免启动失败残留孤立消息
    tm.check_youtube_batch_capacity(1).await?;
    let progress_msg = bot.send_message(msg.chat.id,
        format!("🔄 正在继续 YouTube 上传任务：{}", entry.filename)).await?;
    let task_id_owned = entry.task_id.clone();
    let filename = entry.filename.clone();
    let bot_clone = bot.clone();
    let cfg_clone = Arc::clone(&config);
    let state_clone = Arc::clone(&state);
    let store_clone = Arc::clone(&store);
    let preserve = entry.unconfirmed_full_upload;
    tm.start_youtube_upload(
        task_id_owned.clone(), filename.clone(), path.clone(),
        move |cancel_rx| async move {
            do_upload(
                bot_clone, progress_msg, path, filename, session_uri,
                expected_identity, YoutubeRecoveryMode::Resume, preserve, true, cfg_clone, state_clone,
                store_clone, task_id_owned, cancel_rx,
            ).await
        },
    ).await?;
    Ok(())
}

/// 启动一批 YouTube 上传任务（按用户选择顺序入队，受信号量与队列上限双重约束）。
pub async fn start_youtube_uploads(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    file_paths: Vec<PathBuf>,
) -> Result<()> {
    let bot = bot.clone();
    let msg = msg.clone();
    TaskManager::new(Arc::clone(&state))
        .start_youtube_enqueue(move |cancel_rx| async move {
            if let Err(e) = enqueue_youtube_uploads(&bot, &msg, state, config, file_paths, &cancel_rx).await {
                if !*cancel_rx.borrow() {
                    let _ = super::send_html_bounded(&bot, msg.chat.id,
                        escape_html(&sanitize_user_error(&e))).await;
                }
            }
        })
        .await
}

async fn enqueue_youtube_uploads(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    file_paths: Vec<PathBuf>,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let (monitor, enqueue_lock) = {
        let s = state.read().await;
        (Arc::clone(&s.disk_monitor), Arc::clone(&s.youtube_enqueue_lock))
    };
    monitor.ensure_space_available(0).await?;
    let mut cancel_watch = cancel_rx.clone();
    let _enqueue_guard = tokio::select! {
        guard = enqueue_lock.lock() => guard,
        _ = cancel_watch.wait_for(|cancelled| *cancelled) => return Err(AppError::Cancelled.into()),
    };
    let guard = PathGuard::new(config.base_dir.clone());
    let mut verified_files = Vec::new();
    for raw_path in file_paths {
        if let Ok(valid_path) = guard.assert_regular_file_no_symlink(&raw_path) {
            verified_files.push(valid_path);
        }
    }

    if verified_files.is_empty() {
        return Err(AppError::invalid_argument("没有可上传的有效视频文件。").into());
    }

    if !config.token_file.exists() {
        return Err(AppError::YoutubeTokenMissing {
            path: config.token_file.clone(),
        }
        .into());
    }

    let mut prepared_files = Vec::with_capacity(verified_files.len());
    for file_path in verified_files {
        let source_id = SourceFileIdentity::capture(&file_path)?;
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        prepared_files.push((file_path, source_id, filename));
    }

    let task_manager = TaskManager::new(Arc::clone(&state));
    // 入队前一次性检查独占任务互斥与队列总上限，防止发送半截消息
    task_manager
        .check_youtube_batch_capacity(prepared_files.len())
        .await?;

    let queue_store = Arc::clone(&state.read().await.youtube_queue_store);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut enqueued_paths = Vec::new();
    let mut partial_err: Option<anyhow::Error> = None;

    for (idx, (file_path, source_id, filename)) in prepared_files.into_iter().enumerate() {
        if *cancel_rx.borrow() {
            partial_err = Some(AppError::Cancelled.into());
            break;
        }
        let task_id = format!("yt_{now_ms}_{idx}");

        if queue_store.is_enabled() {
            if let Err(e) = queue_store
                .upsert(PersistedUploadEntry {
                    task_id: task_id.clone(),
                    file_path: file_path.clone(),
                    filename: filename.clone(),
                    file_size: source_id.size,
                    modified_nanos: source_id.modified_nanos,
                    chat_id: msg.chat.id.0,
                    session_uri: None,
                    unconfirmed_full_upload: false,
                })
                .await
            {
                partial_err = Some(e);
                break;
            }
        }

        let progress_request = bot
            .send_message(
                msg.chat.id,
                format!(
                    "⏳ 创建 YouTube 上传任务:\n<code>{}</code> ({})",
                    escape_html(&filename),
                    format_file_size_bytes(source_id.size),
                ),
            )
            .parse_mode(ParseMode::Html);
        let progress_result = tokio::select! {
            result = progress_request => Some(result),
            _ = cancel_watch.wait_for(|cancelled| *cancelled) => None,
        };
        let progress_msg = match progress_result {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => {
                let _ = queue_store.remove(&task_id).await;
                partial_err = Some(e.into());
                break;
            }
            None => {
                partial_err = Some(queue_store.remove(&task_id).await.err().unwrap_or_else(|| AppError::Cancelled.into()));
                break;
            }
        };
        if *cancel_rx.borrow() {
            partial_err = Some(queue_store.remove(&task_id).await.err().unwrap_or_else(|| AppError::Cancelled.into()));
            break;
        }

        let bot_clone = bot.clone();
        let path_clone = file_path.clone();
        let fname_clone = filename.clone();
        let cfg_clone = Arc::clone(&config);
        let recovery_mode = config.youtube_recovery_mode;
        let state_clone = Arc::clone(&state);
        let store_clone = Arc::clone(&queue_store);
        let tid_clone = task_id.clone();

        if let Err(e) = task_manager
            .start_youtube_upload(task_id.clone(), filename, file_path.clone(), move |cancel_rx| {
                async move {
                    do_upload(
                        bot_clone,
                        progress_msg,
                        path_clone,
                        fname_clone,
                        None,
                        source_id,
                        recovery_mode,
                        false,
                        false,
                        cfg_clone,
                        state_clone,
                        store_clone,
                        tid_clone,
                        cancel_rx,
                    )
                    .await
                }
            })
            .await
        {
            let _ = queue_store.remove(&task_id).await;
            partial_err = Some(e);
            break;
        }

        enqueued_paths.push(file_path);
    }

    let created_count = enqueued_paths.len();
    if created_count > 0 {
        let menu_lock = Arc::clone(&state.read().await.menu_lock);
        let _menu_guard = menu_lock.lock().await;
        state.write().await.remove_menu_selections((msg.chat.id.0, msg.id.0), &ActionType::Youtube, &enqueued_paths);
    }

    if let Some(err) = partial_err {
        if created_count > 0 {
            let text = format!("⚠️ 已加入 <code>{created_count}</code> 个上传任务，后续入队中断：{}",
                escape_html(&sanitize_user_error(&err)));
            let _ = super::send_html_bounded(bot, msg.chat.id, text).await;
            return Ok(());
        }
        return Err(err);
    }

    let _ = super::send_html_bounded(
            bot,
            msg.chat.id,
            format!(
                "✅ 已加入 <code>{created_count}</code> 个 YouTube 上传任务。\n\
                 ⚙️ 并发上限: <code>{}</code> | 额外排队上限: <code>{}</code> | 分块: <code>{}MB</code>\n\
                 发送 /uploads 查看进度详情，发送 /stop 可立即取消所有运行中及排队中的任务。",
                config.youtube_max_concurrent_uploads,
                config.youtube_max_queue_size,
                config.youtube_upload_chunk_mb,
            ),
        ).await;

    Ok(())
}

/// 服务启动时按 `YOUTUBE_RECOVERY_MODE` 检查并恢复未完成任务（保守默认 `none` 下直接返回 `0`）。
pub async fn recover_pending_uploads(
    bot: &Bot,
    state: SharedState,
    config: Arc<Config>,
) -> Result<usize> {
    if config.youtube_recovery_mode == YoutubeRecoveryMode::None {
        return Ok(0);
    }

    let enqueue_lock = Arc::clone(&state.read().await.youtube_enqueue_lock);
    let Ok(_enqueue_guard) = enqueue_lock.try_lock() else {
        return Ok(0);
    };
    // Active tasks own their records until their result and queue updates finish.
    let (queue_store, active_ids): (_, std::collections::HashSet<_>) = {
        let s = state.read().await;
        if s.shutting_down || s.stop_in_progress {
            return Ok(0);
        }
        (Arc::clone(&s.youtube_queue_store), s.youtube_pool.keys().cloned().collect())
    };
    let entries = queue_store.load_recoverable_entries().await?;
    if entries.is_empty() {
        return Ok(0);
    }
    // Fail closed if storage mount is unavailable: never remove persisted tasks while disk is unmounted.
    let disk_monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    if let Err(e) = disk_monitor.ensure_space_available(0).await {
        warn!(
            error = %redact_secrets(&e.to_string()),
            "Storage mount unavailable during YouTube recovery; retaining persisted queue intact"
        );
        return Ok(0);
    }

    // Fail closed if result history is unreadable: never accidentally re-upload
    // a task whose completion may already be durable.
    let completed = queue_store.results().load().await?;

    let guard = PathGuard::new(config.base_dir.clone());
    let task_manager = TaskManager::new(Arc::clone(&state));
    let mut recovered = 0usize;

    for entry in entries {
        let stopping = {
            let s = state.read().await;
            s.shutting_down || s.stop_in_progress
        };
        if stopping {
            break;
        }
        if active_ids.contains(&entry.task_id) {
            continue;
        }
        if completed.iter().any(|result| result.task_id == entry.task_id) {
            if let Err(e) = queue_store.remove(&entry.task_id).await {
                warn!(task_id = %entry.task_id, error = %redact_secrets(&e.to_string()), "Cannot clear stale recovery entry for confirmed video");
            }
            continue;
        }
        if entry.unconfirmed_full_upload {
            warn!(
                task_id = %entry.task_id,
                filename = %entry.filename,
                "Upload final state is unconfirmed; retaining session for manual verification"
            );
            continue;
        }
        // A missing path or a different file can be the directory exposed by an unmounted disk.
        let valid_path = match guard.assert_regular_file_no_symlink(&entry.file_path) {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    task_id = %entry.task_id,
                    filename = %entry.filename,
                    "Retaining persisted YouTube task: file missing or outside BASE_DIR"
                );
                continue;
            }
        };

        let expected_id = SourceFileIdentity {
            size: entry.file_size,
            modified_nanos: entry.modified_nanos,
        };
        if expected_id.verify_unchanged(&valid_path).is_err() {
            warn!(
                task_id = %entry.task_id,
                filename = %entry.filename,
                "Retaining persisted YouTube task: source no longer matches the saved file"
            );
            continue;
        }

        let target_chat = ChatId(if entry.chat_id != 0 {
            entry.chat_id
        } else {
            config.admin_id
        });

        let bot_clone = bot.clone();
        let fname_clone = entry.filename.clone();
        let sess_clone = entry.session_uri.clone();
        let cfg_clone = Arc::clone(&config);
        let recovery_mode = config.youtube_recovery_mode;
        let state_clone = Arc::clone(&state);
        let store_clone = Arc::clone(&queue_store);
        let tid_clone = entry.task_id.clone();

        match task_manager
            .start_youtube_upload(
                entry.task_id.clone(),
                entry.filename,
                valid_path.clone(),
                move |cancel_rx| async move {
                    let message = format!(
                        "🔄 <b>正在恢复 YouTube 上传任务（策略: <code>{}</code>）</b>\n<code>{}</code>",
                        recovery_mode.as_str(), escape_html(&fname_clone),
                    );
                    let progress_msg = super::send_html_cancellable(&bot_clone, target_chat, message, &cancel_rx).await;
                    let progress_msg = match progress_msg {
                        Ok(msg) => msg,
                        Err(e) => {
                            let cancelled = *cancel_rx.borrow();
                            if cancelled && !state_clone.read().await.shutting_down {
                                store_clone.remove(&tid_clone).await?;
                            }
                            return Err(e);
                        }
                    };
                    do_upload(
                        bot_clone,
                        progress_msg,
                        valid_path,
                        fname_clone,
                        sess_clone,
                        expected_id,
                        recovery_mode,
                        false,
                        true,
                        cfg_clone,
                        state_clone,
                        store_clone,
                        tid_clone,
                        cancel_rx,
                    )
                    .await
                },
            )
            .await
        {
            Ok(_) => recovered += 1,
            Err(e) if matches!(e.downcast_ref::<AppError>(), Some(
                AppError::TaskStopping | AppError::TaskAlreadyRunning { .. } | AppError::YoutubeQueueFull { .. }
            )) => break,
            Err(e) => return Err(e),
        }
    }

    Ok(recovered)
}

#[allow(clippy::too_many_arguments)]
async fn do_upload(
    bot: Bot,
    progress_msg: Message,
    file_path: PathBuf,
    filename: String,
    existing_session_uri: Option<String>,
    expected_identity: SourceFileIdentity,
    recovery_mode: YoutubeRecoveryMode,
    preserve_on_error: bool,
    retain_on_source_error: bool,
    config: Arc<Config>,
    state: SharedState,
    queue_store: Arc<YoutubeQueueStore>,
    task_id: String,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut source_preflight_failed = false;
    let result: Result<String> = async {
        let semaphore = {
            let s = state.read().await;
            Arc::clone(&s.youtube_semaphore)
        };
        update_task_status(&state, &task_id, "排队等待并发槽位", Some((0.0, 0.0))).await;
        super::edit_html_cancellable(
            &bot, progress_msg.chat.id, progress_msg.id,
            format!("⏳ <b>已进入 YouTube 上传队列</b>\n📄 <code>{}</code>\n发送 /stop 可取消。", escape_html(&filename)),
            &cancel_rx,
        ).await;
        let mut cancel_while_queued = cancel_rx.clone();
        if *cancel_while_queued.borrow() {
            return Err(AppError::YoutubeUploadCancelled { filename: filename.clone() }.into());
        }
        let _permit = tokio::select! {
            permit = semaphore.acquire() => permit.map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?,
            _ = cancel_while_queued.changed() => {
                return Err(AppError::YoutubeUploadCancelled { filename: filename.clone() }.into());
            }
        };
        let monitor = { Arc::clone(&state.read().await.disk_monitor) };
        source_preflight_failed = true;
        monitor.ensure_space_available(0).await?;
        let guard = PathGuard::new(config.base_dir.clone());
        let verified_path = guard.assert_regular_file_no_symlink(&file_path)?;
        tokio::select! {
            result = ensure_stable_files(std::slice::from_ref(&verified_path), config.file_stable_seconds) => result?,
            _ = cancel_while_queued.changed() => {
                source_preflight_failed = false;
                return Err(AppError::YoutubeUploadCancelled { filename: filename.clone() }.into());
            }
        }
        source_preflight_failed = false;
        update_task_status(&state, &task_id, "初始化会话", Some((0.0, 0.0))).await;
        super::edit_html_cancellable(
            &bot, progress_msg.chat.id, progress_msg.id,
            format!("🔄 正在建立 YouTube 可恢复上传会话...\n<code>{}</code>", escape_html(&filename)),
            &cancel_rx,
        ).await;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<UploadProgress>(64);
        let store_sess = Arc::clone(&queue_store);
        let tid_sess = task_id.clone();

        let bot_bg = bot.clone();
        let msg_id = progress_msg.id;
        let chat_id = progress_msg.chat.id;
        let filename_bg = filename.clone();
        let state_bg = Arc::clone(&state);
        let task_id_bg = task_id.clone();

        let progress_updater = tokio::spawn(async move {
            let progress_bar = ProgressBar::default();
            let mut last_update: Option<Instant> = None;
            let mut last_sent_pct = -1.0_f64;
            let mut last_conf_pct = -1.0_f64;

            while let Some(prog) = progress_rx.recv().await {
                let sent_pct = prog.local_sent_percent();
                let conf_pct = prog.server_confirmed_percent();

                let delta_moved = (sent_pct - last_sent_pct).abs() >= PROGRESS_UPDATE_THRESHOLD
                    || (conf_pct - last_conf_pct).abs() >= PROGRESS_UPDATE_THRESHOLD;
                let interval_ok =
                    last_update.map_or(true, |t| t.elapsed() >= PROGRESS_UPDATE_INTERVAL);
                let should_emit = (delta_moved && interval_ok) || conf_pct >= 100.0;

                if !should_emit {
                    continue;
                }

                let status = if prog.server_confirmed_bytes >= prog.total_bytes {
                    "字节已接收，等待最终 Video ID"
                } else {
                    "上传中"
                };
                update_task_status(
                    &state_bg,
                    &task_id_bg,
                    status,
                    Some((sent_pct, conf_pct)),
                )
                .await;

                let bar = progress_bar.render(conf_pct);
                let text = format!(
                    "📤 <b>YouTube 可恢复上传中</b>:\n\
                     📄 <code>{}</code>\n\n\
                     <code>{}</code>\n\
                     📡 本地已发送: <code>{}</code> / <code>{}</code> (<code>{sent_pct:.1}%</code>)\n\
                     📥 服务端已接收字节: <code>{}</code> / <code>{}</code> (<code>{conf_pct:.1}%</code>)\n\
                     ⏳ 收到最终 Video ID 后才确认上传成功。\n\n\
                     发送 /stop 立即取消上传",
                    escape_html(&filename_bg),
                    bar,
                    format_file_size_bytes(prog.local_sent_bytes),
                    format_file_size_bytes(prog.total_bytes),
                    format_file_size_bytes(prog.server_confirmed_bytes),
                    format_file_size_bytes(prog.total_bytes),
                );
                let _ = tokio::time::timeout(
                    super::TELEGRAM_PROGRESS_EDIT_TIMEOUT,
                    bot_bg.edit_message_text(chat_id, msg_id, text)
                        .parse_mode(ParseMode::Html),
                )
                .await;

                last_update = Some(Instant::now());
                last_sent_pct = sent_pct;
                last_conf_pct = conf_pct;
            }
        });

        let uploader = YoutubeUploader::new(config.token_file.clone(), config.youtube_chunk_bytes());
        let store_final = Arc::clone(&queue_store);
        let tid_final = task_id.clone();
        let result = uploader
            .upload(
                &verified_path,
                &filename,
                existing_session_uri,
                Some(expected_identity),
                recovery_mode,
                preserve_on_error,
                cancel_rx.clone(),
                move |prog| {
                    let _ = progress_tx.try_send(prog);
                },
                move |sess_uri| {
                    let store = Arc::clone(&store_sess);
                    let tid = tid_sess.clone();
                    async move { store.update_session_uri(&tid, &sess_uri).await }
                },
                move || {
                    let store = Arc::clone(&store_final);
                    let tid = tid_final.clone();
                    async move { store.mark_state_unconfirmed(&tid, None).await }
                },
            )
            .await;

        // A terminal result supersedes queued progress; do not delay saving its
        // Video ID while draining Telegram edits from the progress buffer.
        progress_updater.abort();
        let _ = progress_updater.await;

        result
    }.await;

    match result {
        Ok(video_id) => {
            complete_upload(&bot, &progress_msg, &state, &queue_store, &task_id, &filename, &video_id, true).await;
            Ok(())
        }
        Err(e) => {
            // 先复制到局部 bool，避免 watch::Ref 跨 .await 导致 Future 不满足 Send 约束
            let was_cancelled = *cancel_rx.borrow();
            if retain_on_source_error && !preserve_on_error && (source_preflight_failed
                || matches!(e.downcast_ref::<AppError>(), Some(AppError::SourceFileChanged { .. })))
            {
                let text = format!("⏸️ <b>暂缓恢复上传</b>\n{}\n已保留恢复记录。",
                    escape_html(&sanitize_user_error(&e)));
                super::edit_html_bounded(&bot, progress_msg.chat.id, progress_msg.id, text).await;
                return Err(e);
            }
            let e = if preserve_on_error && matches!(e.downcast_ref::<AppError>(),
                Some(AppError::Cancelled | AppError::YoutubeUploadCancelled { .. }))
            {
                AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into()
            } else {
                e
            };
            finalize_upload_error(
                &bot,
                &progress_msg,
                &state,
                &queue_store,
                &task_id,
                &filename,
                preserve_on_error,
                was_cancelled,
                e,
            )
            .await
        }
    }
}

/// Best-effort cleanup and notification always preserve the original task error.
#[allow(clippy::too_many_arguments)]
async fn finalize_upload_error(
    bot: &Bot, progress_msg: &Message, state: &SharedState, queue_store: &YoutubeQueueStore,
    task_id: &str, filename: &str, preserve_on_error: bool, was_cancelled: bool, e: anyhow::Error,
) -> Result<()> {
    let cancelled = matches!(e.downcast_ref::<AppError>(),
        Some(AppError::YoutubeUploadCancelled { .. } | AppError::Cancelled));
    let unconfirmed = matches!(e.downcast_ref::<AppError>(),
        Some(AppError::YoutubeUploadStateUnconfirmed { .. }));
    let keep_session = unconfirmed || preserve_on_error;
    let cleanup = if keep_session {
        queue_store.mark_state_unconfirmed(&task_id, None).await
    } else {
        queue_store.remove(&task_id).await
    };
    let mut extra = String::new();
    if let Err(cleanup_error) = cleanup {
        warn!(task_id = %task_id, error = %redact_secrets(&cleanup_error.to_string()),
            original_error = %redact_secrets(&e.to_string()), "YouTube finalization persistence failed");
        extra.push_str("\n⚠️ 恢复记录更新失败，请保留服务端日志，核实后再决定是否重传。");
    }
    if keep_session && !queue_store.is_enabled() {
        extra.push_str("\n⚠️ 当前未启用会话持久化，重启后无法通过机器人核实原会话，请检查 YouTube Studio。");
    }
    let (status, heading) = if unconfirmed && was_cancelled {
        ("已停止等待，结果待核实", "🛑 <b>已停止等待，结果待核实（请勿直接重传）</b>")
    } else if cancelled {
        ("已取消", "🛑 <b>YouTube 上传已取消</b>")
    } else if unconfirmed {
        ("待核实", "⚠️ <b>YouTube 上传结果待核实，请勿直接重传</b>")
    } else {
        ("失败", "❌ <b>YouTube 上传失败</b>")
    };
    update_task_status(state, task_id, status, None).await;
    let text = format!("{heading}\n📄 <code>{}</code>\n{}{}",
        escape_html(filename), escape_html(&sanitize_user_error(&e)), extra);
    // 取消场景采用单次限时卡片更新，避免多轮长重试拖慢 /stop 并触发 Telegram 429 限流
    if cancelled || (unconfirmed && was_cancelled) {
        super::youtube_notify::notify_cancelled_fast(bot, progress_msg, &text).await;
    } else {
        super::youtube_notify::notify_result(bot, progress_msg, &text, None).await;
    }
    warn!(task_id = %task_id, filename = %filename,
        error = %redact_secrets(&e.to_string()), "YouTube upload ended without confirmed success");
    Err(e)
}

/// Save the known video before clearing recovery state or contacting Telegram.
#[allow(clippy::too_many_arguments)]
async fn complete_upload(
    bot: &Bot, progress_msg: &Message, state: &SharedState, queue: &YoutubeQueueStore,
    task_id: &str, filename: &str, video_id: &str, edit_card: bool,
) {
    use crate::youtube::results::UploadResultRecord;
    let results = queue.results();
    let record = UploadResultRecord::confirmed(task_id, filename, progress_msg.chat.id.0, video_id);
    let saved = results.save_confirmed(record.clone()).await;
    let (record, durable) = match saved {
        Ok(saved) => (saved, true),
        Err(e) => {
            warn!(task_id, video_id, error = %redact_secrets(&e.to_string()), "Confirmed YouTube result could not be persisted; retaining recovery record");
            if let Err(retain_error) = queue.mark_state_unconfirmed(task_id, None).await {
                warn!(task_id, error = %redact_secrets(&retain_error.to_string()), "Could not mark recovery record for verification");
            }
            (record, false)
        }
    };
    if durable {
        if let Err(e) = queue.remove(task_id).await {
            warn!(task_id, error = %redact_secrets(&e.to_string()), "Failed to clear completed upload from recovery queue");
        }
    }
    update_task_completed(state, task_id, video_id).await;
    info!(task_id, filename, video_id, "YouTube upload verified complete");
    let mut text = success_text(&record);
    if !durable {
        text.push_str("\n⚠️ 视频已确认成功，但结果保存失败；请保存以上链接。");
        if queue.is_enabled() {
            text.push_str("恢复记录未主动删除，重启前请核实服务端日志。");
        } else {
            text.push_str("当前未启用会话持久化，请勿依赖重启后查询。");
        }
    }
    let tracking = if durable { Some((&results, task_id)) } else { None };
    if edit_card {
        super::youtube_notify::notify_result(bot, progress_msg, &text, tracking).await;
    } else {
        super::youtube_notify::send_result_only(bot, progress_msg.chat.id, &text, tracking).await;
    }
}

fn success_text(record: &crate::youtube::results::UploadResultRecord) -> String {
    let vid = escape_html(&record.video_id);
    format!(
        "✅ <b>YouTube 上传成功（服务端已确认完成）！</b>\n\
         🎬 视频名称: <code>{}</code>\n\
         🆔 Video ID: <code>{vid}</code>\n\
         🕒 完成时间: <code>{}</code>\n\
         📺 观看链接: <a href=\"https://youtu.be/{vid}\">https://youtu.be/{vid}</a>\n\
         🛠️ Studio: <a href=\"https://studio.youtube.com/video/{vid}/edit\">https://studio.youtube.com/video/{vid}/edit</a>",
        escape_html(&record.filename), escape_html(&record.completed_at)
    )
}

async fn update_task_status(
    state: &SharedState,
    task_id: &str,
    status: &str,
    progress_pair: Option<(f64, f64)>,
) {
    let mut s = state.write().await;
    if let Some(task) = s.youtube_pool.get_mut(task_id) {
        task.status = status.to_string();
        if let Some((sent_pct, conf_pct)) = progress_pair {
            task.update_progress(sent_pct, conf_pct);
        }
    }
}

async fn update_task_completed(state: &SharedState, task_id: &str, video_id: &str) {
    let mut s = state.write().await;
    if let Some(task) = s.youtube_pool.get_mut(task_id) {
        task.status = "已完成".to_string();
        task.update_progress(100.0, 100.0);
        task.video_id = Some(video_id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Response, Server};
    use std::convert::Infallible;
    use std::sync::Mutex;

    fn progress_message() -> Message {
        serde_json::from_value(serde_json::json!({
            "message_id": 1, "date": 0,
            "chat": {"id": 1, "type": "private", "first_name": "Admin"},
            "text": "progress"
        })).unwrap()
    }

    fn telegram_mock() -> (Bot, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let texts = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&texts);
        let service = make_service_fn(move |_| {
            let captured = Arc::clone(&captured);
            async move {
                Ok::<_, Infallible>(service_fn(move |request: hyper::Request<Body>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        let bytes = hyper::body::to_bytes(request.into_body()).await.unwrap();
                        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        captured.lock().unwrap().push(payload["text"].as_str().unwrap().to_string());
                        let response = serde_json::json!({"ok": true, "result": progress_message()});
                        Ok::<_, Infallible>(Response::new(Body::from(response.to_string())))
                    }
                }))
            }
        });
        let server = Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
        let bot = Bot::new("123456:test").set_api_url(format!("http://{}/", server.local_addr()).parse().unwrap());
        let handle = tokio::spawn(async move { server.await.unwrap(); });
        (bot, texts, handle)
    }

    #[tokio::test]
    async fn queue_write_failure_does_not_hide_original_error_or_skip_notification() {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");
        std::fs::write(&queue_path, b"broken").unwrap();
        let queue = YoutubeQueueStore::new(YoutubeRecoveryMode::Resume, queue_path);
        let state = crate::core::state::AppState::new(dir.path().to_path_buf(), 1).into_shared();
        let (bot, texts, server) = telegram_mock();
        let original = AppError::YoutubeUploadStateUnconfirmed { filename: "video.mp4".to_string() };
        let error = finalize_upload_error(&bot, &progress_message(), &state, &queue,
            "task", "video.mp4", false, original.into()).await.unwrap_err();
        server.abort();
        assert!(matches!(error.downcast_ref::<AppError>(), Some(AppError::YoutubeUploadStateUnconfirmed { .. })));
        let texts = texts.lock().unwrap();
        assert_eq!(texts.len(), 2);
        assert!(texts[1].contains("恢复记录更新失败"));
        assert!(texts[1].contains("待核实"));
    }

    #[tokio::test]
    async fn confirmed_video_remains_successful_when_result_storage_fails() {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");
        let queue = YoutubeQueueStore::new(YoutubeRecoveryMode::Resume, queue_path);
        queue.upsert(PersistedUploadEntry {
            task_id: "task".into(), file_path: dir.path().join("video.mp4"),
            filename: "video.mp4".into(), file_size: 128, modified_nanos: None,
            chat_id: 1, session_uri: Some("https://example.invalid/session".into()),
            unconfirmed_full_upload: false,
        }).await.unwrap();
        // Force result storage to fail while leaving the recovery queue writable.
        std::fs::create_dir(dir.path().join("queue.json.results.json")).unwrap();
        let state = crate::core::state::AppState::new(dir.path().to_path_buf(), 1).into_shared();
        let (bot, texts, server) = telegram_mock();
        complete_upload(&bot, &progress_message(), &state, &queue, "task", "video.mp4", "video123").await;
        server.abort();
        let retained = queue.load_recoverable_entries().await.unwrap();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].unconfirmed_full_upload);
        let texts = texts.lock().unwrap();
        assert!(texts[1].contains("上传成功"));
        assert!(texts[1].contains("结果保存失败"));
        assert!(texts[1].contains("video123"));
    }
}
