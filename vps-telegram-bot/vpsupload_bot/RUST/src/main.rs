mod config;
mod media_utils;
mod state;
mod ui;
mod actions;
mod youtube;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use teloxide::prelude::*;
use teloxide::types::{MaybeInaccessibleMessage, Message, ParseMode};
use tracing::{info, error};

use config::AppConfig;
use state::{AppState, ActiveTask};

fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_thread_ids(true)
        .with_ansi(true) 
        .init();

    let _ = dotenvy::dotenv();

    let config = match AppConfig::load_from_env() {
        Ok(c) => c,
        Err(e) => {
            error!("加载配置失败: {}", e);
            std::process::exit(1);
        }
    };

    info!("🚀 Rust VPS 媒体管理 Bot 正在启动... BASE_DIR: {:?}", config.base_dir);

    let state = Arc::new(AppState::new(config.clone()));
    let bot = Bot::new(config.bot_token);

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    if !state.config.is_admin(msg.chat.id.0 as u64) { return Ok(()); }
    
    if let Some(text) = msg.text() {
        if text.starts_with("/start") {
            let (content, kb) = ui::build_main_menu(state.clone(), msg.chat.id.0 as i64).await;
            bot.send_message(msg.chat.id, content).parse_mode(ParseMode::Html).reply_markup(kb).await?;
        } else if text.starts_with("/stop") {
            let mut active = state.active_task.lock().await;
            if let Some(task) = active.take() {
                task.cancel_flag.store(true, Ordering::SeqCst);
                task.cancel_notify.notify_waiters();
                
                bot.send_message(msg.chat.id, format!("🛑 <b>已发送信号终止任务</b>: <code>{}</code>", escape_html(&task.name)))
                    .parse_mode(ParseMode::Html).await?;
            } else {
                bot.send_message(msg.chat.id, "ℹ️ 当前没有正在运行的独占任务。").await?;
            }
        } else if text.starts_with("/uploads") {
            let map = state.youtube_uploads.lock().await;
            if map.is_empty() {
                bot.send_message(msg.chat.id, "ℹ️ 当前没有 YouTube 上传任务。").await?;
            } else {
                let mut lines = vec!["📤 <b>正在进行的 YouTube 队列:</b>\n".to_string()];
                for (idx, (_, info)) in map.iter().enumerate() {
                    lines.push(format!(
                        "{}. <code>{}</code>\n   状态: {} ({:.1}%)", 
                        idx + 1, 
                        escape_html(&info.filename), 
                        escape_html(&info.status), 
                        info.progress
                    ));
                }
                bot.send_message(msg.chat.id, lines.join("\n")).parse_mode(ParseMode::Html).await?;
            }
        }
    }
    Ok(())
}

async fn handle_callback(bot: Bot, q: CallbackQuery, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = q.from.id.0 as i64;
    if !state.config.is_admin(user_id as u64) { return Ok(()); }

    let data = match q.data.clone() {
        Some(d) => d,
        None => return Ok(()),
    };

    if data == "menu_main" {
        let (content, kb) = ui::build_main_menu(state.clone(), user_id).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data == "menu_skin_settings" {
        let (content, kb) = ui::build_skin_selector_menu();
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("set_skin_") {
        let theme_id: usize = data.strip_prefix("set_skin_").unwrap().parse().unwrap_or(0);
        let mut session = state.get_session(user_id).await;
        session.progress_bar_theme = theme_id;
        state.save_session(user_id, session).await;

        let ok_text = match theme_id {
            1 => "✅ 皮肤已成功切换为：彩色水果 🟩🟩⬜⬜",
            2 => "✅ 皮肤已成功切换为：简约细线 ━━──",
            _ => "✅ 皮肤已成功切换为：科幻方块 ▰▰▱▱",
        };
        bot.answer_callback_query(q.id).text(ok_text).show_alert(true).await?;
        
        let (content, kb) = ui::build_main_menu(state.clone(), user_id).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("init_") {
        let action = data.strip_prefix("init_").unwrap();
        let mut session = state.get_session(user_id).await;
        session.current_dir = state.config.base_dir.clone();
        session.clear_all_selected();
        state.save_session(user_id, session).await;
        
        let (content, kb) = ui::build_file_selector(state.clone(), user_id, action, 0).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("menu_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let page: usize = parts[2].parse().unwrap_or(0);
        let (content, kb) = ui::build_file_selector(state.clone(), user_id, action, page).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("enterdir_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let idx: usize = parts[2].parse().unwrap_or(0);
        
        let mut session = state.get_session(user_id).await;
        if let Some(folder_name) = session.current_files.get(idx).cloned() {
            session.current_dir = session.current_dir.join(folder_name);
            session.clear_all_selected();
            state.save_session(user_id, session).await;
        }
        let (content, kb) = ui::build_file_selector(state.clone(), user_id, action, 0).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("updir_") {
        let action = data.strip_prefix("updir_").unwrap();
        let mut session = state.get_session(user_id).await;
        if session.current_dir != state.config.base_dir {
            if let Some(parent) = session.current_dir.parent() {
                session.current_dir = parent.to_path_buf();
                session.clear_all_selected();
                state.save_session(user_id, session).await;
            }
        }
        let (content, kb) = ui::build_file_selector(state.clone(), user_id, action, 0).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("toggle_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let idx: usize = parts[2].parse().unwrap_or(0);
        let page: usize = parts[3].parse().unwrap_or(0);

        let mut session = state.get_session(user_id).await;
        let selected = session.get_selected(action);
        if selected.contains(&idx) { selected.remove(&idx); } else { selected.insert(idx); }
        state.save_session(user_id, session).await;

        let (content, kb) = ui::build_file_selector(state.clone(), user_id, action, page).await;
        if let Some(m) = q.message { bot.edit_message_text(m.chat().id, m.id(), content).parse_mode(ParseMode::Html).reply_markup(kb).await?; }
    } else if data.starts_with("execsingle_") {
        let parts: Vec<&str> = data.split('_').collect();
        let action = parts[1];
        let idx: usize = parts[2].parse().unwrap_or(0);

        let session = state.get_session(user_id).await;
        if let Some(filename) = session.current_files.get(idx) {
            let path = session.current_dir.join(filename);
            
            let mut active_lock = state.active_task.lock().await;
            if active_lock.is_some() && action != "browse" {
                bot.answer_callback_query(q.id).text("⚠️ 有任务正在运行中，请先 /stop 终止！").show_alert(true).await?;
                return Ok(());
            }

            if action == "browse" {
                let size = media_utils::get_formatted_file_size(&path);
                let dur = media_utils::get_video_duration(&path, state.config.ffprobe_timeout_seconds).await;
                bot.answer_callback_query(q.id).text(format!("📄 {}\n大小: {}\n时长: {}", filename, size, media_utils::format_duration(dur))).show_alert(true).await?;
            } else if action == "stream" {
                let notify = Arc::new(Notify::new());
                let flag = Arc::new(AtomicBool::new(false));
                *active_lock = Some(ActiveTask { name: format!("推流: {}", filename), cancel_flag: flag.clone(), cancel_notify: notify.clone() });
                if let Some(MaybeInaccessibleMessage::Regular(msg)) = q.message.clone() {
                    tokio::spawn(actions::action_stream(bot.clone(), msg, path, state.clone(), flag, notify, user_id));
                }
            } else if action == "youtube" {
                let notify = Arc::new(Notify::new());
                let flag = Arc::new(AtomicBool::new(false));
                if let Some(MaybeInaccessibleMessage::Regular(msg)) = q.message.clone() {
                    let progress_msg = bot.send_message(msg.chat.id, "正在初始化 YouTube...").await?;
                    tokio::spawn(youtube::start_youtube_upload(bot.clone(), progress_msg, path, state.clone(), flag, notify, user_id));
                }
            }
        }
    } else if data.starts_with("execbatch_") {
        let action = data.strip_prefix("execbatch_").unwrap();
        let mut session = state.get_session(user_id).await;
        let selected = session.get_selected(action).clone();
        
        let mut target_files = Vec::new();
        for idx in selected {
            if let Some(name) = session.current_files.get(idx) {
                target_files.push(session.current_dir.join(name));
            }
        }

        if target_files.is_empty() {
            bot.answer_callback_query(q.id).text("❌ 请勾选至少一个视频！").show_alert(true).await?;
            return Ok(());
        }

        let mut active_lock = state.active_task.lock().await;
        if active_lock.is_some() {
            bot.answer_callback_query(q.id).text("⚠️ 后台正在执行其他独占任务！").show_alert(true).await?;
            return Ok(());
        }

        let notify = Arc::new(Notify::new());
        let flag = Arc::new(AtomicBool::new(false));
        *active_lock = Some(ActiveTask { name: format!("批量操作: {}", action), cancel_flag: flag.clone(), cancel_notify: notify.clone() });

        if let Some(MaybeInaccessibleMessage::Regular(msg)) = q.message.clone() {
            if action == "concat" {
                tokio::spawn(actions::action_concat(bot.clone(), msg, target_files, state.clone(), flag, notify));
            } else if action == "convert" {
                tokio::spawn(actions::action_convert(bot.clone(), msg, target_files, state.clone(), flag, notify, user_id));
            } else if action == "delete" {
                tokio::spawn(actions::action_delete(bot.clone(), msg, target_files, state.clone()));
            }
        }
    }

    Ok(())
}