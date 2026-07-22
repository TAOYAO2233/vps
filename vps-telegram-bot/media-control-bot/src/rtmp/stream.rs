//! RTMP 推流命令封装。
//!
//! 封装 `ffmpeg -re -i {input} -c copy -f flv {rtmp_url}` 命令的执行逻辑。

use std::path::Path;

use anyhow::Result;
use tracing::debug;

/// RTMP 推流命令构建器。
#[allow(dead_code)]
pub struct RtmpStreamer {
    rtmp_url: String,
}

impl RtmpStreamer {
    /// 创建推流器实例。
    #[must_use]
    #[allow(dead_code)]
    pub fn new(rtmp_url: String) -> Self {
        Self { rtmp_url }
    }

    /// 构建 FFmpeg 推流命令参数列表。
    #[must_use]
    #[allow(dead_code)]
    pub fn build_args(&self, input: &Path) -> Vec<String> {
        vec![
            "-re".to_string(),
            "-i".to_string(),
            input.to_string_lossy().to_string(),
            "-c".to_string(),
            "copy".to_string(),
            "-f".to_string(),
            "flv".to_string(),
            self.rtmp_url.clone(),
        ]
    }

    /// 启动推流子进程（返回 tokio 子进程句柄）。
    ///
    /// # Errors
    ///
    /// 若 FFmpeg 启动失败，返回错误。
    #[allow(dead_code)]
    pub async fn spawn(&self, input: &Path) -> Result<tokio::process::Child> {
        debug!(input = ?input, rtmp_url = %self.rtmp_url, "Spawning RTMP stream process");

        let child = tokio::process::Command::new("ffmpeg")
            .args(self.build_args(input))
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn ffmpeg for RTMP: {e}"))?;

        Ok(child)
    }
}
