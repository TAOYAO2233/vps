//! FFmpeg 命令封装。
//!
//! 严格遵守“**零重编码**”铁律：本模块中的所有 FFmpeg 参数构造器仅允许 `-c copy` 流复制与容器重封装，
//! 严禁出现任何视频或音频编码器参数（如 `libx264`、`libx265`、`aac`、`-vf`、`-crf` 等）。
//!
//! 同时提供：
//! - [`FfmpegLines`]：以 `\r` 与 `\n` 双分隔符切分输出流，防止单行堆积；
//! - [`BoundedStderrCollector`]：持续排空 `stderr`、提取最后媒体时间戳并保留脱敏后的有界错误摘要；
//! - [`FfmpegProcess`]：基于 `-nostats -progress pipe:1` 的结构化进度读取与并发 `stderr` 排空（支持未知时长流）；
//! - [`FfmpegRunner`]：面向无损合并（直接 concat、TS 重封装、容错重封装、前置 `-t` 有界片尾截除）的执行器。

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::core::state::SharedState;
use crate::storage::filesystem::remove_if_exists;
use crate::utils::redact::redact_secrets;

/// 进度上报的最小时间间隔。
pub const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// 进度上报的最小百分比增量。
pub const PROGRESS_MIN_DELTA: f64 = 1.0;

/// 单行字节缓冲上限（64 KiB），防止异常无换行输出导致内存无限增长。
const MAX_LINE_BYTES: usize = 64 * 1024;

/// 有界错误摘要最多保留的诊断行数。
const MAX_STDERR_SUMMARY_LINES: usize = 14;

/// 单条错误摘要行的最大字符长度。
const MAX_STDERR_LINE_CHARS: usize = 240;

/// 发送 FFmpeg `q` 退出指令后的优雅停止等待时限。
const GRACEFUL_QUIT_TIMEOUT: Duration = Duration::from_secs(3);

/// 优雅退出超时后升级 `SIGKILL` 的回收等待时限。
const FORCE_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

/// 先通过 `stdin` 写入 `q\n` 请求 FFmpeg 优雅收尾退出；仅当超时未退出时才升级 `SIGKILL` 并返回诊断标记 `true`。
async fn terminate_ffmpeg_gracefully(
    child: &mut Child,
    stdin_slot: &mut Option<ChildStdin>,
    pid: Option<u32>,
) -> bool {
    if matches!(child.try_wait(), Ok(Some(_))) {
        let _ = stdin_slot.take();
        return false;
    }

    if let Some(mut stdin) = stdin_slot.take() {
        let _ = tokio::time::timeout(Duration::from_millis(250), async {
            let _ = stdin.write_all(b"q\n").await;
            let _ = stdin.flush().await;
        })
        .await;
        drop(stdin);
    }

    if let Ok(Ok(_)) = tokio::time::timeout(GRACEFUL_QUIT_TIMEOUT, child.wait()).await {
        debug!(pid = ?pid, "FFmpeg exited cleanly after graceful 'q' quit signal");
        return false;
    }

    warn!(
        pid = ?pid,
        graceful_timeout_secs = GRACEFUL_QUIT_TIMEOUT.as_secs(),
        "FFmpeg did not exit within graceful timeout; escalating to SIGKILL fallback"
    );
    let _ = child.kill().await;
    match tokio::time::timeout(FORCE_KILL_WAIT_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            warn!(pid = ?pid, error = %e, "Error waiting for FFmpeg after SIGKILL fallback");
        }
        Err(_) => {
            error!(
                pid = ?pid,
                "FFmpeg process still uncollected after SIGKILL fallback timeout"
            );
        }
    }
    true
}

/// 匹配 `time=HH:MM:SS.ss`、`time=MM:SS.ss` 或 `out_time=HH:MM:SS.ssssss`。
static TIME_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|\s|out_)time=(?:(\d+):)?(\d{2}):(\d{2}(?:\.\d+)?)").unwrap());

/// 解析 FFmpeg 输出或 `-progress` 行中的时间戳，返回秒数。
#[must_use]
pub fn parse_progress_time(line: &str) -> Option<f64> {
    let caps = TIME_REGEX.captures(line)?;
    let hours: f64 = caps
        .get(1)
        .map_or(0.0, |m| m.as_str().parse().unwrap_or(0.0));
    let minutes: f64 = caps.get(2)?.as_str().parse().unwrap_or(0.0);
    let seconds: f64 = caps.get(3)?.as_str().parse().unwrap_or(0.0);
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// 以 `\r` 或 `\n` 为分隔符的增量行读取器。
pub struct FfmpegLines<R> {
    reader: R,
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

    /// 读取下一行；返回 `None` 表示 EOF。
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            if let Some(pos) = self.pending.iter().position(|b| *b == b'\r' || *b == b'\n') {
                let buffer: Vec<u8> = self.pending.drain(..=pos).collect();
                let line = &buffer[..buffer.len() - 1];
                if self.pending.first() == Some(&b'\n') {
                    self.pending.drain(..1);
                }
                return Ok(Some(String::from_utf8_lossy(line).into_owned()));
            }

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

/// 有界且自动脱敏的 FFmpeg `stderr` 收集器。
#[derive(Debug, Default)]
pub struct BoundedStderrCollector {
    lines: VecDeque<String>,
    last_progress_secs: Option<f64>,
}

impl BoundedStderrCollector {
    /// 创建空收集器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: VecDeque::with_capacity(MAX_STDERR_SUMMARY_LINES),
            last_progress_secs: None,
        }
    }

    /// 吸收一行原始 `stderr` 文本：提取进度时间戳，并将非刷屏诊断行脱敏截断后存入环形缓冲。
    pub fn push_line(&mut self, raw_line: &str) {
        if let Some(secs) = parse_progress_time(raw_line) {
            if secs.is_finite() && secs >= 0.0 {
                self.last_progress_secs = Some(secs);
            }
        }

        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed.starts_with("frame=") || trimmed.starts_with("size=") {
            return;
        }

        let redacted = redact_secrets(trimmed);
        let mut bounded = String::new();
        for (idx, ch) in redacted.chars().enumerate() {
            if idx >= MAX_STDERR_LINE_CHARS {
                bounded.push('…');
                break;
            }
            bounded.push(ch);
        }

        if self.lines.len() >= MAX_STDERR_SUMMARY_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(bounded);
    }

    /// 返回观测到的最后媒体处理时间位置（秒）。
    #[must_use]
    pub fn last_progress_secs(&self) -> Option<f64> {
        self.last_progress_secs
    }

    /// 输出拼接后的有界脱敏错误摘要。
    #[must_use]
    pub fn summary(&self) -> String {
        self.lines
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 来自 `-progress pipe:1` 的结构化进度事件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FfmpegProgressTick {
    /// 当前已处理/已发送媒体时间戳（秒）
    pub out_time_secs: f64,
    /// 当前已写入/已发送字节数
    pub total_size_bytes: u64,
    /// 若媒体总时长已知（`> 0`），返回百分比（0.0 ~ 100.0）；若时长未知则为 `None`
    pub percent: Option<f64>,
}

/// 已启动的结构化进度 FFmpeg 子进程（并发排空 `stderr` + 读取 `stdout` `-progress pipe:1`）。
pub struct FfmpegProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_lines: FfmpegLines<ChildStdout>,
    stderr_collector: Arc<Mutex<BoundedStderrCollector>>,
    stderr_task: Option<JoinHandle<()>>,
    duration: f64,
    state: SharedState,
    last_emit: Option<Instant>,
    last_percent: f64,
    cur_out_time_secs: f64,
    cur_total_size: u64,
    stopped: bool,
    forced_kill_used: bool,
}

impl FfmpegProcess {
    /// 启动带 `-hide_banner -nostats -stats_period 1 -progress pipe:1` 的无损 FFmpeg 子进程。
    ///
    /// 使用受控管道 `stdin` 以便取消时优先发送 `q\n` 优雅退出，避免常规路径直接 `SIGKILL`。
    pub async fn spawn(args: Vec<OsString>, duration: f64, state: &SharedState) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-hide_banner")
            .arg("-nostats")
            .arg("-stats_period")
            .arg("1")
            .arg("-progress")
            .arg("pipe:1")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "Failed to spawn ffmpeg: {}",
                redact_secrets(&e.to_string())
            )
        })?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture ffmpeg stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture ffmpeg stderr"))?;

        let pid = child.id();
        if let Some(pid) = pid {
            state.write().await.current_process_pid = Some(pid);
        }
        debug!(pid = ?pid, duration = duration, "FFmpeg structured progress process spawned");

        let collector = Arc::new(Mutex::new(BoundedStderrCollector::new()));
        let collector_bg = Arc::clone(&collector);
        let stderr_task = tokio::spawn(async move {
            let mut reader = FfmpegLines::new(stderr);
            while let Ok(Some(line)) = reader.next_line().await {
                collector_bg.lock().await.push_line(&line);
            }
        });

        Ok(Self {
            child,
            stdin,
            stdout_lines: FfmpegLines::new(stdout),
            stderr_collector: collector,
            stderr_task: Some(stderr_task),
            duration,
            state: Arc::clone(state),
            last_emit: None,
            last_percent: -1.0,
            cur_out_time_secs: 0.0,
            cur_total_size: 0,
            stopped: false,
            forced_kill_used: false,
        })
    }

    /// 返回子进程 PID。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// 读取下一个经节流的结构化进度事件；返回 `None` 表示进度流已结束。
    ///
    /// 支持总时长未知（`duration <= 0`）的流：此时 `percent` 为 `None`，按固定间隔上报已推流时长与发送字节数。
    pub async fn next_tick(&mut self) -> std::io::Result<Option<FfmpegProgressTick>> {
        loop {
            let Some(line) = self.stdout_lines.next_line().await? else {
                return Ok(None);
            };

            let trimmed = line.trim();
            if let Some(val) = trimmed.strip_prefix("out_time_us=") {
                if let Ok(us) = val.trim().parse::<i64>() {
                    if us >= 0 {
                        self.cur_out_time_secs = us as f64 / 1_000_000.0;
                    }
                }
            } else if trimmed.starts_with("out_time=") {
                if let Some(secs) = parse_progress_time(trimmed) {
                    if secs >= 0.0 {
                        self.cur_out_time_secs = secs;
                    }
                }
            } else if let Some(val) = trimmed.strip_prefix("total_size=") {
                if let Ok(sz) = val.trim().parse::<u64>() {
                    self.cur_total_size = sz;
                }
            } else if let Some(state_val) = trimmed.strip_prefix("progress=") {
                let is_end = state_val.trim() == "end";
                let interval_ok = match self.last_emit {
                    Some(at) => at.elapsed() >= PROGRESS_MIN_INTERVAL,
                    None => true,
                };

                if self.duration > 0.0 {
                    let pct = ((self.cur_out_time_secs / self.duration) * 100.0).clamp(0.0, 100.0);
                    let delta_ok = pct - self.last_percent >= PROGRESS_MIN_DELTA;
                    if !((delta_ok && interval_ok) || pct >= 100.0 || is_end) {
                        continue;
                    }
                    self.last_emit = Some(Instant::now());
                    self.last_percent = pct.floor();
                    return Ok(Some(FfmpegProgressTick {
                        out_time_secs: self.cur_out_time_secs,
                        total_size_bytes: self.cur_total_size,
                        percent: Some(pct),
                    }));
                } else {
                    if !(interval_ok || is_end) {
                        continue;
                    }
                    self.last_emit = Some(Instant::now());
                    return Ok(Some(FfmpegProgressTick {
                        out_time_secs: self.cur_out_time_secs,
                        total_size_bytes: self.cur_total_size,
                        percent: None,
                    }));
                }
            }
        }
    }

    /// 兼容已知时长调用方的进度方法：返回 `(percent, out_time_secs)`。
    #[allow(dead_code)]
    pub async fn next_progress(&mut self) -> std::io::Result<Option<(f64, f64)>> {
        while let Some(tick) = self.next_tick().await? {
            if let Some(pct) = tick.percent {
                return Ok(Some((pct, tick.out_time_secs)));
            }
        }
        Ok(None)
    }

    /// 优先请求优雅退出并等待子进程回收；若超时未退则触发 `SIGKILL` 兜底并返回 `forced_kill_used = true`。
    pub async fn stop_and_wait(&mut self) -> bool {
        if !self.stopped {
            let pid = self.child.id();
            self.forced_kill_used =
                terminate_ffmpeg_gracefully(&mut self.child, &mut self.stdin, pid).await;
            if let Some(mut task) = self.stderr_task.take() {
                if tokio::time::timeout(Duration::from_secs(1), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                }
            }
            self.state.write().await.current_process_pid = None;
            self.stopped = true;
        }
        self.forced_kill_used
    }

    /// 排空剩余输出并等待子进程退出（期间继续监听取消信号），返回 `(exit_code, redacted_stderr_summary, forced_kill_used)`。
    pub async fn wait_with_summary(
        &mut self,
        cancel_rx: &tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<(Option<i32>, String, bool)> {
        if self.stopped {
            let summary = self.stderr_collector.lock().await.summary();
            return Ok((None, summary, self.forced_kill_used));
        }

        let _ = self.stdin.take();
        let mut cancel_watch = cancel_rx.clone();

        loop {
            if *cancel_watch.borrow() {
                let forced = self.stop_and_wait().await;
                let summary = self.stderr_collector.lock().await.summary();
                return Ok((None, summary, forced));
            }

            tokio::select! {
                line_res = self.stdout_lines.next_line() => {
                    if line_res?.is_none() {
                        break;
                    }
                }
                _ = cancel_watch.changed() => {
                    let forced = self.stop_and_wait().await;
                    let summary = self.stderr_collector.lock().await.summary();
                    return Ok((None, summary, forced));
                }
            }
        }

        let exit_code = tokio::select! {
            status_res = self.child.wait() => status_res?.code(),
            _ = cancel_watch.changed() => {
                let forced = self.stop_and_wait().await;
                let summary = self.stderr_collector.lock().await.summary();
                return Ok((None, summary, forced));
            }
        };

        if let Some(mut task) = self.stderr_task.take() {
            if tokio::time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }

        self.state.write().await.current_process_pid = None;
        self.stopped = true;
        let summary = self.stderr_collector.lock().await.summary();
        Ok((exit_code, summary, self.forced_kill_used))
    }
}

/// 单步 FFmpeg 批处理执行结果（供合并与分段重封装诊断使用）。
#[derive(Debug, Clone)]
pub struct FfmpegStepOutcome {
    /// 退出码（被取消或信号杀死时为 `None`）
    pub exit_code: Option<i32>,
    /// 是否由用户 `/stop` 取消终止
    pub cancelled: bool,
    /// 若触发了取消，子进程是否因优雅退出超时而升级使用了 `SIGKILL`
    pub forced_kill_used: bool,
    /// `stderr` 中观测到的最后处理时间位置（秒）
    pub last_progress_secs: Option<f64>,
    /// 脱敏且有界的 `stderr` 错误摘要
    pub stderr_summary: String,
}

impl FfmpegStepOutcome {
    /// 该步是否以退出码 `0` 正常完成。
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self.cancelled && self.exit_code == Some(0)
    }
}

/// 分段无损重封装为 MPEG-TS 的参数选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct TsRemuxOptions {
    /// 是否启用 `-fflags +discardcorrupt+genpts` 丢弃损坏包并重建时间戳
    pub discard_corrupt_genpts: bool,
    /// 可选的有界保留时长（秒）；启用时严格置于 `-i` 之前以避免全文件解码扫描
    pub keep_duration_secs: Option<f64>,
}

/// 构造 concat 无损合并参数（严禁 `-y` 覆盖，统一使用 `-n` 与 `-c copy`）。
#[must_use]
pub fn build_concat_args(list_file: &Path, temp_output: &Path) -> Vec<OsString> {
    vec![
        OsString::from("-n"),
        OsString::from("-hide_banner"),
        OsString::from("-stats"),
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
        temp_output.as_os_str().to_os_string(),
    ]
}

/// 构造分段无损重封装为 MPEG-TS 的参数。
///
/// 关键约束：
/// - 绝不传入编码器参数，强制 `-c copy -f mpegts`；
/// - 若指定 `keep_duration_secs`，`-t <keep>` 严格置于 `-i <input>` **之前**；
/// - 统一使用 `-n` 禁止覆盖。
#[must_use]
pub fn build_remux_to_ts_args(
    input: &Path,
    ts_output: &Path,
    opts: TsRemuxOptions,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-n"),
        OsString::from("-hide_banner"),
        OsString::from("-stats"),
    ];

    if opts.discard_corrupt_genpts {
        args.push(OsString::from("-fflags"));
        args.push(OsString::from("+discardcorrupt+genpts"));
    }

    if let Some(keep) = opts.keep_duration_secs {
        args.push(OsString::from("-t"));
        args.push(OsString::from(format!("{keep:.3}")));
    }

    args.push(OsString::from("-i"));
    args.push(input.as_os_str().to_os_string());
    args.push(OsString::from("-c"));
    args.push(OsString::from("copy"));
    args.push(OsString::from("-f"));
    args.push(OsString::from("mpegts"));
    args.push(ts_output.as_os_str().to_os_string());
    args
}

/// 构造无损转封装为 MP4 的参数（供 `FfmpegProcess::spawn` 调用，严格 `-n` + `-c copy`）。
#[must_use]
pub fn build_remux_to_mp4_args(input: &Path, temp_output: &Path) -> Vec<OsString> {
    vec![
        OsString::from("-n"),
        OsString::from("-i"),
        input.as_os_str().to_os_string(),
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-movflags"),
        OsString::from("+faststart"),
        temp_output.as_os_str().to_os_string(),
    ]
}

/// 无损合并与分段重封装 FFmpeg 执行器。
pub struct FfmpegRunner;

impl FfmpegRunner {
    /// 创建执行器实例。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 执行一次受控的 FFmpeg 命令（并发排空 `stderr`、即时响应作用域取消通道并优先优雅回收子进程）。
    async fn run_step<I, S>(
        &self,
        args: I,
        state: &SharedState,
        cancel_rx: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<FfmpegStepOutcome>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if *cancel_rx.borrow() {
            return Ok(FfmpegStepOutcome {
                exit_code: None,
                cancelled: true,
                forced_kill_used: false,
                last_progress_secs: None,
                stderr_summary: String::new(),
            });
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "Failed to spawn ffmpeg: {}",
                redact_secrets(&e.to_string())
            )
        })?;

        let mut stdin = child.stdin.take();
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture ffmpeg stderr"))?;

        let pid = child.id();
        if let Some(pid) = pid {
            state.write().await.current_process_pid = Some(pid);
        }

        let collector = Arc::new(Mutex::new(BoundedStderrCollector::new()));
        let collector_bg = Arc::clone(&collector);
        let mut stderr_task = tokio::spawn(async move {
            let mut reader = FfmpegLines::new(stderr);
            while let Ok(Some(line)) = reader.next_line().await {
                collector_bg.lock().await.push_line(&line);
            }
        });

        let mut cancel_watch = cancel_rx.clone();
        let status = tokio::select! {
            wait_res = child.wait() => {
                let _ = stdin.take();
                match wait_res {
                    Ok(st) => st,
                    Err(e) => {
                        if tokio::time::timeout(Duration::from_secs(1), &mut stderr_task).await.is_err() {
                            stderr_task.abort();
                        }
                        state.write().await.current_process_pid = None;
                        return Err(e.into());
                    }
                }
            }
            _ = cancel_watch.changed() => {
                let forced = terminate_ffmpeg_gracefully(&mut child, &mut stdin, pid).await;
                if tokio::time::timeout(Duration::from_secs(1), &mut stderr_task).await.is_err() {
                    stderr_task.abort();
                }
                state.write().await.current_process_pid = None;
                let guard = collector.lock().await;
                return Ok(FfmpegStepOutcome {
                    exit_code: None,
                    cancelled: true,
                    forced_kill_used: forced,
                    last_progress_secs: guard.last_progress_secs(),
                    stderr_summary: guard.summary(),
                });
            }
        };

        if tokio::time::timeout(Duration::from_secs(1), &mut stderr_task)
            .await
            .is_err()
        {
            stderr_task.abort();
        }
        state.write().await.current_process_pid = None;
        let guard = collector.lock().await;

        Ok(FfmpegStepOutcome {
            exit_code: Some(status.code().unwrap_or(-1)),
            cancelled: false,
            forced_kill_used: false,
            last_progress_secs: guard.last_progress_secs(),
            stderr_summary: guard.summary(),
        })
    }

    /// 执行无损 concat 合并（写入临时目录内的 `temp_output`）。
    pub async fn run_concat(
        &self,
        list_file: &Path,
        temp_output: &Path,
        state: &SharedState,
        cancel_rx: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<FfmpegStepOutcome> {
        remove_if_exists(temp_output);
        debug!(list_file = ?list_file, temp_output = ?temp_output, "Running lossless ffmpeg concat");
        let args = build_concat_args(list_file, temp_output);
        self.run_step(args, state, cancel_rx).await
    }

    /// 将单个视频分段无损重封装为临时目录内的 MPEG-TS 文件。
    pub async fn remux_to_ts(
        &self,
        input: &Path,
        ts_output: &Path,
        opts: TsRemuxOptions,
        state: &SharedState,
        cancel_rx: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<FfmpegStepOutcome> {
        remove_if_exists(ts_output);
        debug!(input = ?input, ts_output = ?ts_output, opts = ?opts, "Remuxing segment to lossless TS");
        let args = build_remux_to_ts_args(input, ts_output, opts);
        self.run_step(args, state, cancel_rx).await
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
    fn test_parse_progress_time_variants() {
        let line = "frame= 100 fps=25 q=-1.0 size= 1024kB time=00:01:23.45 bitrate=100.0kbits/s";
        assert!((parse_progress_time(line).unwrap() - 83.45).abs() < 1e-6);
        assert!((parse_progress_time("time=01:23.45").unwrap() - 83.45).abs() < 1e-6);
        assert!((parse_progress_time("out_time=00:02:00.500000").unwrap() - 120.5).abs() < 1e-6);
        assert!(parse_progress_time("frame= 1 fps=0.0 q=0.0").is_none());
    }

    #[test]
    fn test_zero_reencoding_and_t_before_i_in_arg_builders() {
        let forbidden = [
            "-y",
            "libx264",
            "libx265",
            "aac",
            "libopus",
            "-vf",
            "-af",
            "-crf",
            "-b:v",
            "-preset",
        ];

        let concat_args: Vec<String> = build_concat_args(Path::new("list.txt"), Path::new("out.mp4"))
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        for f in forbidden {
            assert!(!concat_args.iter().any(|a| a == f), "Forbidden arg {f} found");
        }
        assert!(concat_args.windows(2).any(|w| w == ["-c", "copy"]));

        let ts_args: Vec<String> = build_remux_to_ts_args(
            Path::new("seg1.flv"),
            Path::new("seg1.ts"),
            TsRemuxOptions {
                discard_corrupt_genpts: true,
                keep_duration_secs: Some(1798.5),
            },
        )
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

        for f in forbidden {
            assert!(!ts_args.iter().any(|a| a == f), "Forbidden arg {f} found");
        }
        assert!(ts_args.windows(2).any(|w| w == ["-c", "copy"]));

        // 验证 -t 严格位于 -i 之前
        let pos_t = ts_args.iter().position(|a| a == "-t").unwrap();
        let pos_i = ts_args.iter().position(|a| a == "-i").unwrap();
        assert!(pos_t < pos_i, "-t ({pos_t}) must precede -i ({pos_i})");
    }

    #[test]
    fn test_bounded_stderr_collector_redacts_and_caps_lines() {
        crate::utils::redact::register_rtmp_secret(
            "rtmp://live.example.invalid/app/fake_stream_secret_xyz",
        );
        let mut c = BoundedStderrCollector::new();
        c.push_line("frame= 50 fps=30 size= 512kB time=00:05:10.00 bitrate=4000kbits/s");
        for idx in 0..30 {
            c.push_line(&format!(
                "[flv] error #{idx} writing to rtmp://live.example.invalid/app/fake_stream_secret_xyz"
            ));
        }

        assert_eq!(c.last_progress_secs(), Some(310.0));
        let summary = c.summary();
        assert!(!summary.contains("fake_stream_secret_xyz"));
        assert_eq!(summary.lines().count(), MAX_STDERR_SUMMARY_LINES);
    }

    #[tokio::test]
    async fn test_ffmpeg_lines_split_and_utf8() {
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
