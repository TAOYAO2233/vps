//! 批量视频转码操作。
//!
//! 对应 Python 版本的 `action_convert` 函数。
//! 将选中的视频文件无损封装转换为 MP4 格式（`-c copy -movflags +faststart`），
//! 实时解析 FFmpeg 进度并更新 Telegram 消息。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{info, warn};

use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::media::ffmpeg::FfmpegProcess;
use crate::media::ffprobe::get_video_duration;
use crate::storage::filesystem::remove_if_exists;
use crate::storage::path::unique_path;
use crate::utils::format::escape_html;

/// 取消标志轮询间隔：即使 FFmpeg 长时间不输出内容，也能及时响应 `/stop`。
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 启动批量转码独占任务。
///
/// # Errors
///
/// 已有任务占用时返回错误。
pub async fn start_convert(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    _config: Arc<Config>,
    files: Vec<PathBuf>,
) -> Result<()> {
    let task_manager = TaskManager::new(Arc::clone(&state));
    let bot_clone = bot.clone();
    let msg_clone = msg.clone();

    task_manager
        .start_exclusive("批量转码", move || {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let state_inner = Arc::clone(&state);
            async move { do_convert(bot, msg, state_inner, files).await }
        })
        .await
        .map_err(|e| match e.downcast_ref::<AppError>() {
            Some(AppError::TaskAlreadyRunning { task_name }) => {
                anyhow::anyhow!("已有任务正在运行：{task_name}\n请先发送 /stop 或等待完成。")
            }
            Some(AppError::YoutubeUploadBlocking { count }) => {
                anyhow::anyhow!(
                    "当前有 {count} 个 YouTube 上传任务在运行/排队，请先 /stop 或等待完成后再转码。"
                )
            }
            _ => e,
        })?;

    Ok(())
}

/// 实际执行批量转码逻辑。
async fn do_convert(
    bot: Bot,
    msg: Message,
    state: SharedState,
    files_to_convert: Vec<PathBuf>,
) -> Result<()> {
    let total = files_to_convert.len();
    let progress_msg = bot
        .send_message(msg.chat.id, format!("🔄 准备转换 {total} 个文件..."))
        .await?;

    let progress_bar = ProgressBar::default();
    let mut success_count = 0usize;

    for (idx, file_path) in files_to_convert.iter().enumerate() {
        if state.read().await.cancel_flag {
            bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 <b>批量转换已手动终止。</b>")
                .parse_mode(ParseMode::Html)
                .await?;
            return Err(AppError::Cancelled.into());
        }

        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // 生成输出路径（同目录，.mp4 后缀，自动避让同名文件）
        let stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let parent = file_path.parent().unwrap_or(std::path::Path::new("."));
        let output_path = unique_path(&parent.join(format!("{stem}.mp4")));
        let output_filename = output_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("output.mp4")
            .to_string();

        // 获取视频时长（用于进度计算）
        let duration = get_video_duration(file_path).await.unwrap_or(0.0);

        if state.read().await.cancel_flag {
            bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 <b>批量转换已手动终止。</b>")
                .parse_mode(ParseMode::Html)
                .await?;
            return Err(AppError::Cancelled.into());
        }

        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!(
                "🔄 <b>正在转换</b> ({}/{total}):\n<code>{}</code>\n-&gt; <code>{}</code>\n⏳ 获取进度中...",
                idx + 1,
                escape_html(&filename),
                escape_html(&output_filename)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;

        // 启动 FFmpeg 转码进程
        let args = vec![
            OsString::from("-y"),
            OsString::from("-i"),
            file_path.as_os_str().to_os_string(),
            OsString::from("-c"),
            OsString::from("copy"),
            OsString::from("-movflags"),
            OsString::from("+faststart"),
            output_path.as_os_str().to_os_string(),
        ];
        let mut process = FfmpegProcess::spawn(args, duration, &state).await?;

        loop {
            if state.read().await.cancel_flag {
                let _ = process.kill().await;
                break;
            }

            // 进度读取与取消轮询并行：即使 FFmpeg 不再输出，也能在 500ms 内响应 /stop
            tokio::select! {
                progress = process.next_progress() => {
                    match progress {
                        Ok(Some((percent, current_sec))) => {
                            let bar = progress_bar.render(percent);
                            let _ = bot
                                .edit_message_text(
                                    msg.chat.id,
                                    progress_msg.id,
                                    format!(
                                        "🔄 <b>正在转换</b> ({}/{total}):\n<code>{}</code>\n\n<code>{}</code>\n⏱️ {}s / {}s",
                                        idx + 1,
                                        escape_html(&filename),
                                        bar,
                                        current_sec as u64,
                                        duration as u64
                                    ),
                                )
                                .parse_mode(ParseMode::Html)
                                .await;
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {}
            }
        }

        let exit_code = process.wait().await?;

        if state.read().await.cancel_flag {
            remove_if_exists(&output_path);
            bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 <b>批量转换已手动终止。</b>")
                .parse_mode(ParseMode::Html)
                .await?;
            return Err(AppError::Cancelled.into());
        }

        if exit_code == Some(0)
            && output_path.exists()
            && output_path.metadata().map(|m| m.len()).unwrap_or(0) > 0
        {
            success_count += 1;
            info!(input = %filename, output = %output_filename, "Convert success");
        } else {
            remove_if_exists(&output_path);
            warn!(input = %filename, "Convert failed, removed partial output");
        }
    }

    bot.edit_message_text(
        msg.chat.id,
        progress_msg.id,
        format!(
            "✅ <b>批量转换完成!</b>\n成功转换 {success_count}/{total} 个文件。\n同名输出已自动避让。"
        ),
    )
    .parse_mode(ParseMode::Html)
    .await?;

    Ok(())
}
