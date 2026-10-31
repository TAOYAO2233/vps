//! VPS 媒体控制 Bot 主程序入口。
//!
//! 核心职责：
//! - 初始化统一流式脱敏日志器（`RedactingMakeWriter`），确保全生命周期无密钥泄露；
//! - 加载并校验 [`Config`]，输出安全的单实例与资源配置诊断摘要；
//! - 启动时立即执行一次 `/storage512` 挂载与实时容量检查，并启动后台定时磁盘监测任务（默认每 5 分钟）；
//! - 在显式启用恢复模式时按策略恢复未完成 YouTube 上传（保守默认 `none` 下不自动启动）；
//! - 同时监听 `SIGINT` (`Ctrl+C`) 与 Unix `SIGTERM`（适配 `systemd` 停止），优雅取消所有任务并等待子进程回收。

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
use std::time::Duration;

use tracing::{error, info, warn};

use crate::bot::router::build_dispatcher;
use crate::config::Config;
use crate::core::state::AppState;
use crate::core::task_manager::TaskManager;
use crate::storage::trash::TrashStore;
use crate::utils::logger::init_logger_from_env;
use crate::utils::redact::{flush_global_stdout, redact_rtmp_target, redact_secrets};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 初始化统一脱敏日志系统（内部预载环境变量敏感项）
    init_logger_from_env()?;

    // 2. 加载并校验配置
    let config = match Config::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            let safe_err = redact_secrets(&format!("{e:#}"));
            error!("Configuration validation failed: {safe_err}");
            flush_global_stdout();
            std::process::exit(1);
        }
    };

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        base_dir = %config.base_dir.display(),
        storage_mount = %config.storage_mount_point.display(),
        disk_check_interval_secs = config.disk_check_interval_secs,
        rtmp_target = %redact_rtmp_target(&config.rtmp_url),
        yt_concurrent = config.youtube_max_concurrent_uploads,
        yt_queue_limit = config.youtube_max_queue_size,
        yt_chunk_mb = config.youtube_upload_chunk_mb,
        yt_recovery_mode = config.youtube_recovery_mode.as_str(),
        merge_allow_auto_trim = config.merge_allow_auto_trim,
        merge_max_loss_secs = config.merge_max_loss_seconds,
        "Starting Media Control Bot (lossless copy-only mode)"
    );
    info!(
        "Single-instance notice: ensure legacy Python videobot.service or duplicate instances sharing this BOT_TOKEN are stopped to avoid Telegram 409 Conflict."
    );

    // 3. 初始化全局状态与 Teloxide 客户端
    let app_state = AppState::from_config(&config).into_shared();
    let tg_bot = teloxide::Bot::new(&config.bot_token);

    // 4. 启动时立即执行一次 /storage512 挂载与容量监测（含超阈值报警）
    let disk_monitor = {
        let s = app_state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    let initial_disk = disk_monitor.check_and_notify(&tg_bot).await;
    info!(
        disk_status = %initial_disk.format_menu_line(),
        "Initial storage health check completed"
    );
    if disk_monitor.ensure_space_available(0).await.is_ok() {
        let trash = TrashStore::new(config.base_dir.clone(), config.trash_retention_days);
        match tokio::task::spawn_blocking(move || trash.cleanup_expired()).await {
            Ok(Ok(count)) if count > 0 => info!(count, "Expired trash files removed"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(error = %redact_secrets(&e.to_string()), "Trash cleanup failed"),
            Err(e) => warn!(error = %e, "Trash cleanup worker failed"),
        }
    }

    // 5. 启动后台周期性磁盘监测任务（默认每 300 秒检查一次）
    let (bg_stop_tx, mut bg_stop_rx) = tokio::sync::watch::channel(false);
    let bg_monitor = Arc::clone(&disk_monitor);
    let bg_bot = tg_bot.clone();
    let bg_base_dir = config.base_dir.clone();
    let bg_trash_days = config.trash_retention_days;
    let check_interval = Duration::from_secs(config.disk_check_interval_secs.max(10));
    let disk_bg_handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(check_interval);
        let mut last_trash_cleanup = tokio::time::Instant::now();
        // 跳过首次立即触发（上方已完成启动检查）
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let _ = bg_monitor.check_and_notify(&bg_bot).await;
                    if last_trash_cleanup.elapsed() >= Duration::from_secs(86_400)
                        && bg_monitor.ensure_space_available(0).await.is_ok()
                    {
                        let trash = TrashStore::new(bg_base_dir.clone(), bg_trash_days);
                        match tokio::task::spawn_blocking(move || trash.cleanup_expired()).await {
                            Ok(Ok(count)) if count > 0 => info!(count, "Expired trash files removed"),
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => warn!(error = %redact_secrets(&e.to_string()), "Trash cleanup failed"),
                            Err(e) => warn!(error = %e, "Trash cleanup worker failed"),
                        }
                        last_trash_cleanup = tokio::time::Instant::now();
                    }
                }
                _ = bg_stop_rx.changed() => {
                    break;
                }
            }
        }
    });

    // 6. 可选恢复未完成 YouTube 上传任务（保守默认 YOUTUBE_RECOVERY_MODE=none 时为空操作）
    match actions::youtube::recover_pending_uploads(
        &tg_bot,
        Arc::clone(&app_state),
        Arc::clone(&config),
    )
    .await
    {
        Ok(count) if count > 0 => {
            info!(recovered_tasks = count, "Recovered pending YouTube upload tasks");
        }
        Ok(_) => {}
        Err(e) => {
            warn!(
                error = %redact_secrets(&e.to_string()),
                "Failed to recover pending YouTube uploads (non-fatal)"
            );
        }
    }

    // 7. 构建 Dispatcher 并监听 SIGINT / SIGTERM 优雅停机
    let mut dispatcher = build_dispatcher(tg_bot, Arc::clone(&app_state), Arc::clone(&config));
    info!(admin_id = config.admin_id, "Telegram Bot dispatcher running");

    tokio::select! {
        _ = dispatcher.dispatch() => {
            error!("Telegram dispatcher exited unexpectedly; cleaning up active tasks...");
        }
        sig_name = wait_for_shutdown_signal() => {
            info!(signal = sig_name, "Shutdown signal received; stopping tasks gracefully...");
        }
    }

    let _ = bg_stop_tx.send(true);
    let _ = disk_bg_handle.await;

    let task_manager = TaskManager::new(Arc::clone(&app_state));
    task_manager
        .cancel_and_wait_all(Duration::from_secs(5))
        .await;

    info!("Graceful shutdown complete.");
    flush_global_stdout();
    Ok(())
}

/// 跨平台监听停机信号：同时支持 `Ctrl+C` (`SIGINT`) 与 Unix `SIGTERM`（`systemctl stop`）。
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = sigterm.recv() => "SIGTERM",
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}
