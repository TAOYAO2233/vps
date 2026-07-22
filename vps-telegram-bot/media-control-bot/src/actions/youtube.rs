//! YouTube 批量上传操作。
//!
//! 对应 Python 版本的 `upload_youtube_file` 和 `start_youtube_uploads` 函数。
//! 支持多个视频并发上传，通过 `Semaphore` 控制最大并发数。
//! 每个上传任务独立运行，通过 `watch::channel` 接收取消信号。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{info, warn};

use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::utils::format::escape_html;
use crate::youtube::upload::YoutubeUploader;

const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const PROGRESS_UPDATE_THRESHOLD: f64 = 1.0;

/// 启动一批 YouTube 上传任务（加入上传池，并发执行）。
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `msg` - 触发此操作的消息
/// * `state` - 全局共享状态
/// * `config` - 应用配置
/// * `file_paths` - 要上传的文件路径列表（已通过路径安全校验）
pub async fn start_youtube_uploads(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    file_paths: Vec<PathBuf>,
) -> Result<()> {
    // 过滤出实际存在的文件
    let target_files: Vec<PathBuf> = file_paths.into_iter().filter(|p| p.is_file()).collect();

    if target_files.is_empty() {
        return Err(AppError::invalid_argument("没有可上传的视频文件。").into());
    }

    // 检查 token 文件
    if !config.token_file.exists() {
        return Err(AppError::YoutubeTokenMissing {
            path: config.token_file.clone(),
        }
        .into());
    }

    // 检查是否有独占任务
    {
        let mut s = state.write().await;
        s.cleanup_active_task();
        if s.has_active_task() {
            let name = s.active_task_name().unwrap_or("运行中任务").to_string();
            return Err(AppError::TaskAlreadyRunning { task_name: name }.into());
        }
    }

    let task_manager = TaskManager::new(Arc::clone(&state));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut created_filenames = Vec::new();

    for (idx, file_path) in target_files.iter().enumerate() {
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let task_id = format!(
            "yt_{now_ms}_{idx}_{}",
            (file_path.to_string_lossy().len() + idx) % 100000
        );

        // 发送初始进度消息
        let progress_msg = bot
            .send_message(
                msg.chat.id,
                format!("⏳ 创建 YouTube 上传任务:\n<code>{}</code>", escape_html(&filename)),
            )
            .parse_mode(ParseMode::Html)
            .await?;

        let bot_clone = bot.clone();
        let file_path_clone = file_path.clone();
        let filename_clone = filename.clone();
        let config_clone = Arc::clone(&config);
        let state_clone = Arc::clone(&state);
        let task_id_clone = task_id.clone();

        task_manager
            .start_youtube_upload(
                task_id.clone(),
                filename.clone(),
                file_path.clone(),
                move |cancel_rx| {
                    let bot = bot_clone.clone();
                    let path = file_path_clone.clone();
                    let fname = filename_clone.clone();
                    let cfg = Arc::clone(&config_clone);
                    let state = Arc::clone(&state_clone);
                    let tid = task_id_clone.clone();
                    async move {
                        do_upload(bot, progress_msg, path, fname, cfg, state, tid, cancel_rx).await
                    }
                },
            )
            .await?;

        created_filenames.push(filename);
    }

    // 清空选中集合
    {
        let mut s = state.write().await;
        s.clear_selections(&crate::core::state::ActionType::Youtube);
    }

    let created_count = created_filenames.len();
    let max_concurrent = config.youtube_max_concurrent_uploads;

    let _ = bot
        .edit_message_text(
            msg.chat.id,
            msg.id,
            format!(
                "✅ 已启动 <code>{created_count}</code> 个 YouTube 上传任务。\n并发上限: <code>{max_concurrent}</code>\n发送 /uploads 查看任务，/stop 停止所有任务。"
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;

    Ok(())
}

/// 实际执行单个 YouTube 上传任务。
async fn do_upload(
    bot: Bot,
    progress_msg: Message,
    file_path: PathBuf,
    filename: String,
    config: Arc<Config>,
    state: SharedState,
    task_id: String,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let semaphore = {
        let s = state.read().await;
        Arc::clone(&s.youtube_semaphore)
    };

    // 更新状态为排队中
    update_task_status(&state, &task_id, "排队中", Some(0.0)).await;

    let _ = bot
        .edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!(
                "⏳ <b>已加入 YouTube 上传队列</b>\n<code>{}</code>\n并发上限: <code>{}</code>\n发送 /uploads 查看，/stop 停止。",
                escape_html(&filename),
                config.youtube_max_concurrent_uploads
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;

    // 等待 Semaphore 许可
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?;

    // 检查取消信号
    if *cancel_rx.borrow() {
        update_task_status(&state, &task_id, "已取消", Some(0.0)).await;
        let _ = bot
            .edit_message_text(
                progress_msg.chat.id,
                progress_msg.id,
                format!("🛑 <b>YouTube 上传已取消</b>:\n<code>{}</code>", escape_html(&filename)),
            )
            .parse_mode(ParseMode::Html)
            .await;
        return Err(AppError::YoutubeUploadCancelled {
            filename: filename.clone(),
        }
        .into());
    }

    update_task_status(&state, &task_id, "初始化", Some(0.0)).await;
    let _ = bot
        .edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!("🔄 初始化 YouTube API...\n<code>{}</code>", escape_html(&filename)),
        )
        .parse_mode(ParseMode::Html)
        .await;

    // 创建 mpsc 异步消息通道
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<f64>(100);

    // 启动后台刷新任务
    let bot_bg = bot.clone();
    let msg_id = progress_msg.id;
    let chat_id = progress_msg.chat.id;
    let filename_bg = filename.clone();
    let state_bg = Arc::clone(&state);
    let task_id_bg = task_id.clone();

    let progress_updater = tokio::spawn(async move {
        let progress_bar = ProgressBar::default();
        let mut last_update = Instant::now();
        let mut last_percent = -1.0_f64;

        while let Some(percent) = progress_rx.recv().await {
            // 更新全局状态（使 /uploads 指令可见）
            update_task_status(&state_bg, &task_id_bg, "上传中", Some(percent)).await;

            // 节流刷新 Telegram 界面消息
            if (percent - last_percent >= PROGRESS_UPDATE_THRESHOLD
                && last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL)
                || percent >= 100.0
            {
                let bar = progress_bar.render(percent);
                let text = format!(
                    "📤 <b>YouTube 上传中</b>:\n<code>{}</code>\n\n<code>{}</code>\n\n发送 /stop 取消任务",
                    escape_html(&filename_bg),
                    bar
                );
                let _ = bot_bg
                    .edit_message_text(chat_id, msg_id, text)
                    .parse_mode(ParseMode::Html)
                    .await;
                last_update = Instant::now();
                last_percent = percent.floor();
            }
        }
    });

    // 执行上传并传入进度回调与取消检测闭包
    let uploader = YoutubeUploader::new(config.token_file.clone(), config.youtube_chunk_bytes());
    update_task_status(&state, &task_id, "上传中", Some(0.0)).await;

    let cancel_rx_clone = cancel_rx.clone();
    let result = uploader
        .upload(
            &file_path,
            &filename,
            move |percent| {
                let _ = progress_tx.try_send(percent);
            },
            move || *cancel_rx_clone.borrow(),
        )
        .await;

    // 等待刷新任务结束
    let _ = progress_updater.await;

    match result {
        Ok(video_id) => {
            update_task_status(&state, &task_id, "完成", Some(100.0)).await;
            let upload_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let success_text = format!(
                "✅ <b>上传成功！</b>\n\
                 🎬 视频名称: <code>{}</code>\n\
                 🕒 上传时间: <code>{}</code>\n\
                 📺 观看链接: <a href=\"https://youtu.be/{video_id}\">https://youtu.be/{video_id}</a>\n\
                 🛠️ Studio: <a href=\"https://studio.youtube.com/video/{video_id}/edit\">https://studio.youtube.com/video/{video_id}/edit</a>",
                escape_html(&filename),
                escape_html(&upload_time)
            );
            let _ = bot
                .edit_message_text(progress_msg.chat.id, progress_msg.id, success_text)
                .parse_mode(ParseMode::Html)
                .await;
            info!(filename = %filename, video_id = %video_id, "YouTube upload completed");
        }
        Err(e) => {
            // 如果是因为用户手动点击 /stop 导致的中断
            if *cancel_rx.borrow() {
                update_task_status(&state, &task_id, "已取消", Some(0.0)).await;
                let _ = bot
                    .edit_message_text(
                        progress_msg.chat.id,
                        progress_msg.id,
                        format!("🛑 <b>YouTube 上传已取消</b>:\n<code>{}</code>", escape_html(&filename)),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
                info!(filename = %filename, "YouTube upload cancelled by user");
                return Ok(());
            }

            update_task_status(&state, &task_id, "失败", None).await;
            let _ = bot
                .edit_message_text(
                    progress_msg.chat.id,
                    progress_msg.id,
                    format!("❌ 上传异常:\n<code>{}</code>", escape_html(&e.to_string())),
                )
                .parse_mode(ParseMode::Html)
                .await;
            warn!(filename = %filename, error = %e, "YouTube upload failed");
            return Err(e);
        }
    }

    Ok(())
}

/// 更新上传任务状态。
async fn update_task_status(
    state: &SharedState,
    task_id: &str,
    status: &str,
    progress: Option<f64>,
) {
    let mut s = state.write().await;
    if let Some(task) = s.youtube_pool.get_mut(task_id) {
        task.status = status.to_string();
        if let Some(p) = progress {
            task.progress = p;
        }
    }
}