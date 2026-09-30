//! 文件选择器 UI 渲染。
//!
//! 负责渲染文件浏览/选择界面，对应 Python 版本的 `render_file_selector` 函数。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::bot::keyboard::file_selector_keyboard;
use crate::config::Config;
use crate::core::state::{ActionType, SharedState};
use crate::storage::filesystem::format_file_size_bytes;
use crate::storage::path::PathGuard;
use crate::storage::scanner::scan_directory;
use crate::ui::pagination::Paginator;
use crate::utils::format::escape_html;

/// 渲染文件选择器界面并编辑现有消息。
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `msg` - 要编辑的消息
/// * `state` - 全局共享状态
/// * `config` - 应用配置
/// * `action` - 当前操作类型
/// * `page` - 请求的页码（0-indexed）
pub async fn render_file_selector(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Arc<Config>,
    action: &ActionType,
    page: usize,
) -> Result<()> {
    let path_guard = PathGuard::new(config.base_dir.clone());

    // 获取并校验当前目录
    let current_dir = {
        let s = state.read().await;
        let dir = s.current_dir.clone();
        match path_guard.assert_inside(&dir) {
            Ok(_) => dir,
            Err(_) => {
                drop(s);
                let mut s = state.write().await;
                s.current_dir = config.base_dir.clone();
                config.base_dir.clone()
            }
        }
    };

    if !current_dir.is_dir() {
        bot.edit_message_text(msg.chat.id, msg.id, "❌ 目录不存在！")
            .await?;
        return Ok(());
    }

    // 扫描目录：同步文件系统调用放入阻塞线程池，避免卡住异步运行时
    let scan_dir = current_dir.clone();
    let listing = match tokio::task::spawn_blocking(move || scan_directory(&scan_dir)).await {
        Ok(Ok(listing)) => listing,
        Ok(Err(e)) => {
            bot.edit_message_text(
                msg.chat.id,
                msg.id,
                format!("❌ 无法读取目录: {}", escape_html(&e.to_string())),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            bot.edit_message_text(
                msg.chat.id,
                msg.id,
                format!("❌ 目录扫描失败: {}", escape_html(&e.to_string())),
            )
            .await?;
            return Ok(());
        }
    };

    // 更新状态中的文件列表缓存
    {
        let mut s = state.write().await;
        s.current_files = listing.all_items();
    }

    // 分页计算
    let paginator = Paginator::new(listing.total(), config.items_per_page, page);
    let page_items = &listing.items[paginator.range()];

    // 获取当前选中集合
    let selected_paths: HashSet<PathBuf> = {
        let s = state.read().await;
        s.selections_ref(action).cloned().unwrap_or_default()
    };

    let is_multi_select = action.is_multi_select();

    // 构建按钮数据
    let mut button_items: Vec<(String, String)> = Vec::new();
    for (i, item) in page_items.iter().enumerate() {
        let real_idx = paginator.start_index() + i;
        let item_path = current_dir.join(&item.name);

        if item.is_dir {
            button_items.push((
                format!("📁 {}", item.name),
                format!("enterdir_{}_{real_idx}", action.as_str()),
            ));
        } else {
            let size_str = format_file_size_bytes(item.size);
            if is_multi_select {
                let is_selected = selected_paths.contains(&item_path);
                let checkbox = if is_selected { "✅ " } else { "⬜️ " };
                button_items.push((
                    format!("{checkbox}[{size_str}] {}", item.name),
                    format!(
                        "toggle_{}_{}_{}",
                        action.as_str(),
                        real_idx,
                        paginator.current_page
                    ),
                ));
            } else {
                button_items.push((
                    format!("[{size_str}] {}", item.name),
                    format!("execsingle_{}_{real_idx}", action.as_str()),
                ));
            }
        }
    }

    let is_at_base = current_dir == config.base_dir;
    let selected_count = selected_paths.len();

    let keyboard = file_selector_keyboard(
        &button_items,
        paginator.current_page,
        paginator.total_pages(),
        action.as_str(),
        is_at_base,
        if is_multi_select { selected_count } else { 0 },
    );

    // 构建标题文本
    let rel_path = current_dir
        .strip_prefix(&config.base_dir)
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

    // 追加任务状态信息
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
        header.push_str("\n\n⚠️ 当前目录下既无子文件夹也无视频文件。");
    }

    bot.edit_message_text(msg.chat.id, msg.id, header)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}
