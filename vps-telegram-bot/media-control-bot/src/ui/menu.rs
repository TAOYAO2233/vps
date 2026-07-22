//! 主菜单 UI 组件。
//!
//! 提供主菜单文本和键盘的生成函数。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, ParseMode};

use crate::bot::keyboard::main_menu_keyboard;
use crate::config::Config;
use crate::core::state::SharedState;

/// 主菜单 UI 构建器。
pub struct MainMenu;

impl MainMenu {
    /// 生成主菜单文本和键盘。
    ///
    /// # Arguments
    ///
    /// * `base_dir` - 媒体文件根目录
    /// * `active_task_name` - 当前独占任务名称（若有）
    /// * `upload_count` - 当前 YouTube 上传任务数量
    ///
    /// # Returns
    ///
    /// `(text, keyboard)` 元组。
    #[must_use]
    pub fn render(
        base_dir: &Path,
        active_task_name: Option<&str>,
        upload_count: usize,
    ) -> (String, InlineKeyboardMarkup) {
        let busy_text = active_task_name
            .map(|name| format!("\n🔒 当前独占任务: `{name}`"))
            .unwrap_or_default();

        let upload_text = if upload_count > 0 {
            format!("\n☁️ YouTube 上传/排队: `{upload_count}`")
        } else {
            String::new()
        };

        let text = format!(
            "=== 🎬 VPS 多媒体主控面板 ===\n\
             根目录: `{}`\n\
             💡 提示: /uploads 查看上传，/stop 中断运行任务\
             {busy_text}\
             {upload_text}",
            base_dir.display()
        );

        (text, main_menu_keyboard())
    }
}

/// 渲染主菜单并编辑现有消息。
pub async fn render_main_menu(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Arc<Config>,
) -> Result<()> {
    let (active_name, upload_count) = {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        let active = s.active_task_name().map(|n| n.to_string());
        let count = s.active_youtube_count();
        (active, count)
    };

    let (text, keyboard) = MainMenu::render(&config.base_dir, active_name.as_deref(), upload_count);

    bot.edit_message_text(msg.chat.id, msg.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}
