//! Telegram Bot 交互层
//!
//! 负责处理所有来自 Telegram 的消息和回调，将用户操作转发给对应的业务逻辑层。
//! 本模块是 Presentation 层，不包含任何业务逻辑，只负责解析输入和格式化输出。

pub mod callback;
pub mod commands;
pub mod keyboard;
pub mod router;
