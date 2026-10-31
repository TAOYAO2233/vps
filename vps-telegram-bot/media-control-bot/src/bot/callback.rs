//! CallbackQuery 统一路由器与菜单快照安全校验器。
//!
//! ## 回调数据格式（全部 < 36 字节，远低于 Telegram 64 字节上限）
//!
//! | 格式 | 说明 |
//! |------|------|
//! | `menu_main` | 返回主菜单（实时刷新 `/storage512` 容量） |
//! | `menu_trash` | 打开回收站，支持恢复与二次确认清空 |
//! | `trash_clear_ask` / `trash_clear_confirm_{token}` | 对当前回收站清单进行永久清空确认 |
//! | `init_{action}` | 初始化指定模式的文件选择器 |
//! | `page_{action}_{snap_id}_{page}` | 翻页（校验快照与目录变动） |
//! | `enterdir_{action}_{snap_id}_{idx}` | 进入子目录（校验快照、目录类型与 `PathGuard`） |
//! | `updir_{action}_{snap_id}` | 返回上级目录（校验不越出 `BASE_DIR`） |
//! | `toggle_{action}_{snap_id}_{idx}_{page}` | 切换多选选中状态（严格保持点击顺序） |
//! | `execsingle_{action}_{snap_id}_{idx}` | 执行单文件操作 |
//! | `execbatch_{action}_{snap_id}` | 执行多选批量操作（不重排合并顺序） |
//! | `confirm_del_{token}` | 校验一次性令牌并执行批量删除 |
//! | `cancel_del_{token}` | 取消删除确认批次 |

use std::path::PathBuf;
use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::MaybeInaccessibleMessage;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use tracing::{info, warn};

use crate::actions;
use crate::config::Config;
use crate::core::state::{
    compute_listing_hash, ActionType, MenuSnapshot, SharedState, SnapshotEntry,
};
use crate::core::PermissionGuard;
use crate::core::task_manager::TaskManager;
use crate::storage::path::PathGuard;
use crate::storage::filesystem::format_file_size_bytes;
use crate::storage::trash::TrashStore;
use crate::storage::scanner::scan_directory;
use crate::ui::menu::render_main_menu;
use crate::bot::commands::render_trash_panel;
use crate::ui::pagination::Paginator;
use crate::ui::selector::render_file_selector;
use crate::utils::redact::{redact_secrets, sanitize_user_error};

/// 处理所有 CallbackQuery 的入口函数。
pub async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    state: SharedState,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let guard = PermissionGuard::new(config.admin_id);
    let user_id = q.from.id.0 as i64;
    if guard.check_user_id(user_id).is_err() {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    }

    let Some(data) = q.data.clone() else {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    };

    let Some(MaybeInaccessibleMessage::Regular(msg)) = q.message.clone() else {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    };

    info!(user_id = user_id, callback_data = %data, "Callback received");

    let menu_key = (msg.chat.id.0, msg.id.0);
    let menu_lock = {
        let s = state.read().await;
        Arc::clone(&s.menu_lock)
    };
    let outcome = {
        let _menu_guard = menu_lock.lock().await;
        state.write().await.load_menu_session(menu_key, &config.base_dir);
        let result = dispatch_callback(&bot, &q, &msg, &data, Arc::clone(&state), Arc::clone(&config)).await;
        state.write().await.save_menu_session(menu_key);
        result
    };
    if let Err(e) = outcome {
        let safe_log = redact_secrets(&e.to_string());
        warn!(error = %safe_log, callback_data = %data, "Callback handler error");

        let safe_user_text = sanitize_user_error(&e);
        let mut alert_text = format!("❌ {safe_user_text}");
        if alert_text.chars().count() > 185 {
            alert_text = alert_text.chars().take(184).collect::<String>() + "…";
        }

        let _ = bot
            .answer_callback_query(&q.id)
            .text(alert_text)
            .show_alert(true)
            .await;
    }

    Ok(())
}

/// 分发具体回调动作。
async fn dispatch_callback(
    bot: &Bot,
    q: &CallbackQuery,
    msg: &Message,
    data: &str,
    state: SharedState,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let path_guard = PathGuard::new(config.base_dir.clone());

    if let Some(task_id) = data.strip_prefix("pending_resume_") {
        bot.answer_callback_query(&q.id).await?;
        actions::youtube::resume_pending_upload(bot, msg, state, config, task_id).await?;
        return Ok(());
    }
    if let Some(task_id) = data.strip_prefix("pending_dropask_") {
        bot.answer_callback_query(&q.id).await?;
        bot.send_message(msg.chat.id, "确认放弃这条上传恢复记录？这不会删除 YouTube 上的视频。")
            .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("确认放弃", format!("pending_drop_{task_id}")),
                InlineKeyboardButton::callback("取消", "pending_noop"),
            ]]))
            .await?;
        return Ok(());
    }
    if data == "pending_noop" {
        bot.answer_callback_query(&q.id).text("已取消。").await?;
        return Ok(());
    }
    if let Some(task_id) = data.strip_prefix("pending_drop_") {
        let store = {
            let s = state.read().await;
            if s.youtube_pool.contains_key(task_id) {
                anyhow::bail!("该任务正在上传，先停止任务后再放弃恢复记录");
            }
            Arc::clone(&s.youtube_queue_store)
        };
        let exists = store.load_recoverable_entries().await?
            .iter().any(|e| e.task_id == task_id && e.unconfirmed_full_upload);
        if !exists { anyhow::bail!("待核实任务已不存在"); }
        store.remove(task_id).await?;
        bot.answer_callback_query(&q.id).text("已放弃恢复记录。").await?;
        return Ok(());
    }

    if let Some(id) = data.strip_prefix("restore_") {
        bot.answer_callback_query(&q.id).await?;
        let monitor = {
            let s = state.read().await;
            Arc::clone(&s.disk_monitor)
        };
        monitor.ensure_space_available(0).await?;
        let store = TrashStore::new(config.base_dir.clone(), config.trash_retention_days);
        let restored = store.restore(id)?;
        bot.send_message(msg.chat.id, format!("✅ 已恢复：{}", restored.display())).await?;
        render_trash_panel(bot, msg, &state, &config, true).await?;
        return Ok(());
    }

    if data == "menu_trash" {
        bot.answer_callback_query(&q.id).await?;
        render_trash_panel(bot, msg, &state, &config, true).await?;
        return Ok(());
    }

    if data == "trash_clear_ask" {
        let monitor = { Arc::clone(&state.read().await.disk_monitor) };
        monitor.ensure_space_available(0).await?;
        let store = TrashStore::new(config.base_dir.clone(), config.trash_retention_days);
        let entries = store.list()?;
        if entries.is_empty() {
            bot.answer_callback_query(&q.id).text("回收站已清空。").await?;
            render_trash_panel(bot, msg, &state, &config, true).await?;
            return Ok(());
        }
        let bytes = entries.iter().fold(0_u64, |sum, entry| sum.saturating_add(entry.size));
        let token = TrashStore::snapshot_token(&entries);
        bot.answer_callback_query(&q.id).await?;
        bot.edit_message_text(msg.chat.id, msg.id, format!(
            "⚠️ <b>确认永久清空回收站？</b>\n\n将删除 <b>{}</b> 个文件，约 <b>{}</b>。\n此操作无法恢复。",
            entries.len(), format_file_size_bytes(bytes)
        ))
            .parse_mode(teloxide::types::ParseMode::Html)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("确认永久清空", format!("trash_clear_confirm_{token}"))],
                vec![InlineKeyboardButton::callback("取消，返回回收站", "menu_trash")],
            ]))
            .await?;
        return Ok(());
    }

    if let Some(token) = data.strip_prefix("trash_clear_confirm_") {
        let monitor = { Arc::clone(&state.read().await.disk_monitor) };
        monitor.ensure_space_available(0).await?;
        let store = TrashStore::new(config.base_dir.clone(), config.trash_retention_days);
        let entries = store.list()?;
        if entries.is_empty() || TrashStore::snapshot_token(&entries) != token {
            bot.answer_callback_query(&q.id)
                .text("回收站内容已变化，请重新确认。")
                .show_alert(true).await?;
            render_trash_panel(bot, msg, &state, &config, true).await?;
            return Ok(());
        }
        bot.answer_callback_query(&q.id).text("正在清空回收站…").await?;
        let expected = token.to_string();
        let report = tokio::task::spawn_blocking(move || store.clear_all(&expected)).await;
        match report {
            Ok(Ok(report)) => {
                let heading = if report.failed == 0 {
                    "🧹 <b>回收站已清空</b>"
                } else {
                    "⚠️ <b>回收站部分清理失败</b>"
                };
                bot.edit_message_text(msg.chat.id, msg.id, format!(
                    "{heading}\n已清理：{} 个\n释放空间：{}\n失败：{} 个",
                    report.removed, format_file_size_bytes(report.freed_bytes), report.failed
                ))
                    .parse_mode(teloxide::types::ParseMode::Html)
                    .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("返回回收站", "menu_trash"),
                    ]]))
                    .await?;
            }
            Ok(Err(e)) => {
                bot.send_message(msg.chat.id, format!("清空失败：{}", sanitize_user_error(&e))).await?;
                render_trash_panel(bot, msg, &state, &config, true).await?;
            }
            Err(e) => {
                let err: anyhow::Error = e.into();
                bot.send_message(msg.chat.id, format!("清空任务异常：{}", sanitize_user_error(&err))).await?;
            }
        }
        return Ok(());
    }

    if let Some(task_id) = data.strip_prefix("verify_yt_") {
        bot.answer_callback_query(&q.id).text("正在查询服务端状态…").await?;
        actions::youtube::verify_pending_upload(bot, msg, &state, &config, task_id).await?;
        return Ok(());
    }

    if let Some(task_id) = data.strip_prefix("stop_yt_") {
        let stopped = TaskManager::new(Arc::clone(&state)).request_stop_youtube(task_id).await;
        bot.answer_callback_query(&q.id)
            .text(if stopped { "已向该上传任务发送停止信号。" } else { "任务已结束或不存在。" })
            .show_alert(true)
            .await?;
        return Ok(());
    }
    if let Some(raw_id) = data.strip_prefix("stop_excl_") {
        let stopped = match raw_id.parse::<u64>() {
            Ok(id) => TaskManager::new(Arc::clone(&state)).request_stop_exclusive(id).await,
            Err(_) => false,
        };
        bot.answer_callback_query(&q.id)
            .text(if stopped { "已向该独占任务发送停止信号。" } else { "任务已结束或不存在。" })
            .show_alert(true)
            .await?;
        return Ok(());
    }

    // ── 返回主菜单 ─────────────────────────────────────────────────────────────
    if data == "menu_main" {
        bot.answer_callback_query(&q.id).await?;
        render_main_menu(bot, msg, &state, &config).await?;
        return Ok(());
    }

    // ── 删除确认 / 取消（绑定一次性确认令牌）─────────────────────────────────────
    if let Some(token) = data.strip_prefix("cancel_del_") {
        {
            let mut s = state.write().await;
            let _ = s.take_pending_delete(token);
        }
        bot.answer_callback_query(&q.id)
            .text("已取消删除。")
            .await?;
        render_file_selector(bot, msg, &state, &config, &ActionType::Delete, 0).await?;
        return Ok(());
    }

    if let Some(token) = data.strip_prefix("confirm_del_") {
        let batch_opt = {
            let mut s = state.write().await;
            s.take_pending_delete(token)
        };
        let Some(batch) = batch_opt else {
            bot.answer_callback_query(&q.id)
                .text("⚠️ 删除确认令牌已失效或已执行，请重新选择文件。")
                .show_alert(true)
                .await?;
            render_file_selector(bot, msg, &state, &config, &ActionType::Delete, 0).await?;
            return Ok(());
        };

        bot.answer_callback_query(&q.id).await?;
        actions::delete::start_delete(bot, msg, state, config, batch).await?;
        return Ok(());
    }

    // ── init_{action} ──────────────────────────────────────────────────────────
    if let Some(action_str) = data.strip_prefix("init_") {
        let action = action_str.parse::<ActionType>()?;
        {
            let mut s = state.write().await;
            s.current_dir = config.base_dir.clone();
            s.clear_selections(&action);
            s.pending_delete = None;
            s.current_snapshot = None;
        }
        bot.answer_callback_query(&q.id).await?;
        render_file_selector(bot, msg, &state, &config, &action, 0).await?;
        return Ok(());
    }

    // ── page_{action}_{snap_id}_{page} ─────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("page_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 3 {
            let action = parts[0].parse::<ActionType>()?;
            let snap_id = parts[1];
            let page: usize = parts[2].parse().unwrap_or(0);

            let Some(_snap) =
                verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, snap_id)
                    .await?
            else {
                return Ok(());
            };

            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, page).await?;
            return Ok(());
        }
    }

    // ── enterdir_{action}_{snap_id}_{idx} ──────────────────────────────────────
    if let Some(rest) = data.strip_prefix("enterdir_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 3 {
            let action = parts[0].parse::<ActionType>()?;
            let snap_id = parts[1];
            let idx: usize = parts[2].parse().unwrap_or(usize::MAX);

            let Some(snap) =
                verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, snap_id)
                    .await?
            else {
                return Ok(());
            };

            let Some(entry) = snap.entries.get(idx) else {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            };
            if !entry.is_dir {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            }

            let new_dir = path_guard.safe_join(&snap.dir, &entry.name)?;
            if !new_dir.is_dir() {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            }

            {
                let mut s = state.write().await;
                s.current_dir = new_dir;
                s.clear_selections(&action);
                s.pending_delete = None;
            }
            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, 0).await?;
            return Ok(());
        }
    }

    // ── updir_{action}_{snap_id} ───────────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("updir_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 2 {
            let action = parts[0].parse::<ActionType>()?;
            let snap_id = parts[1];

            let Some(snap) =
                verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, snap_id)
                    .await?
            else {
                return Ok(());
            };

            let canonical_base = path_guard.canonical_base()?;
            let current_checked = path_guard.assert_inside(&snap.dir)?;
            let next_dir = if current_checked != canonical_base {
                match current_checked.parent() {
                    Some(parent) => path_guard
                        .assert_inside(parent)
                        .unwrap_or(canonical_base),
                    None => canonical_base,
                }
            } else {
                canonical_base
            };

            {
                let mut s = state.write().await;
                s.current_dir = next_dir;
                s.clear_selections(&action);
                s.pending_delete = None;
            }
            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, 0).await?;
            return Ok(());
        }
    }

    if let Some(rest) = data.strip_prefix("selectpage_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 3 {
            let action = parts[0].parse::<ActionType>()?;
            let page: usize = parts[2].parse().unwrap_or(0);
            if !action.is_multi_select() { anyhow::bail!("该模式不支持批量选择"); }
            let Some(snap) = verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, parts[1]).await? else { return Ok(()); };
            let paginator = Paginator::new(snap.entries.len(), config.items_per_page, page);
            let mut paths = Vec::new();
            for entry in &snap.entries[paginator.range()] {
                if !entry.is_dir {
                    paths.push(path_guard.safe_join(&snap.dir, &entry.name)?);
                }
            }
            {
                let mut s = state.write().await;
                let selected = s.selections.entry(action.clone()).or_default();
                for path in paths {
                    if !selected.contains(&path) { selected.push(path); }
                }
                s.pending_delete = None;
            }
            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, page).await?;
            return Ok(());
        }
    }
    if let Some(rest) = data.strip_prefix("clearselection_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 2 {
            let action = parts[0].parse::<ActionType>()?;
            let Some(_) = verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, parts[1]).await? else { return Ok(()); };
            state.write().await.clear_selections(&action);
            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, 0).await?;
            return Ok(());
        }
    }

    // ── toggle_{action}_{snap_id}_{idx}_{page} ─────────────────────────────────
    if let Some(rest) = data.strip_prefix("toggle_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 4 {
            let action = parts[0].parse::<ActionType>()?;
            let snap_id = parts[1];
            let idx: usize = parts[2].parse().unwrap_or(usize::MAX);
            let page: usize = parts[3].parse().unwrap_or(0);

            let Some(snap) =
                verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, snap_id)
                    .await?
            else {
                return Ok(());
            };

            let Some(entry) = snap.entries.get(idx) else {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            };
            if entry.is_dir {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            }

            let safe_path = path_guard.safe_join(&snap.dir, &entry.name)?;
            {
                let mut s = state.write().await;
                s.toggle_selection(&action, safe_path);
                s.pending_delete = None;
            }

            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, page).await?;
            return Ok(());
        }
    }

    // ── execsingle_{action}_{snap_id}_{idx} ────────────────────────────────────
    if let Some(rest) = data.strip_prefix("execsingle_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 3 {
            let action = parts[0].parse::<ActionType>()?;
            let snap_id = parts[1];
            let idx: usize = parts[2].parse().unwrap_or(usize::MAX);

            let Some(snap) =
                verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, snap_id)
                    .await?
            else {
                return Ok(());
            };

            let Some(entry) = snap.entries.get(idx) else {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            };
            if entry.is_dir {
                reject_stale_and_refresh(bot, q, msg, &state, &config, &action).await?;
                return Ok(());
            }

            let safe_path = path_guard.safe_join(&snap.dir, &entry.name)?;

            match action {
                ActionType::Browse => {
                    // action_browse 内部会自行调用 answer_callback_query（弹窗或确认），此处不重复应答
                    actions::browse::action_browse(bot, q, msg, &config, safe_path).await?;
                }
                ActionType::Stream => {
                    bot.answer_callback_query(&q.id).await?;
                    actions::stream::start_stream(bot, msg, state, config, safe_path).await?;
                }
                ActionType::Youtube => {
                    bot.answer_callback_query(&q.id).await?;
                    actions::youtube::start_youtube_uploads(
                        bot,
                        msg,
                        state,
                        config,
                        vec![safe_path],
                    )
                    .await?;
                }
                _ => {
                    bot.answer_callback_query(&q.id).await?;
                }
            }
            return Ok(());
        }
    }

    // ── execbatch_{action}_{snap_id} ───────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("execbatch_") {
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() == 2 {
            let action = parts[0].parse::<ActionType>()?;
            let snap_id = parts[1];

            let Some(_snap) =
                verify_snapshot_or_refresh(bot, q, msg, &state, &config, &path_guard, &action, snap_id)
                    .await?
            else {
                return Ok(());
            };

            // 严格保持用户选择的先后顺序，严禁执行 .sort() 重排！
            let selected_files: Vec<PathBuf> = {
                let s = state.read().await;
                s.selections_slice(&action).to_vec()
            };

            if selected_files.is_empty() {
                bot.answer_callback_query(&q.id)
                    .text("❌ 请先选择至少一个文件！")
                    .show_alert(true)
                    .await?;
                return Ok(());
            }

            match action {
                ActionType::Delete => {
                    let (has_active, active_name, upload_count) = {
                        let mut s = state.write().await;
                        s.cleanup_active_task();
                        s.cleanup_youtube_pool();
                        (
                            s.has_active_task(),
                            s.active_task_name().map(str::to_string),
                            s.active_youtube_count(),
                        )
                    };

                    if has_active {
                        bot.answer_callback_query(&q.id)
                            .text(format!(
                                "已有任务正在运行：{}\n请先发送 /stop 或等待完成。",
                                active_name.as_deref().unwrap_or("运行中任务")
                            ))
                            .show_alert(true)
                            .await?;
                        return Ok(());
                    }
                    if upload_count > 0 {
                        bot.answer_callback_query(&q.id)
                            .text(format!(
                                "当前有 {upload_count} 个 YouTube 上传/排队任务，请先 /stop 或等待完成后再删除。"
                            ))
                            .show_alert(true)
                            .await?;
                        return Ok(());
                    }

                    bot.answer_callback_query(&q.id).await?;
                    actions::delete::render_delete_confirmation(
                        bot,
                        msg,
                        &state,
                        &config,
                        selected_files,
                    )
                    .await?;
                }
                ActionType::Youtube => {
                    bot.answer_callback_query(&q.id).await?;
                    actions::youtube::start_youtube_uploads(
                        bot,
                        msg,
                        state,
                        config,
                        selected_files,
                    )
                    .await?;
                }
                ActionType::Concat => {
                    bot.answer_callback_query(&q.id).await?;
                    actions::concat::start_concat(bot, msg, state, config, selected_files).await?;
                }
                ActionType::Convert => {
                    bot.answer_callback_query(&q.id).await?;
                    actions::convert::start_convert(bot, msg, state, config, selected_files)
                        .await?;
                }
                _ => {
                    bot.answer_callback_query(&q.id).await?;
                }
            }
            return Ok(());
        }
    }

    // 兼容过期旧版菜单消息或未知回调：友好提示并引导刷新
    warn!(callback_data = %data, "Stale or unknown callback data received");
    bot.answer_callback_query(&q.id)
        .text("⚠️ 菜单已过期或列表已更新，请重新确认。")
        .show_alert(true)
        .await?;
    render_main_menu(bot, msg, &state, &config).await?;

    Ok(())
}

/// 校验回调携带的 `snap_id` 与内存快照一致，且底层目录当前未发生增删改名变化。
///
/// 若检测到旧菜单点击或目录结构已变动，自动向用户弹出“列表已更新，请重新确认”并刷新当前菜单。
#[allow(clippy::too_many_arguments)]
async fn verify_snapshot_or_refresh(
    bot: &Bot,
    q: &CallbackQuery,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    path_guard: &PathGuard,
    expected_action: &ActionType,
    callback_snap_id: &str,
) -> anyhow::Result<Option<MenuSnapshot>> {
    let snap_opt = {
        let s = state.read().await;
        s.current_snapshot.clone()
    };

    let Some(snap) = snap_opt else {
        reject_stale_and_refresh(bot, q, msg, state, config, expected_action).await?;
        return Ok(None);
    };

    if snap.snap_id != callback_snap_id || &snap.action != expected_action {
        reject_stale_and_refresh(bot, q, msg, state, config, expected_action).await?;
        return Ok(None);
    }

    let Ok(canonical_dir) = path_guard.assert_inside(&snap.dir) else {
        reject_stale_and_refresh(bot, q, msg, state, config, expected_action).await?;
        return Ok(None);
    };

    let scan_dir = canonical_dir.clone();
    let live_listing = match tokio::task::spawn_blocking(move || scan_directory(&scan_dir)).await {
        Ok(Ok(l)) => l,
        _ => {
            reject_stale_and_refresh(bot, q, msg, state, config, expected_action).await?;
            return Ok(None);
        }
    };

    let search_query = {
        let s = state.read().await;
        s.search_query.to_lowercase()
    };
    let live_entries: Vec<SnapshotEntry> = live_listing.items.iter()
        .filter(|item| search_query.is_empty() || item.name.to_lowercase().contains(&search_query))
        .map(SnapshotEntry::from)
        .collect();
    let live_hash = compute_listing_hash(&canonical_dir, &live_entries);

    if live_hash != snap.listing_hash {
        reject_stale_and_refresh(bot, q, msg, state, config, expected_action).await?;
        return Ok(None);
    }

    Ok(Some(snap))
}

async fn reject_stale_and_refresh(
    bot: &Bot,
    q: &CallbackQuery,
    msg: &Message,
    state: &SharedState,
    config: &Config,
    action: &ActionType,
) -> anyhow::Result<()> {
    let _ = bot
        .answer_callback_query(&q.id)
        .text("⚠️ 列表已更新，请重新确认后再操作。")
        .show_alert(true)
        .await;
    render_file_selector(bot, msg, state, config, action, 0).await?;
    Ok(())
}
