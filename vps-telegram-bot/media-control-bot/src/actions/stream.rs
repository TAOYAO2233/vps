//! RTMP 推流操作。
//!
//! 对应 Python 版本的 `action_stream` 函数。
//! 使用 FFmpeg 将视频文件推送到 RTMP 地址，实时解析进度并更新 Telegram 消息。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::info;

use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::media::ffmpeg::FfmpegProcess;
use crate::media::ffprobe::get_video_duration;
use crate::storage::filesystem::format_file_size;
use crate::utils::format::escape_html;

/// 取消标志轮询间隔：即使 FFmpeg 长时间不输出内容，也能及时响应 `/stop`。
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 启动 RTMP 推流独占任务。
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `msg` - 触发此操作的消息（用于发送进度回复）
/// * `state` - 全局共享状态
/// * `config` - 应用配置
/// * `file_path` - 要推流的视频文件路径（已通过路径安全校验）
///
/// # Errors
///
/// 已有任务占用、RTMP 地址未配置或推流启动失败时返回错误。
pub async fn start_stream(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    file_path: PathBuf,
) -> Result<()> {
    if config.rtmp_url.is_empty() {
        let _edit_msg = bot
            .send_message(
                msg.chat.id,
                "❌ RTMP_URL 未配置。请在 <code>.env</code> 中添加 <code>RTMP_URL=你的推流地址</code>。",
            )
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let task_manager = TaskManager::new(Arc::clone(&state));
    let bot_clone = bot.clone();
    let msg_clone = msg.clone();
    let rtmp_url = config.rtmp_url.clone();

    task_manager
        .start_exclusive("RTMP 推流", move || {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let path = file_path.clone();
            let rtmp = rtmp_url.clone();
            let state_inner = Arc::clone(&state);
            async move { do_stream(bot, msg, state_inner, path, rtmp).await }
        })
        .await
        .map_err(|e| {
            // 将任务互斥错误转换为用户友好提示
            match e.downcast_ref::<AppError>() {
                Some(AppError::TaskAlreadyRunning { task_name }) => {
                    anyhow::anyhow!(
                        "已有任务正在运行：{task_name}\n请先发送 /stop 或等待完成。"
                    )
                }
                Some(AppError::YoutubeUploadBlocking { count }) => {
                    anyhow::anyhow!(
                        "当前有 {count} 个 YouTube 上传任务在运行/排队。\n为避免边上传边推流导致文件冲突，请先 /stop 或等待上传完成。"
                    )
                }
                _ => e,
            }
        })?;

    Ok(())
}

/// 实际执行推流逻辑（在独占任务内运行）。
async fn do_stream(
    bot: Bot,
    msg: Message,
    state: SharedState,
    file_path: PathBuf,
    rtmp_url: String,
) -> Result<()> {
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let size_str = format_file_size(&file_path);

    let progress_msg = bot
        .send_message(
            msg.chat.id,
            format!(
                "⏳ 正在分析推流文件: <code>{}</code> ({size_str})...",
                escape_html(&filename)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;

    // 检查取消标志
    if state.read().await.cancel_flag {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!("🛑 <b>推流已手动终止</b>:\n<code>{}</code>", escape_html(&filename)),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Err(AppError::Cancelled.into());
    }

    let duration = get_video_duration(&file_path).await.unwrap_or(0.0);

    if duration <= 0.0 {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            "❌ 无法获取视频时长，推流终止。",
        )
        .await?;
        return Ok(());
    }

    // 启动 FFmpeg 推流进程
    let args = vec![
        OsString::from("-re"),
        OsString::from("-i"),
        file_path.as_os_str().to_os_string(),
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-f"),
        OsString::from("flv"),
        OsString::from(rtmp_url.as_str()),
    ];
    let mut process = FfmpegProcess::spawn(args, duration, &state).await?;

    let progress_bar = ProgressBar::default();
    info!(
        filename = %filename,
        rtmp_url = %rtmp_url,
        pid = ?process.pid(),
        "RTMP stream started"
    );

    loop {
        // 检查取消标志
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
                                    "📡 <b>推流中</b>: <code>{}</code>\n\n<code>{}</code>\n⏱️ {}s / {}s",
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
    let cancelled = state.read().await.cancel_flag;

    if cancelled {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!("🛑 <b>推流已手动终止</b>:\n<code>{}</code>", escape_html(&filename)),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Err(AppError::Cancelled.into());
    }

    if exit_code == Some(0) {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!("✅ <b>推流结束</b>:\n<code>{}</code>", escape_html(&filename)),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        info!(filename = %filename, "RTMP stream completed successfully");
    } else {
        let code = exit_code.unwrap_or(-1);
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!(
                "❌ <b>推流异常结束</b>:\n<code>{}</code>\n退出码: <code>{code}</code>",
                escape_html(&filename)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Err(AppError::RtmpStreamFailed { exit_code: code }.into());
    }

    Ok(())
}
