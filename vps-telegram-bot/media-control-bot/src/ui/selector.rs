//! 文件选择器 UI 渲染。
//!
//! 每次渲染均在后台线程扫描目录、分配或校验 [`MenuSnapshot`] 快照令牌 `snap_id`，
//! 并在多选（尤其是视频合并）模式下直观展示用户选择的严格先后顺序。

use std::path::PathBuf;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::bot::keyboard::{back_to_main_keyboard, file_selector_keyboard};
use crate::config::Config;
use crate::core::state::{ActionType, SharedState, SnapshotEntry};
use crate::storage::filesystem::format_file_size_bytes;
use crate::storage::path::PathGuard;
use crate::storage::scanner::scan_directory;
use crate::ui::pagination::Paginator;
use crate::utils::format::escape_html;
use crate::utils::redact::sanitize_user_error;

/// 渲染文件选择器界面并更新当前消息与菜单快照。
pub async fn render_file_selector(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    action: &ActionType,
    page: usize,
) -> Result<()> {
    let path_guard = PathGuard::new(config.base_dir.clone());

    let canonical_base = match path_guard.canonical_base() {
        Ok(b) => b,
        Err(e) => {
            bot.edit_message_text(
                msg.chat.id,
                msg.id,
                format!(
                    "❌ <b>媒体根目录当前不可访问</b>\n{}",
                    escape_html(&sanitize_user_error(&e))
                ),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(back_to_main_keyboard())
            .await?;
            return Ok(());
        }
    };

    let current_dir = {
        let s = state.read().await;
        let candidate = s.current_dir.clone();
        drop(s);
        match path_guard.assert_inside(&candidate) {
            Ok(valid) => {
                let mut s = state.write().await;
                s.current_dir = valid.clone();
                valid
            }
            Err(_) => {
                let mut s = state.write().await;
                s.current_dir = canonical_base.clone();
                canonical_base.clone()
            }
        }
    };

    let scan_dir = current_dir.clone();
    let mut listing = match tokio::task::spawn_blocking(move || scan_directory(&scan_dir)).await {
        Ok(Ok(listing)) => listing,
        Ok(Err(e)) => {
            bot.edit_message_text(
                msg.chat.id,
                msg.id,
                format!("❌ 无法读取目录: {}", escape_html(&sanitize_user_error(&e))),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(back_to_main_keyboard())
            .await?;
            return Ok(());
        }
        Err(e) => {
            let err = anyhow::anyhow!("目录扫描线程异常: {e}");
            bot.edit_message_text(
                msg.chat.id,
                msg.id,
                format!("❌ {}", escape_html(&sanitize_user_error(&err))),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(back_to_main_keyboard())
            .await?;
            return Ok(());
        }
    };

    let search_query = {
        let s = state.read().await;
        s.search_query.to_lowercase()
    };
    if !search_query.is_empty() {
        listing.items.retain(|item| item.name.to_lowercase().contains(&search_query));
    }

    let snapshot_entries: Vec<SnapshotEntry> = listing.items.iter().map(SnapshotEntry::from).collect();

    let (snapshot, ordered_selections): (_, Vec<PathBuf>) = {
        let mut s = state.write().await;
        let snap = s.update_menu_snapshot(action.clone(), current_dir.clone(), snapshot_entries);
        let sel = s.selections_slice(action).to_vec();
        (snap, sel)
    };

    let snap_id = &snapshot.snap_id;
    let action_str = action.as_str();
    let paginator = Paginator::new(listing.total(), config.items_per_page, page);
    let page_items = &listing.items[paginator.range()];
    let is_multi_select = action.is_multi_select();

    let mut button_items: Vec<(String, String)> = Vec::with_capacity(page_items.len());
    for (i, item) in page_items.iter().enumerate() {
        let real_idx = paginator.start_index() + i;
        let joined_path = current_dir.join(&item.name);
        let lookup_path = joined_path
            .canonicalize()
            .unwrap_or_else(|_| joined_path.clone());

        if item.is_dir {
            button_items.push((
                format!("📁 {}", item.name),
                format!("enterdir_{action_str}_{snap_id}_{real_idx}"),
            ));
        } else {
            let size_str = format_file_size_bytes(item.size);
            if is_multi_select {
                let order_opt = ordered_selections
                    .iter()
                    .position(|p| p == &lookup_path)
                    .map(|pos| pos + 1);
                let checkbox = match (action, order_opt) {
                    (ActionType::Concat, Some(ord)) => format!("✅#{ord} "),
                    (_, Some(_)) => "✅ ".to_string(),
                    (_, None) => "⬜️ ".to_string(),
                };
                button_items.push((
                    format!("{checkbox}[{size_str}] {}", item.name),
                    format!(
                        "toggle_{action_str}_{snap_id}_{real_idx}_{}",
                        paginator.current_page
                    ),
                ));
            } else {
                button_items.push((
                    format!("[{size_str}] {}", item.name),
                    format!("execsingle_{action_str}_{snap_id}_{real_idx}"),
                ));
            }
        }
    }

    let is_at_base = current_dir == canonical_base;
    let selected_count = ordered_selections.len();

    let keyboard = file_selector_keyboard(
        &button_items,
        paginator.current_page,
        paginator.total_pages(),
        action_str,
        snap_id,
        is_at_base,
        if is_multi_select { selected_count } else { 0 },
    );

    let rel_path = current_dir
        .strip_prefix(&canonical_base)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| current_dir.to_string_lossy().to_string());
    let display_path = if rel_path.is_empty() || rel_path == "." {
        "🏠".to_string()
    } else {
        format!("🏠/{rel_path}")
    };

    let mut header = format!(
        "📂 路径: <code>{}</code>\n\
         👉 模式: [{}] (页 {}/{})",
        escape_html(&display_path),
        escape_html(action.display_name()),
        paginator.current_page + 1,
        paginator.total_pages()
    );
    if !search_query.is_empty() {
        header.push_str(&format!("\n🔎 筛选: <code>{}</code>（/clearfilter 清除）", escape_html(&search_query)));
    }

    // 合并模式下展示严格按用户点击顺序排列的分段预览
    if action == &ActionType::Concat && !ordered_selections.is_empty() {
        header.push_str("\n📋 <b>已选拼接顺序（严格按点击先后）</b>:");
        for (ord, p) in ordered_selections.iter().take(6).enumerate() {
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
            header.push_str(&format!("\n  {}. <code>{}</code>", ord + 1, escape_html(fname)));
        }
        if ordered_selections.len() > 6 {
            header.push_str(&format!(
                "\n  ... 共 {} 个分段",
                ordered_selections.len()
            ));
        }
    }

    {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        if let Some(name) = s.active_task_name() {
            header.push_str(&format!("\n🔒 独占任务: <code>{}</code>", escape_html(name)));
        }
        let upload_count = s.active_youtube_count();
        if upload_count > 0 {
            header.push_str(&format!("\n☁️ YouTube 上传/排队: <code>{upload_count}</code>"));
        }
    }

    if listing.is_empty() {
        header.push_str("\n\n⚠️ 当前目录下既无子文件夹也无支持的视频文件。");
    }

    bot.edit_message_text(msg.chat.id, msg.id, header)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}
