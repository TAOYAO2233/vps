//! 应用领域错误类型定义。
//!
//! 使用 [`thiserror`] 宏为每种错误场景提供精确的错误信息，
//! 同时通过 `#[from]` 自动实现标准库错误的转换。

use std::path::PathBuf;

use thiserror::Error;

/// 应用领域错误枚举。
///
/// 每个变体对应一类具体的业务或基础设施错误，
/// 便于在调用方进行精确的错误匹配与处理。
#[derive(Debug, Error, PartialEq)]
#[allow(dead_code)]
pub enum AppError {
    // ── 权限与安全 ─────────────────────────────────────────────────────────────
    /// 操作被拒绝：用户无管理员权限。
    #[error("Permission denied: user {user_id} is not an administrator")]
    PermissionDenied { user_id: i64 },

    /// 路径越界：目标路径超出 BASE_DIR 安全边界。
    #[error("Path traversal detected: {path:?} is outside BASE_DIR")]
    PathTraversal { path: PathBuf },

    // ── 任务管理 ───────────────────────────────────────────────────────────────
    /// 已有独占任务正在运行，无法启动新任务。
    #[error("An exclusive task is already running: {task_name}")]
    TaskAlreadyRunning { task_name: String },

    /// YouTube 上传任务正在运行，阻止独占任务启动。
    #[error("YouTube upload tasks are running ({count} active), cannot start exclusive task")]
    YoutubeUploadBlocking { count: usize },

    /// 任务未找到。
    #[error("Task not found: {task_id}")]
    TaskNotFound { task_id: String },

    // ── 文件系统 ───────────────────────────────────────────────────────────────
    /// 文件不存在。
    #[error("File not found: {path:?}")]
    FileNotFound { path: PathBuf },

    /// 目录不存在。
    #[error("Directory not found: {path:?}")]
    DirectoryNotFound { path: PathBuf },

    /// 文件 I/O 错误。
    #[error("File I/O error on {path:?}: {message}")]
    FileIo { path: PathBuf, message: String },

    /// 通用 I/O 错误。
    #[error("I/O error: {0}")]
    Io(String),

    // ── 媒体处理 ───────────────────────────────────────────────────────────────
    /// FFmpeg 命令执行失败。
    #[error("FFmpeg failed with exit code {exit_code}: {stderr}")]
    FfmpegFailed { exit_code: i32, stderr: String },

    /// FFprobe 无法获取视频时长。
    #[error("FFprobe failed to get duration for {path:?}")]
    FfprobeFailed { path: PathBuf },

    /// 合并结果校验失败（时长或体积不达标）。
    #[error(
        "Merge validation failed: duration ratio={duration_ratio:.2}, size ratio={size_ratio:.2}"
    )]
    MergeValidationFailed {
        duration_ratio: f64,
        size_ratio: f64,
    },

    /// 合并文件数量不足。
    #[error("Concat requires at least 2 files, got {count}")]
    ConcatInsufficientFiles { count: usize },

    // ── YouTube ────────────────────────────────────────────────────────────────
    /// YouTube OAuth Token 文件不存在。
    #[error("YouTube token file not found: {path:?}. Run OAuth flow first.")]
    YoutubeTokenMissing { path: PathBuf },

    /// YouTube API 调用失败。
    #[error("YouTube API error: {message}")]
    YoutubeApiError { message: String },

    /// YouTube 上传被取消。
    #[error("YouTube upload cancelled for: {filename}")]
    YoutubeUploadCancelled { filename: String },

    // ── RTMP ───────────────────────────────────────────────────────────────────
    /// RTMP URL 未配置。
    #[error("RTMP_URL is not configured. Set it in .env or environment variables.")]
    RtmpUrlNotConfigured,

    /// RTMP 推流失败。
    #[error("RTMP stream failed with exit code {exit_code}")]
    RtmpStreamFailed { exit_code: i32 },

    // ── 配置 ───────────────────────────────────────────────────────────────────
    /// 配置加载失败。
    #[error("Configuration error: {message}")]
    ConfigError { message: String },

    // ── Telegram ───────────────────────────────────────────────────────────────
    /// Telegram API 调用失败。
    #[error("Telegram API error: {0}")]
    TelegramError(String),

    // ── 通用 ───────────────────────────────────────────────────────────────────
    /// 操作被用户手动取消。
    #[error("Operation cancelled by user")]
    Cancelled,

    /// 无效的参数或状态。
    #[error("Invalid argument: {message}")]
    InvalidArgument { message: String },
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl AppError {
    /// 构造路径越界错误。
    #[allow(dead_code)]
    pub fn path_traversal(path: impl Into<PathBuf>) -> Self {
        Self::PathTraversal { path: path.into() }
    }

    /// 构造文件不存在错误。
    #[allow(dead_code)]
    pub fn file_not_found(path: impl Into<PathBuf>) -> Self {
        Self::FileNotFound { path: path.into() }
    }

    /// 构造目录不存在错误。
    pub fn directory_not_found(path: impl Into<PathBuf>) -> Self {
        Self::DirectoryNotFound { path: path.into() }
    }

    /// 构造无效参数错误。
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = AppError::PermissionDenied { user_id: 42 };
        assert!(err.to_string().contains("42"));

        let err = AppError::PathTraversal {
            path: PathBuf::from("/etc/passwd"),
        };
        assert!(err.to_string().contains("PATH_TRAVERSAL") || err.to_string().contains("outside"));

        let err = AppError::ConcatInsufficientFiles { count: 1 };
        assert!(err.to_string().contains('1'));
    }

    #[test]
    fn test_error_constructors() {
        let err = AppError::path_traversal("/etc/passwd");
        assert!(matches!(err, AppError::PathTraversal { .. }));

        let err = AppError::file_not_found("/nonexistent");
        assert!(matches!(err, AppError::FileNotFound { .. }));
    }
}
