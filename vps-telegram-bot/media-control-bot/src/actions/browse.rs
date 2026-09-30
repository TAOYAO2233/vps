//! 文件浏览与详情查看。
//!
//! 对应 Python 版本的 `action_browse` 函数。
//! 通过 `ffprobe` 获取视频时长，并以 Telegram alert 弹窗形式展示文件详情；
//! 详情超过 Telegram callback 文本上限时改为编辑原消息展示。

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::warn;

use crate::bot::keyboard::back_to_main_keyboard;
use crate::media::ffprobe::get_video_duration;
use crate::storage::filesystem::format_file_size;
use crate::utils::format::{escape_html, format_duration};

/// Telegram `answerCallbackQuery` 的文本上限为 200 字符，留出余量后取 190。
const ALERT_TEXT_LIMIT: usize = 190;

/// 展示单个文件的详情信息。
///
/// 文本较短时通过 CallbackQuery alert 弹窗展示；超过 [`ALERT_TEXT_LIMIT`] 时
/// 改为编辑原消息（HTML 模式）并附上「返回主菜单」按钮，避免 Telegram 拒绝请求。
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `q` - 触发此操作的 CallbackQuery
/// * `msg` - 触发此操作的消息（超长详情时用于编辑）
/// * `file_path` - 目标文件路径（已通过路径安全校验）
///
/// # Errors
///
/// Telegram API 调用失败时返回错误。
pub async fn action_browse(
    bot: &Bot,
    q: &CallbackQuery,
    msg: &Message,
    file_path: PathBuf,
) -> Result<()> {
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let size_str = format_file_size(&file_path);

    let duration = get_video_duration(&file_path).await.unwrap_or(0.0);
    let dur_str = if duration > 0.0 {
        format_duration(duration)
    } else {
        "未知或无损流".to_string()
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
         🕒 修改时间: {mtime_str}"
    );

    if info_text.chars().count() <= ALERT_TEXT_LIMIT {
        bot.answer_callback_query(&q.id)
            .text(info_text)
            .show_alert(true)
            .await
            .map_err(|e| {
                warn!(error = %e, "Failed to answer browse callback");
                e
            })?;
        return Ok(());
    }

    // 超长文本无法放进 alert（Telegram 上限 200 字符），改为编辑当前消息
    let html_text = format!(
        "📄 <b>文件详情</b>\n\
         ━━━━━━━━━━━━\n\
         📁 <code>{}</code>\n\
         📏 大小: <code>{}</code>\n\
         ⏱️ 时长: <code>{}</code>\n\
         🕒 修改时间: <code>{}</code>",
        escape_html(filename),
        escape_html(&size_str),
        escape_html(&dur_str),
        escape_html(&mtime_str)
    );

    bot.answer_callback_query(&q.id).await?;
    bot.edit_message_text(msg.chat.id, msg.id, html_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(back_to_main_keyboard())
        .await?;

    Ok(())
}
