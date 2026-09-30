//! FFmpeg 命令封装。
//!
//! 提供统一的 FFmpeg 子进程构造与进度解析：
//! - [`FfmpegLines`]：以 `\r` 与 `\n` 双分隔切分 stderr（FFmpeg 的周期性统计行以 `\r` 结尾）
//! - [`parse_progress_time`]：解析 `time=HH:MM:SS.ss` / `time=MM:SS.ss`
//! - [`FfmpegProcess`]：带节流进度回调、取消支持与 `kill_on_drop` 的推流/转码子进程
//! - [`FfmpegRunner`]：concat 合并与无损封转为 TS（不需要进度解析）
//!
//! ## 为什么不能直接用 `AsyncBufReadExt::read_line`
//!
//! FFmpeg 的周期性统计行以 `\r` 结尾，只有最后一次报告使用 `\n`。
//! `read_line` 只按 `\n` 分行，会让整个推流/转码过程中的进度都不刷新，
//! 并把整段 stderr 堆积在内存里直到进程退出。

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, Command};
use tracing::debug;

use crate::core::state::SharedState;

/// 进度上报的最小时间间隔。
pub const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// 进度上报的最小百分比增量。
pub const PROGRESS_MIN_DELTA: f64 = 1.0;

/// stderr 单行缓冲上限（字节）。
///
/// 超过该长度仍未遇到 `\r`/`\n` 时强制吐出一行，避免上游异常输出导致内存无上限增长。
const MAX_LINE_BYTES: usize = 64 * 1024;

/// 匹配 FFmpeg 输出中的 `time=HH:MM:SS.ss` 或 `time=MM:SS.ss`。
static TIME_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"time=(?:(\d+):)?(\d{2}):(\d{2}(?:\.\d+)?)").unwrap());

/// 构造统一配置的 FFmpeg 命令。
///
/// - `-nostdin`：避免 FFmpeg 抢占 Bot 进程的标准输入
/// - stdin 置空、stdout 丢弃
/// - `kill_on_drop`：句柄被丢弃时立即终止子进程，避免孤儿进程
///
/// stderr 策略由调用方决定（需要解析进度时置为 `Stdio::piped()`，否则保持继承）。
fn ffmpeg_command<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("ffmpeg");
    command
        .arg("-nostdin")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    command
}

/// 解析 FFmpeg 日志行中的时间戳，返回秒数。
///
/// 支持 `time=00:01:23.45`（时:分:秒）与 `time=01:23.45`（分:秒）两种写法。
#[must_use]
pub fn parse_progress_time(line: &str) -> Option<f64> {
    let caps = TIME_REGEX.captures(line)?;
    let hours: f64 = caps.get(1).map_or(0.0, |m| m.as_str().parse().unwrap_or(0.0));
    let minutes: f64 = caps.get(2)?.as_str().parse().unwrap_or(0.0);
    let seconds: f64 = caps.get(3)?.as_str().parse().unwrap_or(0.0);
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// 以 `\r` 或 `\n` 为分隔符的增量行读取器。
///
/// 内部使用字节缓冲，因此跨读取块的多字节 UTF-8 字符不会被破坏。
pub struct FfmpegLines<R> {
    /// 底层读取器（通常是子进程的 stderr）
    reader: R,
    /// 尚未遇到分隔符的字节
    pending: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FfmpegLines<R> {
    /// 创建行读取器。
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::with_capacity(8 * 1024),
        }
    }

    /// 读取下一行；返回 `None` 表示流已结束（EOF）。
    ///
    /// # Errors
    ///
    /// 底层读取失败时返回 I/O 错误。
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            if let Some(pos) = self.pending.iter().position(|b| *b == b'\r' || *b == b'\n') {
                let buffer: Vec<u8> = self.pending.drain(..=pos).collect();
                // 去掉分隔符本身
                let line = &buffer[..buffer.len() - 1];
                // `\r\n` 组合：`\r` 已被消费，紧随其后的 `\n` 也一并跳过
                if self.pending.first() == Some(&b'\n') {
                    self.pending.drain(..1);
                }
                return Ok(Some(String::from_utf8_lossy(line).into_owned()));
            }

            // 上游长时间不输出分隔符时强制吐出一行，避免内存无上限增长
            if self.pending.len() >= MAX_LINE_BYTES {
                let line = std::mem::take(&mut self.pending);
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }

            let mut buffer = [0u8; 8192];
            let read = self.reader.read(&mut buffer).await?;
            if read == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.pending);
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            self.pending.extend_from_slice(&buffer[..read]);
        }
    }
}

/// 已启动的 FFmpeg 子进程（带进度解析与取消支持）。
pub struct FfmpegProcess {
    /// 子进程句柄
    child: Child,
    /// stderr 行读取器
    lines: FfmpegLines<ChildStderr>,
    /// 媒体总时长（秒），`0` 表示未知
    duration: f64,
    /// 共享状态（登记/清理当前子进程 PID）
    state: SharedState,
    /// 上次上报时间
    last_emit: Option<Instant>,
    /// 上次上报的百分比
    last_percent: f64,
}

impl FfmpegProcess {
    /// 启动 FFmpeg 子进程，并把 PID 登记到全局状态。
    ///
    /// # Arguments
    ///
    /// * `args` - 除全局选项之外的 FFmpeg 参数
    /// * `duration` - 媒体总时长（秒），`0` 表示未知（不上报进度）
    /// * `state` - 全局状态
    ///
    /// # Errors
    ///
    /// 子进程启动失败或 stderr 无法捕获时返回错误。
    pub async fn spawn(args: Vec<OsString>, duration: f64, state: &SharedState) -> Result<Self> {
        let mut child = ffmpeg_command(args)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn ffmpeg: {e}"))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture ffmpeg stderr"))?;

        let pid = child.id();
        if let Some(pid) = pid {
            state.write().await.current_process_pid = Some(pid);
        }
        debug!(pid = ?pid, duration = duration, "FFmpeg process spawned");

        Ok(Self {
            child,
            lines: FfmpegLines::new(stderr),
            duration,
            state: Arc::clone(state),
            last_emit: None,
            last_percent: -1.0,
        })
    }

    /// 返回子进程 PID。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// 返回下一个进度事件 `(percent, current_secs)`；返回 `None` 表示 stderr 已结束。
    ///
    /// 内部完成节流：百分比增量不小于 [`PROGRESS_MIN_DELTA`] 且距上次上报不短于
    /// [`PROGRESS_MIN_INTERVAL`]，或进度达到 100% 时才返回。
    ///
    /// # Errors
    ///
    /// 读取 stderr 失败时返回 I/O 错误。
    pub async fn next_progress(&mut self) -> std::io::Result<Option<(f64, f64)>> {
        loop {
            let line = match self.lines.next_line().await? {
                Some(line) => line,
                None => return Ok(None),
            };

            let current_secs = match parse_progress_time(&line) {
                Some(secs) => secs,
                None => continue,
            };
            if self.duration <= 0.0 {
                continue;
            }

            let percent = (current_secs / self.duration) * 100.0;
            let interval_ok = match self.last_emit {
                Some(at) => at.elapsed() >= PROGRESS_MIN_INTERVAL,
                None => true,
            };
            let delta_ok = percent - self.last_percent >= PROGRESS_MIN_DELTA;
            if !((delta_ok && interval_ok) || percent >= 100.0) {
                continue;
            }

            self.last_emit = Some(Instant::now());
            self.last_percent = percent.floor();
            return Ok(Some((percent.min(100.0), current_secs)));
        }
    }

    /// 强制终止子进程。
    ///
    /// # Errors
    ///
    /// 终止失败时返回 I/O 错误。
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
    }

    /// 读尽残余 stderr、等待子进程结束，返回退出码。
    ///
    /// `None` 表示进程被信号终止（例如用户取消）。
    ///
    /// # Errors
    ///
    /// 读取 stderr 或等待进程失败时返回 I/O 错误。
    pub async fn wait(&mut self) -> std::io::Result<Option<i32>> {
        // 必须持续消费 stderr，否则管道写满会让 FFmpeg 阻塞
        while self.lines.next_line().await?.is_some() {}
        let status = self.child.wait().await?;
        self.state.write().await.current_process_pid = None;
        Ok(status.code())
    }
}

/// FFmpeg 命令执行器（用于不需要进度解析的合并/封装操作）。
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
    /// FFmpeg 进程的退出码（`None` 表示被取消）
    ///
    /// # Errors
    ///
    /// 子进程启动或等待失败时返回错误。
    pub async fn run_concat(
        &self,
        list_file: &Path,
        output: &Path,
        state: &SharedState,
    ) -> Result<Option<i32>> {
        debug!(list_file = ?list_file, output = ?output, "Running ffmpeg concat");

        let args = vec![
            OsString::from("-y"),
            OsString::from("-nostats"),
            OsString::from("-f"),
            OsString::from("concat"),
            OsString::from("-safe"),
            OsString::from("0"),
            OsString::from("-i"),
            list_file.as_os_str().to_os_string(),
            OsString::from("-c"),
            OsString::from("copy"),
            OsString::from("-movflags"),
            OsString::from("+faststart"),
            output.as_os_str().to_os_string(),
        ];

        let mut child = ffmpeg_command(args)
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

            match tokio::time::timeout(Duration::from_millis(200), child.wait()).await {
                Ok(Ok(status)) => break status,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => continue, // 超时，重新检查取消标志
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
    ///
    /// # Errors
    ///
    /// 子进程启动或等待失败时返回错误。
    pub async fn remux_to_ts(
        &self,
        input: &Path,
        output: &Path,
        state: &SharedState,
    ) -> Result<bool> {
        debug!(input = ?input, output = ?output, "Remuxing to TS");

        let args = vec![
            OsString::from("-y"),
            OsString::from("-nostats"),
            OsString::from("-i"),
            input.as_os_str().to_os_string(),
            OsString::from("-c"),
            OsString::from("copy"),
            OsString::from("-f"),
            OsString::from("mpegts"),
            output.as_os_str().to_os_string(),
        ];

        let mut child = ffmpeg_command(args)
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

            match tokio::time::timeout(Duration::from_millis(200), child.wait()).await {
                Ok(Ok(status)) => break status,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::ReadBuf;

    /// 测试用读取器：每次最多返回 `chunk_size` 字节，用于验证跨读取块的多字节字符。
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let remaining = this.data.len().saturating_sub(this.pos);
            let count = remaining.min(this.chunk_size).min(buf.remaining());
            buf.put_slice(&this.data[this.pos..this.pos + count]);
            this.pos += count;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn test_parse_progress_time_hms() {
        let line = "frame= 100 fps=25 q=-1.0 size= 1024kB time=00:01:23.45 bitrate=100.0kbits/s";
        assert!((parse_progress_time(line).unwrap() - 83.45).abs() < 1e-6);
    }

    #[test]
    fn test_parse_progress_time_ms() {
        assert!((parse_progress_time("time=01:23.45").unwrap() - 83.45).abs() < 1e-6);
    }

    #[test]
    fn test_parse_progress_time_absent() {
        assert!(parse_progress_time("frame= 1 fps=0.0 q=0.0").is_none());
    }

    #[tokio::test]
    async fn test_ffmpeg_lines_split_on_cr_and_lf() {
        let mut lines = FfmpegLines::new(Cursor::new(b"a\rb\r\nc\nd".to_vec()));
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "a");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "b");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "c");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "d");
        assert!(lines.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_ffmpeg_lines_utf8_across_chunks() {
        let reader = ChunkedReader {
            data: "开始\r中文\n结束\n".as_bytes().to_vec(),
            pos: 0,
            chunk_size: 1,
        };
        let mut lines = FfmpegLines::new(reader);
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "开始");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "中文");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "结束");
        assert!(lines.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_ffmpeg_lines_caps_line_length() {
        let reader = Cursor::new(vec![b'x'; MAX_LINE_BYTES + 10]);
        let mut lines = FfmpegLines::new(reader);
        assert_eq!(
            lines.next_line().await.unwrap().unwrap().len(),
            MAX_LINE_BYTES
        );
        assert_eq!(lines.next_line().await.unwrap().unwrap().len(), 10);
        assert!(lines.next_line().await.unwrap().is_none());
    }
}
