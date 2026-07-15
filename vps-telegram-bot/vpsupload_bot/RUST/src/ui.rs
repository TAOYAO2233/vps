use std::sync::Arc;
use std::path::Path;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use crate::state::AppState;
use crate::media_utils::*;

fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
}

pub async fn build_main_menu(state: Arc<AppState>, user_id: i64) -> (String, InlineKeyboardMarkup) {
    let session = state.get_session(user_id).await;
    let active_guard = state.active_task.lock().await;
    
    let busy_text = match &*active_guard {
        Some(t) => format!("\n⚡ <b>[当前任务]</b> <code>{}</code>", escape_html(&t.name)),
        None => "\n🟢 <b>[系统状态]</b> <code>空闲中 (随时就绪)</code>".to_string(),
    };

    let upload_count = state.youtube_uploads.lock().await.len();
    let upload_text = if upload_count > 0 {
        format!("\n☁️ <b>[上传任务]</b> <code>{} 个任务在排队/运行</code>", upload_count)
    } else {
        String::new()
    };

    let theme_name = match session.progress_bar_theme {
        1 => "🟩🟩⬜⬜ 彩色水果",
        2 => "━━── 简约细线",
        _ => "▰▰▱▱ 科幻方块",
    };

    let text = format!(
        "<b>╔══════ 🎬 VPS 多媒体控制面板 ══════╗</b>\n\n\
         📁 <b>存储根目录:</b> <code>{}</code>\n\
         🎨 <b>当前进度条皮肤:</b> <code>{}</code>\n\
         {}\n\
         {}\n\n\
         <i>💡 提示: 发送 /uploads 查看上传，/stop 中断独占任务。</i>\n\
         <b>╚════════════════════════════════╝</b>",
         escape_html(&state.config.base_dir.to_string_lossy()),
         theme_name,
         busy_text,
         upload_text
    );

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("📂 浏览与操作本地文件", "init_browse")],
        vec![
            InlineKeyboardButton::callback("📡 RTMP 推流", "init_stream"),
            InlineKeyboardButton::callback("☁️ YouTube 上传", "init_youtube"),
        ],
        vec![
            InlineKeyboardButton::callback("✂️ 智能视频合并", "init_concat"),
            InlineKeyboardButton::callback("🔄 批量转码 MP4", "init_convert"),
        ],
        vec![
            InlineKeyboardButton::callback("🎨 切换进度条皮肤", "menu_skin_settings"),
            InlineKeyboardButton::callback("🗑️ 批量清理文件", "init_delete"),
        ],
    ]);

    (text, keyboard)
}

pub fn build_skin_selector_menu() -> (String, InlineKeyboardMarkup) {
    let text = "<b>🎨 选择你喜爱的进度条“视觉皮肤”：</b>\n\n\
                设置后，推流、转码、以及 YouTube 上传进度条将实时切换为该样式。".to_string();

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("▰▰▱▱ 科幻方块", "set_skin_0")],
        vec![InlineKeyboardButton::callback("🟩🟩⬜⬜ 彩色水果", "set_skin_1")],
        vec![InlineKeyboardButton::callback("━━── 简约细线", "set_skin_2")],
        vec![InlineKeyboardButton::callback("🔙 返回主菜单", "menu_main")],
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
    let display_path = if rel_path.as_os_str().is_empty() { 
        "🏠".to_string() 
    } else { 
        format!("🏠/{}", rel_path.display()) 
    };
    
    let header = format!(
        "📂 路径: <code>{}</code>\n👉 模式: <b>[{}]</b> (页 {}/{})", 
        escape_html(&display_path), 
        escape_html(&action_type.to_uppercase()), 
        page + 1, 
        total_pages.max(1)
    );

    (header, InlineKeyboardMarkup::new(rows))
}