//! 批量文件回收站操作。
//!
//! 关键安全机制（对应交接文档 §5.1 与 v1.1 §1.1）：
//! - 生成二次确认面板时，逐文件调用 [`ConfirmedFileTarget::capture`] 核验 `PathGuard::assert_regular_file_no_symlink`
//!   并记录文件大小、`mtime` 与 Unix `(dev, ino)` 身份指纹，生成一次性确认令牌 `token`；
//! - 点击 `confirm_del_{token}` 时原子消费对应批次，防止旧确认消息或并发改动状态导致错删；
//! - 移入同盘回收站前逐文件再次调用 [`ConfirmedFileTarget::verify_unchanged`]；
//! - 若执行中途收到 `/stop`，立即停止后续文件移动，已移动文件可由 `/trash` 恢复。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{info, warn};

use super::edit_html_bounded;
use crate::bot::keyboard::delete_confirmation_keyboard;
use crate::config::Config;
use crate::core::state::{ActionType, ConfirmedFileTarget, PendingDeleteBatch, SharedState};
use crate::core::task_manager::TaskManager;
use crate::errors::AppError;
use crate::storage::filesystem::format_file_size_bytes;
use crate::storage::path::PathGuard;
use crate::storage::trash::TrashStore;
use crate::utils::format::escape_html;
use crate::utils::redact::redact_secrets;

/// 渲染二次删除确认界面，并将绑定令牌与文件身份快照的批次存入共享状态。
pub async fn render_delete_confirmation(
    bot: &Bot,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    target_files: Vec<PathBuf>,
) -> Result<()> {
    {
        let monitor = {
            let s = state.read().await;
            Arc::clone(&s.disk_monitor)
        };
        monitor.ensure_space_available(0).await?;
    }
    let guard = PathGuard::new(config.base_dir.clone());
    let mut confirmed_targets = Vec::with_capacity(target_files.len());

    for raw_path in &target_files {
        let target = ConfirmedFileTarget::capture(&guard, raw_path)?;
        if !confirmed_targets
            .iter()
            .any(|t: &ConfirmedFileTarget| t.path == target.path)
        {
            confirmed_targets.push(target);
        }
    }

    if confirmed_targets.is_empty() {
        return Err(AppError::invalid_argument("没有可删除的有效普通视频文件。").into());
    }

    let total_bytes: u64 = confirmed_targets.iter().map(|t| t.size).sum();

    let preview_count = 12;
    let mut preview_lines: Vec<String> = confirmed_targets
        .iter()
        .take(preview_count)
        .map(|t| {
            format!(
                "• <code>{}</code> ({})",
                escape_html(&t.display_name()),
                format_file_size_bytes(t.size)
            )
        })
        .collect();

    if confirmed_targets.len() > preview_count {
        preview_lines.push(format!(
            "... 其余 {} 个文件未展开",
            confirmed_targets.len() - preview_count
        ));
    }

    let total_count = confirmed_targets.len();
    let token = {
        let mut s = state.write().await;
        s.set_pending_delete(confirmed_targets)
    };

    let text = format!(
        "⚠️ <b>二次确认：即将把以下 <code>{total_count}</code> 个文件移入回收站</b>\n\
         📦 回收站预计占用: <code>{}</code>\n\
         🔐 确认批次码: <code>{token}</code>\n\n\
         {}\n\n\
         文件保留 <code>{}</code> 天，可用 /trash 恢复；到期后清理。执行前将逐文件核验身份。",
        format_file_size_bytes(total_bytes),
        preview_lines.join("\n"),
        config.trash_retention_days
    );

    bot.edit_message_text(msg.chat.id, msg.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(delete_confirmation_keyboard(&token))
        .await?;

    Ok(())
}

/// 启动经令牌核实的批量删除独占任务。
pub async fn start_delete(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    batch: PendingDeleteBatch,
) -> Result<()> {
    let task_manager = TaskManager::new(Arc::clone(&state));
    let bot_clone = bot.clone();
    let msg_clone = msg.clone();
    let cfg_clone = Arc::clone(&config);
    let state_for_clear = Arc::clone(&state);

    task_manager
        .start_exclusive("移入回收站", move |task_id, cancel_rx| {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let state_inner = Arc::clone(&state);
            let cfg = Arc::clone(&cfg_clone);
            async move { do_delete(bot, msg, state_inner, cfg, batch, task_id, cancel_rx).await }
        })
        .await?;

    {
        let mut s = state_for_clear.write().await;
        s.clear_selections(&ActionType::Delete);
    }

    Ok(())
}

/// 在独占任务内逐文件执行二次身份核验与删除。
async fn do_delete(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
    batch: PendingDeleteBatch,
    task_id: u64,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    monitor.ensure_space_available(0).await?;
    let guard = PathGuard::new(config.base_dir.clone());
    let trash = TrashStore::new(config.base_dir.clone(), config.trash_retention_days);
    let total = batch.targets.len();
    let mut deleted = 0usize;
    let mut skipped_or_failed = 0usize;

    state
        .write()
        .await
        .record_exclusive_deleted_progress(task_id, 0, total, 0);

    for target in &batch.targets {
        if *cancel_rx.borrow() {
            break;
        }

        match target.verify_unchanged(&guard) {
            Ok(verified_path) => match trash.move_to_trash(&verified_path) {
                Ok(_) => {
                    deleted += 1;
                    state.write().await.record_exclusive_deleted_progress(
                        task_id,
                        deleted,
                        total,
                        0,
                    );
                    info!(
                        filename = %target.display_name(),
                        bytes = target.size,
                        "Verified regular file moved to trash"
                    );
                }
                Err(e) => {
                    skipped_or_failed += 1;
                    warn!(
                        filename = %target.display_name(),
                        error = %redact_secrets(&e.to_string()),
                        "Failed to remove verified file"
                    );
                }
            },
            Err(e) => {
                skipped_or_failed += 1;
                warn!(
                    filename = %target.display_name(),
                    reason = %redact_secrets(&e.to_string()),
                    "Refused to delete file because identity/boundary verification failed"
                );
            }
        }
    }

    if *cancel_rx.borrow() {
        let untouched = total.saturating_sub(deleted + skipped_or_failed);
        edit_html_bounded(
            &bot,
            msg.chat.id,
            msg.id,
            format!(
                "🛑 <b>回收站任务已停止。</b>\n\
                 ✅ 停止前已移入: <code>{deleted}/{total}</code> 个文件（可用 /trash 恢复）\n\
                 ⏭️ 未执行删除: <code>{untouched}</code> 个文件\n\
                 ⚠️ 跳过或失败: <code>{skipped_or_failed}</code> 个"
            ),
        )
        .await;
        return Err(AppError::Cancelled.into());
    }

    edit_html_bounded(
        &bot,
        msg.chat.id,
        msg.id,
        format!(
            "🗑️ <b>文件已移入回收站！</b>\n\
             ✅ 成功移入: <code>{deleted}/{total}</code> 个（可用 /trash 恢复）\n\
             ⚠️ 跳过或失败: <code>{skipped_or_failed}</code> 个"
        ),
    )
    .await;

    Ok(())
}
