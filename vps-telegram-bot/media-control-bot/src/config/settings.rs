//! 应用配置定义与加载逻辑。
//!
//! [`Config`] 是整个应用的唯一配置入口，在 `main.rs` 中调用 [`Config::load`]
//! 一次性加载，之后通过 `Arc<Config>` 在各模块间共享，保证不可变性与线程安全。
//!
//! ## 安全设计
//!
//! [`Config`] **不派生**原始 `Debug`，而是手动实现仅输出安全字段白名单的 [`std::fmt::Debug`]，
//! 防止任何调试输出泄露 `bot_token` 或完整 `rtmp_url` 推流密钥。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::utils::redact::{
    redact_rtmp_target, register_rtmp_secret, register_secret, REDACTED,
};

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

/// 服务重启后的 YouTube 队列恢复策略（待用户确认，保守默认为 [`Self::None`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum YoutubeRecoveryMode {
    /// 不在重启后自动恢复上传（仅内存队列，保守默认）。
    #[default]
    None,
    /// 任务级恢复：重启后先核实既有会话是否已完成，未完成则从头重新上传。
    Requeue,
    /// 断点续传恢复：持久化会话 URI 与文件身份，重启后查询 `308 Range` 继续上传。
    Resume,
}

impl YoutubeRecoveryMode {
    /// 返回配置值字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Requeue => "requeue",
            Self::Resume => "resume",
        }
    }
}

impl std::str::FromStr for YoutubeRecoveryMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "off" | "false" => Ok(Self::None),
            "requeue" | "restart" => Ok(Self::Requeue),
            "resume" => Ok(Self::Resume),
            other => anyhow::bail!(
                "YOUTUBE_RECOVERY_MODE must be 'none', 'requeue', or 'resume', got: {other:?}"
            ),
        }
    }
}

/// 应用全局配置。
///
/// 所有字段均通过环境变量注入，在程序启动时调用 [`Config::load`] 一次性加载。
/// 加载后通过 `Arc<Config>` 在各模块间只读共享。
#[derive(Clone, Deserialize)]
pub struct Config {
    /// Telegram Bot Token（必填，敏感）
    pub bot_token: String,

    /// Telegram 管理员用户 ID（必填）
    pub admin_id: i64,

    /// 媒体文件根目录（默认 `/storage512/bilivego/download`）
    pub base_dir: PathBuf,

    /// 目标存储挂载点（默认 `/storage512`）
    #[serde(default = "default_storage_mount_point")]
    pub storage_mount_point: PathBuf,

    /// 磁盘后台定时检查间隔（秒，默认 300 即 5 分钟；待用户最终确认）
    #[serde(default = "default_disk_check_interval_secs")]
    pub disk_check_interval_secs: u64,

    /// 一级磁盘报警（>90%）回落复位阈值百分比（默认 85.0；待用户确认）
    #[serde(default = "default_disk_alert_reset_l1")]
    pub disk_alert_reset_l1: f64,

    /// 二级磁盘报警（>95%）回落复位阈值百分比（默认 90.0；待用户确认）
    #[serde(default = "default_disk_alert_reset_l2")]
    pub disk_alert_reset_l2: f64,

    /// 三级磁盘报警（>98%）回落复位阈值百分比（默认 95.0；待用户确认）
    #[serde(default = "default_disk_alert_reset_l3")]
    pub disk_alert_reset_l3: f64,

    /// 磁盘报警状态跨重启去重持久化文件（默认 `./.disk_alert_state.json`）
    #[serde(default = "default_disk_alert_state_file")]
    pub disk_alert_state_file: PathBuf,

    /// RTMP 推流地址（可选，敏感）
    #[serde(default)]
    pub rtmp_url: String,

    /// RTMP 网络读写超时秒数（默认 30 秒；待结合实际直播平台确认）
    #[serde(default = "default_rtmp_rw_timeout_secs")]
    pub rtmp_rw_timeout_secs: u64,

    /// RTMP 源片码率上行风险提示阈值（Mbps，默认 0.0 表示仅展示码率不强行告警）
    #[serde(default)]
    pub rtmp_warn_bitrate_mbps: f64,

    /// 每页显示条目数（默认 8）
    #[serde(default = "default_items_per_page")]
    pub items_per_page: usize,

    /// FFprobe 超时时间（秒，默认 30）
    #[serde(default = "default_ffprobe_timeout_secs")]
    pub ffprobe_timeout_secs: u64,

    /// Minimum time since the last source-file modification before processing.
    #[serde(default = "default_file_stable_seconds")]
    pub file_stable_seconds: u64,

    /// Days to keep deleted media in the same-volume recovery directory.
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u64,

    /// YouTube OAuth Token 文件路径（默认 `./token.json`）
    #[serde(default = "default_token_file")]
    pub token_file: PathBuf,

    /// YouTube 最大并发上传数（代码默认保持 2；低配 VPS 建议在 `.env` 设为 1）
    #[serde(default = "default_youtube_max_concurrent_uploads")]
    pub youtube_max_concurrent_uploads: usize,

    /// YouTube 除并发运行外允许额外排队等待的最大任务数（默认 10；待用户确认）
    #[serde(default = "default_youtube_max_queue_size")]
    pub youtube_max_queue_size: usize,

    /// YouTube 可恢复上传实际分块大小（MB，默认 10，即 40 × 256 KiB）
    #[serde(default = "default_youtube_upload_chunk_mb")]
    pub youtube_upload_chunk_mb: usize,

    /// YouTube 重启恢复策略（保守默认 `none`；待用户确认后可设为 `requeue` 或 `resume`）
    #[serde(default)]
    pub youtube_recovery_mode: YoutubeRecoveryMode,

    /// YouTube 上传队列持久化文件路径（默认 `./youtube_upload_queue.json`）
    #[serde(default = "default_youtube_queue_file")]
    pub youtube_queue_file: PathBuf,

    /// 合并结果保留时长最低比例（默认 0.95）
    #[serde(default = "default_merge_min_duration_ratio")]
    pub merge_min_duration_ratio: f64,

    /// 合并结果体积辅助比对阈值（默认 0.80；仅作辅助指标，不单独决定合并成败）
    #[serde(default = "default_merge_min_size_ratio")]
    pub merge_min_size_ratio: f64,

    /// 无损片尾截除建议上限秒数（默认 3 秒；待用户确认）
    #[serde(default = "default_merge_max_loss_seconds")]
    pub merge_max_loss_seconds: u64,

    /// 是否允许合并时自动启用有界片尾无损截除（保守默认 `false`：仅探测报告候选方案，不自动丢内容）
    #[serde(default)]
    pub merge_allow_auto_trim: bool,

    /// 日志格式（`pretty` | `json`，默认 `pretty`）
    #[serde(default = "default_log_format")]
    pub log_format: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bot_token", &REDACTED)
            .field("admin_id", &self.admin_id)
            .field("base_dir", &self.base_dir)
            .field("storage_mount_point", &self.storage_mount_point)
            .field("disk_check_interval_secs", &self.disk_check_interval_secs)
            .field("disk_alert_reset_l1", &self.disk_alert_reset_l1)
            .field("disk_alert_reset_l2", &self.disk_alert_reset_l2)
            .field("disk_alert_reset_l3", &self.disk_alert_reset_l3)
            .field("rtmp_target", &redact_rtmp_target(&self.rtmp_url))
            .field("rtmp_rw_timeout_secs", &self.rtmp_rw_timeout_secs)
            .field("items_per_page", &self.items_per_page)
            .field("ffprobe_timeout_secs", &self.ffprobe_timeout_secs)
            .field("file_stable_seconds", &self.file_stable_seconds)
            .field("trash_retention_days", &self.trash_retention_days)
            .field("token_file", &self.token_file)
            .field(
                "youtube_max_concurrent_uploads",
                &self.youtube_max_concurrent_uploads,
            )
            .field("youtube_max_queue_size", &self.youtube_max_queue_size)
            .field("youtube_upload_chunk_mb", &self.youtube_upload_chunk_mb)
            .field("youtube_recovery_mode", &self.youtube_recovery_mode)
            .field("merge_min_duration_ratio", &self.merge_min_duration_ratio)
            .field("merge_min_size_ratio", &self.merge_min_size_ratio)
            .field("merge_max_loss_seconds", &self.merge_max_loss_seconds)
            .field("merge_allow_auto_trim", &self.merge_allow_auto_trim)
            .field("log_format", &self.log_format)
            .finish()
    }
}

// ── 默认值函数 ─────────────────────────────────────────────────────────────────

fn default_storage_mount_point() -> PathBuf {
    PathBuf::from("/storage512")
}

fn default_disk_check_interval_secs() -> u64 {
    300
}

fn default_disk_alert_reset_l1() -> f64 {
    85.0
}

fn default_disk_alert_reset_l2() -> f64 {
    90.0
}

fn default_disk_alert_reset_l3() -> f64 {
    95.0
}

fn default_disk_alert_state_file() -> PathBuf {
    PathBuf::from("./.disk_alert_state.json")
}

fn default_rtmp_rw_timeout_secs() -> u64 {
    30
}

fn default_items_per_page() -> usize {
    8
}

fn default_ffprobe_timeout_secs() -> u64 {
    30
}

fn default_file_stable_seconds() -> u64 {
    30
}

fn default_trash_retention_days() -> u64 {
    7
}

fn default_token_file() -> PathBuf {
    PathBuf::from("./token.json")
}

fn default_youtube_max_concurrent_uploads() -> usize {
    2
}

fn default_youtube_max_queue_size() -> usize {
    10
}

fn default_youtube_upload_chunk_mb() -> usize {
    10
}

fn default_youtube_queue_file() -> PathBuf {
    PathBuf::from("./youtube_upload_queue.json")
}

fn default_merge_min_duration_ratio() -> f64 {
    0.95
}

fn default_merge_min_size_ratio() -> f64 {
    0.80
}

fn default_merge_max_loss_seconds() -> u64 {
    3
}

fn default_log_format() -> String {
    "pretty".to_string()
}

// ── 加载与验证 ─────────────────────────────────────────────────────────────────

impl Config {
    /// 从 `.env` 文件和环境变量加载配置，注册敏感值并执行合法性验证。
    ///
    /// # Errors
    ///
    /// 若必填字段缺失或字段值不合法，返回 [`anyhow::Error`]。
    pub fn load() -> Result<Self> {
        match dotenvy::dotenv() {
            Ok(path) => tracing::info!("Loaded .env from: {}", path.display()),
            Err(dotenvy::Error::Io(_)) => {
                tracing::debug!(".env file not found, using environment variables only");
            }
            Err(e) => return Err(e).context("Failed to parse .env file"),
        }

        let config = Self::from_env().context("Failed to read configuration from environment")?;

        // 将实际加载到的敏感凭据注册到全局脱敏器
        register_secret(&config.bot_token);
        register_rtmp_secret(&config.rtmp_url);

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

        let raw_base = std::env::var("BASE_DIR")
            .unwrap_or_else(|_| "/storage512/bilivego/download".to_string());
        let trimmed_base = raw_base.trim();
        let base_dir = PathBuf::from(trimmed_base)
            .canonicalize()
            .unwrap_or_else(|_| {
                warn!("BASE_DIR does not currently exist or is not accessible; PathGuard will re-verify at operation time");
                PathBuf::from(trimmed_base)
            });

        let storage_mount_point = std::env::var("STORAGE_MOUNT_POINT")
            .map(|s| PathBuf::from(s.trim()))
            .unwrap_or_else(|_| default_storage_mount_point());

        let disk_check_interval_secs = parse_env_u64(
            "DISK_CHECK_INTERVAL_SECS",
            default_disk_check_interval_secs(),
        )?;

        let disk_alert_reset_l1 =
            parse_env_f64("DISK_ALERT_RESET_L1", default_disk_alert_reset_l1())?;
        let disk_alert_reset_l2 =
            parse_env_f64("DISK_ALERT_RESET_L2", default_disk_alert_reset_l2())?;
        let disk_alert_reset_l3 =
            parse_env_f64("DISK_ALERT_RESET_L3", default_disk_alert_reset_l3())?;

        let disk_alert_state_file = std::env::var("DISK_ALERT_STATE_FILE")
            .map(|s| PathBuf::from(s.trim()))
            .unwrap_or_else(|_| default_disk_alert_state_file());

        let rtmp_url = std::env::var("RTMP_URL")
            .unwrap_or_default()
            .trim()
            .to_string();

        let rtmp_rw_timeout_secs =
            parse_env_u64("RTMP_RW_TIMEOUT_SECS", default_rtmp_rw_timeout_secs())?;

        let rtmp_warn_bitrate_mbps = parse_env_f64("RTMP_WARN_BITRATE_MBPS", 0.0)?;

        let items_per_page = parse_env_usize("ITEMS_PER_PAGE", default_items_per_page())?;

        let ffprobe_timeout_secs =
            parse_env_u64("FFPROBE_TIMEOUT_SECONDS", default_ffprobe_timeout_secs())?;
        let file_stable_seconds =
            parse_env_u64("FILE_STABLE_SECONDS", default_file_stable_seconds())?;
        let trash_retention_days =
            parse_env_u64("TRASH_RETENTION_DAYS", default_trash_retention_days())?;

        let token_file = std::env::var("TOKEN_FILE")
            .map(|s| PathBuf::from(s.trim()))
            .unwrap_or_else(|_| default_token_file());

        let youtube_max_concurrent_uploads = parse_env_usize(
            "YOUTUBE_MAX_CONCURRENT_UPLOADS",
            default_youtube_max_concurrent_uploads(),
        )?;

        let youtube_max_queue_size =
            parse_env_usize("YOUTUBE_MAX_QUEUE_SIZE", default_youtube_max_queue_size())?;

        let youtube_upload_chunk_mb =
            parse_env_usize("YOUTUBE_UPLOAD_CHUNK_MB", default_youtube_upload_chunk_mb())?;

        let youtube_recovery_mode = match std::env::var("YOUTUBE_RECOVERY_MODE") {
            Ok(val) => val.parse::<YoutubeRecoveryMode>()?,
            Err(_) => YoutubeRecoveryMode::None,
        };

        let youtube_queue_file = std::env::var("YOUTUBE_UPLOAD_QUEUE_FILE")
            .map(|s| PathBuf::from(s.trim()))
            .unwrap_or_else(|_| default_youtube_queue_file());

        let merge_min_duration_ratio = parse_env_f64(
            "MERGE_MIN_DURATION_RATIO",
            default_merge_min_duration_ratio(),
        )?;

        let merge_min_size_ratio =
            parse_env_f64("MERGE_MIN_SIZE_RATIO", default_merge_min_size_ratio())?;

        let merge_max_loss_seconds =
            parse_env_u64("MERGE_MAX_LOSS_SECONDS", default_merge_max_loss_seconds())?;

        let merge_allow_auto_trim = parse_env_bool("MERGE_ALLOW_AUTO_TRIM", false)?;

        let log_format = std::env::var("LOG_FORMAT")
            .unwrap_or_else(|_| default_log_format())
            .trim()
            .to_lowercase();

        Ok(Self {
            bot_token,
            admin_id,
            base_dir,
            storage_mount_point,
            disk_check_interval_secs,
            disk_alert_reset_l1,
            disk_alert_reset_l2,
            disk_alert_reset_l3,
            disk_alert_state_file,
            rtmp_url,
            rtmp_rw_timeout_secs,
            rtmp_warn_bitrate_mbps,
            items_per_page,
            ffprobe_timeout_secs,
            file_stable_seconds,
            trash_retention_days,
            token_file,
            youtube_max_concurrent_uploads,
            youtube_max_queue_size,
            youtube_upload_chunk_mb,
            youtube_recovery_mode,
            youtube_queue_file,
            merge_min_duration_ratio,
            merge_min_size_ratio,
            merge_max_loss_seconds,
            merge_allow_auto_trim,
            log_format,
        })
    }

    /// 验证配置合法性。
    ///
    /// # Errors
    ///
    /// 若任何必填字段为空或数值超出合法区间，返回错误。
    pub fn validate(&self) -> Result<()> {
        if self.bot_token.is_empty() {
            anyhow::bail!("BOT_TOKEN must not be empty");
        }
        if self.admin_id == 0 {
            anyhow::bail!("ADMIN_ID must be a non-zero integer");
        }
        if self.storage_mount_point.as_os_str().is_empty() {
            anyhow::bail!("STORAGE_MOUNT_POINT must not be empty");
        }
        if self.disk_check_interval_secs == 0 {
            anyhow::bail!("DISK_CHECK_INTERVAL_SECS must be greater than 0");
        }
        if !self.disk_alert_reset_l1.is_finite()
            || self.disk_alert_reset_l1 <= 0.0
            || self.disk_alert_reset_l1 > 90.0
        {
            anyhow::bail!("DISK_ALERT_RESET_L1 must be in (0.0, 90.0]");
        }
        if !self.disk_alert_reset_l2.is_finite()
            || self.disk_alert_reset_l2 <= 0.0
            || self.disk_alert_reset_l2 > 95.0
        {
            anyhow::bail!("DISK_ALERT_RESET_L2 must be in (0.0, 95.0]");
        }
        if !self.disk_alert_reset_l3.is_finite()
            || self.disk_alert_reset_l3 <= 0.0
            || self.disk_alert_reset_l3 > 98.0
        {
            anyhow::bail!("DISK_ALERT_RESET_L3 must be in (0.0, 98.0]");
        }
        if self.rtmp_rw_timeout_secs == 0 {
            anyhow::bail!("RTMP_RW_TIMEOUT_SECS must be greater than 0");
        }
        if !self.rtmp_warn_bitrate_mbps.is_finite() || self.rtmp_warn_bitrate_mbps < 0.0 {
            anyhow::bail!("RTMP_WARN_BITRATE_MBPS must be >= 0.0");
        }
        if self.items_per_page == 0 {
            anyhow::bail!("ITEMS_PER_PAGE must be greater than 0");
        }
        if self.ffprobe_timeout_secs == 0 {
            anyhow::bail!("FFPROBE_TIMEOUT_SECONDS must be greater than 0");
        }
        if self.file_stable_seconds > 86_400 {
            anyhow::bail!("FILE_STABLE_SECONDS must be at most 86400");
        }
        if self.trash_retention_days == 0 || self.trash_retention_days > 365 {
            anyhow::bail!("TRASH_RETENTION_DAYS must be between 1 and 365");
        }
        if self.youtube_max_concurrent_uploads == 0 {
            anyhow::bail!("YOUTUBE_MAX_CONCURRENT_UPLOADS must be greater than 0");
        }
        if self.youtube_max_queue_size == 0 {
            anyhow::bail!("YOUTUBE_MAX_QUEUE_SIZE must be greater than 0");
        }
        if self.youtube_upload_chunk_mb == 0 || self.youtube_upload_chunk_mb > 64 {
            anyhow::bail!("YOUTUBE_UPLOAD_CHUNK_MB must be between 1 and 64");
        }
        // 1 MiB = 4 × 256 KiB，校验确实满足 YouTube Resumable Upload 256 KiB 整数倍要求
        if self.youtube_chunk_bytes() % (256 * 1024) != 0 {
            anyhow::bail!("YOUTUBE_UPLOAD_CHUNK_MB in bytes must be a multiple of 256 KiB");
        }
        if !(0.0..=1.0).contains(&self.merge_min_duration_ratio) {
            anyhow::bail!("MERGE_MIN_DURATION_RATIO must be between 0.0 and 1.0");
        }
        if !(0.0..=1.0).contains(&self.merge_min_size_ratio) {
            anyhow::bail!("MERGE_MIN_SIZE_RATIO must be between 0.0 and 1.0");
        }
        if self.merge_max_loss_seconds > 60 {
            anyhow::bail!("MERGE_MAX_LOSS_SECONDS must be <= 60");
        }
        if !matches!(self.log_format.as_str(), "pretty" | "json") {
            anyhow::bail!("LOG_FORMAT must be 'pretty' or 'json'");
        }
        Ok(())
    }

    /// 返回 YouTube 可恢复上传实际分块大小（字节数，恒为 256 KiB 的整数倍）。
    #[must_use]
    pub fn youtube_chunk_bytes(&self) -> usize {
        self.youtube_upload_chunk_mb * 1024 * 1024
    }

    /// 返回允许同时存在于上传池（运行中 + 排队中）的最大任务总数。
    #[must_use]
    pub fn youtube_max_total_tasks(&self) -> usize {
        self.youtube_max_concurrent_uploads
            .saturating_add(self.youtube_max_queue_size)
    }
}

// ── 辅助解析函数 ───────────────────────────────────────────────────────────────

fn parse_env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(val) => {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return Ok(default);
            }
            trimmed
                .parse::<usize>()
                .with_context(|| format!("{name} must be a non-negative integer"))
        }
        Err(_) => Ok(default),
    }
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(val) => {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return Ok(default);
            }
            trimmed
                .parse::<u64>()
                .with_context(|| format!("{name} must be a non-negative integer"))
        }
        Err(_) => Ok(default),
    }
}

fn parse_env_f64(name: &str, default: f64) -> Result<f64> {
    match std::env::var(name) {
        Ok(val) => {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return Ok(default);
            }
            trimmed
                .parse::<f64>()
                .with_context(|| format!("{name} must be a valid float"))
        }
        Err(_) => Ok(default),
    }
}

fn parse_env_bool(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Ok(val) => match val.trim().to_ascii_lowercase().as_str() {
            "" => Ok(default),
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => anyhow::bail!("{name} must be a boolean (true/false)"),
        },
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        Config {
            bot_token: "123456789:AAFakeSecretConfigToken0000000000".into(),
            admin_id: 1,
            base_dir: PathBuf::from("/storage512/bilivego/download"),
            storage_mount_point: default_storage_mount_point(),
            disk_check_interval_secs: default_disk_check_interval_secs(),
            disk_alert_reset_l1: default_disk_alert_reset_l1(),
            disk_alert_reset_l2: default_disk_alert_reset_l2(),
            disk_alert_reset_l3: default_disk_alert_reset_l3(),
            disk_alert_state_file: default_disk_alert_state_file(),
            rtmp_url: "rtmp://push.example.invalid/live/fake_secret_stream_key".into(),
            rtmp_rw_timeout_secs: default_rtmp_rw_timeout_secs(),
            rtmp_warn_bitrate_mbps: 0.0,
            items_per_page: default_items_per_page(),
            ffprobe_timeout_secs: default_ffprobe_timeout_secs(),
            file_stable_seconds: default_file_stable_seconds(),
            trash_retention_days: default_trash_retention_days(),
            token_file: default_token_file(),
            youtube_max_concurrent_uploads: default_youtube_max_concurrent_uploads(),
            youtube_max_queue_size: default_youtube_max_queue_size(),
            youtube_upload_chunk_mb: default_youtube_upload_chunk_mb(),
            youtube_recovery_mode: YoutubeRecoveryMode::None,
            youtube_queue_file: default_youtube_queue_file(),
            merge_min_duration_ratio: default_merge_min_duration_ratio(),
            merge_min_size_ratio: default_merge_min_size_ratio(),
            merge_max_loss_seconds: default_merge_max_loss_seconds(),
            merge_allow_auto_trim: false,
            log_format: default_log_format(),
        }
    }

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
    fn test_default_values_and_conservative_flags() {
        assert_eq!(default_items_per_page(), 8);
        assert_eq!(default_youtube_max_concurrent_uploads(), 2);
        assert_eq!(default_youtube_upload_chunk_mb(), 10);
        assert_eq!(default_merge_max_loss_seconds(), 3);
        assert!((default_merge_min_duration_ratio() - 0.95).abs() < f64::EPSILON);
        assert!((default_merge_min_size_ratio() - 0.80).abs() < f64::EPSILON);

        let cfg = sample_config();
        assert!(!cfg.merge_allow_auto_trim);
        assert_eq!(cfg.youtube_recovery_mode, YoutubeRecoveryMode::None);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_debug_never_leaks_secrets() {
        let cfg = sample_config();
        let dbg_str = format!("{cfg:?}");
        assert!(!dbg_str.contains("AAFakeSecretConfigToken"));
        assert!(!dbg_str.contains("fake_secret_stream_key"));
        assert!(dbg_str.contains("rtmp://push.example.invalid/[REDACTED]"));
        assert!(dbg_str.contains(REDACTED));
    }

    #[test]
    fn test_youtube_chunk_bytes_multiple_of_256kib() {
        let cfg = sample_config();
        assert_eq!(cfg.youtube_chunk_bytes(), 10 * 1024 * 1024);
        assert_eq!(cfg.youtube_chunk_bytes() % (256 * 1024), 0);
    }
}
