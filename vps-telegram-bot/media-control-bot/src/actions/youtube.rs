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
    let store = {
        let s = state.read().await;
        if s.youtube_pool.contains_key(task_id) {
            return Err(AppError::invalid_argument("该任务正在上传，请等待完成或先停止任务").into());
        }
        Arc::clone(&s.youtube_queue_store)
    };
    let entry = store
        .load_recoverable_entries()
        .await?
        .into_iter()
        .find(|entry| entry.task_id == task_id && entry.unconfirmed_full_upload)
        .ok_or_else(|| AppError::invalid_argument("待核实任务已不存在"))?;
    let session_uri = entry.session_uri.as_deref()
        .ok_or_else(|| AppError::invalid_argument("任务没有可核实的会话地址"))?;
    let uploader = YoutubeUploader::new(config.token_file.clone(), config.youtube_chunk_bytes());
    let result = uploader.verify_session(session_uri, entry.file_size).await;
    let text = match result {
        Ok(Some(video_id)) => {
            store.remove(task_id).await?;
            format!("✅ <b>服务端已确认上传完成</b>\n{}\nVideo ID: <code>{}</code>",
                escape_html(&entry.filename), escape_html(&video_id))
        }
        Ok(None) => format!("⏳ <b>服务端仍未确认完成</b>\n{}\n已保留会话记录；没有重新上传。",
            escape_html(&entry.filename)),
        Err(e) => format!("⚠️ <b>暂时无法核实</b>\n{}\n{}\n已保留会话记录。",
            escape_html(&entry.filename), escape_html(&sanitize_user_error(&e))),
    };
    bot.send_message(msg.chat.id, text).parse_mode(ParseMode::Html).await?;
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
    let (store, monitor) = {
        let s = state.read().await;
        if s.youtube_pool.contains_key(task_id) {
            return Err(AppError::invalid_argument("该任务正在上传，请等待完成或先停止任务").into());
        }
        (Arc::clone(&s.youtube_queue_store), Arc::clone(&s.disk_monitor))
    };
    monitor.ensure_space_available(0).await?;
    let entry = store.load_recoverable_entries().await?
        .into_iter()
        .find(|e| e.task_id == task_id && e.unconfirmed_full_upload)
        .ok_or_else(|| AppError::invalid_argument("待核实任务已不存在"))?;
    let session_uri = entry.session_uri.clone()
        .ok_or_else(|| AppError::invalid_argument("任务没有可续传会话"))?;
    let path = PathGuard::new(config.base_dir.clone())
        .assert_regular_file_no_symlink(&entry.file_path)?;
    SourceFileIdentity {
        size: entry.file_size,
        modified_nanos: entry.modified_nanos,
    }.verify_unchanged(&path)?;
    let progress_msg = bot.send_message(msg.chat.id,
        format!("🔄 正在续传原 YouTube 会话：{}", entry.filename)).await?;
    let task_id_owned = entry.task_id.clone();
    let filename = entry.filename.clone();
    let bot_clone = bot.clone();
    let cfg_clone = Arc::clone(&config);
    let state_clone = Arc::clone(&state);
    let store_clone = Arc::clone(&store);
    TaskManager::new(state).start_youtube_upload(
        task_id_owned.clone(), filename.clone(), path.clone(),
        move |cancel_rx| async move {
            do_upload(
                bot_clone, progress_msg, path, filename, Some(session_uri),
                YoutubeRecoveryMode::Resume, true, cfg_clone, state_clone,
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
    let (monitor, enqueue_lock) = {
        let s = state.read().await;
        (Arc::clone(&s.disk_monitor), Arc::clone(&s.youtube_enqueue_lock))
    };
    monitor.ensure_space_available(0).await?;
    let _enqueue_guard = enqueue_lock.lock().await;
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

    let task_manager = TaskManager::new(Arc::clone(&state));
    // 入队前一次性检查独占任务互斥与队列总上限，防止发送半截消息
    task_manager
        .check_youtube_batch_capacity(verified_files.len())
        .await?;

    let queue_store = {
        let s = state.read().await;
        Arc::clone(&s.youtube_queue_store)
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut created_count = 0usize;

    for (idx, file_path) in verified_files.into_iter().enumerate() {
        let source_id = SourceFileIdentity::capture(&file_path)?;
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let task_id = format!("yt_{now_ms}_{idx}");

        if queue_store.is_enabled() {
            queue_store
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
                .await?;
        }

        let progress_msg = match bot
            .send_message(
                msg.chat.id,
                format!(
                    "⏳ 创建 YouTube 上传任务:\n<code>{}</code> ({})",
                    escape_html(&filename),
                    format_file_size_bytes(source_id.size),
                ),
            )
            .parse_mode(ParseMode::Html)
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                queue_store.remove(&task_id).await?;
                return Err(e.into());
            }
        };

        let bot_clone = bot.clone();
        let path_clone = file_path.clone();
        let fname_clone = filename.clone();
        let cfg_clone = Arc::clone(&config);
        let recovery_mode = config.youtube_recovery_mode;
        let state_clone = Arc::clone(&state);
        let store_clone = Arc::clone(&queue_store);
        let tid_clone = task_id.clone();

        if let Err(e) = task_manager
            .start_youtube_upload(task_id.clone(), filename, file_path, move |cancel_rx| {
                async move {
                    do_upload(
                        bot_clone,
                        progress_msg,
                        path_clone,
                        fname_clone,
                        None,
                        recovery_mode,
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
            queue_store.remove(&task_id).await?;
            return Err(e);
        }

        created_count += 1;
    }

    {
        let mut s = state.write().await;
        s.clear_selections(&ActionType::Youtube);
    }

    let _ = bot
        .edit_message_text(
            msg.chat.id,
            msg.id,
            format!(
                "✅ 已加入 <code>{created_count}</code> 个 YouTube 上传任务。\n\
                 ⚙️ 并发上限: <code>{}</code> | 额外排队上限: <code>{}</code> | 分块: <code>{}MB</code>\n\
                 发送 /uploads 查看进度详情，发送 /stop 可立即取消所有运行中及排队中的任务。",
                config.youtube_max_concurrent_uploads,
                config.youtube_max_queue_size,
                config.youtube_upload_chunk_mb,
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;

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

    let queue_store = {
        let s = state.read().await;
        Arc::clone(&s.youtube_queue_store)
    };
    let entries = queue_store.load_recoverable_entries().await?;
    if entries.is_empty() {
        return Ok(0);
    }

    let guard = PathGuard::new(config.base_dir.clone());
    let task_manager = TaskManager::new(Arc::clone(&state));
    let mut recovered = 0usize;

    for entry in entries {
        if entry.unconfirmed_full_upload {
            warn!(
                task_id = %entry.task_id,
                filename = %entry.filename,
                "Full upload is unconfirmed; retaining session for manual verification"
            );
            continue;
        }
        // 核验路径仍在安全目录内且源文件身份未变
        let valid_path = match guard.assert_regular_file_no_symlink(&entry.file_path) {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    task_id = %entry.task_id,
                    filename = %entry.filename,
                    "Skipping persisted YouTube task: file missing or outside BASE_DIR"
                );
                queue_store.remove(&entry.task_id).await?;
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
                "Skipping persisted YouTube task: source file changed since last run"
            );
            queue_store.remove(&entry.task_id).await?;
            continue;
        }

        let target_chat = ChatId(if entry.chat_id != 0 {
            entry.chat_id
        } else {
            config.admin_id
        });

        let Ok(progress_msg) = bot
            .send_message(
                target_chat,
                format!(
                    "🔄 <b>检测到重启前未完成的 YouTube 上传任务（策略: <code>{}</code>）</b>\n<code>{}</code>",
                    config.youtube_recovery_mode.as_str(),
                    escape_html(&entry.filename),
                ),
            )
            .parse_mode(ParseMode::Html)
            .await
        else {
            continue;
        };

        let bot_clone = bot.clone();
        let fname_clone = entry.filename.clone();
        let sess_clone = entry.session_uri.clone();
        let cfg_clone = Arc::clone(&config);
        let recovery_mode = config.youtube_recovery_mode;
        let state_clone = Arc::clone(&state);
        let store_clone = Arc::clone(&queue_store);
        let tid_clone = entry.task_id.clone();

        if task_manager
            .start_youtube_upload(
                entry.task_id.clone(),
                entry.filename,
                valid_path.clone(),
                move |cancel_rx| async move {
                    do_upload(
                        bot_clone,
                        progress_msg,
                        valid_path,
                        fname_clone,
                        sess_clone,
                        recovery_mode,
                        false,
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
            .is_ok()
        {
            recovered += 1;
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
    recovery_mode: YoutubeRecoveryMode,
    preserve_on_error: bool,
    config: Arc<Config>,
    state: SharedState,
    queue_store: Arc<YoutubeQueueStore>,
    task_id: String,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let semaphore = {
        let s = state.read().await;
        Arc::clone(&s.youtube_semaphore)
    };

    update_task_status(&state, &task_id, "排队等待并发槽位", Some((0.0, 0.0))).await;

    let _ = bot
        .edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!(
                "⏳ <b>已进入 YouTube 上传队列</b>\n\
                 📄 <code>{}</code>\n\
                 ⚙️ 并发上限: <code>{}</code>\n\
                 发送 /uploads 查看列表，发送 /stop 可立即取消排队。",
                escape_html(&filename),
                config.youtube_max_concurrent_uploads
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;

    // 关键改进：等待 Semaphore 期间同时监听取消信号，收到 /stop 立即中断无需等待前序大文件传完
    let mut cancel_while_queued = cancel_rx.clone();
    if *cancel_while_queued.borrow() {
        queue_store.remove(&task_id).await;
        update_task_status(&state, &task_id, "已取消", Some((0.0, 0.0))).await;
        let _ = bot
            .edit_message_text(
                progress_msg.chat.id,
                progress_msg.id,
                format!(
                    "🛑 <b>YouTube 排队任务已取消</b>:\n<code>{}</code>",
                    escape_html(&filename)
                ),
            )
            .parse_mode(ParseMode::Html)
            .await;
        return Err(AppError::YoutubeUploadCancelled { filename }.into());
    }

    let _permit = tokio::select! {
        permit_res = semaphore.acquire() => {
            permit_res.map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?
        }
        _ = cancel_while_queued.changed() => {
            queue_store.remove(&task_id).await;
            update_task_status(&state, &task_id, "已取消", Some((0.0, 0.0))).await;
            let _ = bot
                .edit_message_text(
                    progress_msg.chat.id,
                    progress_msg.id,
                    format!(
                        "🛑 <b>YouTube 排队任务已立即取消</b>:\n<code>{}</code>",
                        escape_html(&filename)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
            return Err(AppError::YoutubeUploadCancelled { filename }.into());
        }
    };

    let monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    monitor.ensure_space_available(0).await?;

    // 获取信号量后再次核实文件安全边界
    let guard = PathGuard::new(config.base_dir.clone());
    let verified_path = match guard.assert_regular_file_no_symlink(&file_path) {
        Ok(p) => p,
        Err(e) => {
            queue_store.remove(&task_id).await;
            let _ = bot
                .edit_message_text(
                    progress_msg.chat.id,
                    progress_msg.id,
                    format!(
                        "❌ <b>上传启动失败</b>: <code>{}</code>\n{}",
                        escape_html(&filename),
                        escape_html(&sanitize_user_error(&e))
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
            return Err(e);
        }
    };

    if let Err(e) = ensure_stable_files(&[verified_path.clone()], config.file_stable_seconds).await {
        if !preserve_on_error {
            let _ = queue_store.remove(&task_id).await;
        }
        update_task_status(&state, &task_id, "源文件未稳定", None).await;
        let _ = bot.edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!("❌ <b>上传未开始</b>：<code>{}</code>\n{}",
                escape_html(&filename), escape_html(&sanitize_user_error(&e))),
        ).parse_mode(ParseMode::Html).await;
        return Err(e);
    }
    update_task_status(&state, &task_id, "初始化会话", Some((0.0, 0.0))).await;
    let _ = bot
        .edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!(
                "🔄 正在建立 YouTube 可恢复上传会话...\n<code>{}</code>",
                escape_html(&filename)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;

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
        let mut last_update = Instant::now() - PROGRESS_UPDATE_INTERVAL;
        let mut last_sent_pct = -1.0_f64;
        let mut last_conf_pct = -1.0_f64;

        while let Some(prog) = progress_rx.recv().await {
            let sent_pct = prog.local_sent_percent();
            let conf_pct = prog.server_confirmed_percent();

            let delta_moved = (sent_pct - last_sent_pct).abs() >= PROGRESS_UPDATE_THRESHOLD
                || (conf_pct - last_conf_pct).abs() >= PROGRESS_UPDATE_THRESHOLD;
            let should_emit = (delta_moved && last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL)
                || conf_pct >= 100.0;

            if !should_emit {
                continue;
            }

            update_task_status(
                &state_bg,
                &task_id_bg,
                "上传中",
                Some((sent_pct, conf_pct)),
            )
            .await;

            let bar = progress_bar.render(conf_pct);
            let text = format!(
                "📤 <b>YouTube 可恢复上传中</b>:\n\
                 📄 <code>{}</code>\n\n\
                 <code>{}</code>\n\
                 📡 本地已发送: <code>{}</code> / <code>{}</code> (<code>{sent_pct:.1}%</code>)\n\
                 ✅ 服务端已确认 (308): <code>{}</code> / <code>{}</code> (<code>{conf_pct:.1}%</code>)\n\n\
                 发送 /stop 立即取消上传",
                escape_html(&filename_bg),
                bar,
                format_file_size_bytes(prog.local_sent_bytes),
                format_file_size_bytes(prog.total_bytes),
                format_file_size_bytes(prog.server_confirmed_bytes),
                format_file_size_bytes(prog.total_bytes),
            );
            let _ = bot_bg
                .edit_message_text(chat_id, msg_id, text)
                .parse_mode(ParseMode::Html)
                .await;

            last_update = Instant::now();
            last_sent_pct = sent_pct;
            last_conf_pct = conf_pct;
        }
    });

    let uploader = YoutubeUploader::new(config.token_file.clone(), config.youtube_chunk_bytes());
    let result = uploader
        .upload(
            &verified_path,
            &filename,
            existing_session_uri,
            recovery_mode,
            cancel_rx.clone(),
            move |prog| {
                let _ = progress_tx.try_send(prog);
            },
            move |sess_uri| {
                let store = Arc::clone(&store_sess);
                let tid = tid_sess.clone();
                async move { store.update_session_uri(&tid, &sess_uri).await }
            },
        )
        .await;

    let _ = progress_updater.await;

    match result {
        Ok(video_id) => {
            if let Err(e) = queue_store.remove(&task_id).await {
                warn!(task_id = %task_id, error = %redact_secrets(&e.to_string()), "Failed to clear completed upload from recovery queue");
            }
            update_task_completed(&state, &task_id, &video_id).await;
            let upload_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let safe_vid = escape_html(&video_id);
            let success_text = format!(
                "✅ <b>YouTube 上传成功（服务端已确认完成）！</b>\n\
                 🎬 视频名称: <code>{}</code>\n\
                 🆔 Video ID: <code>{safe_vid}</code>\n\
                 🕒 完成时间: <code>{}</code>\n\
                 📺 观看链接: <a href=\"https://youtu.be/{safe_vid}\">https://youtu.be/{safe_vid}</a>\n\
                 🛠️ Studio: <a href=\"https://studio.youtube.com/video/{safe_vid}/edit\">https://studio.youtube.com/video/{safe_vid}/edit</a>",
                escape_html(&filename),
                escape_html(&upload_time)
            );
            let _ = bot
                .edit_message_text(progress_msg.chat.id, progress_msg.id, success_text)
                .parse_mode(ParseMode::Html)
                .await;
            info!(filename = %filename, video_id = %video_id, "YouTube upload verified complete");
            Ok(())
        }
        Err(e) => {
            if *cancel_rx.borrow()
                || matches!(
                    e.downcast_ref::<AppError>(),
                    Some(AppError::YoutubeUploadCancelled { .. } | AppError::Cancelled)
                )
            {
                if let Err(e) = queue_store.remove(&task_id).await {
                    warn!(task_id = %task_id, error = %redact_secrets(&e.to_string()), "Failed to clear cancelled upload from recovery queue");
                }
                update_task_status(&state, &task_id, "已取消", None).await;
                let _ = bot
                    .edit_message_text(
                        progress_msg.chat.id,
                        progress_msg.id,
                        format!(
                            "🛑 <b>YouTube 上传已取消</b>:\n<code>{}</code>",
                            escape_html(&filename)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
                info!(filename = %filename, "YouTube upload cancelled by user");
                return Err(AppError::YoutubeUploadCancelled { filename }.into());
            }

            if preserve_on_error || matches!(
                e.downcast_ref::<AppError>(),
                Some(AppError::YoutubeUploadStateUnconfirmed { .. })
            ) {
                queue_store.mark_state_unconfirmed(&task_id, None).await?;
            } else if let Err(remove_err) = queue_store.remove(&task_id).await {
                warn!(task_id = %task_id, error = %redact_secrets(&remove_err.to_string()), "Failed to clear failed upload from recovery queue");
            }
            update_task_status(&state, &task_id, "失败", None).await;
            let safe_user_msg = sanitize_user_error(&e);
            let _ = bot
                .edit_message_text(
                    progress_msg.chat.id,
                    progress_msg.id,
                    format!(
                        "❌ <b>YouTube 上传失败</b>:\n📄 <code>{}</code>\n⚠️ {}",
                        escape_html(&filename),
                        escape_html(&safe_user_msg)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
            warn!(
                filename = %filename,
                error = %redact_secrets(&e.to_string()),
                "YouTube upload failed"
            );
            Err(e)
        }
    }
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
