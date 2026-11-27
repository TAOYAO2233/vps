use std::{collections::HashMap, sync::Arc, time::Duration};
use teloxide::prelude::*;
use crate::{state::AppState, ui::text::{short, MESSAGE_LIMIT}};

pub(crate) async fn run(bot: Bot, state: Arc<AppState>) {
    let mut last_rec_state: HashMap<String, bool> = HashMap::new();

    loop {
        tokio::time::sleep(Duration::from_secs(state.config.polling_interval)).await;
        log::debug!("开始轮询开播状态...");

        let lives = match state.api.get_lives().await {
            Ok(lives) => lives,
            Err(error) => {
                log::warn!("轮询失败：{}", error);
                continue;
            }
        };
        last_rec_state.retain(|id, _| lives.iter().any(|live| &live.id == id));
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
                    "🚨 开播提醒\n\n主播：{}\n正在录制：{}\n直播间：{}",
                    host_name, room_name, live_url
                );

                for admin_id in &state.config.admin_ids {
                    let chat_id = ChatId(*admin_id);
                    if let Err(e) = bot.send_message(chat_id, short(&notify_text, MESSAGE_LIMIT)).await {
                        log::error!("推送开播消息失败 to {}: {}", admin_id, e);
                    }
                }
            }
            last_rec_state.insert(l_id, is_recording);
        }
    }
}
