//! VPS 媒体控制 Bot 主程序入口。
//!
//! 企业级 Rust 架构特性：
//! - 全异步 Tokio 运行时
//! - Tracing 结构化日志
//! - Teloxide Telegram Bot 框架
//! - Anyhow + ThisError 统一错误处理
//! - `Arc<RwLock<AppState>>` 全局状态管理

mod actions;
mod bot;
mod config;
mod core;
mod errors;
mod media;
mod rtmp;
mod storage;
mod ui;
mod utils;
mod youtube;

use std::sync::Arc;
use tracing::{error, info};

use crate::bot::router::build_dispatcher;
use crate::config::Config;
use crate::core::state::AppState;
use crate::utils::logger::init_logger;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 加载配置（Config::load 内部已处理 .env 加载）
    let config = match Config::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Configuration error: {e:#}");
            std::process::exit(1);
        }
    };

    // 2. 初始化日志系统（必须在 Config::load 之后，因为 Config::load 会调用 tracing）
    init_logger(&config.log_format)?;
    info!("Starting Media Control Bot v{}", env!("CARGO_PKG_VERSION"));
    info!("Base directory: {}", config.base_dir.display());
    info!(
        "YouTube max concurrent uploads: {}",
        config.youtube_max_concurrent_uploads
    );

    // 3. 初始化全局状态
    let app_state = AppState::new(
        config.base_dir.clone(),
        config.youtube_max_concurrent_uploads,
    )
    .into_shared();

    // 4. 初始化 Teloxide Bot
    let tg_bot = teloxide::Bot::new(&config.bot_token);

    // 5. 构建 Dispatcher（dptree 路由树）
    info!("Setting up Telegram dispatcher...");
    let mut dispatcher = build_dispatcher(tg_bot, app_state, Arc::clone(&config));

    // 6. 注册优雅停机信号 (Ctrl+C / SIGTERM)
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        info!("Received Ctrl+C, shutting down gracefully...");
    };

    // 7. 运行 Bot（同时监听停机信号）
    info!("Bot is now running! Admin ID: {}", config.admin_id);
    tokio::select! {
        _ = dispatcher.dispatch() => {
            error!("Dispatcher exited unexpectedly");
        }
        _ = ctrl_c => {
            info!("Shutdown complete.");
        }
    }

    Ok(())
}
