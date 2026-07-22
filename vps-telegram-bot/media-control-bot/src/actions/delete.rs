//! 批量文件删除操作。
//!
//! 对应 Python 版本的 `action_delete` 和 `render_delete_confirmation` 函数。
//! 实现二次确认机制，防止误删文件。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{info, warn};

use crate::bot::keyboard::delete_confirmation_keyboard;
use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::errors::AppError;

/// 渲染删除确认界面（二次确认）。
///
/// 将待删除文件列表存入状态，并显示确认/取消按钮。
pub async fn render_delete_confirmation(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    target_files: Vec<PathBuf>,
) -> Result<()> {
    // 存入待确认删除列表
    {
        let mut s = state.write().await;
        s.pending_delete_files = target_files.clone();
    }

    let preview_count = 12;
    let mut preview_lines: Vec<String> = target_files
        .iter()
        .take(preview_count)
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .map(|name| format!("• `{name}`"))
        .collect();

    if target_files.len() > preview_count {
        preview_lines.push(format!(
            "... 其余 {} 个文件未展开",
            target_files.len() - preview_count
        ));
    }

    let text = format!(
        "⚠️ **二次确认：即将永久删除以下文件**\n\n{}\n\n删除后不可恢复。",
        preview_lines.join("\n")
    );

    bot.edit_message_text(msg.chat.id, msg.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(delete_confirmation_keyboard())
        .await?;

    Ok(())
}

/// 启动批量删除独占任务。
///
/// 此函数在用户点击"确认删除"后调用。
pub async fn start_delete(
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
        .start_exclusive("删除文件", move || {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let state_inner = Arc::clone(&state);
            async move { do_delete(bot, msg, state_inner, files).await }
        })
        .await
        .map_err(|e| match e.downcast_ref::<AppError>() {
            Some(AppError::TaskAlreadyRunning { task_name }) => {
                anyhow::anyhow!("已有任务正在运行：{task_name}\n请先发送 /stop 或等待完成。")
            }
            _ => e,
        })?;

    Ok(())
}

/// 实际执行批量删除逻辑。
async fn do_delete(
    bot: Bot,
    msg: Message,
    state: SharedState,
    files_to_delete: Vec<PathBuf>,
) -> Result<()> {
    let mut deleted = 0usize;
    let mut failed = 0usize;

    for file_path in &files_to_delete {
        if state.read().await.cancel_flag {
            break;
        }

        if file_path.is_file() {
            match std::fs::remove_file(file_path) {
                Ok(()) => {
                    deleted += 1;
                    info!(path = ?file_path, "File deleted");
                }
                Err(e) => {
                    failed += 1;
                    warn!(path = ?file_path, error = %e, "Failed to delete file");
                }
            }
        } else {
            failed += 1;
            warn!(path = ?file_path, "Path is not a file, skipping");
        }
    }

    // 清理待删除列表
    {
        let mut s = state.write().await;
        s.pending_delete_files.clear();
    }

    let cancelled = state.read().await.cancel_flag;

    if cancelled {
        bot.edit_message_text(
            msg.chat.id,
            msg.id,
            format!("🛑 **删除任务已手动终止。**\n已删除 {deleted} 个文件。"),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Err(AppError::Cancelled.into());
    }

    bot.edit_message_text(
        msg.chat.id,
        msg.id,
        format!("🗑️ **清理完成!**\n成功删除 {deleted} 个文件，失败 {failed} 个。"),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .await?;

    Ok(())
}
