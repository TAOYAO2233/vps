//! FFprobe 封装。
//!
//! 通过调用 `ffprobe` 命令行工具获取视频文件的元数据（时长等）。
//! 对应 Python 版本的 `get_video_duration` 函数。

use std::path::Path;

use anyhow::Result;
use tracing::warn;

/// 获取视频文件的时长（秒）。
///
/// 调用 `ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1`
/// 获取视频时长。若获取失败，返回 `0.0`。
///
/// # Arguments
///
/// * `path` - 视频文件路径
///
/// # Returns
///
/// 视频时长（秒），若无法获取则返回 `0.0`。
pub async fn get_video_duration(path: &Path) -> Result<f64> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to spawn ffprobe: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let duration_str = stdout.trim();

    match duration_str.parse::<f64>() {
        Ok(d) => Ok(d),
        Err(_) => {
            warn!(path = ?path, output = %duration_str, "ffprobe returned non-numeric duration");
            Ok(0.0)
        }
    }
}

/// 获取视频文件的时长（同步版本，用于测试）。
///
/// 注意：此函数会阻塞当前线程，仅用于非异步上下文。
#[cfg(test)]
pub fn get_video_duration_sync(path: &Path) -> f64 {
    use std::process::Command;
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output();

    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse::<f64>().unwrap_or(0.0)
        }
        Err(_) => 0.0,
    }
}
