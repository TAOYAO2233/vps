use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use reqwest::header::CONTENT_LENGTH;

use crate::state::{AppState, YoutubeUploadInfo};
use crate::media_utils::build_progress_bar;

fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
}

pub async fn start_youtube_upload(
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
    let file_size = std::fs::metadata(&filepath).map(|m| m.len()).unwrap_or(0);

    {
        let mut map = state.youtube_uploads.lock().await;
        map.insert(filepath.to_str().unwrap().to_string(), YoutubeUploadInfo {
            filename: filename.clone(),
            filepath: filepath.clone(),
            status: "排队中".to_string(),
            progress: 0.0,
            cancel_flag: cancel_flag.clone(),
            cancel_notify: cancel_notify.clone(),
            created_at: chrono::Utc::now().timestamp() as f64,
        });
    }

    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("⏳ <b>排队等待 YouTube 分发限制</b>...\n<code>{}</code>", escape_html(&filename)))
        .parse_mode(ParseMode::Html).await;

    let _permit = match state.youtube_semaphore.acquire().await {
        Ok(p) => p,
        Err(_) => return,
    };

    if cancel_flag.load(Ordering::SeqCst) {
        remove_upload_record(&filepath, &state).await;
        return;
    }

    update_upload_status(&filepath, "上传中", 0.0, &state).await;
    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("🚀 <b>正在连接 YouTube 上传端口</b>...\n<code>{}</code>", escape_html(&filename)))
        .parse_mode(ParseMode::Html).await;

    let access_token = "YOUR_OAUTH_ACCESS_TOKEN"; 
    let client = reqwest::Client::new();
    
    let init_url = "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status";
    let body_json = serde_json::json!({
        "snippet": { "title": filename.chars().take(95).collect::<String>(), "categoryId": "22" },
        "status": { "privacyStatus": "private" }
    });

    let res = client.post(init_url)
        .bearer_auth(access_token)
        .header("X-Upload-Content-Length", file_size)
        .header("X-Upload-Content-Type", "video/mp4")
        .json(&body_json)
        .send().await;

    let upload_url = match res {
        Ok(r) if r.status().is_success() => {
            r.headers().get("location").and_then(|h| h.to_str().ok()).unwrap_or("").to_string()
        }
        _ => {
            let _ = bot.edit_message_text(msg.chat.id, msg.id, "❌ 初始化 YouTube 会话失败，请检查 token.json！").await;
            remove_upload_record(&filepath, &state).await;
            return;
        }
    };

    let chunk_size = (state.config.youtube_upload_chunk_mb * 1024 * 1024) as u64;
    let mut file = File::open(&filepath).await.unwrap();
    let mut offset = 0u64;
    let mut last_update = std::time::Instant::now();

    while offset < file_size {
        if cancel_flag.load(Ordering::SeqCst) {
            let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("🛑 <b>YouTube 上传已取消</b>:\n<code>{}</code>", escape_html(&filename))).parse_mode(ParseMode::Html).await;
            remove_upload_record(&filepath, &state).await;
            return;
        }

        let current_chunk = std::cmp::min(chunk_size, file_size - offset);
        let mut buffer = vec![0u8; current_chunk as usize];
        let _ = file.read_exact(&mut buffer).await;

        let content_range = format!("bytes {}-{}/{}", offset, offset + current_chunk - 1, file_size);
        
        let _chunk_res = client.put(&upload_url)
            .header("Content-Range", content_range)
            .header(CONTENT_LENGTH, current_chunk)
            .body(buffer)
            .send().await;

        offset += current_chunk;
        let progress = (offset as f64 / file_size as f64) * 100.0;
        update_upload_status(&filepath, "上传中", progress, &state).await;

        if last_update.elapsed().as_secs() >= 3 || offset >= file_size {
            let bar = build_progress_bar(progress, 20, theme);
            let text = format!("☁️ <b>上传 YouTube</b>:\n<code>{}</code>\n\n{}", escape_html(&filename), bar);
            let _ = bot.edit_message_text(msg.chat.id, msg.id, text).parse_mode(ParseMode::Html).await;
            last_update = std::time::Instant::now();
        }
    }

    let _ = bot.edit_message_text(msg.chat.id, msg.id, format!("✅ <b>上传完成！</b>\n🎬 视频名称: <code>{}</code>", escape_html(&filename))).parse_mode(ParseMode::Html).await;
    remove_upload_record(&filepath, &state).await;
}

async fn update_upload_status(filepath: &Path, status: &str, progress: f64, state: &AppState) {
    let mut map = state.youtube_uploads.lock().await;
    if let Some(info) = map.get_mut(filepath.to_str().unwrap()) {
        info.status = status.to_string();
        info.progress = progress;
    }
}

async fn remove_upload_record(filepath: &Path, state: &AppState) {
    let mut map = state.youtube_uploads.lock().await;
    map.remove(filepath.to_str().unwrap());
}