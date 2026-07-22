//! FFmpeg 命令封装。
//!
//! 提供各种 FFmpeg 操作的类型安全封装，包括：
//! - concat 合并（直连模式）
//! - 无损封转为 TS 格式（容错模式）
//!
//! 所有操作均支持取消标志检查。

use std::path::Path;

use anyhow::Result;
use tracing::debug;

use crate::core::state::SharedState;

/// FFmpeg 命令执行器。
///
/// 封装 FFmpeg 调用逻辑，提供统一的错误处理和取消检查。
pub struct FfmpegRunner;

impl FfmpegRunner {
    /// 创建新的 FFmpeg 执行器实例。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 执行 concat 合并操作。
    ///
    /// 使用 `ffmpeg -f concat -safe 0 -i {list_file} -c copy -movflags +faststart {output}`
    ///
    /// # Arguments
    ///
    /// * `list_file` - concat list 文件路径
    /// * `output` - 输出文件路径
    /// * `state` - 全局状态（用于检查取消标志和记录 PID）
    ///
    /// # Returns
    ///
    /// FFmpeg 进程的退出状态码（`None` 表示被取消）
    pub async fn run_concat(
        &self,
        list_file: &Path,
        output: &Path,
        state: &SharedState,
    ) -> Result<Option<i32>> {
        debug!(list_file = ?list_file, output = ?output, "Running ffmpeg concat");

        let mut child = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-f", "concat", "-safe", "0", "-i"])
            .arg(list_file)
            .args(["-c", "copy", "-movflags", "+faststart"])
            .arg(output)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn ffmpeg concat: {e}"))?;

        if let Some(pid) = child.id() {
            state.write().await.current_process_pid = Some(pid);
        }

        // 等待完成，同时检查取消标志
        let status = loop {
            if state.read().await.cancel_flag {
                let _ = child.kill().await;
                state.write().await.current_process_pid = None;
                return Ok(None);
            }

            match tokio::time::timeout(std::time::Duration::from_millis(200), child.wait()).await {
                Ok(Ok(s)) => break s,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => continue, // timeout, check cancel flag again
            }
        };

        state.write().await.current_process_pid = None;
        Ok(Some(status.code().unwrap_or(-1)))
    }

    /// 将视频文件无损封转为 MPEG-TS 格式。
    ///
    /// 使用 `ffmpeg -y -i {input} -c copy -f mpegts {output}`
    ///
    /// # Returns
    ///
    /// `true` 表示成功，`false` 表示失败或被取消。
    pub async fn remux_to_ts(
        &self,
        input: &Path,
        output: &Path,
        state: &SharedState,
    ) -> Result<bool> {
        debug!(input = ?input, output = ?output, "Remuxing to TS");

        let mut child = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(input)
            .args(["-c", "copy", "-f", "mpegts"])
            .arg(output)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn ffmpeg remux: {e}"))?;

        if let Some(pid) = child.id() {
            state.write().await.current_process_pid = Some(pid);
        }

        let status = loop {
            if state.read().await.cancel_flag {
                let _ = child.kill().await;
                state.write().await.current_process_pid = None;
                return Ok(false);
            }

            match tokio::time::timeout(std::time::Duration::from_millis(200), child.wait()).await {
                Ok(Ok(s)) => break s,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => continue,
            }
        };

        state.write().await.current_process_pid = None;
        Ok(status.success())
    }
}

impl Default for FfmpegRunner {
    fn default() -> Self {
        Self::new()
    }
}
