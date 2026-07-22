//! RTMP 推流操作。
//!
//! 对应 Python 版本的 `action_stream` 函数。
//! 使用 FFmpeg 将视频文件推送到 RTMP 地址，实时解析进度并更新 Telegram 消息。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::info;

use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::media::ffprobe::get_video_duration;
use crate::storage::filesystem::format_file_size;
use crate::utils::format::escape_html;

/// 匹配 FFmpeg stderr 输出中的时间戳，例如 `time=00:01:23.45`
static TIME_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}").unwrap());

/// 进度更新最小间隔（秒）
const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(2);

/// 进度更新最小变化量（百分比）
const PROGRESS_UPDATE_THRESHOLD: f64 = 1.0;

/// 启动 RTMP 推流独占任务。
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `msg` - 触发此操作的消息（用于发送进度回复）
/// * `state` - 全局共享状态
/// * `config` - 应用配置
/// * `file_path` - 要推流的视频文件路径（已通过路径安全校验）
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
    let mut child = tokio::process::Command::new("ffmpeg")
        .args(["-re", "-i"])
        .arg(&file_path)
        .args(["-c", "copy", "-f", "flv"])
        .arg(&rtmp_url)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn ffmpeg: {e}"))?;

    // 记录 PID 以便强制终止
    if let Some(pid) = child.id() {
        state.write().await.current_process_pid = Some(pid);
    }

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture ffmpeg stderr"))?;

    let mut reader = tokio::io::BufReader::new(stderr);
    let mut line_buf = String::new();
    let progress_bar = ProgressBar::default();
    let mut last_update = Instant::now();
    let mut last_percent = -1.0_f64;

    info!(filename = %filename, rtmp_url = %rtmp_url, "RTMP stream started");

    loop {
        // 检查取消标志
        if state.read().await.cancel_flag {
            let _ = child.kill().await;
            break;
        }

        line_buf.clear();

        match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line_buf).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }

        if let Some(caps) = TIME_REGEX.captures(&line_buf) {
            let h: u64 = caps[1].parse().unwrap_or(0);
            let m: u64 = caps[2].parse().unwrap_or(0);
            let s: u64 = caps[3].parse().unwrap_or(0);
            let current_sec = h * 3600 + m * 60 + s;
            let percent = (current_sec as f64 / duration) * 100.0;

            if (percent - last_percent) >= PROGRESS_UPDATE_THRESHOLD
                && last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL
            {
                let bar = progress_bar.render(percent);
                let _ = bot
                    .edit_message_text(
                        msg.chat.id,
                        progress_msg.id,
                        format!(
                            "📡 <b>推流中</b>: <code>{}</code>\n\n<code>{}</code>\n⏱️ {current_sec}s / {}s",
                            escape_html(&filename),
                            bar,
                            duration as u64
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
                last_update = Instant::now();
                last_percent = percent.floor();
            }
        }
    }

    let status = child.wait().await?;
    state.write().await.current_process_pid = None;

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

    if status.success() {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!("✅ <b>推流结束</b>:\n<code>{}</code>", escape_html(&filename)),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        info!(filename = %filename, "RTMP stream completed successfully");
    } else {
        let code = status.code().unwrap_or(-1);
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