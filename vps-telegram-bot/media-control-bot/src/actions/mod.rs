//! 业务逻辑层（Actions）
//!
//! 每个子模块对应一种用户操作，负责协调底层基础设施（FFmpeg、YouTube API、文件系统）
//! 完成具体的业务流程，并通过有界超时的 Telegram 消息接口向用户反馈进度和结果。

pub mod browse;
pub mod concat;
pub mod convert;
pub mod delete;
pub mod stream;
pub mod youtube;

use std::time::Duration;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId, ParseMode};

use crate::errors::AppError;

/// 运行期非关键进度消息编辑超时（避免 Telegram 半开连接阻塞任务主循环或 `/stop`）。
pub const TELEGRAM_PROGRESS_EDIT_TIMEOUT: Duration = Duration::from_secs(8);

/// 任务收尾/取消最终状态通知超时（确保临时文件与子进程清理后不会无限期卡在网络通知上）。
pub const TELEGRAM_FINAL_EDIT_TIMEOUT: Duration = Duration::from_secs(6);

/// 发送初始任务卡片消息超时。
pub const TELEGRAM_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// 发送带取消监听与有界超时的 HTML 消息。
pub async fn send_html_cancellable(
    bot: &Bot,
    chat_id: ChatId,
    html: String,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
) -> Result<Message> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }
    let mut cancel_watch = cancel_rx.clone();
    let send_fut = bot.send_message(chat_id, html).parse_mode(ParseMode::Html);
    tokio::select! {
        res = tokio::time::timeout(TELEGRAM_SEND_TIMEOUT, send_fut) => {
            match res {
                Ok(Ok(msg)) => Ok(msg),
                Ok(Err(e)) => Err(AppError::TelegramError(e.to_string()).into()),
                Err(_) => Err(AppError::TelegramError("Telegram send_message timed out".to_string()).into()),
            }
        }
        _ = cancel_watch.changed() => Err(AppError::Cancelled.into()),
    }
}

/// 发送带有限超时的 HTML 消息（用于收尾通知，忽略网络错误不阻塞退出）。
pub async fn send_html_bounded(bot: &Bot, chat_id: ChatId, html: String) -> Option<Message> {
    let send_fut = bot.send_message(chat_id, html).parse_mode(ParseMode::Html);
    match tokio::time::timeout(TELEGRAM_SEND_TIMEOUT, send_fut).await {
        Ok(Ok(msg)) => Some(msg),
        _ => None,
    }
}

/// 编辑运行期进度 HTML 消息（同时监听取消信号与有界超时，收到 `/stop` 时立即放弃本次非关键刷新）。
pub async fn edit_html_cancellable(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: MessageId,
    html: String,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
) {
    if *cancel_rx.borrow() {
        return;
    }
    let mut cancel_watch = cancel_rx.clone();
    let edit_fut = bot
        .edit_message_text(chat_id, msg_id, html)
        .parse_mode(ParseMode::Html);
    tokio::select! {
        _ = tokio::time::timeout(TELEGRAM_PROGRESS_EDIT_TIMEOUT, edit_fut) => {}
        _ = cancel_watch.changed() => {}
    }
}

/// 编辑最终结果或取消确认 HTML 消息（仅受有限超时保护，确保在已取消状态下仍能尽力发出终态通知但绝不挂死）。
pub async fn edit_html_bounded(bot: &Bot, chat_id: ChatId, msg_id: MessageId, html: String) {
    let edit_fut = bot
        .edit_message_text(chat_id, msg_id, html)
        .parse_mode(ParseMode::Html);
    let _ = tokio::time::timeout(TELEGRAM_FINAL_EDIT_TIMEOUT, edit_fut).await;
}
