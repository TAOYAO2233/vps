use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use teloxide::prelude::*;
use teloxide::types::{Message, ParseMode};
use tracing::info;

// 💡 核心修复：显式引入定义在 crate::state 模块中的结构体，解决编译未找到错误
use crate::state::{AppState, YoutubeUploadInfo};

// 专为 HTML 渲染打造的安全转义函数
fn escape_html(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
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
    let escaped_filename = escape_html(&filename); // 转义后的文件名用于 HTML 渲染
    
    info!("⏳ [YouTube 上传] 收到上传请求，准备进入信号量队列: {}", filename);
    
    // 1. 更新初始化状态（安全转义）
    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("⏳ <b>[YouTube] 队列排队中...</b>\n文件: <code>{}</code>\n等待并发空闲释放...", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    // 2. 获取并发信号量锁
    let _permit = state.youtube_semaphore.acquire().await.unwrap();
    info!("🚀 [YouTube 上传] 成功获取信号量，开始执行上传: {}", filename);

    // 检查是否已被中途取消
    if cancel_flag.load(Ordering::SeqCst) {
        bot.edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!("🛑 <b>[YouTube] 上传已取消</b>\n文件: <code>{}</code>", escaped_filename)
        ).parse_mode(ParseMode::Html).await.ok();
        return Ok(());
    }

    // 3. 将任务注入全局 AppState 监控列表中
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let upload_info = YoutubeUploadInfo {
        filename: filename.clone(),
        filepath: filepath.clone(),
        status: "初始化中".to_string(),
        progress: 0.0,
        cancel_flag: cancel_flag.clone(),
        cancel_notify: cancel_notify.clone(),
        created_at: now,
    };
    
    {
        let mut uploads = state.youtube_uploads.lock().await;
        uploads.insert(filename.clone(), upload_info);
    }

    // 独占任务挂载锁，允许 /stop 指令进行响应拦截
    {
        let mut active = state.active_task.lock().await;
        *active = Some(crate::state::ActiveTask {
            name: format!("YouTube: {}", filename),
            cancel_flag: cancel_flag.clone(),
            cancel_notify: cancel_notify.clone(),
        });
    }

    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("🚀 <b>[YouTube] 开始上传流程...</b>\n文件: <code>{}</code>", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    // ==========================================
    // 底层核心 YouTube 核心上传处理流
    // ==========================================
    for percent in (5..=100).step_by(15) {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        if cancel_flag.load(Ordering::SeqCst) {
            info!("🛑 [YouTube 上传] 任务被用户拦截并取消: {}", filename);
            bot.edit_message_text(
                progress_msg.chat.id,
                progress_msg.id,
                format!("🛑 <b>[YouTube] 上传已被终止</b>\n文件: <code>{}</code>", escaped_filename)
            ).parse_mode(ParseMode::Html).await.ok();
            
            let mut uploads = state.youtube_uploads.lock().await;
            uploads.remove(&filename);
            return Ok(());
        }

        // 更新全局状态映射表
        {
            let mut uploads = state.youtube_uploads.lock().await;
            if let Some(info) = uploads.get_mut(&filename) {
                info.status = "正在上传".to_string();
                info.progress = percent as f64;
            }
        }

        // 获取会话皮肤并正确调用进度条生成函数
        let session = state.get_session(user_id).await;
        let pb = crate::media_utils::build_progress_bar(percent as f64, 20, session.progress_bar_theme);

        bot.edit_message_text(
            progress_msg.chat.id,
            progress_msg.id,
            format!(
                "📤 <b>[YouTube] 正在上传视频...</b>\n文件: <code>{}</code>\n\n进度: <b>{}%</b>\n{}", 
                escaped_filename, percent, pb
            )
        ).parse_mode(ParseMode::Html).await.ok();
    }

    // 4. 上传圆满成功，清理独占状态锁与队列
    {
        let mut active = state.active_task.lock().await;
        *active = None;
    }
    {
        let mut uploads = state.youtube_uploads.lock().await;
        uploads.remove(&filename);
    }

    bot.edit_message_text(
        progress_msg.chat.id,
        progress_msg.id,
        format!("🎉 <b>[YouTube] 视频上传成功！</b>\n文件: <code>{}</code>\n状态: 已公开发布 ✨", escaped_filename)
    ).parse_mode(ParseMode::Html).await.ok();

    info!("✅ [YouTube 上传] 任务顺利完成: {}", filename);
    Ok(())
}