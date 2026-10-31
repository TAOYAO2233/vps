//! 视频智能无损合并操作（严格禁止重编码）。
//!
//! 完整实现交接文档 §3 与 v1.1 §1 的多阶段无损合并、容错恢复与可验证停止流程：
//! 1. **输入与顺序保障**：严格保留用户选择顺序，逐文件校验 `PathGuard`、可取消探测 `probe_media_cancellable` 流参数兼容性，
//!    并预检 `/storage512` 挂载健康与峰值剩余空间（覆盖 `.ts` 中间件 + 临时 `.mp4` + 512 MiB 安全余量）；
//! 2. **阶段 1（直接无损拼接）**：在 `/storage512` 任务独立临时目录中执行 `-f concat -c copy -movflags +faststart`；
//! 3. **阶段 2A/2B（MPEG-TS 中间层无损重封装）**：标准无损 TS 重封装与 `-fflags +discardcorrupt+genpts` 容错重封装；
//! 4. **阶段 2C（有界片尾无损截除）**：仅当尾部损坏导致前序阶段失败时，在 `1..=MERGE_MAX_LOSS_SECONDS` 范围内将 `-t <keep>`
//!    置于 `-i` 之前做无损片尾截除探测；保守默认 `MERGE_ALLOW_AUTO_TRIM=false` 下仅报告候选方案而不自动丢弃内容；
//! 5. **完整性强校验与原子发布**：强制通过退出码、非零大小、FFprobe 音视频流与保留时长校验后，才通过硬链接原子发布且绝不覆盖已有文件；
//! 6. **取消与清理保障**：FFprobe、FFmpeg（优先 `q` 优雅退出）、Telegram 消息编辑全程响应专属 `cancel_rx` 且具备有界超时，
//!    取消时先销毁本任务临时目录再尽力发送终态通知，清理绝不依赖 Telegram 网络成功。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use teloxide::prelude::*;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{edit_html_bounded, edit_html_cancellable, send_html_cancellable};
use crate::config::Config;
use crate::core::state::{ActionType, SharedState};
use crate::core::task_manager::TaskManager;
use crate::errors::AppError;
use crate::media::ffmpeg::{FfmpegRunner, FfmpegStepOutcome, TsRemuxOptions};
use crate::media::ffprobe::{
    check_concat_stream_compatibility, probe_media_cancellable, MediaProbeInfo,
};
use crate::media::merge::{
    format_merge_candidate_trim_prompt, format_merge_check, format_merge_failure_report,
    format_merge_success_report, validate_merged_output, MergeCheckDetails, MergeStrategy,
    SegmentTrimRecord,
};
use crate::storage::disk::estimate_merge_required_bytes;
use crate::storage::filesystem::{ensure_stable_files, remove_if_exists, write_concat_list, TempWorkDir};
use crate::storage::path::{publish_temp_file_no_overwrite, unique_path, PathGuard};
use crate::utils::format::{escape_html, smart_rename};
use crate::utils::redact::sanitize_user_error;

/// 启动无损视频合并独占任务。
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
    let state_for_clear = Arc::clone(&state);

    task_manager
        .start_exclusive("视频无损合并", move |task_id, cancel_rx| {
            let bot = bot_clone.clone();
            let msg = msg_clone.clone();
            let state_inner = Arc::clone(&state);
            let cfg = Arc::clone(&config_clone);
            async move { do_concat(bot, msg, state_inner, cfg, files, task_id, cancel_rx).await }
        })
        .await?;

    {
        let mut s = state_for_clear.write().await;
        s.clear_selections(&ActionType::Concat);
    }

    Ok(())
}

/// 单个分段重封装失败诊断上下文。
struct SegmentFailureDiag {
    segment_index: usize,
    filename: String,
    last_progress_secs: Option<f64>,
    stderr_summary: String,
}

/// 在独占任务内执行完整的无损合并与容错状态机。
async fn do_concat(
    bot: Bot,
    msg: Message,
    state: SharedState,
    config: Arc<Config>,
    raw_files: Vec<PathBuf>,
    task_id: u64,
    cancel_rx: watch::Receiver<bool>,
) -> Result<()> {
    let started_at = Instant::now();
    let guard = PathGuard::new(config.base_dir.clone());

    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }

    // 1. 严格按用户选择顺序逐个校验普通文件路径（绝不排序）
    let mut ordered_files: Vec<PathBuf> = Vec::with_capacity(raw_files.len());
    for raw in &raw_files {
        let verified = guard.assert_regular_file_no_symlink(raw)?;
        if !ordered_files.contains(&verified) {
            ordered_files.push(verified);
        }
    }

    if ordered_files.len() < 2 {
        return Err(AppError::ConcatInsufficientFiles {
            count: ordered_files.len(),
        }
        .into());
    }

    ensure_stable_files(&ordered_files, config.file_stable_seconds).await?;

    let work_dir = ordered_files[0]
        .parent()
        .ok_or_else(|| AppError::path_traversal(&ordered_files[0]))?;
    let canonical_work_dir = guard.assert_inside(work_dir)?;

    let base_smart_name = smart_rename(&ordered_files[0]);
    let output_stem = PathBuf::from(&base_smart_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("merged")
        .to_string();
    let desired_output_path = unique_path(&canonical_work_dir.join(format!("{output_stem}.mp4")));
    let preview_output_name = desired_output_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("merged.mp4")
        .to_string();

    let progress_msg = send_html_cancellable(
        &bot,
        msg.chat.id,
        format!(
            "⏳ <b>正在预检 {} 个分段（流兼容性与磁盘空间）...</b>\n拟输出文件:\n<code>{}</code>",
            ordered_files.len(),
            escape_html(&preview_output_name)
        ),
        &cancel_rx,
    )
    .await?;

    // 2. 逐分段执行带超时与取消监听的 FFprobe 元数据与编码兼容性预检
    let mut probed_segments: Vec<(PathBuf, MediaProbeInfo)> =
        Vec::with_capacity(ordered_files.len());
    let mut total_input_size: u64 = 0;
    let mut input_total_duration: f64 = 0.0;

    for (idx, path) in ordered_files.iter().enumerate() {
        if *cancel_rx.borrow() {
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }

        let seg_name = display_name(path);
        let seg_size = std::fs::metadata(path).map_or(0, |m| m.len());
        total_input_size = total_input_size.saturating_add(seg_size);

        match probe_media_cancellable(path, config.ffprobe_timeout_secs, &cancel_rx).await {
            Ok(info) => {
                input_total_duration += info.duration_secs.max(0.0);
                probed_segments.push((path.clone(), info));
            }
            Err(e) if is_cancelled_err(&e) => {
                notify_cancelled(&bot, &msg, &progress_msg).await;
                return Err(e);
            }
            Err(e) => {
                let report = format_merge_failure_report(
                    "合并前 FFprobe 流探测",
                    Some(&format!("#{} {seg_name}", idx + 1)),
                    None,
                    "无法读取分段容器头或探测超时，文件可能已严重损坏或尚未录制完成",
                    &sanitize_user_error(&e),
                );
                edit_html_bounded(&bot, msg.chat.id, progress_msg.id, report).await;
                return Err(e);
            }
        }
    }

    if let Err(compat_err) = check_concat_stream_compatibility(&probed_segments) {
        let report = format_merge_failure_report(
            "合并前无损流参数兼容性检查",
            None,
            None,
            &compat_err.to_string(),
            "",
        );
        edit_html_bounded(&bot, msg.chat.id, progress_msg.id, report).await;
        return Err(compat_err.into());
    }

    let input_has_audio = probed_segments[0].1.has_audio();

    // 3. 预检 /storage512 挂载状态与剩余空间（含 .ts 中间文件 + 临时 .mp4 + 512MiB 余量）
    let required_bytes = estimate_merge_required_bytes(total_input_size);
    let disk_monitor = {
        let s = state.read().await;
        Arc::clone(&s.disk_monitor)
    };
    if let Err(space_err) = disk_monitor.ensure_space_available(required_bytes).await {
        let report = format_merge_failure_report(
            "存储挂载与剩余空间预检",
            None,
            None,
            &sanitize_user_error(&space_err),
            "",
        );
        edit_html_bounded(&bot, msg.chat.id, progress_msg.id, report).await;
        return Err(space_err);
    }

    if *cancel_rx.borrow() {
        notify_cancelled(&bot, &msg, &progress_msg).await;
        return Err(AppError::Cancelled.into());
    }

    // 4. 在目标媒体目录下创建任务独占临时目录（位于 /storage512，禁止写入 /tmp；Drop 自动清理）
    let temp_dir = TempWorkDir::create_in(&guard, &canonical_work_dir, "concat")?;
    let temp_merged_mp4 = temp_dir.path().join("merged_candidate.mp4");
    let direct_list_file = temp_dir.path().join("direct_concat.txt");
    let ffmpeg = FfmpegRunner::new();

    // ── 阶段 1：直接无损拼接 (-f concat -safe 0 -i list -c copy -movflags +faststart) ──
    edit_html_cancellable(
        &bot,
        msg.chat.id,
        progress_msg.id,
        format!(
            "⏳ <b>阶段 1：正在执行直接无损拼接...</b>\n分段数量: <code>{}</code> | 拟输出: <code>{}</code>",
            ordered_files.len(),
            escape_html(&preview_output_name)
        ),
        &cancel_rx,
    )
    .await;

    write_concat_list(&direct_list_file, &ordered_files)?;
    let stage1_out = ffmpeg
        .run_concat(&direct_list_file, &temp_merged_mp4, &state, &cancel_rx)
        .await?;
    record_forced_kill_if_needed(&state, task_id, &stage1_out).await;

    if stage1_out.cancelled || *cancel_rx.borrow() {
        drop(temp_dir);
        notify_cancelled(&bot, &msg, &progress_msg).await;
        return Err(AppError::Cancelled.into());
    }

    let (stage1_ok, stage1_check) = match validate_merged_output(
        &temp_merged_mp4,
        stage1_out.exit_code,
        input_total_duration,
        total_input_size,
        input_has_audio,
        0,
        config.merge_max_loss_seconds,
        config.merge_min_duration_ratio,
        config.merge_min_size_ratio,
        config.ffprobe_timeout_secs,
        &cancel_rx,
    )
    .await
    {
        Ok(res) => res,
        Err(e) if is_cancelled_err(&e) => {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(e);
        }
        Err(e) => return Err(e),
    };

    if stage1_ok {
        return publish_and_report_success(
            &bot,
            &msg,
            &progress_msg,
            &guard,
            &temp_merged_mp4,
            &desired_output_path,
            temp_dir,
            &MergeStrategy::DirectConcat,
            &stage1_check,
            started_at.elapsed().as_secs_f64(),
        )
        .await;
    }

    // ── 阶段 2A：逐分段标准无损重封装为 MPEG-TS 后拼接 ────────────────────────────
    edit_html_cancellable(
        &bot,
        msg.chat.id,
        progress_msg.id,
        format!(
            "⚠️ <b>阶段 1 直接拼接未通过完整性校验</b>\n{}\n\n⚙️ 正在进入<b>阶段 2A：MPEG-TS 无损重封装</b>...",
            format_merge_check(&stage1_check)
        ),
        &cancel_rx,
    )
    .await;

    let mut ts_paths: Vec<PathBuf> = Vec::with_capacity(probed_segments.len());
    for idx in 0..probed_segments.len() {
        ts_paths.push(temp_dir.path().join(format!("seg_{idx}.ts")));
    }

    let mut failed_seg_indices: Vec<usize> = Vec::new();
    let mut last_failure_diag: Option<SegmentFailureDiag> = None;

    for (idx, (seg_path, seg_probe)) in probed_segments.iter().enumerate() {
        if *cancel_rx.borrow() {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }
        let seg_name = display_name(seg_path);

        edit_html_cancellable(
            &bot,
            msg.chat.id,
            progress_msg.id,
            format!(
                "⚙️ <b>阶段 2A：标准无损 TS 重封装</b> ({}/{}):\n<code>{}</code>",
                idx + 1,
                probed_segments.len(),
                escape_html(&seg_name)
            ),
            &cancel_rx,
        )
        .await;

        let step = ffmpeg
            .remux_to_ts(
                seg_path,
                &ts_paths[idx],
                TsRemuxOptions {
                    discard_corrupt_genpts: false,
                    keep_duration_secs: None,
                },
                &state,
                &cancel_rx,
            )
            .await?;
        record_forced_kill_if_needed(&state, task_id, &step).await;

        if step.cancelled || *cancel_rx.borrow() {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }

        let ts_ok = match verify_ts_segment(
            &ts_paths[idx],
            &step,
            seg_probe.duration_secs,
            config.merge_min_duration_ratio,
            config.ffprobe_timeout_secs,
            &cancel_rx,
        )
        .await
        {
            Ok(v) => v,
            Err(e) if is_cancelled_err(&e) => {
                drop(temp_dir);
                notify_cancelled(&bot, &msg, &progress_msg).await;
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        if !ts_ok {
            failed_seg_indices.push(idx);
            last_failure_diag = Some(SegmentFailureDiag {
                segment_index: idx + 1,
                filename: seg_name,
                last_progress_secs: step.last_progress_secs,
                stderr_summary: step.stderr_summary,
            });
        }
    }

    let ts_list_file = temp_dir.path().join("ts_concat.txt");

    if failed_seg_indices.is_empty() {
        remove_if_exists(&ts_list_file);
        write_concat_list(&ts_list_file, &ts_paths)?;
        let concat_2a = ffmpeg
            .run_concat(&ts_list_file, &temp_merged_mp4, &state, &cancel_rx)
            .await?;
        record_forced_kill_if_needed(&state, task_id, &concat_2a).await;

        if concat_2a.cancelled || *cancel_rx.borrow() {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }

        let (ok_2a, check_2a) = match validate_merged_output(
            &temp_merged_mp4,
            concat_2a.exit_code,
            input_total_duration,
            total_input_size,
            input_has_audio,
            0,
            config.merge_max_loss_seconds,
            config.merge_min_duration_ratio,
            config.merge_min_size_ratio,
            config.ffprobe_timeout_secs,
            &cancel_rx,
        )
        .await
        {
            Ok(res) => res,
            Err(e) if is_cancelled_err(&e) => {
                drop(temp_dir);
                notify_cancelled(&bot, &msg, &progress_msg).await;
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        if ok_2a {
            return publish_and_report_success(
                &bot,
                &msg,
                &progress_msg,
                &guard,
                &temp_merged_mp4,
                &desired_output_path,
                temp_dir,
                &MergeStrategy::TsRemux,
                &check_2a,
                started_at.elapsed().as_secs_f64(),
            )
            .await;
        }
    }

    // ── 阶段 2B：启用 -fflags +discardcorrupt+genpts 容错无损重封装 ───────────────
    // 若 2A 中仅特定分段重封装失败则针对性重做；若全部成功但拼接时间轴失败则全量重建时间戳
    let indices_for_2b: Vec<usize> = if failed_seg_indices.is_empty() {
        (0..probed_segments.len()).collect()
    } else {
        failed_seg_indices.clone()
    };

    failed_seg_indices.clear();

    for idx in indices_for_2b {
        if *cancel_rx.borrow() {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }
        let (seg_path, seg_probe) = &probed_segments[idx];
        let seg_name = display_name(seg_path);

        edit_html_cancellable(
            &bot,
            msg.chat.id,
            progress_msg.id,
            format!(
                "🛠️ <b>阶段 2B：容错重建时间戳 TS 重封装</b> ({}/{}):\n<code>{}</code>",
                idx + 1,
                probed_segments.len(),
                escape_html(&seg_name)
            ),
            &cancel_rx,
        )
        .await;

        let step = ffmpeg
            .remux_to_ts(
                seg_path,
                &ts_paths[idx],
                TsRemuxOptions {
                    discard_corrupt_genpts: true,
                    keep_duration_secs: None,
                },
                &state,
                &cancel_rx,
            )
            .await?;
        record_forced_kill_if_needed(&state, task_id, &step).await;

        if step.cancelled || *cancel_rx.borrow() {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }

        let ts_ok = match verify_ts_segment(
            &ts_paths[idx],
            &step,
            seg_probe.duration_secs,
            config.merge_min_duration_ratio,
            config.ffprobe_timeout_secs,
            &cancel_rx,
        )
        .await
        {
            Ok(v) => v,
            Err(e) if is_cancelled_err(&e) => {
                drop(temp_dir);
                notify_cancelled(&bot, &msg, &progress_msg).await;
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        if !ts_ok {
            failed_seg_indices.push(idx);
            last_failure_diag = Some(SegmentFailureDiag {
                segment_index: idx + 1,
                filename: seg_name,
                last_progress_secs: step.last_progress_secs,
                stderr_summary: step.stderr_summary,
            });
        }
    }

    let mut last_concat_check: Option<MergeCheckDetails> = None;
    let mut last_concat_stderr = String::new();
    let mut last_concat_time: Option<f64> = None;

    if failed_seg_indices.is_empty() {
        remove_if_exists(&ts_list_file);
        write_concat_list(&ts_list_file, &ts_paths)?;
        let concat_2b = ffmpeg
            .run_concat(&ts_list_file, &temp_merged_mp4, &state, &cancel_rx)
            .await?;
        record_forced_kill_if_needed(&state, task_id, &concat_2b).await;

        if concat_2b.cancelled || *cancel_rx.borrow() {
            drop(temp_dir);
            notify_cancelled(&bot, &msg, &progress_msg).await;
            return Err(AppError::Cancelled.into());
        }

        let (ok_2b, check_2b) = match validate_merged_output(
            &temp_merged_mp4,
            concat_2b.exit_code,
            input_total_duration,
            total_input_size,
            input_has_audio,
            0,
            config.merge_max_loss_seconds,
            config.merge_min_duration_ratio,
            config.merge_min_size_ratio,
            config.ffprobe_timeout_secs,
            &cancel_rx,
        )
        .await
        {
            Ok(res) => res,
            Err(e) if is_cancelled_err(&e) => {
                drop(temp_dir);
                notify_cancelled(&bot, &msg, &progress_msg).await;
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        if ok_2b {
            return publish_and_report_success(
                &bot,
                &msg,
                &progress_msg,
                &guard,
                &temp_merged_mp4,
                &desired_output_path,
                temp_dir,
                &MergeStrategy::TsRemuxCorruptTolerant,
                &check_2b,
                started_at.elapsed().as_secs_f64(),
            )
            .await;
        }

        last_concat_stderr = concat_2b.stderr_summary;
        last_concat_time = concat_2b.last_progress_secs;
        // 若拼接在某个边界中断，根据输出或处理时长定位疑似尾部损坏的前序分段
        let broken_time = if check_2b.output_duration > 0.0 {
            Some(check_2b.output_duration)
        } else {
            last_concat_time
        };
        if let Some(bt) = broken_time {
            if let Some(suspected_idx) = locate_segment_by_cumulative_time(&probed_segments, bt) {
                failed_seg_indices.push(suspected_idx);
            }
        }
        last_concat_check = Some(check_2b);
    }

    // ── 阶段 2C：有界片尾无损截除探测（1..=merge_max_loss_seconds，-t 严格置于 -i 之前）──
    if !failed_seg_indices.is_empty() && config.merge_max_loss_seconds > 0 {
        let mut trims_applied: Vec<SegmentTrimRecord> = Vec::new();
        let mut total_trimmed_secs: u64 = 0;
        let mut all_repaired = true;

        for &idx in &failed_seg_indices {
            let (seg_path, seg_probe) = &probed_segments[idx];
            let seg_name = display_name(seg_path);
            let seg_dur = seg_probe.duration_secs;
            let remaining_budget = config
                .merge_max_loss_seconds
                .saturating_sub(total_trimmed_secs);

            if remaining_budget == 0 || seg_dur <= 2.0 {
                all_repaired = false;
                break;
            }

            let mut repaired_this_seg = false;
            for trim_s in 1..=remaining_budget {
                if *cancel_rx.borrow() {
                    drop(temp_dir);
                    notify_cancelled(&bot, &msg, &progress_msg).await;
                    return Err(AppError::Cancelled.into());
                }

                let keep_dur = seg_dur - (trim_s as f64);
                if keep_dur <= 1.0 {
                    break;
                }

                let step = ffmpeg
                    .remux_to_ts(
                        seg_path,
                        &ts_paths[idx],
                        TsRemuxOptions {
                            discard_corrupt_genpts: true,
                            keep_duration_secs: Some(keep_dur),
                        },
                        &state,
                        &cancel_rx,
                    )
                    .await?;
                record_forced_kill_if_needed(&state, task_id, &step).await;

                if step.cancelled || *cancel_rx.borrow() {
                    drop(temp_dir);
                    notify_cancelled(&bot, &msg, &progress_msg).await;
                    return Err(AppError::Cancelled.into());
                }

                let ts_ok = match verify_ts_segment(
                    &ts_paths[idx],
                    &step,
                    keep_dur,
                    config.merge_min_duration_ratio,
                    config.ffprobe_timeout_secs,
                    &cancel_rx,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) if is_cancelled_err(&e) => {
                        drop(temp_dir);
                        notify_cancelled(&bot, &msg, &progress_msg).await;
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                };

                if ts_ok {
                    total_trimmed_secs += trim_s;
                    trims_applied.push(SegmentTrimRecord {
                        segment_index: idx + 1,
                        filename: seg_name.clone(),
                        original_duration: seg_dur,
                        trimmed_seconds: trim_s,
                    });
                    repaired_this_seg = true;
                    break;
                }
            }

            if !repaired_this_seg {
                all_repaired = false;
                break;
            }
        }

        if all_repaired && !trims_applied.is_empty() {
            remove_if_exists(&ts_list_file);
            write_concat_list(&ts_list_file, &ts_paths)?;
            let concat_2c = ffmpeg
                .run_concat(&ts_list_file, &temp_merged_mp4, &state, &cancel_rx)
                .await?;
            record_forced_kill_if_needed(&state, task_id, &concat_2c).await;

            if concat_2c.cancelled || *cancel_rx.borrow() {
                drop(temp_dir);
                notify_cancelled(&bot, &msg, &progress_msg).await;
                return Err(AppError::Cancelled.into());
            }

            let (ok_2c, check_2c) = match validate_merged_output(
                &temp_merged_mp4,
                concat_2c.exit_code,
                input_total_duration,
                total_input_size,
                input_has_audio,
                total_trimmed_secs,
                config.merge_max_loss_seconds,
                config.merge_min_duration_ratio,
                config.merge_min_size_ratio,
                config.ffprobe_timeout_secs,
                &cancel_rx,
            )
            .await
            {
                Ok(res) => res,
                Err(e) if is_cancelled_err(&e) => {
                    drop(temp_dir);
                    notify_cancelled(&bot, &msg, &progress_msg).await;
                    return Err(e);
                }
                Err(e) => return Err(e),
            };

            if ok_2c {
                if config.merge_allow_auto_trim {
                    // 用户已显式确认开启上限内自动无损片尾截除：原子发布成品并详细报告截除秒数
                    let strategy = MergeStrategy::TsRemuxBoundedTailTrim {
                        trims: trims_applied,
                    };
                    return publish_and_report_success(
                        &bot,
                        &msg,
                        &progress_msg,
                        &guard,
                        &temp_merged_mp4,
                        &desired_output_path,
                        temp_dir,
                        &strategy,
                        &check_2c,
                        started_at.elapsed().as_secs_f64(),
                    )
                    .await;
                }

                // 保守默认（MERGE_ALLOW_AUTO_TRIM=false）：不擅自丢弃任何内容，先清理临时产物再向管理员汇报候选方案
                drop(temp_dir);
                let prompt = format_merge_candidate_trim_prompt(
                    &trims_applied,
                    &check_2c,
                    config.merge_max_loss_seconds,
                );
                edit_html_bounded(&bot, msg.chat.id, progress_msg.id, prompt).await;
                info!(
                    total_trimmed_secs = total_trimmed_secs,
                    "Bounded tail-trim candidate verified in temp dir; awaiting user MERGE_ALLOW_AUTO_TRIM confirmation"
                );
                return Ok(());
            }
        }
    }

    // ── 阶段 3：无法无损修复，先彻底清理临时目录再给出精确诊断报告（绝不产出残损文件）──
    drop(temp_dir);

    let (suspected_label, approx_pos, reason_text, stderr_text) = match last_failure_diag {
        Some(diag) => (
            Some(format!("#{} {}", diag.segment_index, diag.filename)),
            diag.last_progress_secs,
            format!(
                "分段 #{} 在无损重封装或有界片尾截除（≤{}s）后仍无法完整修复，疑似中段存在较长损坏区间",
                diag.segment_index, config.merge_max_loss_seconds
            ),
            diag.stderr_summary,
        ),
        None => {
            let reason = last_concat_check
                .as_ref()
                .and_then(|c| c.failure_reason.clone())
                .unwrap_or_else(|| "拼接后输出未通过严格流与时长校验".to_string());
            (None, last_concat_time, reason, last_concat_stderr)
        }
    };

    let failure_html = format_merge_failure_report(
        "阶段 2 无损重封装与完整性校验",
        suspected_label.as_deref(),
        approx_pos,
        &reason_text,
        &stderr_text,
    );

    edit_html_bounded(&bot, msg.chat.id, progress_msg.id, failure_html).await;

    warn!("Lossless concat failed validation; temporary directory cleaned without partial output");
    Err(AppError::MergeValidationFailed {
        duration_ratio: last_concat_check.as_ref().map_or(0.0, |c| c.duration_ratio),
        size_ratio: last_concat_check.as_ref().map_or(0.0, |c| c.size_ratio),
    }
    .into())
}

/// 原子发布已通过完整性强校验的临时 `.mp4` 成品、立即清理临时工作目录并更新成功消息。
#[allow(clippy::too_many_arguments)]
async fn publish_and_report_success(
    bot: &Bot,
    msg: &Message,
    progress_msg: &Message,
    guard: &PathGuard,
    temp_merged_mp4: &Path,
    desired_output_path: &Path,
    temp_dir: TempWorkDir,
    strategy: &MergeStrategy,
    details: &MergeCheckDetails,
    elapsed_secs: f64,
) -> Result<()> {
    let published_path =
        publish_temp_file_no_overwrite(guard, temp_merged_mp4, desired_output_path)?;
    drop(temp_dir);
    let final_name = display_name(&published_path);

    let report = format_merge_success_report(&final_name, strategy, details, elapsed_secs);
    edit_html_bounded(bot, msg.chat.id, progress_msg.id, report).await;

    info!(
        output = %final_name,
        strategy = %strategy.display_label(),
        duration_secs = details.output_duration,
        "Lossless video merge published atomically"
    );
    Ok(())
}

/// 验证单个中间 `.ts` 文件是否非空、FFprobe 可正常解析视频流且保留时长达标（同时响应取消信号）。
async fn verify_ts_segment(
    ts_path: &Path,
    step: &FfmpegStepOutcome,
    expected_duration: f64,
    min_duration_ratio: f64,
    ffprobe_timeout_secs: u64,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<bool> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }
    if !step.is_success() {
        return Ok(false);
    }
    let size = std::fs::symlink_metadata(ts_path)
        .ok()
        .filter(|m| m.file_type().is_file())
        .map_or(0, |m| m.len());
    if size == 0 {
        return Ok(false);
    }
    let probe = match probe_media_cancellable(ts_path, ffprobe_timeout_secs, cancel_rx).await {
        Ok(p) => p,
        Err(e) if is_cancelled_err(&e) => return Err(e),
        Err(_) => return Ok(false),
    };
    if !probe.has_video() || probe.duration_secs <= 0.0 {
        return Ok(false);
    }
    if expected_duration > 0.0 && probe.duration_secs < expected_duration * min_duration_ratio {
        return Ok(false);
    }
    Ok(true)
}

async fn record_forced_kill_if_needed(state: &SharedState, task_id: u64, step: &FfmpegStepOutcome) {
    if step.forced_kill_used {
        state.write().await.record_exclusive_forced_kill(task_id);
    }
}

fn is_cancelled_err(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AppError>() == Some(&AppError::Cancelled)
}

/// 根据拼接中断时的累计时间戳定位疑似发生片尾损坏的分段下标。
fn locate_segment_by_cumulative_time(
    segments: &[(PathBuf, MediaProbeInfo)],
    broken_at_secs: f64,
) -> Option<usize> {
    if broken_at_secs <= 0.0 {
        return None;
    }
    let mut acc = 0.0;
    for (idx, (_, probe)) in segments.iter().enumerate() {
        acc += probe.duration_secs.max(0.0);
        if broken_at_secs <= acc + 5.0 {
            return Some(idx);
        }
    }
    segments.len().checked_sub(1)
}

async fn notify_cancelled(bot: &Bot, msg: &Message, progress_msg: &Message) {
    edit_html_bounded(
        bot,
        msg.chat.id,
        progress_msg.id,
        "🛑 <b>无损合并任务已手动终止（所有临时文件已自动清理，原文件未受影响）。</b>".to_string(),
    )
    .await;
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}
