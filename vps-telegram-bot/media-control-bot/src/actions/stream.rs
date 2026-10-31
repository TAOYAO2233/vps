//! RTMP 无损推流操作控制器。
//!
//! 统一通过 [`crate::rtmp::stream::RtmpStreamer`] 执行推流：
//! - 推流前二次校验 `PathGuard` 并使用带超时与取消监听的 `probe_media_cancellable` 检查音视频流与码率；
//! - 允许总时长未知（`duration <= 0`）的录播文件正常推流，展示已推流时长与已发送数据量；
//! - 维护读写与无进展看门狗超时，在取消或异常时优先向 FFmpeg 发送 `q` 优雅退出并等待进程回收（超时兜底 `SIGKILL` 并记录诊断状态）；
//! - 本地退出码为 `0` 时明确表述为“本地文件已发送完毕”，绝不在日志或消息中泄露 `rtmp_url` 推流密钥。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{edit_html_bounded, edit_html_cancellable, send_html_bounded, send_html_cancellable};
use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::media::ffprobe::probe_media_cancellable;
use crate::rtmp::stream::{classify_rtmp_failure, validate_rtmp_url, RtmpStreamer};
use crate::storage::filesystem::{ensure_stable_files, format_file_size, format_file_size_bytes};
use crate::storage::path::PathGuard;
use crate::utils::format::{escape_html, format_duration, format_elapsed};
use crate::utils::redact::{redact_secrets, sanitize_user_error};

/// 看门狗检查间隔。
const STREAM_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 启动 RTMP 推流独占任务。
pub async fn start_stream(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    file_path: PathBuf,
) -> Result<()> {
    if config.rtmp_url.trim().is_empty() {
        send_html_bounded(
            bot,
            msg.chat.id,
            "❌ <code>RTMP_URL</code> 未配置。请在 <code>.env</code> 中配置推流地址后重启服务。"
                .to_string(),
        )
        .await;
        return Ok(());
    }

    if let Err(e) = validate_rtmp_url(&config.rtmp_url) {
        send_html_bounded(
            bot,
            msg.chat.id,
            format!("❌ {}", escape_html(&sanitize_user_error(&e))),
        )
        .await;
        return Ok(());
    }

    let task_manager = TaskManager::new(Arc::clone(&state));
    let bot_clone = bot.clone();
    let msg_clone = msg.clone();
    let config_clone = Arc::clone(&config);

    task_manager
        .start_exclusive("RTMP 推流", move |task_id, cancel_rx| {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let path = file_path.clone();
            let cfg = Arc::clone(&config_clone);
            let state_inner = Arc::clone(&state);
            async move { do_stream(bot, msg, state_inner, cfg, path, task_id, cancel_rx).await }
        })
        .await?;

    Ok(())
}

/// 在独占任务内执行完整的推流生命周期。
async fn do_stream(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
    raw_file_path: PathBuf,
    task_id: u64,
    cancel_rx: watch::Receiver<bool>,
) -> Result<()> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }

    let monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    monitor.ensure_space_available(0).await?;

    let guard = PathGuard::new(config.base_dir.clone());
    let file_path = guard.assert_regular_file_no_symlink(&raw_file_path)?;
    ensure_stable_files(&[file_path.clone()], config.file_stable_seconds).await?;

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let file_size_bytes = std::fs::metadata(&file_path).map_or(0, |m| m.len());
    let size_str = format_file_size(&file_path);

    let streamer = RtmpStreamer::new(config.rtmp_url.clone(), config.rtmp_rw_timeout_secs)?;
    let safe_target = streamer.redacted_target();

    let progress_msg = send_html_cancellable(
        &bot,
        msg.chat.id,
        format!(
            "⏳ 正在预检推流文件: <code>{}</code> ({size_str})\n🎯 目标服务器: <code>{}</code>",
            escape_html(&filename),
            escape_html(&safe_target),
        ),
        &cancel_rx,
    )
    .await?;

    if *cancel_rx.borrow() {
        notify_stream_cancelled(&bot, &msg, &progress_msg, &filename).await;
        return Err(AppError::Cancelled.into());
    }

    // 带超时与取消监听探测媒体流与兼容性（若流不兼容 FLV 无损直推，直接友好提示并拒绝）
    let probe = match probe_media_cancellable(
        &file_path,
        config.ffprobe_timeout_secs,
        &cancel_rx,
    )
    .await
    {
        Ok(p) => p,
        Err(e) if is_cancelled_err(&e) => {
            notify_stream_cancelled(&bot, &msg, &progress_msg, &filename).await;
            return Err(e);
        }
        Err(e) => {
            edit_html_bounded(
                &bot,
                msg.chat.id,
                progress_msg.id,
                format!(
                    "❌ <b>推流预检失败</b>: <code>{}</code>\n{}",
                    escape_html(&filename),
                    escape_html(&sanitize_user_error(&e))
                ),
            )
            .await;
            return Err(e);
        }
    };

    // 计算估算码率（Mbps）与可选高码率警告
    let est_bitrate_mbps = probe
        .bit_rate_bps
        .map(|bps| bps as f64 / 1_000_000.0)
        .or_else(|| {
            if probe.duration_secs > 0.0 && file_size_bytes > 0 {
                Some((file_size_bytes as f64 * 8.0) / probe.duration_secs / 1_000_000.0)
            } else {
                None
            }
        });

    let bitrate_note = match est_bitrate_mbps {
        Some(mbps) if config.rtmp_warn_bitrate_mbps > 0.0 && mbps > config.rtmp_warn_bitrate_mbps => {
            format!(
                "\n📶 源片码率: <code>{mbps:.2} Mbps</code>\n⚠️ <b>提示</b>: 超过设定预警线 (<code>{:.1} Mbps</code>)，上行带宽不足时可能卡顿或断流",
                config.rtmp_warn_bitrate_mbps
            )
        }
        Some(mbps) => format!("\n📶 源片码率: <code>{mbps:.2} Mbps</code>"),
        None => String::new(),
    };

    if *cancel_rx.borrow() {
        notify_stream_cancelled(&bot, &msg, &progress_msg, &filename).await;
        return Err(AppError::Cancelled.into());
    }

    let mut process = match streamer.spawn_process(&file_path, &probe, &state).await {
        Ok(proc) => proc,
        Err(e) => {
            edit_html_bounded(
                &bot,
                msg.chat.id,
                progress_msg.id,
                format!(
                    "❌ <b>无法启动无损直推</b>: <code>{}</code>\n{}",
                    escape_html(&filename),
                    escape_html(&sanitize_user_error(&e))
                ),
            )
            .await;
            return Err(e);
        }
    };

    let duration = probe.duration_secs;
    let progress_bar = ProgressBar::default();
    let started_at = Instant::now();
    let mut last_activity_at = Instant::now();
    let stall_timeout = Duration::from_secs(config.rtmp_rw_timeout_secs.saturating_add(15));
    let mut timed_out_by_watchdog = false;
    let mut cancel_watch = cancel_rx.clone();

    loop {
        if *cancel_watch.borrow() {
            if process.stop_and_wait().await {
                state.write().await.record_exclusive_forced_kill(task_id);
            }
            break;
        }

        if last_activity_at.elapsed() >= stall_timeout {
            warn!(
                filename = %filename,
                target = %safe_target,
                stall_secs = stall_timeout.as_secs(),
                "RTMP stream watchdog triggered due to prolonged inactivity; stopping ffmpeg"
            );
            timed_out_by_watchdog = true;
            if process.stop_and_wait().await {
                state.write().await.record_exclusive_forced_kill(task_id);
            }
            break;
        }

        tokio::select! {
            tick_res = process.next_tick() => {
                match tick_res {
                    Ok(Some(tick)) => {
                        last_activity_at = Instant::now();
                        let sent_str = format_file_size_bytes(tick.total_size_bytes);
                        let status_html = if let Some(percent) = tick.percent {
                            let bar = progress_bar.render(percent);
                            format!(
                                "📡 <b>RTMP 无损直推中</b>: <code>{}</code>\n\
                                 🎯 目标: <code>{}</code>{bitrate_note}\n\n\
                                 <code>{}</code>\n\
                                 ⏱️ 进度: <code>{}</code> / <code>{}</code> | 已发送 <code>{sent_str}</code>",
                                escape_html(&filename),
                                escape_html(&safe_target),
                                bar,
                                format_duration(tick.out_time_secs),
                                format_duration(duration),
                            )
                        } else {
                            // 总时长未知（duration <= 0）时不报错中断，展示已推流媒体时长与运行时间
                            format!(
                                "📡 <b>RTMP 无损直推中（源片未记录总时长）</b>\n\
                                 📄 文件: <code>{}</code> ({size_str})\n\
                                 🎯 目标: <code>{}</code>{bitrate_note}\n\n\
                                 ⏱️ 已推流媒体时长: <code>{}</code>（运行 <code>{}</code>）\n\
                                 📦 已发送数据量: <code>{sent_str}</code>",
                                escape_html(&filename),
                                escape_html(&safe_target),
                                format_duration(tick.out_time_secs.max(1.0)),
                                format_elapsed(started_at.elapsed().as_secs_f64()),
                            )
                        };

                        // 带取消监听与有界超时刷新消息，避免半开连接阻塞推流或 /stop
                        edit_html_cancellable(
                            &bot,
                            msg.chat.id,
                            progress_msg.id,
                            status_html,
                            &cancel_rx,
                        )
                        .await;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            _ = cancel_watch.changed() => {
                if process.stop_and_wait().await {
                    state.write().await.record_exclusive_forced_kill(task_id);
                }
                break;
            }
            _ = tokio::time::sleep(STREAM_POLL_INTERVAL) => {}
        }
    }

    let (exit_code, stderr_summary, forced_after) = process.wait_with_summary(&cancel_rx).await?;
    if forced_after {
        state.write().await.record_exclusive_forced_kill(task_id);
    }

    if *cancel_rx.borrow() {
        notify_stream_cancelled(&bot, &msg, &progress_msg, &filename).await;
        return Err(AppError::Cancelled.into());
    }

    if !timed_out_by_watchdog && exit_code == Some(0) {
        edit_html_bounded(
            &bot,
            msg.chat.id,
            progress_msg.id,
            format!(
                "✅ <b>本地文件已发送完毕</b>\n\
                 ━━━━━━━━━━━━\n\
                 📄 文件: <code>{}</code>\n\
                 🎯 目标: <code>{}</code>\n\
                 ⏱️ 总耗时: <code>{}</code>\n\n\
                 ℹ️ 说明：FFmpeg 已完成本地媒体流发送并正常退出；目标平台直播间画面状态请以平台后台为准。",
                escape_html(&filename),
                escape_html(&safe_target),
                format_elapsed(started_at.elapsed().as_secs_f64()),
            ),
        )
        .await;
        info!(filename = %filename, target = %safe_target, "Local RTMP stream transmission finished");
        return Ok(());
    }

    let code = exit_code.unwrap_or(-1);
    let category = classify_rtmp_failure(&stderr_summary, timed_out_by_watchdog);
    let safe_diag = redact_secrets(&stderr_summary);
    let mut short_diag = String::new();
    for (idx, ch) in safe_diag.chars().enumerate() {
        if idx >= 320 {
            short_diag.push('…');
            break;
        }
        short_diag.push(ch);
    }

    let diag_block = if short_diag.trim().is_empty() {
        String::new()
    } else {
        format!("\n🧾 摘要诊断:\n<pre>{}</pre>", escape_html(short_diag.trim()))
    };

    edit_html_bounded(
        &bot,
        msg.chat.id,
        progress_msg.id,
        format!(
            "❌ <b>RTMP 推流异常中断</b>\n\
             ━━━━━━━━━━━━\n\
             📄 文件: <code>{}</code>\n\
             🎯 目标: <code>{}</code>\n\
             🔢 退出码: <code>{code}</code>\n\
             💡 诊断说明: {}{diag_block}",
            escape_html(&filename),
            escape_html(&safe_target),
            escape_html(category.user_hint()),
        ),
    )
    .await;

    Err(AppError::RtmpStreamFailed { exit_code: code }.into())
}

fn is_cancelled_err(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AppError>() == Some(&AppError::Cancelled)
}

async fn notify_stream_cancelled(
    bot: &Bot,
    msg: &Message,
    progress_msg: &Message,
    filename: &str,
) {
    edit_html_bounded(
        bot,
        msg.chat.id,
        progress_msg.id,
        format!("🛑 <b>推流已手动终止</b>:\n<code>{}</code>", escape_html(filename)),
    )
    .await;
}
