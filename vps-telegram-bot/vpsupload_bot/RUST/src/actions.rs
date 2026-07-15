use crate::config::Config;
use crate::media_utils::*;
use crate::task_manager::AppState;
use anyhow::Result;
use chrono::{DateTime, Local};
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use teloxide::payloads::EditMessageTextSetters;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub async fn action_browse(bot: Bot, q: CallbackQuery, path: PathBuf, config: Arc<Config>) -> Result<()> {
    let safe_path = assert_path_inside_base(&config.base_dir, &path)?;
    let size_str = get_formatted_file_size(&safe_path);
    let duration = get_video_duration(&safe_path, config.ffprobe_timeout_seconds).await;
    let mtime = fs::metadata(&safe_path)?
        .modified()
        .map(|t| DateTime::<Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|_| "未知".to_string());

    let dur_str = if duration > 0.0 { format_duration(duration) } else { "未知或无损流".to_string() };
    let filename = safe_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let text = format!("📄 {}\n━━━━━━━━━━━━\n📏 大小: {}\n⏱️ 时长: {}\n🕒 修改时间: {}", filename, size_str, dur_str, mtime);
    bot.answer_callback_query(q.id).text(text).show_alert(true).await?;
    Ok(())
}

pub async fn action_stream(
    bot: Bot,
    q: CallbackQuery,
    path: PathBuf,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    let safe_path = assert_path_inside_base(&config.base_dir, &path)?;
    let filename = safe_path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let size_str = get_formatted_file_size(&safe_path);

    if config.rtmp_url.is_empty() {
        if let Some(msg) = q.message {
            bot.edit_message_text(msg.chat.id, msg.id, "❌ RTMP_URL 未配置，请在 `.env` 中添加。").await?;
        }
        return Ok(());
    }

    let mut rx = match state.start_exclusive_task("RTMP推流").await {
        Ok(receiver) => receiver,
        Err(err) => {
            bot.answer_callback_query(q.id).text(err).show_alert(true).await?;
            return Ok(());
        }
    };

    let msg = if let Some(m) = q.message {
        bot.edit_message_text(m.chat.id, m.id, format!("⏳ 正在分析推流文件: `{}` ({})...", filename, size_str))
            .parse_mode(ParseMode::MarkdownV2)
            .await?
    } else {
        return Ok(());
    };

    let duration = get_video_duration(&safe_path, config.ffprobe_timeout_seconds).await;
    if duration <= 0.0 {
        bot.edit_message_text(msg.chat.id, msg.id, "❌ 无法获取视频时长，推流终止。").await?;
        let mut active = state.active_task.lock().await;
        *active = None;
        return Ok(());
    }

    let mut cmd = Command::new("ffmpeg");
    cmd.args(&["-re", "-i"])
        .arg(&safe_path)
        .args(&["-c", "copy", "-f", "flv", &config.rtmp_url])
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take().unwrap();
    let mut reader = BufReader::new(stderr).lines();
    let time_re = Regex::new(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}").unwrap();

    let mut last_update = Instant::now();
    let mut last_percent = -1.0;

    loop {
        tokio::select! {
            _ = rx.changed() => {
                if *rx.borrow() {
                    let _ = child.kill().await;
                    bot.edit_message_text(msg.chat.id, msg.id, format!("🛑 **推流已手动终止**:\n`{}`", filename))
                        .parse_mode(ParseMode::MarkdownV2).await?;
                    break;
                }
            }
            line = reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(caps) = time_re.captures(&l) {
                            let h: f64 = caps[1].parse().unwrap_or(0.0);
                            let m: f64 = caps[2].parse().unwrap_or(0.0);
                            let s: f64 = caps[3].parse().unwrap_or(0.0);
                            let curr_sec = h * 3600.0 + m * 60.0 + s;
                            let percent = (curr_sec / duration) * 100.0;
                            if (percent - last_percent >= 1.0) && last_update.elapsed().as_secs() >= 2 {
                                let bar = build_progress_bar(percent, 20);
                                let _ = bot.edit_message_text(
                                    msg.chat.id,
                                    msg.id,
                                    format!("📡 **推流中**: `{}`\n\n`{}`\n⏱️ {:.0}s / {:.0}s", filename, bar, curr_sec, duration)
                                ).parse_mode(ParseMode::MarkdownV2).await;
                                last_update = Instant::now();
                                last_percent = percent.floor();
                            }
                        }
                    }
                    _ => {
                        let status = child.wait().await?;
                        if status.success() {
                            bot.edit_message_text(msg.chat.id, msg.id, format!("✅ **推流结束**:\n`{}`", filename))
                                .parse_mode(ParseMode::MarkdownV2).await?;
                        } else {
                            bot.edit_message_text(msg.chat.id, msg.id, format!("❌ **推流异常结束**:\n`{}`", filename))
                                .parse_mode(ParseMode::MarkdownV2).await?;
                        }
                        break;
                    }
                }
            }
        }
    }

    let mut active = state.active_task.lock().await;
    *active = None;
    Ok(())
}

pub async fn action_concat(
    bot: Bot,
    q: CallbackQuery,
    files: Vec<PathBuf>,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    if files.len() < 2 {
        bot.answer_callback_query(q.id).text("❌ 至少需要选择 2 个文件！").show_alert(true).await?;
        return Ok(());
    }

    let mut rx = match state.start_exclusive_task("视频合并").await {
        Ok(r) => r,
        Err(e) => {
            bot.answer_callback_query(q.id).text(e).show_alert(true).await?;
            return Ok(());
        }
    };

    let safe_files: Vec<PathBuf> = files.iter()
        .filter_map(|f| assert_path_inside_base(&config.base_dir, f).ok())
        .collect();

    let work_dir = safe_files[0].parent().unwrap();
    let run_id = format!("{}_{}", std::process::id(), chrono::Utc::now().timestamp_millis());
    let list_path = work_dir.join(format!(".concat_list_{}.txt", run_id));
    let list_path_ts = work_dir.join(format!(".concat_list_ts_{}.txt", run_id));

    let base_smart_name = smart_rename(&safe_files[0]);
    let output_name = format!("{}.mp4", Path::new(&base_smart_name).file_stem().unwrap().to_str().unwrap());
    let output_path = unique_path(&work_dir.join(&output_name));
    let output_filename = output_path.file_name().unwrap().to_str().unwrap().to_string();

    let msg = if let Some(m) = q.message {
        bot.edit_message_text(m.chat.id, m.id, format!("⏳ **正在分析 {} 个文件...**\n输出:\n`{}`", safe_files.len(), output_filename))
            .parse_mode(ParseMode::MarkdownV2).await?
    } else { return Ok(()); };

    let mut total_size = 0u64;
    let mut total_duration = 0.0f64;
    for f in &safe_files {
        if *rx.borrow() {
            bot.edit_message_text(msg.chat.id, msg.id, "🛑 **合并任务已手动终止。**").parse_mode(ParseMode::MarkdownV2).await?;
            let mut active = state.active_task.lock().await; *active = None; return Ok(());
        }
        total_size += fs::metadata(f)?.len();
        total_duration += get_video_duration(f, config.ffprobe_timeout_seconds).await;
    }

    bot.edit_message_text(msg.chat.id, msg.id, format!("⏳ **尝试极速直连拼接...**\n输出:\n`{}`", output_filename))
        .parse_mode(ParseMode::MarkdownV2).await?;

    write_concat_list(&list_path, &safe_files)?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(&["-y", "-f", "concat", "-safe", "0", "-i"]).arg(&list_path)
        .args(&["-c", "copy", "-movflags", "+faststart"]).arg(&output_path)
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let status = tokio::select! {
        _ = rx.changed() => {
            let _ = child.kill().await;
            remove_if_exists(&output_path);
            remove_if_exists(&list_path);
            bot.edit_message_text(msg.chat.id, msg.id, "🛑 **合并任务已手动终止。**").parse_mode(ParseMode::MarkdownV2).await?;
            let mut active = state.active_task.lock().await; *active = None; return Ok(());
        }
        res = child.wait() => res?
    };
    remove_if_exists(&list_path);

    let mut check_res = validate_merged_file(
        &output_path, status.success(), total_duration, total_size,
        config.merge_min_duration_ratio, config.merge_min_size_ratio, config.ffprobe_timeout_seconds
    ).await;

    if !check_res.is_success {
        bot.edit_message_text(msg.chat.id, msg.id, format!("⚠️ **直连拼接失败！**\n{}\n\n触发 `.ts` 容错机制...", format_merge_check(&check_res)))
            .parse_mode(ParseMode::MarkdownV2).await?;

        let mut ts_files = Vec::new();
        let mut ts_ok = true;
        for (idx, f) in safe_files.iter().enumerate() {
            if *rx.borrow() { break; }
            let ts_path = work_dir.join(format!(".temp_fallback_{}_{}.ts", run_id, idx));
            ts_files.push(ts_path.clone());
            bot.edit_message_text(msg.chat.id, msg.id, format!("⚙️ **TS 容错转换中** ({}/{})\n`{}`", idx+1, safe_files.len(), f.file_name().unwrap().to_str().unwrap()))
                .parse_mode(ParseMode::MarkdownV2).await?;

            let mut cmd_ts = Command::new("ffmpeg");
            cmd_ts.args(&["-y", "-i"]).arg(f).args(&["-c", "copy", "-f", "mpegts"]).arg(&ts_path).kill_on_drop(true);
            let mut proc_ts = cmd_ts.spawn()?;
            let st = tokio::select! {
                _ = rx.changed() => { let _ = proc_ts.kill().await; break; }
                res = proc_ts.wait() => res?
            };
            if !st.success() || !ts_path.exists() || fs::metadata(&ts_path)?.len() == 0 {
                ts_ok = false; break;
            }
        }

        if *rx.borrow() {
            for ts in &ts_files { remove_if_exists(ts); }
            remove_if_exists(&output_path);
            bot.edit_message_text(msg.chat.id, msg.id, "🛑 **合并任务已手动终止。**").parse_mode(ParseMode::MarkdownV2).await?;
            let mut active = state.active_task.lock().await; *active = None; return Ok(());
        }

        if !ts_ok {
            for ts in &ts_files { remove_if_exists(ts); }
            remove_if_exists(&output_path);
            bot.edit_message_text(msg.chat.id, msg.id, "❌ **TS 容错转换失败**\n请先单文件转码。").parse_mode(ParseMode::MarkdownV2).await?;
            let mut active = state.active_task.lock().await; *active = None; return Ok(());
        }

        write_concat_list(&list_path_ts, &ts_files)?;
        bot.edit_message_text(msg.chat.id, msg.id, format!("✂️ **TS 最终拼接...**\n输出:\n`{}`", output_filename))
            .parse_mode(ParseMode::MarkdownV2).await?;

        let mut cmd_concat_ts = Command::new("ffmpeg");
        cmd_concat_ts.args(&["-y", "-f", "concat", "-safe", "0", "-i"]).arg(&list_path_ts)
            .args(&["-c", "copy", "-movflags", "+faststart"]).arg(&output_path).kill_on_drop(true);
        let mut proc_ts_final = cmd_concat_ts.spawn()?;
        let st_ts = tokio::select! {
            _ = rx.changed() => { let _ = proc_ts_final.kill().await; proc_ts_final.wait().await? }
            res = proc_ts_final.wait() => res?
        };

        check_res = validate_merged_file(
            &output_path, st_ts.success(), total_duration, total_size,
            config.merge_min_duration_ratio, config.merge_min_size_ratio, config.ffprobe_timeout_seconds
        ).await;

        remove_if_exists(&list_path_ts);
        for ts in &ts_files { remove_if_exists(ts); }
    }

    if check_res.is_success {
        bot.edit_message_text(msg.chat.id, msg.id, format!("✅ **合并完成!**\n\n📁 新文件: `{}`\n{}", output_filename, format_merge_check(&check_res)))
            .parse_mode(ParseMode::MarkdownV2).await?;
    } else {
        remove_if_exists(&output_path);
        bot.edit_message_text(msg.chat.id, msg.id, format!("❌ **合并彻底失败**\n{}", format_merge_check(&check_res)))
            .parse_mode(ParseMode::MarkdownV2).await?;
    }

    let mut active = state.active_task.lock().await; *active = None;
    Ok(())
}

pub async fn action_convert(
    bot: Bot,
    q: CallbackQuery,
    files: Vec<PathBuf>,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    let total = files.len();
    let mut rx = match state.start_exclusive_task("批量转码").await {
        Ok(r) => r,
        Err(e) => { bot.answer_callback_query(q.id).text(e).show_alert(true).await?; return Ok(()); }
    };

    let msg = if let Some(m) = q.message {
        bot.edit_message_text(m.chat.id, m.id, format!("🔄 准备转换 {} 个文件...", total)).await?
    } else { return Ok(()); };

    let mut success_count = 0;
    let time_re = Regex::new(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}").unwrap();

    for (idx, path) in files.iter().enumerate() {
        if *rx.borrow() { break; }
        let safe_path = assert_path_inside_base(&config.base_dir, path)?;
        let stem = safe_path.file_stem().unwrap().to_str().unwrap();
        let output_path = unique_path(&safe_path.parent().unwrap().join(format!("{}.mp4", stem)));
        let in_name = safe_path.file_name().unwrap().to_str().unwrap().to_string();
        let duration = get_video_duration(&safe_path, config.ffprobe_timeout_seconds).await;

        let mut cmd = Command::new("ffmpeg");
        cmd.args(&["-y", "-i"]).arg(&safe_path).args(&["-c", "copy", "-movflags", "+faststart"]).arg(&output_path)
            .stderr(Stdio::piped()).kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stderr = child.stderr.take().unwrap();
        let mut reader = BufReader::new(stderr).lines();
        let mut last_update = Instant::now();
        let mut last_percent = -1.0;

        loop {
            tokio::select! {
                _ = rx.changed() => { let _ = child.kill().await; remove_if_exists(&output_path); break; }
                line = reader.next_line() => {
                    match line {
                        Ok(Some(l)) => {
                            if duration > 0.0 {
                                if let Some(caps) = time_re.captures(&l) {
                                    let h: f64 = caps[1].parse().unwrap_or(0.0);
                                    let m: f64 = caps[2].parse().unwrap_or(0.0);
                                    let s: f64 = caps[3].parse().unwrap_or(0.0);
                                    let curr = h * 3600.0 + m * 60.0 + s;
                                    let p = (curr / duration) * 100.0;
                                    if (p - last_percent >= 1.0) && last_update.elapsed().as_secs() >= 2 {
                                        let bar = build_progress_bar(p, 20);
                                        let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("🔄 **正在转换** ({}/{})\n`{}`\n\n`{}`", idx+1, total, in_name, bar))
                                            .parse_mode(ParseMode::MarkdownV2).await;
                                        last_update = Instant::now();
                                        last_percent = p.floor();
                                    }
                                }
                            }
                        }
                        _ => {
                            let st = child.wait().await?;
                            if st.success() && output_path.exists() && fs::metadata(&output_path)?.len() > 0 {
                                success_count += 1;
                            } else {
                                remove_if_exists(&output_path);
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    if *rx.borrow() {
        bot.edit_message_text(msg.chat.id, msg.id, "🛑 **批量转换已手动终止。**").parse_mode(ParseMode::MarkdownV2).await?;
    } else {
        bot.edit_message_text(msg.chat.id, msg.id, format!("✅ **批量转换完成!**\n成功转换 {}/{} 个文件。", success_count, total))
            .parse_mode(ParseMode::MarkdownV2).await?;
    }

    let mut active = state.active_task.lock().await; *active = None;
    Ok(())
}

pub async fn action_delete(
    bot: Bot,
    q: CallbackQuery,
    files: Vec<PathBuf>,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    let mut deleted = 0;
    let mut failed = 0;
    let mut rx = match state.start_exclusive_task("删除文件").await {
        Ok(r) => r,
        Err(e) => { bot.answer_callback_query(q.id).text(e).show_alert(true).await?; return Ok(()); }
    };

    for path in files {
        if *rx.borrow() { break; }
        match assert_path_inside_base(&config.base_dir, &path) {
            Ok(p) if p.is_file() => {
                if fs::remove_file(&p).is_ok() { deleted += 1; } else { failed += 1; }
            }
            _ => failed += 1,
        }
    }

    let user_id = q.from.id.0 as i64;
    state.update_session(user_id, |s| s.pending_delete.clear()).await;

    if let Some(msg) = q.message {
        if *rx.borrow() {
            bot.edit_message_text(msg.chat.id, msg.id, format!("🛑 **删除任务已手动终止。**\n已删除 {} 个文件。", deleted))
                .parse_mode(ParseMode::MarkdownV2).await?;
        } else {
            bot.edit_message_text(msg.chat.id, msg.id, format!("🗑️ **清理完成!**\n成功删除 {}，失败 {}。", deleted, failed))
                .parse_mode(ParseMode::MarkdownV2).await?;
        }
    }
    let mut active = state.active_task.lock().await; *active = None;
    Ok(())
}