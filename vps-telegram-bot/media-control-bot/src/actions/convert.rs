//! 批量视频无损转封装 MP4 操作（严格禁止重编码）。
//!
//! 关键工程保证：
//! - 全程仅使用 `-c copy -movflags +faststart` 流复制重封装，绝不调用任何音视频编码器；
//! - 转换前校验 `PathGuard` 与 `/storage512` 剩余磁盘空间；
//! - 先写入目标媒体目录下的独立临时目录 [`TempWorkDir`]，经 `probe_media_cancellable` 校验容器与音视频流完整后，
//!   再通过硬链接 [`publish_temp_file_no_overwrite`] 原子发布，绝不覆盖任何已有文件、绝不残留半截输出；
//! - 全程响应任务专属 `cancel_rx`（FFprobe、FFmpeg 优雅退出 `q` + 超时兜底、Telegram 有界超时通知），
//!   取消时仅销毁当前未完成文件临时目录，完整保留用户原视频与前序已原子发布的有效成品。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{edit_html_bounded, edit_html_cancellable, send_html_cancellable};
use crate::config::Config;
use crate::core::state::{ActionType, SharedState};
use crate::core::task_manager::TaskManager;
use crate::core::ProgressBar;
use crate::errors::AppError;
use crate::media::ffmpeg::{build_remux_to_mp4_args, FfmpegProcess};
use crate::media::ffprobe::probe_media_cancellable;
use crate::storage::disk::estimate_convert_required_bytes;
use crate::storage::filesystem::{ensure_stable_files, remove_if_exists, TempWorkDir};
use crate::storage::path::{publish_temp_file_no_overwrite, unique_path, PathGuard};
use crate::utils::format::{escape_html, format_duration};
use crate::utils::redact::{redact_secrets, sanitize_user_error};

/// 启动批量无损转封装独占任务。
pub async fn start_convert(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    files: Vec<PathBuf>,
) -> Result<()> {
    if files.is_empty() {
        return Err(AppError::invalid_argument("请先选择至少一个待转封装文件。").into());
    }

    let task_manager = TaskManager::new(Arc::clone(&state));
    let bot_clone = bot.clone();
    let msg_clone = msg.clone();
    let cfg_clone = Arc::clone(&config);
    let state_for_clear = Arc::clone(&state);

    task_manager
        .start_exclusive("批量无损转封装 MP4", move |task_id, cancel_rx| {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let state_inner = Arc::clone(&state);
            let cfg = Arc::clone(&cfg_clone);
            async move { do_convert(bot, msg, state_inner, cfg, files, task_id, cancel_rx).await }
        })
        .await?;

    {
        let mut s = state_for_clear.write().await;
        s.clear_selections(&ActionType::Convert);
    }

    Ok(())
}

/// 在独占任务内执行批量无损转封装。
async fn do_convert(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
    raw_files: Vec<PathBuf>,
    task_id: u64,
    cancel_rx: watch::Receiver<bool>,
) -> Result<()> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }

    let guard = PathGuard::new(config.base_dir.clone());
    let mut verified_files = Vec::new();
    let mut total_input_bytes: u64 = 0;

    for raw in &raw_files {
        let valid = guard.assert_regular_file_no_symlink(raw)?;
        let sz = std::fs::metadata(&valid).map_or(0, |m| m.len());
        total_input_bytes = total_input_bytes.saturating_add(sz);
        verified_files.push(valid);
    }

    ensure_stable_files(&verified_files, config.file_stable_seconds).await?;
    let total = verified_files.len();
    state
        .write()
        .await
        .record_exclusive_converted_progress(task_id, 0, total);

    let progress_msg = send_html_cancellable(
        &bot,
        msg.chat.id,
        format!("🔄 正在预检磁盘空间并准备无损转封装 <code>{total}</code> 个文件..."),
        &cancel_rx,
    )
    .await?;

    let disk_monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    let required_bytes = estimate_convert_required_bytes(total_input_bytes);
    if let Err(e) = disk_monitor.ensure_space_available(required_bytes).await {
        edit_html_bounded(
            &bot,
            msg.chat.id,
            progress_msg.id,
            format!(
                "❌ <b>转封装预检未通过</b>:\n{}",
                escape_html(&sanitize_user_error(&e))
            ),
        )
        .await;
        return Err(e);
    }

    let progress_bar = ProgressBar::default();
    let mut success_count = 0usize;
    let mut failed_notes: Vec<String> = Vec::new();

    for (idx, file_path) in verified_files.iter().enumerate() {
        if *cancel_rx.borrow() {
            notify_convert_cancelled(&bot, &msg, &progress_msg, success_count, total).await;
            return Err(AppError::Cancelled.into());
        }

        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let parent = match file_path.parent() {
            Some(p) => guard.assert_inside(p)?,
            None => continue,
        };

        let src_probe = match probe_media_cancellable(
            file_path,
            config.ffprobe_timeout_secs,
            &cancel_rx,
        )
        .await
        {
            Ok(p) => p,
            Err(e) if is_cancelled_err(&e) => {
                notify_convert_cancelled(&bot, &msg, &progress_msg, success_count, total).await;
                return Err(e);
            }
            Err(_) => {
                failed_notes.push(format!("{filename}: FFprobe 无法读取容器头"));
                continue;
            }
        };

        let stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let desired_output = unique_path(&parent.join(format!("{stem}.mp4")));
        let preview_out_name = desired_output
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("output.mp4")
            .to_string();

        let temp_dir = TempWorkDir::create_in(&guard, &parent, "remux")?;
        let temp_output = temp_dir.path().join("candidate.mp4");
        remove_if_exists(&temp_output);

        edit_html_cancellable(
            &bot,
            msg.chat.id,
            progress_msg.id,
            format!(
                "🔄 <b>无损转封装中</b> ({}/{total}):\n<code>{}</code>\n➡️ <code>{}</code>",
                idx + 1,
                escape_html(&filename),
                escape_html(&preview_out_name),
            ),
            &cancel_rx,
        )
        .await;

        if *cancel_rx.borrow() {
            drop(temp_dir);
            notify_convert_cancelled(&bot, &msg, &progress_msg, success_count, total).await;
            return Err(AppError::Cancelled.into());
        }

        let args = build_remux_to_mp4_args(file_path, &temp_output);
        let duration = src_probe.duration_secs;
        let mut process = FfmpegProcess::spawn(args, duration, &state).await?;
        let mut cancel_watch = cancel_rx.clone();

        loop {
            if *cancel_watch.borrow() {
                if process.stop_and_wait().await {
                    state.write().await.record_exclusive_forced_kill(task_id);
                }
                break;
            }

            tokio::select! {
                tick_res = process.next_tick() => {
                    match tick_res {
                        Ok(Some(tick)) => {
                            let status_html = if let Some(pct) = tick.percent {
                                let bar = progress_bar.render(pct);
                                format!(
                                    "🔄 <b>无损转封装中</b> ({}/{total}):\n<code>{}</code>\n\n<code>{}</code>\n⏱️ <code>{}</code> / <code>{}</code>",
                                    idx + 1,
                                    escape_html(&filename),
                                    bar,
                                    format_duration(tick.out_time_secs),
                                    format_duration(duration),
                                )
                            } else {
                                format!(
                                    "🔄 <b>无损转封装中</b> ({}/{total}):\n<code>{}</code>\n⏱️ 已封装媒体时长: <code>{}</code>",
                                    idx + 1,
                                    escape_html(&filename),
                                    format_duration(tick.out_time_secs.max(1.0)),
                                )
                            };
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
            }
        }

        let (exit_code, stderr_summary, forced_after) =
            process.wait_with_summary(&cancel_rx).await?;
        if forced_after {
            state.write().await.record_exclusive_forced_kill(task_id);
        }

        if *cancel_rx.borrow() {
            drop(temp_dir);
            notify_convert_cancelled(&bot, &msg, &progress_msg, success_count, total).await;
            return Err(AppError::Cancelled.into());
        }

        let temp_size = std::fs::symlink_metadata(&temp_output)
            .ok()
            .filter(|m| m.file_type().is_file())
            .map_or(0, |m| m.len());

        let valid_mp4 = if exit_code == Some(0) && temp_size > 0 {
            match probe_media_cancellable(&temp_output, config.ffprobe_timeout_secs, &cancel_rx)
                .await
            {
                Ok(out_probe) => {
                    let duration_ok = src_probe.duration_secs > 0.0
                        && out_probe.duration_secs > 0.0
                        && out_probe.duration_secs >= src_probe.duration_secs * 0.95
                        && out_probe.duration_secs <= src_probe.duration_secs * 1.05 + 2.0;
                    let streams_ok = out_probe.video_streams.len() == src_probe.video_streams.len()
                        && out_probe.audio_streams.len() == src_probe.audio_streams.len()
                        && out_probe.video_streams.iter().zip(&src_probe.video_streams)
                            .all(|(out, src)| out.codec_name == src.codec_name)
                        && out_probe.audio_streams.iter().zip(&src_probe.audio_streams)
                            .all(|(out, src)| out.codec_name == src.codec_name);
                    duration_ok && streams_ok && out_probe.has_video()
                }
                Err(e) if is_cancelled_err(&e) => {
                    drop(temp_dir);
                    notify_convert_cancelled(&bot, &msg, &progress_msg, success_count, total).await;
                    return Err(e);
                }
                Err(_) => false,
            }
        } else {
            false
        };

        if valid_mp4 {
            match publish_temp_file_no_overwrite(&guard, &temp_output, &desired_output) {
                Ok(published) => {
                    success_count += 1;
                    state
                        .write()
                        .await
                        .record_exclusive_converted_progress(task_id, success_count, total);
                    info!(
                        input = %filename,
                        output = ?published.file_name(),
                        "Lossless MP4 remux published atomically"
                    );
                }
                Err(e) => {
                    failed_notes.push(format!(
                        "{filename}: 发布失败 ({})",
                        sanitize_user_error(&e)
                    ));
                }
            }
        } else {
            warn!(
                input = %filename,
                exit_code = ?exit_code,
                stderr = %redact_secrets(&stderr_summary),
                "Lossless MP4 remux failed validation (likely incompatible codec for MP4 container; re-encoding forbidden)"
            );
            failed_notes.push(format!(
                "{filename}: 源编码不兼容 MP4 无损直封或流损坏（禁止重编码，未生成残损文件）"
            ));
        }

        drop(temp_dir);
    }

    let mut summary = format!(
        "✅ <b>批量无损转封装完成！</b>\n成功: <code>{success_count}/{total}</code> 个文件（全程 <code>-c copy</code> 零重编码，同名自动避让）。"
    );
    if !failed_notes.is_empty() {
        summary.push_str("\n\n⚠️ <b>未完成条目</b>:");
        for note in failed_notes.iter().take(6) {
            summary.push_str(&format!("\n• {}", escape_html(note)));
        }
    }

    edit_html_bounded(&bot, msg.chat.id, progress_msg.id, summary).await;

    Ok(())
}

fn is_cancelled_err(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AppError>() == Some(&AppError::Cancelled)
}

async fn notify_convert_cancelled(
    bot: &Bot,
    msg: &Message,
    progress_msg: &Message,
    success_count: usize,
    total: usize,
) {
    edit_html_bounded(
        bot,
        msg.chat.id,
        progress_msg.id,
        format!(
            "🛑 <b>批量无损转封装已手动终止。</b>\n已完成并发布 <code>{success_count}/{total}</code> 个文件（未完成临时文件已清理，原文件与已发布成品未受影响）。"
        ),
    )
    .await;
}
