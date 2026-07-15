use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Notify;
use std::process::Stdio;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use regex::Regex;

use crate::state::AppState;
use crate::media_utils::*;

fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
}

pub async fn action_stream(
    bot: Bot,
    msg: Message,
    filepath: PathBuf,
    state: Arc<AppState>,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
    user_id: i64,
) {
    let session = state.get_session(user_id).await;
    let theme = session.progress_bar_theme;

    let filename = filepath.file_name().unwrap().to_string_lossy().to_string();
    let rtmp = &state.config.rtmp_url;
    if rtmp.is_empty() {
        let _ = bot.send_message(msg.chat.id, "❌ RTMP_URL 未配置。").await;
        return;
    }

    let duration = get_video_duration(&filepath, state.config.ffprobe_timeout_seconds).await;
    if duration <= 0.0 {
        let _ = bot.send_message(msg.chat.id, "❌ 无法获取视频时长，终止推流。").await;
        return;
    }

    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("⏳ 正在开始推流: <code>{}</code>...", escape_html(&filename)))
        .parse_mode(ParseMode::Html).await;

    let mut child = Command::new("ffmpeg")
        .args(&["-re", "-i", filepath.to_str().unwrap(), "-c", "copy", "-f", "flv", rtmp])
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("启动 ffmpeg 推流失败");

    let stderr = child.stderr.take().unwrap();
    let mut reader = BufReader::new(stderr).lines();
    let time_re = Regex::new(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}").unwrap();

    let mut last_update = std::time::Instant::now();
    let mut last_percent = -1.0;

    loop {
        tokio::select! {
            _ = cancel_notify.notified() => {
                let _ = child.kill().await;
                let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("🛑 <b>推流已手动终止</b>:\n<code>{}</code>", escape_html(&filename)))
                    .parse_mode(ParseMode::Html).await;
                break;
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
                                let bar = build_progress_bar(percent, 20, theme);
                                let text = format!("📡 <b>推流中</b>: <code>{}</code>\n\n{}\n⏱️ {}s / {}s", escape_html(&filename), bar, curr_sec, duration);
                                let _ = bot.edit_message_text(msg.chat.id, msg.id, text).parse_mode(ParseMode::Html).await;
                                last_update = std::time::Instant::now();
                                last_percent = percent;
                            }
                        }
                    }
                    _ => break,
                }
            }
        }
    }
    
    if !cancel_flag.load(Ordering::SeqCst) {
        if let Ok(status) = child.wait().await {
            if status.success() {
                let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("✅ <b>推流结束</b>:\n<code>{}</code>", escape_html(&filename))).parse_mode(ParseMode::Html).await;
            }
        }
    }
}

pub async fn action_concat(
    bot: Bot,
    msg: Message,
    files: Vec<PathBuf>,
    _state: Arc<AppState>,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
) {
    if files.len() < 2 {
        let _ = bot.send_message(msg.chat.id, "❌ 至少需要 2 个视频才能合并。").await;
        return;
    }

    let work_dir = files[0].parent().unwrap();
    let run_id = format!("{}_{}", std::process::id(), chrono::Utc::now().timestamp_millis());
    let list_file = work_dir.join(format!(".concat_list_{}.txt", run_id));
    
    let base_smart = smart_rename(&files[0]);
    let stem = Path::new(&base_smart).file_stem().unwrap().to_str().unwrap();
    let output_path = unique_path(&work_dir.join(format!("{}.mp4", stem)));
    let output_name = output_path.file_name().unwrap().to_string_lossy().to_string();

    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("⏳ <b>正在验证和准备拼接</b>...\n输出: <code>{}</code>", escape_html(&output_name)))
        .parse_mode(ParseMode::Html).await;

    let mut list_content = String::new();
    for f in &files {
        let escaped = f.to_str().unwrap().replace("'", "'\\''");
        list_content.push_str(&format!("file '{}'\n", escaped));
    }
    let _ = std::fs::write(&list_file, list_content);

    let mut child = Command::new("ffmpeg")
        .args(&[
            "-y", "-f", "concat", "-safe", "0",
            "-i", list_file.to_str().unwrap(),
            "-c", "copy", "-movflags", "+faststart",
            output_path.to_str().unwrap()
        ])
        .kill_on_drop(true)
        .spawn()
        .expect("生成 concat 进程失败");

    tokio::select! {
        _ = cancel_notify.notified() => {
            let _ = child.kill().await;
            let _ = std::fs::remove_file(&output_path);
            let _ = bot.edit_message_text(msg.chat.id, msg.id, "🛑 <b>合并任务已手动终止。</b>").parse_mode(ParseMode::Html).await;
        }
        res = child.wait() => {
            let _ = std::fs::remove_file(&list_file);
            match res {
                Ok(status) if status.success() => {
                    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("✅ <b>合并成功！</b>\n📁 新文件: <code>{}</code>", escape_html(&output_name))).parse_mode(ParseMode::Html).await;
                }
                _ => {
                    let _ = std::fs::remove_file(&output_path);
                    if !cancel_flag.load(Ordering::SeqCst) {
                        let _ = bot.edit_message_text(msg.chat.id, msg.id, "❌ <b>直连拼接失败。</b> 请尝试转码后再合并。").parse_mode(ParseMode::Html).await;
                    }
                }
            }
        }
    }
}

pub async fn action_convert(
    bot: Bot,
    msg: Message,
    files: Vec<PathBuf>,
    _state: Arc<AppState>,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
    _user_id: i64, // 前缀加上下划线表示故意不使用，消除编译器警告
) {
    let total = files.len();
    let mut success = 0;

    for (idx, file) in files.iter().enumerate() {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }

        let stem = file.file_stem().unwrap().to_str().unwrap();
        let out_path = unique_path(&file.with_file_name(format!("{}.mp4", stem)));
        let filename = file.file_name().unwrap().to_string_lossy().to_string();

        let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("🔄 <b>转换中</b> ({}/{})\n<code>{}</code> -> <code>.mp4</code>", idx + 1, total, escape_html(&filename)))
            .parse_mode(ParseMode::Html).await;

        let mut child = Command::new("ffmpeg")
            .args(&["-y", "-i", file.to_str().unwrap(), "-c", "copy", "-movflags", "+faststart", out_path.to_str().unwrap()])
            .kill_on_drop(true)
            .spawn()
            .expect("转码进程创建失败");

        tokio::select! {
            _ = cancel_notify.notified() => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&out_path);
                break;
            }
            res = child.wait() => {
                if let Ok(status) = res {
                    if status.success() {
                        success += 1;
                    } else {
                        let _ = std::fs::remove_file(&out_path);
                    }
                }
            }
        }
    }

    if cancel_flag.load(Ordering::SeqCst) {
        let _ = bot.edit_message_text(msg.chat.id, msg.id, "🛑 <b>批量转码已被终止！</b>").parse_mode(ParseMode::Html).await;
    } else {
        let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("✅ <b>批量转码完成！</b> 成功: {}/{}", success, total)).parse_mode(ParseMode::Html).await;
    }
}

pub async fn action_delete(
    bot: Bot,
    msg: Message,
    files: Vec<PathBuf>,
    state: Arc<AppState>,
) {
    let mut deleted = 0;
    for f in &files {
        if let Ok(path) = assert_path_inside_base(&state.config.base_dir, f) {
            if std::fs::remove_file(path).is_ok() {
                deleted += 1;
            }
        }
    }
    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("🗑️ <b>清理完成!</b> 成功删除 {} 个文件。", deleted))
        .parse_mode(ParseMode::Html).await;
}