//! 视频智能合并操作。
//!
//! 对应 Python 版本的 `action_concat` 函数。
//! 实现两阶段合并策略：
//! 1. **极速直连拼接**：直接使用 `ffmpeg -f concat -c copy`，速度最快。
//! 2. **TS 容错机制**：若直连失败（时长/体积校验不达标），先将各片段无损封转为 `.ts`，
//!    再进行拼接，解决 FLV/MKV 等格式时间轴不连续的问题。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{info, warn};

use crate::config::Config;
use crate::core::state::SharedState;
use crate::core::task_manager::TaskManager;
use crate::errors::AppError;
use crate::media::ffmpeg::FfmpegRunner;
use crate::media::ffprobe::get_video_duration;
use crate::media::merge::{format_merge_check, validate_merged_file};
use crate::storage::filesystem::{remove_if_exists, write_concat_list};
use crate::storage::path::unique_path;
use crate::utils::format::smart_rename;

/// 启动视频合并独占任务。
pub async fn start_concat(
    bot: &Bot,
    msg: &Message,
    state: SharedState,
    config: Arc<Config>,
    files: Vec<PathBuf>,
) -> Result<()> {
    if files.len() < 2 {
        return Err(AppError::ConcatInsufficientFiles { count: files.len() }.into());
    }

    let task_manager = TaskManager::new(Arc::clone(&state));
    let bot_clone = bot.clone();
    let msg_clone = msg.clone();
    let config_clone = Arc::clone(&config);

    task_manager
        .start_exclusive("视频合并", move || {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let state_inner = Arc::clone(&state);
            let cfg = Arc::clone(&config_clone);
            async move { do_concat(bot, msg, state_inner, cfg, files).await }
        })
        .await
        .map_err(|e| match e.downcast_ref::<AppError>() {
            Some(AppError::TaskAlreadyRunning { task_name }) => {
                anyhow::anyhow!("已有任务正在运行：{task_name}\n请先发送 /stop 或等待完成。")
            }
            Some(AppError::YoutubeUploadBlocking { count }) => {
                anyhow::anyhow!(
                    "当前有 {count} 个 YouTube 上传任务在运行/排队，请先 /stop 或等待完成后再合并。"
                )
            }
            _ => e,
        })?;

    Ok(())
}

/// 实际执行合并逻辑（在独占任务内运行）。
async fn do_concat(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
    files_to_merge: Vec<PathBuf>,
) -> Result<()> {
    let work_dir = files_to_merge[0]
        .parent()
        .unwrap_or(&config.base_dir)
        .to_path_buf();

    let run_id = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let list_file = work_dir.join(format!(".concat_list_{run_id}.txt"));
    let list_file_ts = work_dir.join(format!(".concat_list_ts_{run_id}.txt"));
    let mut ts_files: Vec<PathBuf> = Vec::new();

    // 生成输出文件名（强制 MP4，自动避让同名文件）
    let base_smart_name = smart_rename(&files_to_merge[0]);
    let output_stem = PathBuf::from(&base_smart_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("merged")
        .to_string();
    let output_path = unique_path(&work_dir.join(format!("{output_stem}.mp4")));
    let output_filename = output_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("merged.mp4")
        .to_string();

    let progress_msg = bot
        .send_message(
            msg.chat.id,
            format!(
                "⏳ **正在分析 {} 个文件的大小和时长...**\n输出文件:\n`{output_filename}`",
                files_to_merge.len()
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

    // 计算总大小和总时长
    let total_input_size: u64 = files_to_merge
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    let mut input_total_duration = 0.0_f64;
    for path in &files_to_merge {
        if state.read().await.cancel_flag {
            bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 **合并任务已手动终止。**")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Err(AppError::Cancelled.into());
        }
        input_total_duration += get_video_duration(path).await.unwrap_or(0.0);
    }

    // ── 第一阶段：极速直连拼接 ─────────────────────────────────────────────────
    bot.edit_message_text(
        msg.chat.id,
        progress_msg.id,
        format!("⏳ **正在尝试极速直连拼接...**\n输出文件:\n`{output_filename}`"),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .await?;

    write_concat_list(&list_file, &files_to_merge)?;

    let ffmpeg = FfmpegRunner::new();
    let status = ffmpeg.run_concat(&list_file, &output_path, &state).await?;

    remove_if_exists(&list_file);

    if state.read().await.cancel_flag {
        remove_if_exists(&output_path);
        cleanup_ts(&ts_files);
        remove_if_exists(&list_file_ts);
        bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 **合并任务已手动终止。**")
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        return Err(AppError::Cancelled.into());
    }

    let (is_success, check) = validate_merged_file(
        &output_path,
        status,
        input_total_duration,
        total_input_size,
        config.merge_min_duration_ratio,
        config.merge_min_size_ratio,
    )
    .await;

    // ── 第二阶段：TS 容错机制 ──────────────────────────────────────────────────
    let final_success;
    let final_check;

    if !is_success {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!(
                "⚠️ **直连拼接未通过时长/体积校验！**\n{}\n\n正在触发 `.ts` 容错处理机制，请耐心等待...",
                format_merge_check(&check)
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

        let mut ts_convert_ok = true;
        for (idx, file_path) in files_to_merge.iter().enumerate() {
            if state.read().await.cancel_flag {
                break;
            }

            let ts_path = work_dir.join(format!(".temp_merge_fallback_{run_id}_{idx}.ts"));
            ts_files.push(ts_path.clone());

            bot.edit_message_text(
                msg.chat.id,
                progress_msg.id,
                format!(
                    "⚙️ **TS 容错转换中** ({}/{}):\n`{}`",
                    idx + 1,
                    files_to_merge.len(),
                    file_path.file_name().and_then(|n| n.to_str()).unwrap_or("")
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

            let ts_status = ffmpeg
                .remux_to_ts(file_path, &ts_path, &state)
                .await
                .unwrap_or(false);

            if !ts_status
                || !ts_path.exists()
                || ts_path.metadata().map(|m| m.len()).unwrap_or(0) == 0
            {
                ts_convert_ok = false;
                break;
            }
        }

        if state.read().await.cancel_flag {
            remove_if_exists(&output_path);
            cleanup_ts(&ts_files);
            remove_if_exists(&list_file_ts);
            bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 **合并任务已手动终止。**")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Err(AppError::Cancelled.into());
        }

        if !ts_convert_ok {
            cleanup_ts(&ts_files);
            remove_if_exists(&output_path);
            remove_if_exists(&list_file_ts);
            bot.edit_message_text(
                msg.chat.id,
                progress_msg.id,
                "❌ **TS 容错转换失败**\n部分片段无法无损封装为 TS，建议先单文件转码后再试。",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        write_concat_list(&list_file_ts, &ts_files)?;

        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!("✂️ **TS 容错转换完成，正在进行最终拼接...**\n输出文件:\n`{output_filename}`"),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

        let ts_status = ffmpeg
            .run_concat(&list_file_ts, &output_path, &state)
            .await?;

        let (s, c) = validate_merged_file(
            &output_path,
            ts_status,
            input_total_duration,
            total_input_size,
            config.merge_min_duration_ratio,
            config.merge_min_size_ratio,
        )
        .await;
        final_success = s;
        final_check = c;
    } else {
        final_success = is_success;
        final_check = check;
    }

    // ── 最终结果 ───────────────────────────────────────────────────────────────
    cleanup_ts(&ts_files);
    remove_if_exists(&list_file);
    remove_if_exists(&list_file_ts);

    if state.read().await.cancel_flag {
        remove_if_exists(&output_path);
        bot.edit_message_text(msg.chat.id, progress_msg.id, "🛑 **合并任务已手动终止。**")
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        return Err(AppError::Cancelled.into());
    }

    if final_success {
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!(
                "✅ **合并完成!**\n\n📁 新文件: `{output_filename}`\n{}",
                format_merge_check(&final_check)
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        info!(output = %output_filename, "Concat completed successfully");
    } else {
        remove_if_exists(&output_path);
        bot.edit_message_text(
            msg.chat.id,
            progress_msg.id,
            format!(
                "❌ **合并彻底失败**\n{}\n\n输出文件未通过时长/体积校验。两段视频的编码、分辨率或时间戳可能严重不一致，建议先单文件转码后再试。",
                format_merge_check(&final_check)
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        warn!(output = %output_filename, "Concat failed validation");
    }

    Ok(())
}

/// 清理临时 TS 文件。
fn cleanup_ts(ts_files: &[PathBuf]) {
    for ts in ts_files {
        remove_if_exists(ts);
    }
}
