use dotenvy::dotenv;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

// ================= 全局配置加载 =================
struct Config {
    api_base_url: String,
    admin_ids: Vec<i64>,
    polling_interval: u64,
}

static CONFIG: Lazy<Config> = Lazy::new(|| {
    dotenv().ok();
    let api_base_url = env::var("API_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:9000/api".to_string());
    let admin_ids_raw = env::var("ADMIN_IDS").unwrap_or_default();
    let admin_ids: Vec<i64> = admin_ids_raw
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    let polling_interval = env::var("POLLING_INTERVAL")
        .unwrap_or_else(|_| "30".to_string())
        .parse::<u64>()
        .unwrap_or(30);

    Config {
        api_base_url,
        admin_ids,
        polling_interval,
    }
});

// ================= 数据结构定义 =================
#[derive(Debug, Serialize, Deserialize, Clone)]
struct LiveTask {
    id: String,
    url: Option<String>,         // 部分API可能在不同字段中返回URL
    live_url: Option<String>,    // 兼容可能存在的不同字段
    host_name: Option<String>,
    room_name: Option<String>,
    recording: bool,
    listening: bool,
}

#[derive(Serialize)]
struct AddLivePayload {
    url: String,
    listen: bool,
}

// ================= API 交互层 =================
struct BiliLiveClient;

impl BiliLiveClient {
    async fn request<T, R>(method: reqwest::Method, path: &str, body: Option<&T>) -> Option<R>
    where
        T: Serialize + ?Sized,
        R: for<'de> Deserialize<'de>,
    {
        let client = Client::new();
        let url = format!("{}{}", CONFIG.api_base_url, path);
        let mut req = client.request(method, &url).timeout(Duration::from_secs(10));

        if let Some(b) = body {
            req = req.json(b);
        }

        match req.send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    log::error!("API 响应错误状态码: {}", resp.status());
                    return None;
                }
                // 若无返回值（例如 PUT 返回200空内容），尝试解析
                match resp.json::<R>().await {
                    Ok(data) => Some(data),
                    Err(e) => {
                        log::warn!("API 解析响应失败（或无返回值）: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                log::error!("API 请求失败: {}", e);
                return None;
            }
        }
    }

    async fn sync_config() {
        // PUT /config 通常返回空
        let _: Option<serde_json::Value> = Self::request(reqwest::Method::PUT, "/config", None::<&()>).await;
    }

    async fn get_lives() -> Vec<LiveTask> {
        Self::request::<(), Vec<LiveTask>>(reqwest::Method::GET, "/lives", None).await.unwrap_or_default()
    }

    async fn add_live(url: &str) {
        let payload = vec![AddLivePayload {
            url: url.to_string(),
            listen: true,
        }];
        let _: Option<serde_json::Value> = Self::request(reqwest::Method::POST, "/lives", Some(&payload)).await;
        Self::sync_config().await;
    }

    async fn delete_live(live_id: &str) {
        let path = format!("/lives/{}", live_id);
        let _: Option<serde_json::Value> = Self::request(reqwest::Method::DELETE, &path, None::<&()>).await;
        Self::sync_config().await;
    }

    async fn control_task(action: &str, live_id: &str) {
        let path = format!("/lives/{}/{}", live_id, action);
        let _: Option<serde_json::Value> = Self::request(reqwest::Method::GET, &path, None::<&()>).await;
        Self::sync_config().await;
    }
}

// ================= 键盘与界面构建 =================
async fn build_main_keyboard() -> InlineKeyboardMarkup {
    let lives = BiliLiveClient::get_lives().await;
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
    InlineKeyboardMarkup::new(keyboard)
}

async fn build_detail_keyboard(live_id: &str) -> (String, Option<InlineKeyboardMarkup>) {
    let lives = BiliLiveClient::get_lives().await;
    let target = lives.into_iter().find(|l| l.id == live_id);

    match target {
        None => ("⚠️ 该任务已不存在。".to_string(), None),
        Some(item) => {
            let status_desc = if item.recording {
                "🔴 录制中"
            } else if item.listening {
                "🟢 正在监听"
            } else {
                "⚪ 停止中"
            };

            let live_url = item.live_url.or(item.url).unwrap_or_default();
            let info_text = format!(
                "👤 **主播**: {}\n📺 **房间**: {}\n📊 **当前状态**: {}\n🔗 **链接**: {}",
                item.host_name.unwrap_or_else(|| "未知".to_string()),
                item.room_name.unwrap_or_else(|| "无标题".to_string()),
                status_desc,
                live_url
            );

            let keyboard = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("▶️ 开启监听", format!("op_start_{}", live_id)),
                    InlineKeyboardButton::callback("⏹️ 停止录制", format!("op_stop_{}", live_id)),
                ],
                vec![
                    InlineKeyboardButton::callback("🗑️ 删除任务", format!("conf_del_{}", live_id)),
                    InlineKeyboardButton::callback("🔙 返回主列表", "refresh_main"),
                ],
            ]);

            (info_text, Some(keyboard))
        }
    }
}

// ================= 辅助函数：权限验证 =================
fn is_admin(user_id: UserId) -> bool {
    CONFIG.admin_ids.contains(&(user_id.0 as i64))
}

// ================= 消息处理器逻辑 =================
async fn handle_message(bot: Bot, msg: Message) -> ResponseResult<()> {
    let user_id = match msg.from() {
        Some(user) => user.id,
        None => return Ok(()),
    };

    if !is_admin(user_id) {
        log::warn!("非法访问尝试: User {:?}", user_id);
        bot.send_message(msg.chat.id, "⛔️ **权限拒绝**：您未被授权操作此机器人。")
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.trim(),
        None => return Ok(()),
    };

    if text == "/start" {
        let markup = build_main_keyboard().await;
        bot.send_message(msg.chat.id, "🛠 **Bililive-go 控制面板**\n发送直播间 URL 即可直接添加任务。")
            .reply_markup(markup)
            .await?;
    } else if text.contains("http") {
        let sent_msg = bot.send_message(msg.chat.id, "⌛️ 正在解析并添加任务...").await?;
        BiliLiveClient::add_live(text).await;
        let markup = build_main_keyboard().await;
        bot.edit_message_text(sent_msg.chat.id, sent_msg.id, format!("✅ 任务添加成功！\nURL: {}", text))
            .reply_markup(markup)
            .await?;
    } else {
        bot.send_message(msg.chat.id, "❌ 请发送有效的直播间 URL。").await?;
    }

    Ok(())
}

async fn handle_callback_query(bot: Bot, q: CallbackQuery) -> ResponseResult<()> {
    let user_id = q.from.id;
    if !is_admin(user_id) {
        bot.answer_callback_query(q.id).text("⛔️ 权限拒绝").await?;
        return Ok(());
    }

    bot.answer_callback_query(q.id).await?;

    let data = match q.data {
        Some(d) => d,
        None => return Ok(()),
    };

    let msg = match q.message {
        Some(m) => m,
        None => return Ok(()),
    };

    if data == "refresh_main" {
        let markup = build_main_keyboard().await;
        bot.edit_message_reply_markup(msg.chat.id, msg.id)
            .reply_markup(markup)
            .await?;
    } else if data.starts_with("view_") {
        let l_id = data.trim_start_matches("view_");
        let (text, markup) = build_detail_keyboard(l_id).await;
        if let Some(markup) = markup {
            bot.edit_message_text(msg.chat.id, msg.id, text)
                .reply_markup(markup)
                .await?;
        }
    } else if data.starts_with("op_") {
        let parts: Vec<&str> = data.split('_').collect();
        if parts.len() == 3 {
            let action = parts[1];
            let l_id = parts[2];
            BiliLiveClient::control_task(action, l_id).await;
            let (text, markup) = build_detail_keyboard(l_id).await;
            if let Some(markup) = markup {
                bot.edit_message_text(msg.chat.id, msg.id, text)
                    .reply_markup(markup)
                    .await?;
            }
        }
    } else if data.starts_with("conf_del_") {
        let l_id = data.trim_start_matches("conf_del_");
        BiliLiveClient::delete_live(l_id).await;
        let markup = build_main_keyboard().await;
        bot.edit_message_text(msg.chat.id, msg.id, "✅ 任务已删除并同步配置。")
            .reply_markup(markup)
            .await?;
    }

    Ok(())
}

// ================= 状态检测轮询线程 =================
async fn start_monitor_loop(bot: Bot) {
    let mut last_rec_state: HashMap<String, bool> = HashMap::new();

    loop {
        tokio::time::sleep(Duration::from_secs(CONFIG.polling_interval)).await;
        log::debug!("开始轮询开播状态...");

        let lives = BiliLiveClient::get_lives().await;
        for live in lives {
            let l_id = live.id;
            let is_recording = live.recording;
            let was_recording = *last_rec_state.get(&l_id).unwrap_or(&false);

            // 状态转变：从 False -> True (开始录制)
            if !was_recording && is_recording {
                let live_url = live.live_url.or(live.url).unwrap_or_default();
                let host_name = live.host_name.unwrap_or_else(|| "未知主播".to_string());
                let room_name = live.room_name.unwrap_or_else(|| "无标题".to_string());

                let notify_text = format!(
                    "🚨 **开播提醒**\n\n👤 **主播**: {}\n🎬 **正在录制**: {}\n🔗 [点击进入直播间]({})",
                    host_name, room_name, live_url
                );

                for admin_id in &CONFIG.admin_ids {
                    let chat_id = ChatId(*admin_id);
                    if let Err(e) = bot.send_message(chat_id, &notify_text).await {
                        log::error!("推送开播消息失败 to {}: {}", admin_id, e);
                    }
                }
            }
            last_rec_state.insert(l_id, is_recording);
        }
    }
}

// ================= 主程序入口 =================
#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("正在启动 Bililive-go 机器人...");

    let bot = Bot::from_env();

    // 启动后台开播监控
    let bot_clone = bot.clone();
    tokio::spawn(async move {
        start_monitor_loop(bot_clone).await;
    });

    // 组合 Dispatcher 处理器结构
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query));

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}