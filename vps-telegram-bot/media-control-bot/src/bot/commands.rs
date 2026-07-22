//! Telegram 命令处理器。
//!
//! 处理 `/start`、`/stop`、`/uploads` 等文本命令。
//! 对应 Python 版本的 `render_main_menu`、`cmd_stop`、`cmd_uploads` 函数。

use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::info;

use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::PermissionGuard;
use crate::ui::menu::MainMenu;
use crate::utils::format::format_upload_list;

/// 处理 `/start` 命令，渲染主菜单。
pub async fn cmd_start(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let guard = PermissionGuard::new(config.admin_id);
    if guard.check_message(&msg).is_err() {
        return Ok(());
    }

    let (text, keyboard) = {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        let active_name = s.active_task_name().map(|n| n.to_string());
        let upload_count = s.active_youtube_count();
        MainMenu::render(&config.base_dir, active_name.as_deref(), upload_count)
    };

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;

    info!(user_id = ?msg.from.as_ref().map(|u| u.id), "Main menu displayed via /start");
    Ok(())
}

/// 处理 `/stop` 命令，终止所有正在运行的任务。
///
/// 对应 Python 版本的 `cmd_stop` 函数。
pub async fn cmd_stop(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let guard = PermissionGuard::new(config.admin_id);
    if guard.check_message(&msg).is_err() {
        return Ok(());
    }

    let (has_active, has_uploads, upload_count) = {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        let has_active = s.has_active_task();
        let upload_count = s.active_youtube_count();
        (has_active, upload_count > 0, upload_count)
    };

    if !has_active && !has_uploads {
        bot.send_message(msg.chat.id, "ℹ️ 当前没有正在运行的任务。")
            .await?;
        return Ok(());
    }

    // 发送取消信号
    {
        let mut s = state.write().await;
        s.cancel_all();
    }

    let mut parts = Vec::new();
    if has_active {
        parts.push("正在中断当前独占任务".to_string());
    }
    if has_uploads {
        parts.push(format!("已标记 {upload_count} 个 YouTube 上传任务为取消"));
    }

    let reply = format!(
        "🛑 <b>已接收停止指令！</b>\n{}\n\nYouTube 上传会在当前 chunk 返回后停止。",
        parts.join("\n")
    );

    bot.send_message(msg.chat.id, reply)
        .parse_mode(ParseMode::Html)
        .await?;

    info!("Stop command executed: has_active={has_active}, upload_count={upload_count}");
    Ok(())
}

/// 处理 `/uploads` 命令，显示当前 YouTube 上传任务列表。
///
/// 对应 Python 版本的 `cmd_uploads` 函数。
pub async fn cmd_uploads(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let guard = PermissionGuard::new(config.admin_id);
    if guard.check_message(&msg).is_err() {
        return Ok(());
    }

    let text = {
        let mut s = state.write().await;
        s.cleanup_youtube_pool();

        if s.youtube_pool.is_empty() {
            "ℹ️ 当前没有 YouTube 上传任务。".to_string()
        } else {
            format_upload_list(&s.youtube_pool)
        }
    };

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;

    Ok(())
}