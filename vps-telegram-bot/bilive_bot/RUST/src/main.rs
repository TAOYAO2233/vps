mod api;
mod callbacks;
mod config;
mod error;
mod handlers;
mod models;
mod monitor;
mod speed;
mod state;
mod ui;

use std::sync::Arc;
use teloxide::prelude::*;
use crate::{config::Config, state::AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();

    let config = Config::from_env()?;
    let bot = Bot::new(config.token());
    let state = Arc::new(AppState::new(config)?);
    log::info!("正在启动 Bililive-go 机器人...");

    let monitor = tokio::spawn(monitor::run(bot.clone(), Arc::clone(&state)));
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handlers::handle_message))
        .branch(Update::filter_callback_query().endpoint(handlers::handle_callback_query));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    monitor.abort();
    let _ = monitor.await;
    Ok(())
}
