//! 应用配置定义与加载逻辑。
//!
//! [`Config`] 是整个应用的唯一配置入口，在 `main.rs` 中调用 [`Config::load`]
//! 一次性加载，之后通过 `Arc<Config>` 在各模块间共享，保证不可变性与线程安全。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::warn;

/// 支持的视频文件扩展名列表。
pub const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".mkv", ".flv", ".ts"];

/// 视频扩展名辅助类型，提供扩展名匹配方法。
pub struct VideoExtensions;

impl VideoExtensions {
    /// 判断给定文件名是否为支持的视频格式。
    #[must_use]
    pub fn is_video(filename: &str) -> bool {
        let lower = filename.to_lowercase();
        VIDEO_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
    }
}

/// 应用全局配置。
///
/// 所有字段均通过环境变量注入，在程序启动时调用 [`Config::load`] 一次性加载。
/// 加载后通过 `Arc<Config>` 在各模块间只读共享。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Telegram Bot Token（必填）
    pub bot_token: String,

    /// Telegram 管理员用户 ID（必填）
    pub admin_id: i64,

    /// 媒体文件根目录（必填）
    pub base_dir: PathBuf,

    /// RTMP 推流地址（可选）
    #[serde(default)]
    pub rtmp_url: String,

    /// 每页显示条目数（默认 8）
    #[serde(default = "default_items_per_page")]
    pub items_per_page: usize,

    /// YouTube OAuth Token 文件路径（默认 ./token.json）
    #[serde(default = "default_token_file")]
    pub token_file: PathBuf,

    /// YouTube 最大并发上传数（默认 2）
    #[serde(default = "default_youtube_max_concurrent_uploads")]
    pub youtube_max_concurrent_uploads: usize,

    /// YouTube 上传分块大小（MB，默认 10）
    #[serde(default = "default_youtube_upload_chunk_mb")]
    pub youtube_upload_chunk_mb: usize,

    /// 合并结果时长最低比例（默认 0.95）
    #[serde(default = "default_merge_min_duration_ratio")]
    pub merge_min_duration_ratio: f64,

    /// 合并结果体积最低比例（默认 0.30）
    #[serde(default = "default_merge_min_size_ratio")]
    pub merge_min_size_ratio: f64,

    /// 日志格式（pretty | json，默认 pretty）
    #[serde(default = "default_log_format")]
    pub log_format: String,
}

// ── 默认值函数 ─────────────────────────────────────────────────────────────────

fn default_items_per_page() -> usize {
    8
}

fn default_token_file() -> PathBuf {
    PathBuf::from("./token.json")
}

fn default_youtube_max_concurrent_uploads() -> usize {
    2
}

fn default_youtube_upload_chunk_mb() -> usize {
    10
}

fn default_merge_min_duration_ratio() -> f64 {
    0.95
}

fn default_merge_min_size_ratio() -> f64 {
    0.30
}

fn default_log_format() -> String {
    "pretty".to_string()
}

// ── 加载与验证 ─────────────────────────────────────────────────────────────────

impl Config {
    /// 从 `.env` 文件和环境变量加载配置，并进行合法性验证。
    ///
    /// 加载顺序：
    /// 1. 尝试加载 `.env` 文件（若不存在则跳过，不报错）
    /// 2. 从环境变量读取各字段
    /// 3. 执行业务合法性校验
    ///
    /// # Errors
    ///
    /// 若必填字段缺失或字段值不合法，返回 [`anyhow::Error`]。
    pub fn load() -> Result<Self> {
        // 加载 .env 文件（忽略文件不存在的错误）
        match dotenvy::dotenv() {
            Ok(path) => tracing::info!("Loaded .env from: {}", path.display()),
            Err(dotenvy::Error::Io(_)) => {
                tracing::debug!(".env file not found, using environment variables only");
            }
            Err(e) => return Err(e).context("Failed to parse .env file"),
        }

        let config = Self::from_env().context("Failed to read configuration from environment")?;
        config.validate()?;
        Ok(config)
    }

    /// 从当前环境变量构建 [`Config`]。
    fn from_env() -> Result<Self> {
        let bot_token = std::env::var("BOT_TOKEN")
            .context("BOT_TOKEN is not set")?
            .trim()
            .to_string();

        let admin_id: i64 = std::env::var("ADMIN_ID")
            .context("ADMIN_ID is not set")?
            .trim()
            .parse()
            .context("ADMIN_ID must be a valid integer")?;

        let base_dir = std::env::var("BASE_DIR")
            .unwrap_or_else(|_| "/storage512/bilivego/download".to_string());
        let base_dir = PathBuf::from(base_dir.trim())
            .canonicalize()
            .unwrap_or_else(|_| {
                warn!("BASE_DIR does not exist or is not accessible, using raw path");
                PathBuf::from(base_dir.trim())
            });

        let rtmp_url = std::env::var("RTMP_URL")
            .unwrap_or_default()
            .trim()
            .to_string();

        let items_per_page = parse_env_usize("ITEMS_PER_PAGE", default_items_per_page())?;

        let token_file = std::env::var("TOKEN_FILE")
            .map(|s| PathBuf::from(s.trim()))
            .unwrap_or_else(|_| default_token_file());

        let youtube_max_concurrent_uploads = parse_env_usize(
            "YOUTUBE_MAX_CONCURRENT_UPLOADS",
            default_youtube_max_concurrent_uploads(),
        )?;

        let youtube_upload_chunk_mb =
            parse_env_usize("YOUTUBE_UPLOAD_CHUNK_MB", default_youtube_upload_chunk_mb())?;

        let merge_min_duration_ratio = parse_env_f64(
            "MERGE_MIN_DURATION_RATIO",
            default_merge_min_duration_ratio(),
        )?;

        let merge_min_size_ratio =
            parse_env_f64("MERGE_MIN_SIZE_RATIO", default_merge_min_size_ratio())?;

        let log_format = std::env::var("LOG_FORMAT")
            .unwrap_or_else(|_| default_log_format())
            .trim()
            .to_lowercase();

        Ok(Self {
            bot_token,
            admin_id,
            base_dir,
            rtmp_url,
            items_per_page,
            token_file,
            youtube_max_concurrent_uploads,
            youtube_upload_chunk_mb,
            merge_min_duration_ratio,
            merge_min_size_ratio,
            log_format,
        })
    }

    /// 验证配置合法性。
    ///
    /// # Errors
    ///
    /// 若任何必填字段为空或数值不合法，返回错误。
    fn validate(&self) -> Result<()> {
        if self.bot_token.is_empty() {
            anyhow::bail!("BOT_TOKEN must not be empty");
        }
        if self.admin_id == 0 {
            anyhow::bail!("ADMIN_ID must be a non-zero integer");
        }
        if self.items_per_page == 0 {
            anyhow::bail!("ITEMS_PER_PAGE must be greater than 0");
        }
        if self.youtube_max_concurrent_uploads == 0 {
            anyhow::bail!("YOUTUBE_MAX_CONCURRENT_UPLOADS must be greater than 0");
        }
        if self.youtube_upload_chunk_mb == 0 {
            anyhow::bail!("YOUTUBE_UPLOAD_CHUNK_MB must be greater than 0");
        }
        if !(0.0..=1.0).contains(&self.merge_min_duration_ratio) {
            anyhow::bail!("MERGE_MIN_DURATION_RATIO must be between 0.0 and 1.0");
        }
        if !(0.0..=1.0).contains(&self.merge_min_size_ratio) {
            anyhow::bail!("MERGE_MIN_SIZE_RATIO must be between 0.0 and 1.0");
        }
        if !matches!(self.log_format.as_str(), "pretty" | "json") {
            anyhow::bail!("LOG_FORMAT must be 'pretty' or 'json'");
        }
        Ok(())
    }

    /// 返回 YouTube 上传分块大小（字节数）。
    #[must_use]
    pub fn youtube_chunk_bytes(&self) -> usize {
        self.youtube_upload_chunk_mb * 1024 * 1024
    }
}

// ── 辅助解析函数 ───────────────────────────────────────────────────────────────

fn parse_env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(val) => val
            .trim()
            .parse::<usize>()
            .with_context(|| format!("{name} must be a positive integer, got: {val:?}")),
        Err(_) => Ok(default),
    }
}

fn parse_env_f64(name: &str, default: f64) -> Result<f64> {
    match std::env::var(name) {
        Ok(val) => val
            .trim()
            .parse::<f64>()
            .with_context(|| format!("{name} must be a float, got: {val:?}")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_extensions_is_video() {
        assert!(VideoExtensions::is_video("video.mp4"));
        assert!(VideoExtensions::is_video("VIDEO.MKV"));
        assert!(VideoExtensions::is_video("stream.flv"));
        assert!(VideoExtensions::is_video("recording.ts"));
        assert!(!VideoExtensions::is_video("document.pdf"));
        assert!(!VideoExtensions::is_video("image.jpg"));
        assert!(!VideoExtensions::is_video("noextension"));
    }

    #[test]
    fn test_default_values() {
        assert_eq!(default_items_per_page(), 8);
        assert_eq!(default_youtube_max_concurrent_uploads(), 2);
        assert_eq!(default_youtube_upload_chunk_mb(), 10);
        assert!((default_merge_min_duration_ratio() - 0.95).abs() < f64::EPSILON);
        assert!((default_merge_min_size_ratio() - 0.30).abs() < f64::EPSILON);
    }

    #[test]
    fn test_youtube_chunk_bytes() {
        let config = Config {
            bot_token: "token".into(),
            admin_id: 1,
            base_dir: PathBuf::from("/tmp"),
            rtmp_url: String::new(),
            items_per_page: 8,
            token_file: PathBuf::from("./token.json"),
            youtube_max_concurrent_uploads: 2,
            youtube_upload_chunk_mb: 10,
            merge_min_duration_ratio: 0.95,
            merge_min_size_ratio: 0.30,
            log_format: "pretty".into(),
        };
        assert_eq!(config.youtube_chunk_bytes(), 10 * 1024 * 1024);
    }
}
