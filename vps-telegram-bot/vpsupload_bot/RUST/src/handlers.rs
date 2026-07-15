use crate::actions::*;
use crate::config::Config;
use crate::media_utils::format_elapsed;
use crate::task_manager::AppState;
use crate::ui::*;
use crate::youtube_upload::start_youtube_uploads;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

pub async fn handle_command(bot: Bot, msg: Message, cmd: String, config: Arc<Config>, state: Arc<AppState>) -> ResponseResult<()> {
    if msg.from().map(|u| u.id.0 as i64) != Some(config.admin_id) { return Ok(()); }
    let chat_id = msg.chat.id;

    match cmd.as_str() {
        "/start" => { let _ = render_main_menu(bot, chat_id, None, config, state).await; }
        "/stop" => {
            let stopped = state.stop_all_tasks().await;
            let text = if stopped.is_empty() { "ℹ️ 当前没有在运行的任务。".to_string() } else { format!("🛑 **已发送终止指令**:\n{}", stopped.join("\n")) };
            bot.send_message(chat_id, text).parse_mode(ParseMode::MarkdownV2).await?;
        }
        "/uploads" => {
            let uploads = state.youtube_uploads.lock().await;
            let text = if uploads.is_empty() {
                "ℹ️ 当前没有 YouTube 上传任务。".to_string()
            } else {
                let mut lines = vec![format!("📤 **YouTube 上传队列 ({})**", uploads.len())];
                for (i, info) in uploads.values().enumerate() {
                    let elapsed = format_elapsed(chrono::Utc::now().timestamp() as f64 - info.created_at);
                    lines.push(format!("{}. `{}` - {} ({:.1}%) [耗时 {}]", i+1, info.filename, info.status, info.progress, elapsed));
                }
                lines.join("\n")
            };
            bot.send_message(chat_id, text).parse_mode(ParseMode::MarkdownV2).await?;
        }
        _ => {}
    }
    Ok(())
}

pub async fn handle_callback(bot: Bot, q: CallbackQuery, config: Arc<Config>, state: Arc<AppState>) -> ResponseResult<()> {
    if q.from.id.0 as i64 != config.admin_id { return Ok(()); }
    let data = match q.data.as_ref() { Some(d) => d.clone(), None => return Ok(()) };
    let chat_id = match q.message.as_ref() { Some(m) => m.chat.id, None => return Ok(()) };
    let msg_id = match q.message.as_ref() { Some(m) => m.id, None => return Ok(()) };
    let user_id = q.from.id.0 as i64;

    if data == "menu_main" {
        let _ = render_main_menu(bot, chat_id, Some(msg_id), config, state).await;
    } else if data.starts_with("init_") {
        let action = &data[5..];
        state.update_session(user_id, |s| {
            s.current_dir = config.base_dir.clone();
            s.selected.entry(action.to_string()).or_default().clear();
        }).await;
        let _ = render_file_selector(bot, q, action, 0, config, state).await;
    } else if data.starts_with("menu_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let page: usize = parts[2].parse().unwrap_or(0);
        let _ = render_file_selector(bot, q, action, page, config, state).await;
    } else if data.starts_with("enterdir_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let idx: usize = parts[2].parse().unwrap_or(0);
        let session = state.get_session(user_id, &config.base_dir).await;
        if let Some(folder) = session.current_files.get(idx) {
            let new_dir = session.current_dir.join(folder);
            state.update_session(user_id, |s| { s.current_dir = new_dir; s.selected.entry(action.to_string()).or_default().clear(); }).await;
        }
        let _ = render_file_selector(bot, q, action, 0, config, state).await;
    } else if data.starts_with("updir_") {
        let action = &data[6..];
        state.update_session(user_id, |s| {
            if let Some(parent) = s.current_dir.parent() { if parent.starts_with(&config.base_dir) { s.current_dir = parent.to_path_buf(); } }
            s.selected.entry(action.to_string()).or_default().clear();
        }).await;
        let _ = render_file_selector(bot, q, action, 0, config, state).await;
    } else if data.starts_with("toggle_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let idx: usize = parts[2].parse().unwrap_or(0);
        let page: usize = parts[3].parse().unwrap_or(0);
        state.update_session(user_id, |s| {
            let set = s.selected.entry(action.to_string()).or_default();
            if set.contains(&idx) { set.remove(&idx); } else { set.insert(idx); }
        }).await;
        let _ = render_file_selector(bot, q, action, page, config, state).await;
    } else if data.starts_with("execsingle_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let idx: usize = parts[2].parse().unwrap_or(0);
        let session = state.get_session(user_id, &config.base_dir).await;
        if let Some(file_name) = session.current_files.get(idx) {
            let path = session.current_dir.join(file_name);
            match action {
                "browse" => { let _ = action_browse(bot, q, path, config).await; }
                "stream" => { let _ = action_stream(bot, q, path, config, state).await; }
                "youtube" => { let _ = start_youtube_uploads(bot, q, vec![path], config, state).await; }
                _ => {}
            }
        }
    } else if data.starts_with("execbatch_") {
        let action = &data[10..];
        let session = state.get_session(user_id, &config.base_dir).await;
        let indices = session.selected.get(action).cloned().unwrap_or_default();
        let mut paths = Vec::new();
        for idx in indices {
            if let Some(name) = session.current_files.get(idx) {
                paths.push(session.current_dir.join(name));
            }
        }
        match action {
            "delete" => { let _ = render_delete_confirmation(bot, q, paths, state).await; }
            "youtube" => { let _ = start_youtube_uploads(bot, q, paths, config, state).await; }
            "concat" => { let _ = action_concat(bot, q, paths, config, state).await; }
            "convert" => { let _ = action_convert(bot, q, paths, config, state).await; }
            _ => {}
        }
    } else if data == "confirm_delete" {
        let session = state.get_session(user_id, &config.base_dir).await;
        let _ = action_delete(bot, q, session.pending_delete.clone(), config, state).await;
    } else if data == "cancel_delete" {
        state.update_session(user_id, |s| s.pending_delete.clear()).await;
        let _ = render_file_selector(bot, q, "delete", 0, config, state).await;
    }
    Ok(())
}