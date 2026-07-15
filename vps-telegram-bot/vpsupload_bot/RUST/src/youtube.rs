use crate::config::Config;
use crate::media_utils::{assert_path_inside_base, build_progress_bar};
use crate::task_manager::{save_persisted_queue, AppState, YoutubeUploadInfo};
use anyhow::{anyhow, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::Client;
use serde_json::Value;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use teloxide::payloads::EditMessageTextSetters;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

pub async fn start_youtube_uploads(
    bot: Bot,
    q: CallbackQuery,
    files: Vec<PathBuf>,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    if state.active_task.lock().await.is_some() {
        bot.answer_callback_query(q.id).text("已有独占任务，请等待完成或停用。").show_alert(true).await?;
        return Ok(());
    }

    let valid_files: Vec<PathBuf> = files.into_iter()
        .filter_map(|f| assert_path_inside_base(&config.base_dir, &f).ok())
        .filter(|p| p.is_file())
        .collect();

    if valid_files.is_empty() {
        bot.answer_callback_query(q.id).text("❌ 没有可上传的文件。").show_alert(true).await?;
        return Ok(());
    }

    bot.answer_callback_query(q.id).await?;
    let user_id = q.from.id.0 as i64;
    let chat_id = q.message.as_ref().map(|m| m.chat.id.0).unwrap_or(config.admin_id);

    for path in valid_files {
        let filename = path.file_name().unwrap().to_str().unwrap().to_string();
        let task_id = format!("yt_{}_{}", chrono::Utc::now().timestamp_millis(), abs_hash(&path) % 10000);
        let (tx, rx) = watch::channel(false);

        let msg = bot.send_message(ChatId(chat_id), format!("⏳ 创建 YouTube 上传: `{}`", filename))
            .parse_mode(ParseMode::MarkdownV2).await?;

        let info = YoutubeUploadInfo {
            filename: filename.clone(),
            path: path.to_string_lossy().to_string(),
            chat_id,
            user_id,
            created_at: chrono::Utc::now().timestamp() as f64,
            status: "排队中".to_string(),
            progress: 0.0,
            cancel_tx: Some(tx),
        };

        {
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.insert(task_id.clone(), info);
            save_persisted_queue(&config.youtube_upload_queue_file, &*uploads);
        }

        let bot_cloned = bot.clone();
        let config_cloned = config.clone();
        let state_cloned = state.clone();
        let path_cloned = path.clone();

        tokio::spawn(async move {
            let _ = upload_worker(bot_cloned, msg.id, ChatId(chat_id), path_cloned, task_id.clone(), rx, config_cloned, state_cloned).await;
        });
    }

    if let Some(msg) = q.message {
        bot.edit_message_text(msg.chat.id, msg.id, "✅ 已启动 YouTube 上传队列，发送 /uploads 查看。").await?;
    }
    Ok(())
}

async fn upload_worker(
    bot: Bot,
    msg_id: MessageId,
    chat_id: ChatId,
    file_path: PathBuf,
    task_id: String,
    mut rx: watch::Receiver<bool>,
    config: Arc<Config>,
    state: Arc<AppState>,
) -> Result<()> {
    let filename = file_path.file_name().unwrap().to_str().unwrap().to_string();
    let permit = state.youtube_semaphore.acquire().await?;

    if *rx.borrow() {
        update_status(&state, &config.youtube_upload_queue_file, &task_id, "已取消", 0.0).await;
        let _ = bot.edit_message_text(chat_id, msg_id, format!("🛑 **上传已取消**: `{}`", filename)).parse_mode(ParseMode::MarkdownV2).await;
        return Ok(());
    }

    update_status(&state, &config.youtube_upload_queue_file, &task_id, "上传中", 0.0).await;
    let _ = bot.edit_message_text(chat_id, msg_id, format!("🚀 **开始上传 YouTube**: `{}`", filename)).parse_mode(ParseMode::MarkdownV2).await;

    let token_data = fs::read_to_string(&config.token_file)?;
    let json: Value = serde_json::from_str(&token_data)?;
    let access_token = json["access_token"].as_str().ok_or_else(|| anyhow!("无法解析 access_token"))?;

    let client = Client::new();
    let file_size = fs::metadata(&file_path)?.len();
    let title = file_path.file_stem().unwrap().to_str().unwrap().chars().take(95).collect::<String>();

    let init_body = serde_json::json!({
        "snippet": { "title": title, "categoryId": "22" },
        "status": { "privacyStatus": "private", "selfDeclaredMadeForKids": false }
    });

    let res = client.post("https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status")
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header(CONTENT_TYPE, "application/json; charset=UTF-8")
        .header("X-Upload-Content-Length", file_size.to_string())
        .header("X-Upload-Content-Type", "video/*")
        .json(&init_body)
        .send().await?;

    let upload_url = res.headers().get("location").and_then(|h| h.to_str().ok()).ok_or_else(|| anyhow!("未获取到 resumable upload url"))?.to_string();

    let mut file = File::open(&file_path).await?;
    let chunk_size = (config.youtube_upload_chunk_mb * 1024 * 1024) as u64;
    let mut uploaded = 0u64;
    let mut buffer = vec![0u8; chunk_size as usize];
    let mut last_update = Instant::now();

    while uploaded < file_size {
        if *rx.borrow() {
            update_status(&state, &config.youtube_upload_queue_file, &task_id, "已取消", 0.0).await;
            let _ = bot.edit_message_text(chat_id, msg_id, format!("🛑 **上传手动终止**: `{}`", filename)).parse_mode(ParseMode::MarkdownV2).await;
            return Ok(());
        }

        let to_read = std::cmp::min(chunk_size, file_size - uploaded) as usize;
        let n = file.read_exact(&mut buffer[..to_read]).await?;
        let end_byte = uploaded + n as u64 - 1;

        let res = client.put(&upload_url)
            .header(AUTHORIZATION, format!("Bearer {}", access_token))
            .header(CONTENT_TYPE, "video/*")
            .header(CONTENT_LENGTH, n)
            .header("Content-Range", format!("bytes {}-{}/{}", uploaded, end_byte, file_size))
            .body(buffer[..n].to_vec())
            .send().await?;

        uploaded += n as u64;
        let p = (uploaded as f64 / file_size as f64) * 100.0;
        update_status(&state, &config.youtube_upload_queue_file, &task_id, "上传中", p).await;

        if last_update.elapsed().as_secs() >= 3 || uploaded == file_size {
            let bar = build_progress_bar(p, 20);
            let _ = bot.edit_message_text(chat_id, msg_id, format!("☁️ **上传 YouTube**: `{}`\n\n`{}`", filename, bar)).parse_mode(ParseMode::MarkdownV2).await;
            last_update = Instant::now();
        }

        if res.status().is_success() {
            let resp_json: Value = res.json().await?;
            let vid_id = resp_json["id"].as_str().unwrap_or("unknown");
            update_status(&state, &config.youtube_upload_queue_file, &task_id, "完成", 100.0).await;
            let text = format!("✅ **上传成功！**\n🎬 视频: `{}`\n📺 链接: `https://youtu.be/{}`", filename, vid_id);
            let _ = bot.edit_message_text(chat_id, msg_id, text).parse_mode(ParseMode::MarkdownV2).await;
            break;
        }
    }

    drop(permit);
    let mut uploads = state.youtube_uploads.lock().await;
    uploads.remove(&task_id);
    save_persisted_queue(&config.youtube_upload_queue_file, &*uploads);
    Ok(())
}

async fn update_status(state: &Arc<AppState>, queue_path: &Path, task_id: &str, status: &str, progress: f64) {
    let mut uploads = state.youtube_uploads.lock().await;
    if let Some(info) = uploads.get_mut(task_id) {
        info.status = status.to_string();
        info.progress = progress;
        save_persisted_queue(queue_path, &*uploads);
    }
}

fn abs_hash(path: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}