use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use crate::{error::ApiError, speed::SpeedEstimate, state::AppState};
use super::text::{short, field, number, optional, size_text, split_pages, MESSAGE_LIMIT};

const MAIN_TEXT: &str = "Bililive-go 控制面板\n发送直播间 URL 即可直接添加任务。";

async fn build_main_keyboard(state: &AppState) -> Result<InlineKeyboardMarkup, ApiError> {
    let lives = state.api.get_lives().await?;
    let mut keyboard = Vec::new();

    for item in lives {
        let status_icon = if item.recording {
            "🔴"
        } else if item.listening {
            "🟢"
        } else {
            "⚪"
        };

        let host_name = item.host_name.unwrap_or_else(|| "未知主播".to_string());
        let room_name = item.room_name.unwrap_or_else(|| "无标题".to_string());

        // 限制标题长度防止按钮过长
        let mut truncated_room = room_name;
        if truncated_room.chars().count() > 12 {
            truncated_room = truncated_room.chars().take(12).collect::<String>() + "...";
        }

        let btn_text = format!("{} {} | {}", status_icon, host_name, truncated_room);
        let callback_data = format!("view_{}", item.id);

        keyboard.push(vec![InlineKeyboardButton::callback(btn_text, callback_data)]);
    }

    keyboard.push(vec![InlineKeyboardButton::callback("🔄 刷新状态", "refresh_main")]);
    Ok(InlineKeyboardMarkup::new(keyboard))
}

pub(crate) fn detail_buttons(id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("🔄 刷新详情", format!("view_{}", id)), InlineKeyboardButton::callback("查看流 URL / Headers", format!("stream_{}_0", id))],
        vec![InlineKeyboardButton::callback("▶️ 开启监控", format!("op_start_{}", id)), InlineKeyboardButton::callback("⏹️ 停止监控", format!("op_stop_{}", id))],
        vec![InlineKeyboardButton::callback("🗑️ 删除任务", format!("conf_del_{}", id)), InlineKeyboardButton::callback("🔙 返回列表", "refresh_main")],
    ])
}
pub(crate) async fn detail_panel(state: &AppState, id: &str) -> (String, InlineKeyboardMarkup) {
    let detail = match state.api.get_detail(id).await {
        Ok(detail) => detail, Err(e) => return (format!("读取详情失败：{}", e), detail_buttons(id)),
    };
    let empty_status = serde_json::Value::Null;
    let status = detail.recorder_status.as_ref().unwrap_or(&empty_status);
    let recording = if detail.recording == Some(true) { "🔴 录制中" }
        else if detail.recording_preparing == Some(true) { "🟡 准备中" }
        else if detail.recording == Some(false) { "⚪ 未录制" } else { "暂无数据" };
    let listening = match detail.listening { Some(true) => "🟢 监控中", Some(false) => "⚪ 已停止", None => "暂无数据" };
    let resolution = if field(status, "actual_resolution") != "暂无数据" {
        let suffix = match status.get("resolution_match").and_then(|status| status.as_bool()) {
            Some(true) => " ✓ 与平台一致", Some(false) => " ⚠ 与平台不一致", None => "",
        };
        format!("{}{}", field(status, "actual_resolution"), suffix)
    } else if field(status, "probe_status") == "pending" { "探测中".into() } else { "暂无数据".into() };
    let size = number(status, "file_size").map(size_text).unwrap_or_else(|| "暂无数据".into());
    let bitrate = number(status, "actual_video_bitrate").map(|n| format!("{:.0} kbps", n)).unwrap_or_else(|| "暂无数据".into());
    let fps = number(status, "actual_frame_rate").map(|n| format!("{:.1} fps", n)).unwrap_or_else(|| "暂无数据".into());
    let estimate = state.speed.lock().await.estimate(id, &detail);
    let speed = match estimate {
        SpeedEstimate::Unavailable => "暂无数据".into(),
        SpeedEstimate::Sampling => "采样中，请稍后刷新".into(),
        SpeedEstimate::Rate { bytes_per_second, seconds } =>
            format!("{}/s（估算，近 {:.0} 秒）", size_text(bytes_per_second), seconds),
    };
    let mut text = format!(
        "主播：{}\n房间：{}\n链接：{}\n\n监控状态：{}\n录制状态：{}\n\n实际分辨率：{}\n视频编码：{}\n视频码率：{}\n帧率：{}\n下载速度：{}\n当前文件大小：{}\n录制路径：\n{}\n\n开播时间：{}\n录制开始：{}\n（时间沿用 Bililive-go 服务器时间）",
        short(optional(detail.host_name.as_deref()), 120), short(optional(detail.room_name.as_deref()), 200), short(optional(detail.raw_url.as_deref()), 400),
        listening, recording, short(&resolution, 120), short(&field(status, "actual_video_codec"), 80), bitrate, fps, speed, size,
        short(&field(status, "file_path"), 900), short(optional(detail.live_start_time.as_deref()), 80), short(optional(detail.last_record_time.as_deref()), 80),
    );
    if field(status, "actual_resolution") == "暂无数据" && field(status, "stream_resolution") != "暂无数据" {
        text.push_str(&format!("\n平台声明分辨率：{}（非实际探测值）", short(&field(status, "stream_resolution"), 100)));
    }
    if field(status, "probe_message") != "暂无数据" { text.push_str(&format!("\n探测说明：{}", short(&field(status, "probe_message"), 200))); }
    (short(&text, MESSAGE_LIMIT), detail_buttons(id))
}
pub(crate) async fn stream_panel(state: &AppState, id: &str, page: usize) -> (String, InlineKeyboardMarkup) {
    let back = vec![InlineKeyboardButton::callback("返回详情", format!("view_{}", id))];
    let detail = match state.api.get_detail(id).await {
        Ok(detail) => detail, Err(e) => return (format!("读取流信息失败：{}", e), InlineKeyboardMarkup::new(vec![back])),
    };
    let empty_status = serde_json::Value::Null;
    let status = detail.recorder_status.as_ref().unwrap_or(&empty_status);
    // Preserve server-side redaction. Never reconstruct sensitive headers.
    let headers = match status.get("stream_headers").filter(|status| !status.is_null()) {
        Some(status) => serde_json::to_string_pretty(status).unwrap_or_else(|_| "暂无数据".into()), None => "暂无数据".into(),
    };
    let pages = split_pages(&format!("录制流 URL：\n{}\n\nHeaders（保留服务器脱敏结果）：\n{}", field(status, "stream_url"), headers));
    let index = page.min(pages.len() - 1);
    let mut nav = Vec::new();
    if index > 0 { nav.push(InlineKeyboardButton::callback("上一页", format!("stream_{}_{}", id, index - 1))); }
    if index + 1 < pages.len() { nav.push(InlineKeyboardButton::callback("下一页", format!("stream_{}_{}", id, index + 1))); }
    let mut rows = Vec::new();
    if !nav.is_empty() { rows.push(nav); }
    rows.push(back);
    (format!("流信息 {}/{}（点击时读取）\n\n{}", index + 1, pages.len(), pages[index]), InlineKeyboardMarkup::new(rows))
}
pub(crate) async fn main_panel(state: &AppState) -> (String, InlineKeyboardMarkup) {
    match build_main_keyboard(state).await {
        Ok(k) => (MAIN_TEXT.into(), k),
        Err(e) => (format!("读取任务列表失败：{}", e), InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("重试", "refresh_main")]])),
    }
}
