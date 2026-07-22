//! 文件浏览与详情查看。
//!
//! 对应 Python 版本的 `action_browse` 函数。
//! 通过 `ffprobe` 获取视频时长，并以 Telegram alert 弹窗形式展示文件详情。

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local};
use teloxide::prelude::*;
use tracing::warn;

use crate::media::ffprobe::get_video_duration;
use crate::storage::filesystem::format_file_size;
use crate::utils::format::format_duration;

/// 展示单个文件的详情信息（通过 CallbackQuery alert 弹窗）。
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `q` - 触发此操作的 CallbackQuery
/// * `file_path` - 目标文件路径（已通过路径安全校验）
pub async fn action_browse(bot: &Bot, q: &CallbackQuery, file_path: PathBuf) -> Result<()> {
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

    bot.answer_callback_query(&q.id)
        .text(info_text)
        .show_alert(true)
        .await
        .map_err(|e| {
            warn!(error = %e, "Failed to answer browse callback");
            e
        })?;

    Ok(())
}
