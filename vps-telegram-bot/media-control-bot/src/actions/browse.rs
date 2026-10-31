//! 文件浏览与详情查看操作。
//!
//! 二次核验路径位于 `BASE_DIR` 内后，调用带超时的 `probe_media` 展示文件体积、时长、音视频编码与修改时间。

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::warn;

use crate::bot::keyboard::back_to_main_keyboard;
use crate::config::Config;
use crate::media::ffprobe::probe_media;
use crate::storage::filesystem::format_file_size;
use crate::storage::path::PathGuard;
use crate::utils::format::{escape_html, format_duration};
use crate::utils::redact::redact_secrets;

/// Telegram `answerCallbackQuery` 的弹窗文本上限为 200 字符，留余量取 185。
const ALERT_TEXT_LIMIT: usize = 185;

/// 展示单个视频文件的详情信息（短文本走弹窗提示，超长自动切换为编辑面板展示）。
pub async fn action_browse(
    bot: &Bot,
    q: &CallbackQuery,
    msg: &Message,
    config: &Config,
    raw_file_path: PathBuf,
) -> Result<()> {
    let guard = PathGuard::new(config.base_dir.clone());
    let file_path = guard.assert_regular_file_no_symlink(&raw_file_path)?;

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let size_str = format_file_size(&file_path);

    let (dur_str, stream_summary) = match probe_media(&file_path, config.ffprobe_timeout_secs).await
    {
        Ok(probe) => {
            let d = if probe.duration_secs > 0.0 {
                format_duration(probe.duration_secs)
            } else {
                "未知（容器未记录）".to_string()
            };
            let v_desc = probe
                .video_streams
                .first()
                .map(|v| format!("{} {}x{}", v.codec_name, v.width, v.height))
                .unwrap_or_else(|| "无视频轨".to_string());
            let a_desc = probe
                .audio_streams
                .first()
                .map(|a| a.codec_name.clone())
                .unwrap_or_else(|| "无音频轨".to_string());
            (d, format!("{v_desc} / {a_desc}"))
        }
        Err(_) => ("探测超时或未知".to_string(), "未知".to_string()),
    };

    let mtime_str = std::fs::metadata(&file_path)
        .and_then(|m| m.modified())
        .map(|t| {
            let dt: DateTime<Local> = t.into();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_else(|_| "未知".to_string());

    let info_text = format!(
        "📄 {filename}\n\
         ━━━━━━━━━━━━\n\
         📏 大小: {size_str}\n\
         ⏱️ 时长: {dur_str}\n\
         🎞️ 编码: {stream_summary}\n\
         🕒 修改: {mtime_str}"
    );

    if info_text.chars().count() <= ALERT_TEXT_LIMIT {
        bot.answer_callback_query(&q.id)
            .text(info_text)
            .show_alert(true)
            .await
            .map_err(|e| {
                warn!(error = %redact_secrets(&e.to_string()), "Failed to answer browse callback");
                e
            })?;
        return Ok(());
    }

    let html_text = format!(
        "📄 <b>文件详情</b>\n\
         ━━━━━━━━━━━━\n\
         📁 <code>{}</code>\n\
         📏 大小: <code>{}</code>\n\
         ⏱️ 时长: <code>{}</code>\n\
         🎞️ 编码参数: <code>{}</code>\n\
         🕒 修改时间: <code>{}</code>",
        escape_html(filename),
        escape_html(&size_str),
        escape_html(&dur_str),
        escape_html(&stream_summary),
        escape_html(&mtime_str)
    );

    bot.answer_callback_query(&q.id).await?;
    bot.edit_message_text(msg.chat.id, msg.id, html_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(back_to_main_keyboard())
        .await?;

    Ok(())
}
