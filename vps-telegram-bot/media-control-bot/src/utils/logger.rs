//! 日志系统初始化。
//!
//! 使用 `tracing` + `tracing-subscriber` 初始化结构化日志系统。
//! 支持 `pretty`（开发友好的彩色输出）和 `json`（生产环境结构化日志）两种格式。

use anyhow::Result;
use tracing_subscriber::EnvFilter;

/// 初始化日志系统。
///
/// 根据配置选择日志格式：
/// - `"pretty"`: 人类可读的彩色格式（适合开发环境）
/// - `"json"`: 结构化 JSON 格式（适合生产环境和日志聚合）
///
/// 日志级别通过 `RUST_LOG` 环境变量控制，默认为 `info`。
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

    match format {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .init();
        }
        _ => {
            // 默认使用 pretty 格式
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
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
