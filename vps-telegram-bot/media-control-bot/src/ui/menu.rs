//! 主菜单 UI 组件。
//!
//! 每次渲染主菜单时均实时读取 `/storage512` 挂载与容量统计，
//! 并将其展示在 `根目录` 行与 `提示` 行之间；读取失败时优雅降级显示“磁盘信息暂不可用”。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, ParseMode};

use crate::bot::keyboard::main_menu_keyboard;
use crate::config::Config;
use crate::core::state::SharedState;
use crate::storage::disk::DiskMenuSummary;
use crate::utils::format::escape_html;

/// 主菜单 UI 构建器。
pub struct MainMenu;

impl MainMenu {
    /// 生成主菜单文本和键盘（纯函数，便于测试布局顺序）。
    #[must_use]
    pub fn render(
        base_dir: &Path,
        disk_summary: &DiskMenuSummary,
        active_task_name: Option<&str>,
        upload_count: usize,
    ) -> (String, InlineKeyboardMarkup) {
        let busy_text = active_task_name
            .map(|name| format!("\n🔒 当前独占任务: <code>{}</code>", escape_html(name)))
            .unwrap_or_default();

        let upload_text = if upload_count > 0 {
            format!("\n☁️ YouTube 上传/排队: <code>{upload_count}</code>")
        } else {
            String::new()
        };

        let disk_line = disk_summary.format_menu_line();

        let text = format!(
            "=== 🎬 <b>VPS 多媒体主控面板</b> ===\n\
             根目录: <code>{}</code>\n\
             {disk_line}\n\
             💡 提示: /uploads 查看上传，/stop 中断运行任务\
             {busy_text}\
             {upload_text}",
            escape_html(&base_dir.display().to_string())
        );

        (text, main_menu_keyboard())
    }
}

/// 实时读取最新磁盘统计、渲染主菜单并编辑现有消息。
pub async fn render_main_menu(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Config,
) -> Result<()> {
    let disk_monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    // 每次返回主菜单均实时获取内核文件系统统计并评估报警
    let disk_summary = disk_monitor.check_and_notify(bot).await;

    let (active_name, upload_count) = {
        let mut s = state.write().await;
        s.cleanup_active_task();
        s.cleanup_youtube_pool();
        let active = s.active_task_name().map(str::to_string);
        let count = s.active_youtube_count();
        (active, count)
    };

    let (text, keyboard) = MainMenu::render(
        &config.base_dir,
        &disk_summary,
        active_name.as_deref(),
        upload_count,
    );

    bot.edit_message_text(msg.chat.id, msg.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::disk::DiskUsageStats;
    use std::path::PathBuf;

    #[test]
    fn test_main_menu_places_disk_line_between_base_dir_and_tip() {
        let stats = DiskUsageStats::from_raw_blocks(
            PathBuf::from("/storage512"),
            "ext4".into(),
            4096,
            131_858_432,
            19_136_512,
            12_530_483,
        );
        let (text, _) = MainMenu::render(
            Path::new("/storage512/bilivego/download"),
            &DiskMenuSummary::Available(stats),
            None,
            0,
        );

        let pos_root = text.find("根目录:").unwrap();
        let pos_disk = text.find("💽 <code>/storage512</code>").unwrap();
        let pos_tip = text.find("💡 提示:").unwrap();
        assert!(pos_root < pos_disk && pos_disk < pos_tip);

        // 验证失败降级文案
        let (fallback_text, _) = MainMenu::render(
            Path::new("/storage512/bilivego/download"),
            &DiskMenuSummary::Unavailable {
                mount_point: PathBuf::from("/storage512"),
            },
            None,
            0,
        );
        assert!(fallback_text.contains("磁盘信息暂不可用"));
    }
}
