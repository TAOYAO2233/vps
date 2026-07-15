use std::sync::Arc;
use std::path::{Path, PathBuf};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use crate::state::AppState;
use crate::media_utils::*;

pub async fn build_main_menu(state: Arc<AppState>) -> (String, InlineKeyboardMarkup) {
    let active_guard = state.active_task.lock().await;
    let busy_text = match &*active_guard {
        Some(t) => format!("\n🔒 运行中独占任务: `{}`", t.name),
        None => String::new(),
    };

    let upload_count = state.youtube_uploads.lock().await.len();
    let upload_text = if upload_count > 0 {
        format!("\n☁️ YouTube 排队/上传中: `{}`", upload_count)
    } else {
        String::new()
    };

    let text = format!(
        "=== 🎬 VPS 多媒体控制台 ===\n根目录: `{}`\n💡 提示: /uploads 查看上传，/stop 中断任务{}{}",
        state.config.base_dir.display(), busy_text, upload_text
    );

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("📂 浏览远程目录", "init_browse")],
        vec![
            InlineKeyboardButton::callback("📡 RTMP 单路推流", "init_stream"),
            InlineKeyboardButton::callback("☁️ YouTube 上传", "init_youtube"),
        ],
        vec![InlineKeyboardButton::callback("✂️ 智能视频合并", "init_concat")],
        vec![InlineKeyboardButton::callback("🔄 批量转码 MP4", "init_convert")],
        vec![InlineKeyboardButton::callback("🗑️ 批量删除文件", "init_delete")],
    ]);

    (text, keyboard)
}

pub async fn build_file_selector(
    state: Arc<AppState>,
    user_id: i64,
    action_type: &str,
    page: usize,
) -> (String, InlineKeyboardMarkup) {
    let mut session = state.get_session(user_id).await;
    let current_dir = session.current_dir.clone();

    // 扫描文件夹
    let mut dirs = Vec::new();
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&current_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if path.is_dir() {
                dirs.push(name);
            } else if path.is_file() {
                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                if state.config.video_extensions.iter().any(|e| e.ends_with(&ext)) {
                    files.push(name);
                }
            }
        }
    }
    dirs.sort();
    files.sort();

    let mut all_items = dirs.clone();
    all_items.extend(files.clone());
    session.current_files = all_items.clone();
    state.save_session(user_id, session.clone()).await;

    let is_multi = matches!(action_type, "youtube" | "concat" | "convert" | "delete");
    let selected_set = session.get_selected(action_type).clone();

    let per_page = state.config.items_per_page;
    let total_pages = (all_items.len() + per_page - 1) / per_page;
    let page = page.min(total_pages.saturating_sub(1));
    let start_idx = page * per_page;
    let end_idx = (start_idx + per_page).min(all_items.len());

    let mut rows = Vec::new();
    for i in start_idx..end_idx {
        let name = &all_items[i];
        let path = current_dir.join(name);
        
        if path.is_dir() {
            rows.push(vec![InlineKeyboardButton::callback(
                format!("📁 {}", name),
                format!("enterdir_{}_{}", action_type, i),
            )]);
        } else {
            let size = get_formatted_file_size(&path);
            let label = if is_multi {
                let mark = if selected_set.contains(&i) { "✅ " } else { "⬜️ " };
                format!("{}[{}] {}", mark, size, name)
            } else {
                format!("[{}] {}", size, name)
            };

            let cb_data = if is_multi {
                format!("toggle_{}_{}_{}", action_type, i, page)
            } else {
                format!("execsingle_{}_{}", action_type, i)
            };
            rows.push(vec![InlineKeyboardButton::callback(label, cb_data)]);
        }
    }

    // 导航按键
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(InlineKeyboardButton::callback("⬅️ 上一页", format!("menu_{}_{}", action_type, page - 1)));
    }
    if page + 1 < total_pages {
        nav.push(InlineKeyboardButton::callback("➡️ 下一页", format!("menu_{}_{}", action_type, page + 1)));
    }
    if !nav.is_empty() { rows.push(nav); }

    if current_dir != state.config.base_dir {
        rows.push(vec![InlineKeyboardButton::callback("⬆️ 返回上一层目录", format!("updir_{}", action_type))]);
    }

    if is_multi && !selected_set.is_empty() {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("▶️ 确认执行 (选中 {} 个文件)", selected_set.len()),
            format!("execbatch_{}", action_type),
        )]);
    }

    rows.push(vec![InlineKeyboardButton::callback("🔙 返回主菜单", "menu_main")]);

    let rel_path = current_dir.strip_prefix(&state.config.base_dir).unwrap_or(Path::new(""));
    let display_path = if rel_path.as_os_str().is_empty() { "🏠".to_string() } else { format!("🏠/{}", rel_path.display()) };
    let header = format!("📂 路径: `{}`\n👉 模式: [{}] (页 {}/{})", display_path, action_type.to_uppercase(), page + 1, total_pages.max(1));

    (header, InlineKeyboardMarkup::new(rows))
}