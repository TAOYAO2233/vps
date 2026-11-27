use std::sync::Arc;
use teloxide::{prelude::*, types::InlineKeyboardMarkup};
use crate::{callbacks::Callback, state::AppState, ui::panels::{main_panel, detail_panel, stream_panel, detail_buttons}};

pub(crate) async fn handle_message(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    let user = match msg.from() {
        Some(user) => user,
        None => return Ok(()),
    };
    if !state.config.is_admin(user.id.0) {
        bot.send_message(msg.chat.id, "⛔ 权限拒绝：您未被授权操作此机器人。").await?;
        return Ok(());
    }
    let text = match msg.text() {
        Some(text) => text.trim(),
        None => return Ok(()),
    };
    if text == "/start" || text.starts_with("/start@") {
        let (text, markup) = main_panel(&state).await;
        bot.send_message(msg.chat.id, text).reply_markup(markup).await?;
    } else if text.starts_with("http://") || text.starts_with("https://") {
        let sent = bot.send_message(msg.chat.id, "正在解析并添加任务…").await?;
        let result = state.api.add_live(text).await;
        let (panel, markup) = main_panel(&state).await;
        let outcome = match result {
            Ok(()) => "任务添加成功。".into(),
            Err(error) => format!("添加操作未完全成功：{}", error),
        };
        edit_panel(&bot, &sent, format!("{}\n\n{}", outcome, panel), markup).await?;
    } else {
        bot.send_message(msg.chat.id, "请发送有效的直播间 URL。").await?;
    }
    Ok(())
}

// Telegram rejects identical edits; compare both content and keyboard first.
async fn edit_panel(bot: &Bot, msg: &Message, text: String, markup: InlineKeyboardMarkup) -> ResponseResult<()> {
    if msg.text() == Some(text.as_str()) && msg.reply_markup() == Some(&markup) {
        return Ok(());
    }
    match bot.edit_message_text(msg.chat.id, msg.id, text).reply_markup(markup).await {
        // Concurrent clicks can still race after the comparison above.
        Err(e) if e.to_string().to_ascii_lowercase().contains("message is not modified") => Ok(()),
        result => result.map(|_| ()),
    }
}

pub(crate) async fn handle_callback_query(bot: Bot, q: CallbackQuery, state: Arc<AppState>) -> ResponseResult<()> {
    if !state.config.is_admin(q.from.id.0) {
        bot.answer_callback_query(q.id).text("⛔ 权限拒绝").await?;
        return Ok(());
    }
    bot.answer_callback_query(q.id).await?;
    let data = match q.data {
        Some(data) => data,
        None => return Ok(()),
    };
    let msg = match q.message {
        Some(message) => message,
        None => return Ok(()),
    };
    let callback = match Callback::parse(&data) {
        Some(callback) => callback,
        None => return Ok(()),
    };
    let panel = match callback {
        Callback::Main => main_panel(&state).await,
        Callback::Detail(id) => detail_panel(&state, id).await,
        Callback::Stream { id, page } => stream_panel(&state, id, page).await,
        Callback::Control { action, id } => {
            let outcome = state.api.control_task(action, id).await;
            state.speed.lock().await.reset(id);
            let (text, markup) = detail_panel(&state, id).await;
            let text = match outcome {
                Ok(()) => text,
                Err(error) => format!("操作未完全成功：{}\n\n{}", error, text),
            };
            (text, markup)
        }
        Callback::Delete(id) => {
            let outcome = state.api.delete_live(id).await;
            state.speed.lock().await.reset(id);
            match outcome {
                Ok(()) => {
                    let (text, markup) = main_panel(&state).await;
                    (format!("任务已删除并同步配置。\n\n{}", text), markup)
                }
                Err(error) => (format!("删除操作未完全成功：{}", error), detail_buttons(id)),
            }
        }
    };
    edit_panel(&bot, &msg, panel.0, panel.1).await
}
