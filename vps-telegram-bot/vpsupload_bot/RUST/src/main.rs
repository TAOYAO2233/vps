mod config;
mod media_utils;
mod task_manager;
mod actions;
mod ui;
mod youtube_upload;
mod handlers;

use crate::config::Config;
use crate::handlers::{handle_callback, handle_command};
use crate::task_manager::{load_persisted_queue, AppState};
use std::sync::Arc;
use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("系统初始化中，正在加载环境与依赖配置...");

    let config = Config::from_env();
    let state = AppState::new(config.youtube_max_concurrent_uploads);

    let persisted_uploads = load_persisted_queue(&config.youtube_upload_queue_file);
    if !persisted_uploads.is_empty() {
        tracing::info!("发现 {} 个历史 YouTube 上传队列记录，已加载并标记待恢复状态。", persisted_uploads.len());
        let mut uploads = state.youtube_uploads.lock().await;
        *uploads = persisted_uploads;
    }

    let bot = Bot::new(&config.bot_token);
    tracing::info!("Telegram Bot 鉴权完毕，开始监听全局回调与指令 (BASE_DIR: {:?})...", config.base_dir);

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<String>()
                .endpoint(|bot: Bot, msg: Message, config: Arc<Config>, state: Arc<AppState>| async move {
                    if let Some(text) = msg.text() {
                        let cmd = text.split_whitespace().next().unwrap_or("").to_string();
                        let _ = handle_command(bot, msg, cmd, config, state).await;
                    }
                    Ok(())
                }),
        )
        .branch(
            Update::filter_callback_query()
                .endpoint(|bot: Bot, q: CallbackQuery, config: Arc<Config>, state: Arc<AppState>| async move {
                    let _ = handle_callback(bot, q, config, state).await;
                    Ok(())
                }),
        );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config, state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}