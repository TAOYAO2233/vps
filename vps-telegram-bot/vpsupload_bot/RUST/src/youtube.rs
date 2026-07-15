use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use teloxide::prelude::*;
use teloxide::types::{Message, ParseMode, LinkPreviewOptions};
use tracing::error;

// 💡 核心修复：显式导入请求头所需的核心 HTTP 大写常量
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};

use crate::state::{AppState, YoutubeUploadInfo};
use crate::media_utils::build_progress_bar;

// HTML 敏感字符安全转义函数
fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
}

// 动态从本地 token.json 提取真实的 Google API Access Token
async fn get_local_access_token(token_path: &std::path::Path) -> Result<String, String> {
    let mut file = File::open(token_path).await.map_err(|e| format!("打开 token.json 失败: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content).await.map_err(|e| format!("读取 token.json 失败: {}", e))?;
    
    let json: serde_json::Value = serde_json::from_str(&content).map_err(|e| format!("解析 token.json 失败: {}", e))?;
    
    if let Some(token) = json.get("access_token").and_then(|v| v.as_str()) {
        Ok(token.to_string())
    } else if let Some(token) = json.get("token").and_then(|v| v.as_str()) {
        Ok(token.to_string())
    } else {
        Err("token.json 中未找到 access_token 字段".to_string())
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
    
    // 1. 获取本地文件大小元数据
    let file_meta = match tokio::fs::metadata(&filepath).await {
        Ok(m) => m,
        Err(e) => {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 读取文件失败: {}", e)).await.ok();
            return Ok(());
        }
    };
    let file_size = file_meta.len();

    // 2. 进入排队队列提示
    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("⏳ <b>[YouTube] 队列排队中...</b>\n文件: <code>{}</code>", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    let _permit = state.youtube_semaphore.acquire().await.unwrap();

    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(());
    }

    // 3. 注册任务监控状态
    let now_sec = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    {
        let mut uploads = state.youtube_uploads.lock().await;
        uploads.insert(filename.clone(), YoutubeUploadInfo {
            filename: filename.clone(),
            filepath: filepath.clone(),
            status: "初始化中".to_string(),
            progress: 0.0,
            cancel_flag: cancel_flag.clone(),
            cancel_notify: cancel_notify.clone(),
            created_at: now_sec,
        });
    }

    // 4. 读取配置文件指定的本地 token 路径
    let token_file_path = std::path::Path::new("token.json");
    let access_token = match get_local_access_token(token_file_path).await {
        Ok(t) => t,
        Err(err) => {
            error!("⚠️ 获取 Access Token 失败: {}", err);
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("❌ 凭证错误: <code>{}</code>", escape_html(&err))).parse_mode(ParseMode::Html).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    };

    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("🚀 <b>[YouTube] 正在建立远程分片会话...</b>\n文件: <code>{}</code>", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    let client = reqwest::Client::new();
    let init_url = "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status";
    
    let metadata_body = serde_json::json!({
        "snippet": {
            "title": filename.chars().take(95).collect::<String>(),
            "description": format!("Uploaded by VPS Telegram Bot\nFile: {}", filename),
            "categoryId": "22"
        },
        "status": {
            "privacyStatus": "private" 
        }
    });

    // 5. 向 Google 请求 Resumable 上传通道 URL
    let init_res = match client.post(init_url)
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header("X-Upload-Content-Length", file_size)
        .header("X-Upload-Content-Type", "video/mp4")
        .json(&metadata_body)
        .send().await 
    {
        Ok(res) if res.status().is_success() => res,
        _ => {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, "❌ 初始化 YouTube 接口失败，可能您的 token.json 已过期，请运行 Python 脚本刷新 Token 凭证。").await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }
    };

    let upload_url = init_res.headers().get("location").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();

    // 6. 核心循环：执行真实物理分片上传
    let chunk_size = (state.config.youtube_upload_chunk_mb * 1024 * 1024) as u64; 
    let mut file = File::open(&filepath).await.unwrap();
    let mut offset = 0u64;
    let mut last_update = std::time::Instant::now();

    while offset < file_size {
        if cancel_flag.load(Ordering::SeqCst) {
            bot.edit_message_text(progress_msg.chat.id, progress_msg.id, format!("🛑 <b>[YouTube] 上传被取消</b>\n文件: <code>{}</code>", escaped_filename)).parse_mode(ParseMode::Html).await.ok();
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }

        let current_chunk = std::cmp::min(chunk_size, file_size - offset);
        let mut buffer = vec![0u8; current_chunk as usize];
        file.read_exact(&mut buffer).await.unwrap();

        let content_range = format!("bytes {}-{}/{}", offset, offset + current_chunk - 1, file_size);
        
        let chunk_res = client.put(&upload_url)
            .header("Content-Range", content_range)
            .header(CONTENT_LENGTH, current_chunk)
            .header(CONTENT_TYPE, "video/mp4")
            .body(buffer)
            .send().await;

        offset += current_chunk;
        let progress = (offset as f64 / file_size as f64) * 100.0;

        {
            let mut uploads = state.youtube_uploads.lock().await;
            if let Some(info) = uploads.get_mut(&filename) {
                info.status = "正在传输".to_string();
                info.progress = progress;
            }
        }

        if last_update.elapsed().as_secs() >= 3 || offset >= file_size {
            let session = state.get_session(user_id).await;
            let pb = build_progress_bar(progress, 20, session.progress_bar_theme);
            bot.edit_message_text(
                progress_msg.chat.id,
                progress_msg.id,
                format!("📤 <b>[YouTube] 正在真实同步至云端...</b>\n文件: <code>{}</code>\n\n进度: {}", escaped_filename, pb)
            ).parse_mode(ParseMode::Html).await.ok();
            last_update = std::time::Instant::now();
        }

        // 7. 处理最后一块片上传完毕后 Google 返回的真实视频元数据
        if offset >= file_size {
            if let Ok(res) = chunk_res {
                if res.status().is_success() || res.status().as_u16() == 201 {
                    if let Ok(json) = res.json::<serde_json::Value>().await {
                        if let Some(video_id) = json.get("id").and_then(|v| v.as_str()) {
                            let now_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                            
                            let success_text = format!(
                                "✅ <b>YouTube 上传成功！</b>\n\n\
                                🎬 <b>视频名称:</b> <code>{}</code>\n\
                                🕒 <b>上传时间:</b> <code>{}</code>\n\n\
                                📺 <b>观看链接:</b> https://youtu.be/{}\n\
                                🛠️ <b>Studio 链接:</b> https://studio.youtube.com/video/{}/edit",
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
                }
            }
        }
    }

    bot.edit_message_text(progress_msg.chat.id, progress_msg.id, "❌ 上传结束，未能正常捕获 YouTube 分发 ID 响应。").await.ok();
    let mut uploads = state.youtube_uploads.lock().await;
    uploads.remove(&filename);
    Ok(())
}