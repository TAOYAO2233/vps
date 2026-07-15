use crate::config::{action_name_map, Config};
use crate::media_utils::{assert_path_inside_base, get_formatted_file_size};
use crate::task_manager::AppState;
use anyhow::Result;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use teloxide::payloads::EditMessageTextSetters;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

pub async fn render_main_menu(bot: Bot, chat_id: ChatId, message_id: Option<MessageId>, config: Arc<Config>, state: Arc<AppState>) -> Result<()> {
    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("📂 浏览远程文件", "init_browse")],
        vec![
            InlineKeyboardButton::callback("📡 RTMP 单路推流", "init_stream"),
            InlineKeyboardButton::callback("☁️ YouTube 上传", "init_youtube"),
        ],
        vec![InlineKeyboardButton::callback("✂️ 智能视频合并", "init_concat")],
        vec![InlineKeyboardButton::callback("🔄 批量转码 MP4", "init_convert")],
        vec![InlineKeyboardButton::callback("🗑️ 批量删除文件", "init_delete")],
    ]);

    let active_name = state.active_task.lock().await.as_ref().map(|t| t.name.clone());
    let busy_text = active_name.map(|n| format!("\n🔒 当前独占任务: `{}`", n)).unwrap_or_default();
    let upload_count = state.youtube_uploads.lock().await.len();
    let upload_text = if upload_count > 0 { format!("\n☁️ YouTube 上传/排队: `{}`", upload_count) } else { String::new() };

    let text = format!(
        "=== 🎬 VPS 多媒体主控面板 ===\n根目录: `{}`\n💡 提示: /uploads 查看上传，/stop 中断运行任务{}{}",
        config.base_dir.display(), busy_text, upload_text
    );

    if let Some(msg_id) = message_id {
        bot.edit_message_text(chat_id, msg_id, text).reply_markup(kb).parse_mode(ParseMode::MarkdownV2).await?;
    } else {
        bot.send_message(chat_id, text).reply_markup(kb).parse_mode(ParseMode::MarkdownV2).await?;
    }
    Ok(())
}

pub async fn render_file_selector(
    bot: Bot,
    q: CallbackQuery,
    action: &str,
    page: usize,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    let user_id = q.from.id.0 as i64;
    let session = state.get_session(user_id, &config.base_dir).await;
    let current_dir = assert_path_inside_base(&config.base_dir, &session.current_dir).unwrap_or_else(|_| config.base_dir.clone());

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(&current_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    dirs.push(name);
                } else if file_type.is_file() {
                    let lower = name.to_lowercase();
                    if config.video_extensions.iter().any(|ext| lower.ends_with(ext)) {
                        files.push(name);
                    }
                }
            }
        }
    }
    dirs.sort();
    files.sort();
    let items: Vec<String> = dirs.into_iter().chain(files.into_iter()).collect();

    state.update_session(user_id, |s| s.current_files = items.clone()).await;

    let is_multi = ["youtube", "concat", "convert", "delete"].contains(&action);
    let selected_indices = session.selected.get(action).cloned().unwrap_or_default();

    let total_pages = ((items.len() as f64) / (config.items_per_page as f64)).ceil() as usize;
    let total_pages = total_pages.max(1);
    let page = page.min(total_pages - 1);
    let start_idx = page * config.items_per_page;
    let end_idx = (start_idx + config.items_per_page).min(items.len());
    let page_items = &items[start_idx..end_idx];

    let mut kb_rows = Vec::new();
    for (i, item_name) in page_items.iter().enumerate() {
        let real_idx = start_idx + i;
        let item_path = current_dir.join(item_name);
        let (btn_text, callback_data) = if item_path.is_dir() {
            (format!("📁 {}", item_name), format!("enterdir_{}_{}", action, real_idx))
        } else {
            let size_str = get_formatted_file_size(&item_path);
            if is_multi {
                let checkbox = if selected_indices.contains(&real_idx) { "✅ " } else { "⬜️ " };
                (format!("{}[{}] {}", checkbox, size_str, item_name), format!("toggle_{}_{}_{}", action, real_idx, page))
            } else {
                (format!("[{}] {}", size_str, item_name), format!("execsingle_{}_{}", action, real_idx))
            }
        };
        kb_rows.push(vec![InlineKeyboardButton::callback(btn_text, callback_data)]);
    }

    let mut nav_row = Vec::new();
    if page > 0 { nav_row.push(InlineKeyboardButton::callback("⬅️ 上一页", format!("menu_{}_{}", action, page - 1))); }
    if page < total_pages - 1 { nav_row.push(InlineKeyboardButton::callback("➡️ 下一页", format!("menu_{}_{}", action, page + 1))); }
    if !nav_row.is_empty() { kb_rows.push(nav_row); }

    if current_dir.canonicalize().unwrap_or_default() != config.base_dir.canonicalize().unwrap_or_default() {
        kb_rows.push(vec![InlineKeyboardButton::callback("⬆️ 返回上一级目录", format!("updir_{}", action))]);
    }

    if is_multi && !selected_indices.is_empty() {
        kb_rows.push(vec![InlineKeyboardButton::callback(format!("▶️ 确认执行 ({} 个文件)", selected_indices.len()), format!("execbatch_{}", action))]);
    }
    kb_rows.push(vec![InlineKeyboardButton::callback("🔙 返回主菜单", "menu_main")]);

    let rel_path = current_dir.strip_prefix(&config.base_dir).unwrap_or(Path::new("")).display();
    let display_path = if rel_path.to_string().is_empty() { "🏠".to_string() } else { format!("🏠/{}", rel_path) };
    let mut header = format!("📂 路径: `{}`\n👉 模式: [{}] (页 {}/{})", display_path, action_name_map(action), page + 1, total_pages);

    if let Some(t) = state.active_task.lock().await.as_ref() { header.push_str(&format!("\n🔒 独占任务: `{}`", t.name)); }
    let up_cnt = state.youtube_uploads.lock().await.len();
    if up_cnt > 0 { header.push_str(&format!("\n☁️ YouTube 上传/排队: `{}`", up_cnt)); }
    if items.is_empty() { header.push_str("\n\n⚠️ 当前目录下既无子文件夹也无视频文件。"); }

    if let Some(msg) = q.message {
        bot.edit_message_text(msg.chat.id, msg.id, header).reply_markup(InlineKeyboardMarkup::new(kb_rows)).parse_mode(ParseMode::MarkdownV2).await?;
    }
    Ok(())
}

pub async fn render_delete_confirmation(bot: Bot, q: CallbackQuery, target_files: Vec<PathBuf>, state: Arc<AppState>) -> Result<()> {
    let user_id = q.from.id.0 as i64;
    state.update_session(user_id, |s| s.pending_delete = target_files.clone()).await;

    let mut lines: Vec<String> = target_files.iter().take(12).map(|p| format!("• `{}`", p.file_name().unwrap().to_str().unwrap())).collect();
    if target_files.len() > 12 { lines.push(format!("... 其余 {} 个文件未展开", target_files.len() - 12)); }

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ 确认删除", "confirm_delete"),
        InlineKeyboardButton::callback("取消", "cancel_delete"),
    ]]);

    if let Some(msg) = q.message {
        bot.edit_message_text(msg.chat.id, msg.id, format!("⚠️ **二次确认：即将永久删除以下文件**\n\n{}\n\n删除后不可恢复。", lines.join("\n")))
            .reply_markup(kb).parse_mode(ParseMode::MarkdownV2).await?;
    }
    Ok(())
}