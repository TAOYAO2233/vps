//! 应用领域错误类型定义。
//!
//! 使用 [`thiserror`] 宏为每种错误场景提供精确的结构化错误变体，
//! 便于在日志记录与用户提示之间进行安全隔离。

use std::path::PathBuf;

use thiserror::Error;

/// 应用领域错误枚举。
///
/// 每个变体对应一类具体的业务或基础设施错误，
/// 便于在调用方进行精确的错误匹配与安全脱敏映射。
#[derive(Debug, Error, PartialEq)]
#[allow(dead_code)]
pub enum AppError {
    // ── 权限与安全 ─────────────────────────────────────────────────────────────
    /// 操作被拒绝：用户无管理员权限。
    #[error("Permission denied: user {user_id} is not an administrator")]
    PermissionDenied {
        /// 触发操作的用户 ID
        user_id: i64,
    },

    /// 路径越界：目标路径超出 `BASE_DIR` 安全边界或包含非法符号链接跳转。
    #[error("Path traversal detected: {path:?} is outside BASE_DIR")]
    PathTraversal {
        /// 越界或非法的路径
        path: PathBuf,
    },

    /// 菜单快照已过期或目录内容发生变化。
    #[error("Menu snapshot is stale or directory contents changed")]
    StaleMenuSnapshot,

    // ── 任务管理 ───────────────────────────────────────────────────────────────
    /// 正在等待已收到停止请求的任务退出并完成资源清理，暂不允许启动新任务。
    #[error("Tasks are currently stopping, please wait until cleanup finishes")]
    TaskStopping,

    /// 已有独占任务正在运行，无法启动新任务。
    #[error("An exclusive task is already running: {task_name}")]
    TaskAlreadyRunning {
        /// 当前运行中的任务名称
        task_name: String,
    },

    /// YouTube 上传任务正在运行，阻止独占任务启动。
    #[error("YouTube upload tasks are running ({count} active), cannot start exclusive task")]
    YoutubeUploadBlocking {
        /// 当前活跃或排队中的上传任务数
        count: usize,
    },

    /// YouTube 上传队列已达上限，拒绝新增批量任务。
    #[error(
        "YouTube upload queue full: current={current}, requested={requested}, max_total={max_total}"
    )]
    YoutubeQueueFull {
        /// 当前池中已有任务数
        current: usize,
        /// 本次请求新增任务数
        requested: usize,
        /// 允许的最大并发加排队总数
        max_total: usize,
    },

    /// 任务未找到。
    #[error("Task not found: {task_id}")]
    TaskNotFound {
        /// 任务 ID
        task_id: String,
    },

    // ── 文件系统与磁盘监测 ─────────────────────────────────────────────────────
    /// 文件不存在。
    #[error("File not found: {path:?}")]
    FileNotFound {
        /// 缺失的文件路径
        path: PathBuf,
    },

    /// 目录不存在。
    #[error("Directory not found: {path:?}")]
    DirectoryNotFound {
        /// 缺失的目录路径
        path: PathBuf,
    },

    /// 文件 I/O 错误。
    #[error("File I/O error on {path:?}: {message}")]
    FileIo {
        /// 相关路径
        path: PathBuf,
        /// 错误说明
        message: String,
    },

    /// 通用 I/O 错误。
    #[error("I/O error: {0}")]
    Io(String),

    /// 目标存储文件系统剩余空间不足。
    #[error(
        "Insufficient disk space: required {required_bytes} bytes, available {available_bytes} bytes"
    )]
    InsufficientDiskSpace {
        /// 估算所需字节数（含中间文件与安全余量）
        required_bytes: u64,
        /// 当前可用字节数
        available_bytes: u64,
    },

    /// 存储挂载点不可用或挂载丢失。
    #[error("Storage mount unavailable at {mount_point:?}: {reason}")]
    StorageMountUnavailable {
        /// 预期挂载点路径
        mount_point: PathBuf,
        /// 异常原因摘要
        reason: String,
    },

    /// 源文件在任务进行期间被修改或替换。
    #[error("Source file changed during operation: {filename}")]
    SourceFileChanged {
        /// 文件名
        filename: String,
    },

    // ── 媒体处理 ───────────────────────────────────────────────────────────────
    /// FFmpeg 命令执行失败。
    #[error("FFmpeg failed with exit code {exit_code}: {stderr}")]
    FfmpegFailed {
        /// 退出码
        exit_code: i32,
        /// 脱敏且有界的 stderr 摘要
        stderr: String,
    },

    /// FFprobe 无法解析媒体元数据或超时。
    #[error("FFprobe failed to inspect media for {path:?}")]
    FfprobeFailed {
        /// 媒体文件路径
        path: PathBuf,
    },

    /// 媒体流参数与无损容器/拼接要求不兼容（禁止重编码）。
    #[error("Media stream parameters incompatible for lossless copy: {reason}")]
    StreamIncompatible {
        /// 不兼容原因
        reason: String,
    },

    /// 合并结果校验失败。
    #[error(
        "Merge validation failed: duration ratio={duration_ratio:.2}, size ratio={size_ratio:.2}"
    )]
    MergeValidationFailed {
        /// 时长比例
        duration_ratio: f64,
        /// 体积比例
        size_ratio: f64,
    },

    /// 合并文件数量不足。
    #[error("Concat requires at least 2 files, got {count}")]
    ConcatInsufficientFiles {
        /// 实际选中的文件数
        count: usize,
    },

    // ── YouTube ────────────────────────────────────────────────────────────────
    /// YouTube OAuth Token 文件不存在。
    #[error("YouTube token file not found: {path:?}. Run OAuth flow first.")]
    YoutubeTokenMissing {
        /// Token 文件路径
        path: PathBuf,
    },

    /// YouTube API 调用失败。
    #[error("YouTube API error: {message}")]
    YoutubeApiError {
        /// 脱敏后的错误说明
        message: String,
    },

    /// YouTube 上传被取消。
    #[error("YouTube upload cancelled for: {filename}")]
    YoutubeUploadCancelled {
        /// 文件名
        filename: String,
    },

    /// YouTube 服务端已接收全部字节但反复返回 308 未给出最终 video_id（状态未确认，保留会话供后续核实）。
    #[error("YouTube upload server state unconfirmed after full bytes sent for: {filename}")]
    YoutubeUploadStateUnconfirmed {
        /// 文件名
        filename: String,
    },

    // ── RTMP ───────────────────────────────────────────────────────────────────
    /// RTMP URL 未配置。
    #[error("RTMP_URL is not configured. Set it in .env or environment variables.")]
    RtmpUrlNotConfigured,

    /// RTMP 推流失败。
    #[error("RTMP stream failed with exit code {exit_code}")]
    RtmpStreamFailed {
        /// FFmpeg 退出码
        exit_code: i32,
    },

    // ── 配置 ───────────────────────────────────────────────────────────────────
    /// 配置加载失败。
    #[error("Configuration error: {message}")]
    ConfigError {
        /// 错误说明
        message: String,
    },

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
    InvalidArgument {
        /// 错误说明
        message: String,
    },
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
        assert!(err.to_string().contains("outside"));

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
