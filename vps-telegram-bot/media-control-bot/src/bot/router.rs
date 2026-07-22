//! Teloxide Dispatcher 路由配置。
//!
//! 使用 `dptree` 构建消息路由树，将不同类型的 Telegram 更新分发到对应的处理函数。
//! 这是 Teloxide 推荐的现代路由方式，替代了旧版的 `Handler` 链式调用。

use std::sync::Arc;

use teloxide::dispatching::{DefaultKey, UpdateFilterExt};
use teloxide::dptree;
use teloxide::prelude::*;
use teloxide::types::Update;
use teloxide::RequestError;

use crate::config::Config;
use crate::core::state::SharedState;

use super::callback::callback_handler;
use super::commands::{cmd_start, cmd_stop, cmd_uploads};

/// 构建并返回 Teloxide [`Dispatcher`]。
///
/// 路由树结构：
/// - `Message` → 命令处理（`/start`、`/stop`、`/uploads`）
/// - `CallbackQuery` → 回调处理（所有 InlineKeyboard 交互）
///
/// # Arguments
///
/// * `bot` - Teloxide Bot 实例
/// * `state` - 全局共享状态
/// * `config` - 应用配置
pub fn build_dispatcher(
    bot: Bot,
    state: SharedState,
    config: Arc<Config>,
) -> Dispatcher<Bot, RequestError, DefaultKey> {
    let handler = dptree::entry()
        // ── 消息路由 ─────────────────────────────────────────────────────────
        .branch(
            Update::filter_message()
                .filter_command::<BotCommand>()
                .endpoint({
                    let state = Arc::clone(&state);
                    let config = Arc::clone(&config);
                    move |bot: Bot, msg: Message, cmd: BotCommand| {
                        let state = Arc::clone(&state);
                        let config = Arc::clone(&config);
                        async move {
                            match cmd {
                                BotCommand::Start => cmd_start(bot, msg, state, config).await,
                                BotCommand::Stop => cmd_stop(bot, msg, state, config).await,
                                BotCommand::Uploads => cmd_uploads(bot, msg, state, config).await,
                            }
                        }
                    }
                }),
        )
        // ── CallbackQuery 路由 ────────────────────────────────────────────────
        .branch(Update::filter_callback_query().endpoint({
            let state = Arc::clone(&state);
            let config = Arc::clone(&config);
            move |bot: Bot, q: CallbackQuery| {
                let state = Arc::clone(&state);
                let config = Arc::clone(&config);
                async move { callback_handler(bot, q, state, config).await }
            }
        }));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![])
        .enable_ctrlc_handler()
        .build()
}

/// Bot 命令枚举，使用 `teloxide::macros::BotCommands` 自动生成命令列表。
#[derive(teloxide::macros::BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "VPS 多媒体主控面板命令列表")]
pub enum BotCommand {
    /// 显示主控面板菜单
    #[command(description = "显示主控面板菜单")]
    Start,

    /// 停止当前所有运行中的任务
    #[command(description = "停止当前所有运行中的任务")]
    Stop,

    /// 查看 YouTube 上传任务列表
    #[command(description = "查看 YouTube 上传任务列表")]
    Uploads,
}
