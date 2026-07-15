use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use teloxide::prelude::*;
use teloxide::types::{Message, ParseMode, LinkPreviewOptions};
use tracing::error;

use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};

use crate::state::{AppState, YoutubeUploadInfo};
use crate::media_utils::build_progress_bar;

fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
}

async fn get_local_access_token(token_path: &Path) -> Result<String, String> {
    let mut file = File::open(token_path).await.map_err(|e| format!("打开 API Token 凭证失败: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content).await.map_err(|e| format!("读取凭证文件失败: {}", e))?;
    
    let json: serde_json::Value = serde_json::from_str(&content).map_err(|e| format!("解析凭证 JSON 数据异常: {}", e))?;
    
    if let Some(token) = json.get("access_token").and_then(|v| v.as_str()) {
        Ok(token.to_string())
    } else if let Some(token) = json.get("token").and_then(|v| v.as_str()) {
        Ok(token.to_string())
    } else {
        Err("JSON 凭证中未定位到有效 access_token 字段".to_string())
    }
}

pub async fn start_youtube_upload(
    bot: Bot,
    mut progress_msg: Message,
    filepath: PathBuf,
    state: Arc<AppState>,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
    user_id: i64,
) -> ResponseResult<()> {
    let filename = filepath.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let escaped_filename = escape_html(&filename);
    
    let file_meta = match tokio::fs::metadata(&filepath).await {
        Ok(m) => m,
        Err(e) => {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 读取本地待传文件元数据失败: {}", e)).await.ok();
            return Ok(());
        }
    };
    let file_size = file_meta.len();
    if file_size == 0 {
        bot.edit_message_text(progress_msg.chat.id, progress_msg.id, "❌ 目标上传视频大小为 0 字节，已自动取消。").await.ok();
        return Ok(());
    }

    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("⏳ <b>[YouTube] 任务正在加入上传队列排队中...</b>\n文件: <code>{}</code>", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    let _permit = match state.youtube_semaphore.acquire().await {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(());
    }

    let now_sec = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    {
        let mut uploads = state.youtube_uploads.lock().await;
        uploads.insert(filename.clone(), YoutubeUploadInfo {
            filename: filename.clone(),
            filepath: filepath.clone(),
            status: "初始化云端会话中".to_string(),
            progress: 0.0,
            cancel_flag: cancel_flag.clone(),
            cancel_notify: cancel_notify.clone(),
            created_at: now_sec,
        });
    }

    let access_token = match get_local_access_token(&state.config.token_file).await {
        Ok(t) => t,
        Err(err) => {
            error!("⚠️ 读取 API Token 失败: {}", err);
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 无法读取 Google Access Token: <code>{}</code>", escape_html(&err))).parse_mode(ParseMode::Html).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    };

    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("🚀 <b>[YouTube] 正在向 Google 申请建立可续传分片会话...</b>\n文件: <code>{}</code>", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    let client = reqwest::Client::new();
    let init_url = "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status";
    
    let metadata_body = serde_json::json!({
        "snippet": {
            "title": filename.chars().take(95).collect::<String>(),
            "description": format!("Uploaded by Telegram Rust VPS Bot\nFile: {}", filename),
            "categoryId": "22"
        },
        "status": {
            "privacyStatus": "private" 
        }
    });

    let init_res = match client.post(init_url)
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header("X-Upload-Content-Length", file_size)
        .header("X-Upload-Content-Type", "video/mp4")
        .json(&metadata_body)
        .send().await 
    {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => {
            let err_text = res.text().await.unwrap_or_default();
            error!("初始化 YouTube 可续传接口请求失败: {}", err_text);
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 初始化 YouTube 接口失败 (Token 可能过期，请及时用脚本刷新凭证):\n<code>{}</code>", escape_html(&err_text))).parse_mode(ParseMode::Html).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
        Err(e) => {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 请求网络连接失败: {}", e)).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    };

    let upload_url = match init_res.headers().get("location").and_then(|h| h.to_str().ok()) {
        Some(url) => url.to_string(),
        None => {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, "❌ 未能从 Google 响应 Header 中成功提取 Location 上传地址通道。").await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    };

    let chunk_size = (state.config.youtube_upload_chunk_mb * 1024 * 1024) as u64; 
    let mut file = match File::open(&filepath).await {
        Ok(f) => f,
        Err(e) => {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 打开物理文件流异常: {}", e)).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    };

    let mut offset = 0u64;
    let mut last_update = std::time::Instant::now();

    while offset < file_size {
        if cancel_flag.load(Ordering::SeqCst) {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("🛑 <b>[YouTube] 上传任务已被取消</b>\n文件: <code>{}</code>", escaped_filename)).parse_mode(ParseMode::Html).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }

        let current_chunk = std::cmp::min(chunk_size, file_size - offset);
        let mut buffer = vec![0u8; current_chunk as usize];
        if let Err(e) = file.read_exact(&mut buffer).await {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 读取视频块 IO 数据段流异常: {}", e)).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }

        let content_range = format!("bytes {}-{}/{}", offset, offset + current_chunk - 1, file_size);
        
        let chunk_res = match client.put(&upload_url)
            .header("Content-Range", content_range)
            .header(CONTENT_LENGTH, current_chunk)
            .header(CONTENT_TYPE, "video/mp4")
            .body(buffer)
            .send().await 
        {
            Ok(r) => r,
            Err(e) => {
                bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 同步数据分片至 Google 网络中断: {}", e)).await.ok();
                let mut uploads = state.youtube_uploads.lock().await;
                uploads.remove(&filename);
                return Ok(());
            }
        };

        let status = chunk_res.status();
        
        // 核心逻辑修复：处理 HTTP 308 (Resume Incomplete) 中间状态与 200/201 完成状态
        if status.as_u16() == 308 {
            offset += current_chunk;
            let progress = (offset as f64 / file_size as f64) * 100.0;

            {
                let mut uploads = state.youtube_uploads.lock().await;
                if let Some(info) = uploads.get_mut(&filename) {
                    info.status = "极速上传网络传输中".to_string();
                    info.progress = progress;
                }
            }

            if last_update.elapsed().as_secs() >= 3 {
                let session = state.get_session(user_id).await;
                let pb = build_progress_bar(progress, 20, session.progress_bar_theme);
                bot.edit_message_text(
                    progress_msg.chat.id,
                    progress_msg.id,
                    format!("📤 <b>[YouTube] 正在高效同步至云端...</b>\n文件: <code>{}</code>\n\n进度: {}", escaped_filename, pb)
                ).parse_mode(ParseMode::Html).await.ok();
                last_update = std::time::Instant::now();
            }
        } else if status.is_success() || status.as_u16() == 201 {
            if let Ok(json) = chunk_res.json::<serde_json::Value>().await {
                if let Some(video_id) = json.get("id").and_then(|v| v.as_str()) {
                    let now_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                    
                    let success_text = format!(
                        "✅ <b>YouTube 云端发布成功！</b>\n\n\
                        🎬 <b>视频名称:</b> <code>{}</code>\n\
                        🕒 <b>上传时间:</b> <code>{}</code>\n\n\
                        📺 <b>观看链接:</b> https://youtu.be/{}\n\
                        🛠️ <b>Studio 后台:</b> https://studio.youtube.com/video/{}/edit",
                        escaped_filename, now_time, video_id, video_id
                    );

                    bot.edit_message_text(progress_msg.chat.id, progress_msg.id, success_text)
                        .parse_mode(ParseMode::Html)
                        .link_preview_options(LinkPreviewOptions {
                            is_disabled: true,
                            url: None,
                            prefer_small_media: false,
                            prefer_large_media: false,
                            show_above_text: false,
                        })
                        .await.ok();

                    let mut uploads = state.youtube_uploads.lock().await;
                    uploads.remove(&filename);
                    return Ok(());
                }
            }
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, "❌ 上传结束但解析 YouTube 分发 ID 失败。").await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        } else {
            let err_body = chunk_res.text().await.unwrap_or_default();
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ YouTube 拒绝接收分片请求 [HTTP {}]:\n<code>{}</code>", status.as_u16(), escape_html(&err_body))).parse_mode(ParseMode::Html).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    }

    let mut uploads = state.youtube_uploads.lock().await;
    uploads.remove(&filename);
    Ok(())
}