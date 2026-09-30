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
mod storage;
mod ui;
mod utils;
mod youtube;

use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

use crate::bot::router::build_dispatcher;
use crate::config::Config;
use crate::core::state::AppState;
use crate::utils::logger::init_logger_from_env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 先初始化日志系统（内部会加载 .env），保证后续启动日志能被输出
    init_logger_from_env()?;

    // 2. 加载配置（Config::load 内部同样会加载 .env，重复调用是幂等的）
    let config = match Config::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Configuration error: {e:#}");
            std::process::exit(1);
        }
    };

    info!("Starting Media Control Bot v{}", env!("CARGO_PKG_VERSION"));
    info!("Base directory: {}", config.base_dir.display());
    info!(
        "YouTube max concurrent uploads: {}",
        config.youtube_max_concurrent_uploads
    );
    info!(
        "YouTube upload chunk hint: {} bytes (实际分块由依赖库控制，仅作预留)",
        config.youtube_chunk_bytes()
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
    let mut dispatcher = build_dispatcher(tg_bot, Arc::clone(&app_state), Arc::clone(&config));

    // 6. 注册优雅停机信号 (Ctrl+C)
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
            // 通知所有任务退出：独占任务会在下一次轮询时终止 FFmpeg 子进程，
            // YouTube 上传会在 chunk 边界退出，kill_on_drop 作为兜底。
            app_state.write().await.cancel_all();
            tokio::time::sleep(Duration::from_millis(300)).await;
            info!("Shutdown complete.");
        }
    }

    Ok(())
}
