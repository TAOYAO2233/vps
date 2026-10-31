//! 日志系统初始化。
//!
//! 使用 `tracing` + `tracing-subscriber` 初始化结构化日志系统。
//! 支持 `pretty`（开发友好的可读输出）和 `json`（生产环境结构化日志）两种格式，
//! 并统一接入 [`crate::utils::redact::RedactingMakeWriter`] 输出边界脱敏。

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::utils::redact::{global_stdout_make_writer, preload_secrets_from_env};

/// 从环境变量初始化日志系统（应在程序启动最早期调用）。
///
/// 先尝试加载 `.env`（文件不存在则忽略），立即预注册环境变量中的敏感凭据，
/// 再读取 `LOG_FORMAT`（默认 `pretty`）初始化带脱敏兜底的全局日志订阅器。
///
/// # Errors
///
/// `.env` 文件存在但解析失败时返回错误。
pub fn init_logger_from_env() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) | Err(dotenvy::Error::Io(_)) => {}
        Err(e) => return Err(e).context("Failed to parse .env file"),
    }

    // 在发出任何日志事件前预注册 BOT_TOKEN / RTMP_URL 等运行时秘密
    preload_secrets_from_env();

    let format = std::env::var("LOG_FORMAT").unwrap_or_else(|_| "pretty".to_string());
    init_logger(format.trim())
}

/// 初始化日志系统。
///
/// 根据配置选择日志格式：
/// - `"pretty"`: 人类可读格式（适合终端排障）
/// - `"json"`: 结构化 JSON 格式（适合生产环境和日志聚合）
///
/// 两种格式均通过统一的脱敏 Writer 输出，确保 `RUST_LOG=debug` 下第三方库与异常链也不会泄露密钥。
///
/// # Arguments
///
/// * `format` - 日志格式（`"pretty"` 或 `"json"`）
///
/// # Errors
///
/// 若日志系统初始化失败，返回错误。
pub fn init_logger(format: &str) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("media_control_bot=info,teloxide=warn,hyper=warn"));

    let writer = global_stdout_make_writer();

    match format {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(writer)
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .init();
        }
        _ => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(writer)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .pretty()
                .init();
        }
    }

    Ok(())
}
