//! CallbackQuery 统一处理器。
//!
//! 解析所有 InlineKeyboard 回调数据，并分发到对应的业务逻辑处理函数。
//! 对应 Python 版本的 `callback_router` 函数。
//!
//! ## 回调数据格式约定
//!
//! | 格式 | 说明 |
//! |------|------|
//! | `menu_main` | 返回主菜单 |
//! | `init_{action}` | 初始化文件选择器 |
//! | `menu_{action}_{page}` | 翻页 |
//! | `enterdir_{action}_{idx}` | 进入子目录 |
//! | `updir_{action}` | 返回上级目录 |
//! | `toggle_{action}_{idx}_{page}` | 切换文件选中状态 |
//! | `execsingle_{action}_{idx}` | 执行单文件操作 |
//! | `execbatch_{action}` | 执行批量操作 |
//! | `confirm_delete` | 确认删除 |
//! | `cancel_delete` | 取消删除 |

use std::path::PathBuf;
use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::MaybeInaccessibleMessage;
use tracing::{info, warn};

use crate::actions;
use crate::config::Config;
use crate::core::state::{ActionType, SharedState};
use crate::core::PermissionGuard;
use crate::errors::AppError;
use crate::storage::path::PathGuard;
use crate::ui::menu::render_main_menu;
use crate::ui::selector::render_file_selector;

/// 处理所有 CallbackQuery 的入口函数。
pub async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    state: SharedState,
    config: Arc<Config>,
) -> ResponseResult<()> {
    // 权限检查
    let guard = PermissionGuard::new(config.admin_id);
    let user_id = q.from.id.0 as i64;
    if guard.check_user_id(user_id).is_err() {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    }

    let data = match &q.data {
        Some(d) => d.clone(),
        None => {
            bot.answer_callback_query(&q.id).await?;
            return Ok(());
        }
    };

    let msg = match &q.message {
        Some(MaybeInaccessibleMessage::Regular(m)) => m.clone(),
        _ => {
            bot.answer_callback_query(&q.id).await?;
            return Ok(());
        }
    };

    info!(user_id = user_id, callback_data = %data, "Callback received");

    let result = dispatch_callback(&bot, &q, &msg, &data, state, config).await;

    if let Err(e) = result {
        warn!(error = %e, callback_data = %data, "Callback handler error");
        // 尝试向用户显示错误
        let _ = bot
            .answer_callback_query(&q.id)
            .text(format!("❌ 操作失败: {e}"))
            .show_alert(true)
            .await;
    }

    Ok(())
}

/// 根据回调数据分发到具体处理逻辑。
async fn dispatch_callback(
    bot: &Bot,
    q: &CallbackQuery,
    msg: &Message,
    data: &str,
    state: SharedState,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let path_guard = PathGuard::new(config.base_dir.clone());

    // ── 主菜单 ─────────────────────────────────────────────────────────────────
    if data == "menu_main" {
        bot.answer_callback_query(&q.id).await?;
        render_main_menu(bot, msg, &state, &config).await?;
        return Ok(());
    }

    // ── 删除确认/取消 ──────────────────────────────────────────────────────────
    if data == "cancel_delete" {
        {
            let mut s = state.write().await;
            s.pending_delete_files.clear();
        }
        bot.answer_callback_query(&q.id)
            .text("已取消删除。")
            .await?;
        render_file_selector(bot, msg, &state, &config, &ActionType::Delete, 0).await?;
        return Ok(());
    }

    if data == "confirm_delete" {
        let pending = {
            let s = state.read().await;
            s.pending_delete_files.clone()
        };
        if pending.is_empty() {
            bot.answer_callback_query(&q.id)
                .text("没有待删除文件，请重新选择。")
                .show_alert(true)
                .await?;
            return Ok(());
        }
        bot.answer_callback_query(&q.id).await?;
        actions::delete::start_delete(bot, msg, state, config, pending).await?;
        return Ok(());
    }

    // ── init_{action} ──────────────────────────────────────────────────────────
    if let Some(action_str) = data.strip_prefix("init_") {
        let action = ActionType::from_str(action_str)
            .ok_or_else(|| AppError::invalid_argument(format!("Unknown action: {action_str}")))?;
        {
            let mut s = state.write().await;
            s.current_dir = config.base_dir.clone();
            s.clear_selections(&action);
            s.pending_delete_files.clear();
        }
        bot.answer_callback_query(&q.id).await?;
        render_file_selector(bot, msg, &state, &config, &action, 0).await?;
        return Ok(());
    }

    // ── menu_{action}_{page} ───────────────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("menu_") {
        let parts: Vec<&str> = rest.rsplitn(2, '_').collect();
        if parts.len() == 2 {
            let page: usize = parts[0].parse().unwrap_or(0);
            let action_str = parts[1];
            let action = ActionType::from_str(action_str).ok_or_else(|| {
                AppError::invalid_argument(format!("Unknown action: {action_str}"))
            })?;
            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, page).await?;
        }
        return Ok(());
    }

    // ── enterdir_{action}_{idx} ────────────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("enterdir_") {
        let parts: Vec<&str> = rest.rsplitn(2, '_').collect();
        if parts.len() == 2 {
            let idx: usize = parts[0].parse().unwrap_or(0);
            let action_str = parts[1];
            let action = ActionType::from_str(action_str).ok_or_else(|| {
                AppError::invalid_argument(format!("Unknown action: {action_str}"))
            })?;

            let item_name = {
                let s = state.read().await;
                s.current_files.get(idx).cloned()
            };

            if let Some(name) = item_name {
                let new_dir = {
                    let s = state.read().await;
                    path_guard.safe_join(&s.current_dir, &name)?
                };
                {
                    let mut s = state.write().await;
                    s.current_dir = new_dir;
                    s.clear_selections(&action);
                    s.pending_delete_files.clear();
                }
                bot.answer_callback_query(&q.id).await?;
                render_file_selector(bot, msg, &state, &config, &action, 0).await?;
            }
        }
        return Ok(());
    }

    // ── updir_{action} ─────────────────────────────────────────────────────────
    if let Some(action_str) = data.strip_prefix("updir_") {
        let action = ActionType::from_str(action_str)
            .ok_or_else(|| AppError::invalid_argument(format!("Unknown action: {action_str}")))?;

        {
            let mut s = state.write().await;
            let current = s.current_dir.clone();
            let base = path_guard.assert_inside(&current)?;
            if base != config.base_dir.as_path() {
                if let Some(parent) = current.parent() {
                    let parent_checked = path_guard.assert_inside(parent)?;
                    s.current_dir = parent_checked.to_path_buf();
                }
            }
            s.clear_selections(&action);
            s.pending_delete_files.clear();
        }
        bot.answer_callback_query(&q.id).await?;
        render_file_selector(bot, msg, &state, &config, &action, 0).await?;
        return Ok(());
    }

    // ── toggle_{action}_{idx}_{page} ──────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("toggle_") {
        // 格式: toggle_{action}_{idx}_{page}
        // 因为 action 本身不含下划线，可以从后往前解析
        let parts: Vec<&str> = rest.rsplitn(3, '_').collect();
        if parts.len() == 3 {
            let page: usize = parts[0].parse().unwrap_or(0);
            let idx: usize = parts[1].parse().unwrap_or(0);
            let action_str = parts[2];
            let action = ActionType::from_str(action_str).ok_or_else(|| {
                AppError::invalid_argument(format!("Unknown action: {action_str}"))
            })?;

            let file_path = {
                let s = state.read().await;
                s.current_files
                    .get(idx)
                    .map(|name| s.current_dir.join(name))
            };

            if let Some(path) = file_path {
                let safe_path = path_guard.assert_inside(&path)?.to_path_buf();
                {
                    let mut s = state.write().await;
                    let sel = s.selections_mut(&action);
                    if sel.contains(&safe_path) {
                        sel.remove(&safe_path);
                    } else {
                        sel.insert(safe_path);
                    }
                    s.pending_delete_files.clear();
                }
            }
            bot.answer_callback_query(&q.id).await?;
            render_file_selector(bot, msg, &state, &config, &action, page).await?;
        }
        return Ok(());
    }

    // ── execsingle_{action}_{idx} ──────────────────────────────────────────────
    if let Some(rest) = data.strip_prefix("execsingle_") {
        let parts: Vec<&str> = rest.rsplitn(2, '_').collect();
        if parts.len() == 2 {
            let idx: usize = parts[0].parse().unwrap_or(0);
            let action_str = parts[1];
            let action = ActionType::from_str(action_str).ok_or_else(|| {
                AppError::invalid_argument(format!("Unknown action: {action_str}"))
            })?;

            let file_path = {
                let s = state.read().await;
                s.current_files
                    .get(idx)
                    .map(|name| s.current_dir.join(name))
            };

            if let Some(path) = file_path {
                let safe_path = path_guard.assert_inside(&path)?.to_path_buf();
                bot.answer_callback_query(&q.id).await?;

                match action {
                    ActionType::Browse => {
                        actions::browse::action_browse(bot, q, safe_path).await?;
                    }
                    ActionType::Stream => {
                        actions::stream::start_stream(bot, msg, state, config, safe_path).await?;
                    }
                    ActionType::Youtube => {
                        actions::youtube::start_youtube_uploads(
                            bot,
                            msg,
                            state,
                            config,
                            vec![safe_path],
                        )
                        .await?;
                    }
                    _ => {}
                }
            }
        }
        return Ok(());
    }

    // ── execbatch_{action} ─────────────────────────────────────────────────────
    if let Some(action_str) = data.strip_prefix("execbatch_") {
        let action = ActionType::from_str(action_str)
            .ok_or_else(|| AppError::invalid_argument(format!("Unknown action: {action_str}")))?;

        let selected_files: Vec<PathBuf> = {
            let s = state.read().await;
            s.selections_ref(&action)
                .map(|set| {
                    let mut files: Vec<PathBuf> =
                        set.iter().filter(|p| p.is_file()).cloned().collect();
                    files.sort();
                    files
                })
                .unwrap_or_default()
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
                // 检查独占任务和上传任务
                let (has_active, upload_count) = {
                    let mut s = state.write().await;
                    s.cleanup_active_task();
                    s.cleanup_youtube_pool();
                    (s.has_active_task(), s.active_youtube_count())
                };

                if has_active {
                    let name = {
                        let s = state.read().await;
                        s.active_task_name().unwrap_or("运行中任务").to_string()
                    };
                    bot.answer_callback_query(&q.id)
                        .text(format!(
                            "已有任务正在运行：{name}\n请先发送 /stop 或等待完成。"
                        ))
                        .show_alert(true)
                        .await?;
                    return Ok(());
                }
                if upload_count > 0 {
                    bot.answer_callback_query(&q.id)
                        .text(format!(
                            "当前有 {upload_count} 个 YouTube 上传任务，请先 /stop 或等待完成后再删除。"
                        ))
                        .show_alert(true)
                        .await?;
                    return Ok(());
                }

                bot.answer_callback_query(&q.id).await?;
                actions::delete::render_delete_confirmation(bot, msg, &state, selected_files)
                    .await?;
            }
            ActionType::Youtube => {
                bot.answer_callback_query(&q.id).await?;
                actions::youtube::start_youtube_uploads(bot, msg, state, config, selected_files)
                    .await?;
            }
            ActionType::Concat => {
                bot.answer_callback_query(&q.id).await?;
                actions::concat::start_concat(bot, msg, state, config, selected_files).await?;
            }
            ActionType::Convert => {
                bot.answer_callback_query(&q.id).await?;
                actions::convert::start_convert(bot, msg, state, config, selected_files).await?;
            }
            _ => {}
        }
        return Ok(());
    }

    // 未知回调
    warn!(callback_data = %data, "Unknown callback data received");
    bot.answer_callback_query(&q.id).text("❓ 未知操作").await?;

    Ok(())
}
