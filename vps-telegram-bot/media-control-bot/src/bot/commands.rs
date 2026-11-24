//! Telegram 命令处理器。
//!
//! 处理 `/start`、`/stop`、`/uploads` 文本命令。
//! `/start` 每次触发时均实时读取 `/storage512` 最新容量统计（禁止使用过期缓存）。

use std::sync::Arc;
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tracing::{info, warn};

use crate::config::Config;
use crate::core::state::{ActionType, SharedState};
use crate::core::task_manager::TaskManager;
use crate::core::PermissionGuard;
use crate::ui::menu::MainMenu;
use crate::utils::format::format_upload_list;
use crate::utils::format::escape_html;
use crate::storage::trash::TrashStore;
use crate::storage::filesystem::format_file_size_bytes;
use crate::ui::selector::render_file_selector;

/// 处理 `/start` 命令：实时探测 `/storage512` 容量并渲染主控面板。
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

    let disk_monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    // 严格遵守文档 §4.2：每次 /start 必须实时读取磁盘数据，失败降级显示“磁盘信息暂不可用”而不阻断主菜单
    let disk_summary = disk_monitor.check_and_notify(&bot).await;

    let (text, keyboard) = {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        let active_name = s.active_task_name().map(str::to_string);
        let upload_count = s.active_youtube_count();
        MainMenu::render(
            &config.base_dir,
            &disk_summary,
            active_name.as_deref(),
            upload_count,
        )
    };

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;

    info!(user_id = ?msg.from.as_ref().map(|u| u.id), "Main menu displayed via /start with live disk stats");
    Ok(())
}

/// 处理 `/stop` 命令，立即终止运行中的独占任务与所有排队中/传输中的 YouTube 上传任务。
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

    let tm = TaskManager::new(state);
    let report = match tm.initiate_stop().await {
        crate::core::task_manager::StopInitiation::NoActiveTasks => {
            bot.send_message(msg.chat.id, "ℹ️ 当前没有正在运行或排队中的任务。")
                .await?;
            return Ok(());
        }
        crate::core::task_manager::StopInitiation::AlreadyStopping {
            exclusive_name,
            youtube_count,
        } => {
            let mut note = "⏳ <b>已有停止请求正在等待任务退出，已重申取消信号</b>".to_string();
            if let Some(name) = exclusive_name {
                note.push_str(&format!("\n独占任务: <code>{}</code>", escape_html(&name)));
            }
            if youtube_count > 0 {
                note.push_str(&format!("\nYouTube 任务: {youtube_count}"));
            }
            bot.send_message(msg.chat.id, note)
                .parse_mode(ParseMode::Html)
                .await?;
            return Ok(());
        }
        crate::core::task_manager::StopInitiation::Started(ticket) => {
            tm.wait_for_stop_ticket(ticket, Duration::from_secs(5))
                .await
        }
    };
    let reply = if report.all_exited() {
        format!(
            "🛑 <b>任务已退出并完成清理</b>\n独占任务: {}\nYouTube 任务: {}",
            report.stopped_exclusive.is_some() as usize,
            report.stopped_youtube.len()
        )
    } else {
        format!(
            "⏳ <b>已请求停止，部分任务仍在退出</b>\n独占任务待退出: {}\nYouTube 任务待退出: {}",
            report.timed_out_exclusive.is_some() as usize,
            report.timed_out_youtube.len()
        )
    };

    bot.send_message(msg.chat.id, reply)
        .parse_mode(ParseMode::Html)
        .await?;

    info!(all_exited = report.all_exited(), "Stop command completed");
    Ok(())
}

/// Display active work with buttons that cancel only the selected task.
pub async fn cmd_tasks(bot: Bot, msg: Message, state: SharedState, config: Arc<Config>) -> ResponseResult<()> {
    if PermissionGuard::new(config.admin_id).check_message(&msg).is_err() { return Ok(()); }
    let mut rows = Vec::new();
    let mut lines = vec!["📋 <b>当前任务</b>".to_string()];
    {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        if let Some(task) = &s.active_task {
            lines.push(format!("• 独占 #{}: {}", task.task_id, escape_html(&task.name)));
            rows.push(vec![InlineKeyboardButton::callback(
                format!("停止独占任务 #{}", task.task_id),
                format!("stop_excl_{}", task.task_id),
            )]);
        }
        let mut uploads: Vec<_> = s.youtube_pool.iter().collect();
        uploads.sort_by_key(|(id, _)| *id);
        for (id, task) in uploads.into_iter().take(12) {
            let short_name: String = task.filename.chars().take(60).collect();
            let short_status: String = task.status.chars().take(60).collect();
            lines.push(format!(
                "• {}: {} ({:.1}%)",
                escape_html(&short_name),
                escape_html(&short_status),
                task.progress
            ));
            if id.len() <= 56 {
                rows.push(vec![InlineKeyboardButton::callback(
                    format!("停止 {}", task.filename.chars().take(24).collect::<String>()),
                    format!("stop_yt_{id}"),
                )]);
            }
        }
    }
    if lines.len() == 1 { lines.push("当前没有任务。".to_string()); }
    let mut text = lines.join("\n");
    if text.chars().count() > 3900 {
        text = text.chars().take(3880).collect::<String>() + "\n…（任务列表已截断）";
    }
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(InlineKeyboardMarkup::new(rows))
        .await?;
    Ok(())
}

/// Display recent process tasks and durable confirmed YouTube results.
pub async fn cmd_history(bot: Bot, msg: Message, state: SharedState, config: Arc<Config>) -> ResponseResult<()> {
    if PermissionGuard::new(config.admin_id).check_message(&msg).is_err() { return Ok(()); }
    let records: Vec<_> = { state.read().await.recent_tasks.iter().take(20).cloned().collect() };
    let empty = records.is_empty();
    let mut lines = vec!["🕘 <b>最近任务（本次运行）</b>".to_string()];
    let mut length = lines[0].len();
    for record in records {
        let name: String = record.name.chars().take(120).collect();
        let line = format!("• {} {}：{}", record.finished_at, escape_html(&name), escape_html(&record.result));
        if length + line.len() + 1 > 3500 {
            bot.send_message(msg.chat.id, lines.join("\n")).parse_mode(ParseMode::Html).await?;
            lines.clear();
            length = 0;
        }
        length += line.len() + 1;
        lines.push(line);
    }
    if empty { lines.push("暂无记录。".to_string()); }
    bot.send_message(msg.chat.id, lines.join("\n")).parse_mode(ParseMode::Html).await?;
    if let Err(e) = render_upload_history(&bot, &msg, &state, 0).await {
        warn!(error = %crate::utils::redact::redact_secrets(&e.to_string()), "Cannot display persisted YouTube results");
        bot.send_message(msg.chat.id, "⚠️ 上传结果历史读取失败，请检查服务端日志。").await?;
    }
    Ok(())
}

/// One durable result per page, with an explicit resend action.
pub async fn render_upload_history(bot: &Bot, msg: &Message, state: &SharedState, page: usize) -> anyhow::Result<()> {
    use crate::youtube::results::NotificationStatus;
    let store = { Arc::clone(&state.read().await.youtube_queue_store) };
    let records: Vec<_> = store.results().load().await?.into_iter()
        .filter(|r| r.chat_id == msg.chat.id.0).collect();
    if records.is_empty() {
        bot.send_message(msg.chat.id, "📚 暂无已保存的 YouTube 成功结果（旧版本记录不会自动补入）。").await?;
        return Ok(());
    }
    let page = page.min(records.len() - 1);
    let record = &records[page];
    let status = match record.notification_status {
        NotificationStatus::Pending => "待发送或上次发送中断",
        NotificationStatus::Sent => "已发送",
        NotificationStatus::Failed => "发送失败，可补发",
    };
    let filename: String = record.filename.chars().take(150).collect();
    let text = format!(
        "📚 <b>YouTube 成功结果（持久保存）</b> {}/{}\n🎬 <code>{}</code>\n🆔 <code>{}</code>\n🕒 {}\n📺 https://youtu.be/{}\n通知：{}，累计尝试 {} 次",
        page + 1, records.len(), escape_html(&filename), escape_html(&record.video_id),
        escape_html(&record.completed_at), escape_html(&record.video_id), status, record.notification_attempts
    );
    let mut rows = Vec::new();
    if record.task_id.len() <= 54 {
        rows.push(vec![InlineKeyboardButton::callback("补发此成功通知", format!("resend_yt_{}", record.task_id))]);
    }
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(InlineKeyboardButton::callback("上一条", format!("history_yt_{}", page - 1)));
    }
    if page + 1 < records.len() {
        navigation.push(InlineKeyboardButton::callback("下一条", format!("history_yt_{}", page + 1)));
    }
    if !navigation.is_empty() { rows.push(navigation); }
    bot.send_message(msg.chat.id, text).parse_mode(ParseMode::Html)
        .reply_markup(InlineKeyboardMarkup::new(rows)).await?;
    Ok(())
}

/// List uploads whose final server state requires an explicit status query.
pub async fn cmd_pending(bot: Bot, msg: Message, state: SharedState, config: Arc<Config>) -> ResponseResult<()> {
    if PermissionGuard::new(config.admin_id).check_message(&msg).is_err() { return Ok(()); }
    if let Err(_) = render_pending_panel(&bot, &msg, &state, &config, 0, false).await {
        bot.send_message(msg.chat.id, "无法读取上传恢复队列，请检查服务端日志。").await?;
    }
    Ok(())
}

/// Paginated `/pending` panel filtered by `chat_id`.
pub async fn render_pending_panel(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    page: usize,
    edit: bool,
) -> anyhow::Result<()> {
    const PENDING_PAGE_SIZE: usize = 8;
    let (store, active_ids): (_, std::collections::HashSet<String>) = {
        let s = state.read().await;
        (
            Arc::clone(&s.youtube_queue_store),
            s.youtube_pool.keys().cloned().collect(),
        )
    };
    let entries: Vec<_> = store
        .load_recoverable_entries()
        .await?
        .into_iter()
        .filter(|e| {
            !active_ids.contains(&e.task_id)
                && (e.chat_id == msg.chat.id.0
                    || (e.chat_id == 0 && msg.chat.id.0 == config.admin_id))
        })
        .collect();

    let total_pages = entries.len().div_ceil(PENDING_PAGE_SIZE).max(1);
    let page = page.min(total_pages - 1);
    let start = page * PENDING_PAGE_SIZE;
    let slice = entries
        .get(start..(start + PENDING_PAGE_SIZE).min(entries.len()))
        .unwrap_or(&[]);

    let mut lines = vec![format!(
        "🔎 <b>待核实与搁置的 YouTube 上传</b>（第 {}/{total_pages} 页，共 {} 项）",
        page + 1,
        entries.len()
    )];
    let mut rows = Vec::new();
    for entry in slice {
        let tag = if entry.unconfirmed_full_upload {
            "[待核实]"
        } else {
            "[已搁置]"
        };
        lines.push(format!("• {tag} {}", escape_html(&entry.filename)));
        if entry.task_id.len() <= 42 {
            let mut action_row = Vec::new();
            if entry.unconfirmed_full_upload && entry.session_uri.is_some() {
                action_row.push(InlineKeyboardButton::callback(
                    format!("核实 {}", entry.filename.chars().take(18).collect::<String>()),
                    format!("verify_yt_{}", entry.task_id),
                ));
            }
            action_row.push(InlineKeyboardButton::callback(
                if entry.session_uri.is_some() { "继续原会话" } else { "重新入队" },
                format!("pending_resume_{}", entry.task_id),
            ));
            rows.push(action_row);
            rows.push(vec![InlineKeyboardButton::callback(
                "放弃此恢复记录",
                format!("pending_dropask_{}", entry.task_id),
            )]);
        }
    }
    if entries.is_empty() {
        lines.push("当前聊天没有待核实或搁置的上传记录。".to_string());
    }
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(InlineKeyboardButton::callback("⬅️ 上一页", format!("pending_page_{}", page - 1)));
    }
    if page + 1 < total_pages {
        nav.push(InlineKeyboardButton::callback("下一页 ➡️", format!("pending_page_{}", page + 1)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    let keyboard = InlineKeyboardMarkup::new(rows);
    if edit {
        if let Err(err) = bot
            .edit_message_text(msg.chat.id, msg.id, lines.join("\n"))
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await
        {
            if !matches!(err, teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)) {
                return Err(err.into());
            }
        }
    } else {
        bot.send_message(msg.chat.id, lines.join("\n"))
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
    }
    Ok(())
}

/// Show recoverable deleted files.
pub async fn cmd_trash(bot: Bot, msg: Message, state: SharedState, config: Arc<Config>) -> ResponseResult<()> {
    if PermissionGuard::new(config.admin_id).check_message(&msg).is_err() { return Ok(()); }
    if let Err(e) = render_trash_panel(&bot, &msg, &state, &config, 0, false).await {
        bot.send_message(msg.chat.id,
            format!("无法打开回收站：{}", crate::utils::redact::sanitize_user_error(&e))).await?;
    }
    Ok(())
}

/// Render the same paginated trash view for `/trash` and the main-menu button.
pub async fn render_trash_panel(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    page: usize,
    edit: bool,
) -> anyhow::Result<()> {
    const TRASH_PAGE_SIZE: usize = 8;
    let monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    monitor.ensure_space_available(0).await?;
    let store = TrashStore::new(config.base_dir.clone(), config.trash_retention_days);
    let entries = tokio::task::spawn_blocking(move || store.list())
        .await
        .map_err(|e| anyhow::anyhow!("回收站列表读取线程异常: {e}"))??;
    let total_pages = entries.len().div_ceil(TRASH_PAGE_SIZE).max(1);
    let page = page.min(total_pages - 1);
    let start = page * TRASH_PAGE_SIZE;
    let slice = entries
        .get(start..(start + TRASH_PAGE_SIZE).min(entries.len()))
        .unwrap_or(&[]);

    let mut lines = vec![format!(
        "🗑️ <b>回收站</b>（保留 {} 天，第 {}/{total_pages} 页）",
        config.trash_retention_days,
        page + 1
    )];
    let mut rows = Vec::new();
    for entry in slice {
        let name = entry.original_path.file_name().and_then(|s| s.to_str()).unwrap_or("unknown");
        lines.push(format!("• {}", escape_html(name)));
        rows.push(vec![InlineKeyboardButton::callback(
            format!("恢复 {}", name.chars().take(32).collect::<String>()),
            format!("restore_{}", entry.id),
        )]);
    }
    if entries.is_empty() { lines.push("回收站为空。".to_string()); }
    if !entries.is_empty() {
        let total_bytes = entries.iter().fold(0_u64, |sum, entry| sum.saturating_add(entry.size));
        lines.push(format!("\n共 {} 个文件，约 {}。", entries.len(), format_file_size_bytes(total_bytes)));
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(InlineKeyboardButton::callback("⬅️ 上一页", format!("trash_page_{}", page - 1)));
        }
        if page + 1 < total_pages {
            nav.push(InlineKeyboardButton::callback("下一页 ➡️", format!("trash_page_{}", page + 1)));
        }
        if !nav.is_empty() {
            rows.push(nav);
        }
        rows.push(vec![InlineKeyboardButton::callback("🧹 清空回收站", "trash_clear_ask")]);
    }
    rows.push(vec![InlineKeyboardButton::callback("🔙 返回主菜单", "menu_main")]);
    let keyboard = InlineKeyboardMarkup::new(rows);
    if edit {
        if let Err(err) = bot
            .edit_message_text(msg.chat.id, msg.id, lines.join("\n"))
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await
        {
            if !matches!(
                err,
                teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
            ) {
                return Err(err.into());
            }
        }
    } else {
        bot.send_message(msg.chat.id, lines.join("\n"))
            .parse_mode(ParseMode::Html).reply_markup(keyboard).await?;
    }
    Ok(())
}

/// Apply a file-name filter and open a fresh selector message.
pub async fn cmd_find(bot: Bot, msg: Message, state: SharedState, config: Arc<Config>, query: String) -> ResponseResult<()> {
    if PermissionGuard::new(config.admin_id).check_message(&msg).is_err() { return Ok(()); }
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 80 {
        bot.send_message(msg.chat.id, "请使用 /find 关键词（最多 80 字）。").await?;
        return Ok(());
    }
    let menu_lock = { let s = state.read().await; Arc::clone(&s.menu_lock) };
    let _menu_guard = menu_lock.lock().await;
    let action = {
        let mut s = state.write().await;
        let action = s.current_snapshot.as_ref().map(|snap| snap.action.clone()).unwrap_or(ActionType::Browse);
        s.search_query = query.to_string();
        s.current_snapshot = None;
        s.pending_delete = None;
        s.selections.clear();
        action
    };
    let panel = bot.send_message(msg.chat.id, "正在筛选文件…").await?;
    if let Err(e) = render_file_selector(&bot, &panel, &state, &config, &action, 0).await {
        bot.send_message(msg.chat.id, format!("筛选失败：{}", crate::utils::redact::sanitize_user_error(&e))).await?;
    }
    state.write().await.save_menu_session((panel.chat.id.0, panel.id.0));
    Ok(())
}

pub async fn cmd_clearfilter(bot: Bot, msg: Message, state: SharedState, config: Arc<Config>) -> ResponseResult<()> {
    if PermissionGuard::new(config.admin_id).check_message(&msg).is_err() { return Ok(()); }
    let menu_lock = { let s = state.read().await; Arc::clone(&s.menu_lock) };
    let _menu_guard = menu_lock.lock().await;
    let action = {
        let mut s = state.write().await;
        let action = s.current_snapshot.as_ref().map(|snap| snap.action.clone()).unwrap_or(ActionType::Browse);
        s.search_query.clear();
        s.current_snapshot = None;
        s.pending_delete = None;
        s.selections.clear();
        action
    };
    let panel = bot.send_message(msg.chat.id, "正在显示全部文件…").await?;
    if let Err(e) = render_file_selector(&bot, &panel, &state, &config, &action, 0).await {
        bot.send_message(msg.chat.id, format!("打开目录失败：{}", crate::utils::redact::sanitize_user_error(&e))).await?;
    }
    state.write().await.save_menu_session((panel.chat.id.0, panel.id.0));
    Ok(())
}

/// 处理 `/uploads` 命令，显示当前 YouTube 上传与排队任务列表。
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

    let mut text = {
        let mut s = state.write().await;
        s.cleanup_youtube_pool();

        if s.youtube_pool.is_empty() {
            "ℹ️ 当前没有运行中或排队中的 YouTube 上传任务。".to_string()
        } else {
            format_upload_list(&s.youtube_pool)
        }
    };

    if text.chars().count() > 3900 {
        text = text.chars().take(3880).collect::<String>() + "\n…（列表过长已截断）";
    }

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;

    Ok(())
}
