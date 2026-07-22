//! 业务逻辑层（Actions）
//!
//! 每个子模块对应一种用户操作，负责协调底层基础设施（FFmpeg、YouTube API、文件系统）
//! 完成具体的业务流程，并通过 Telegram 消息向用户反馈进度和结果。

pub mod browse;
pub mod concat;
pub mod convert;
pub mod delete;
pub mod stream;
pub mod youtube;
