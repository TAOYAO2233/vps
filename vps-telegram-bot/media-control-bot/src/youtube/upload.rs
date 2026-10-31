//! 自管 YouTube Data API v3 可恢复上传（Resumable Upload）协议实现。
//!
//! 关键工程保证：
//! - **真实分块控制**：严格按 `YOUTUBE_UPLOAD_CHUNK_MB`（恒为 `256 KiB` 整数倍）切分每次 HTTP `PUT` 请求，
//!   并以 `64 KiB` 流式缓冲直接从原视频文件读取发送，绝不复制整部视频文件或将大块驻留内存；
//! - **源文件身份一致性**：每次分块读取前重新校验路径常规文件属性、大小与 `mtime`，若录播文件被外部篡改立即中止；
//! - **双轨进度模型**：明确区分“本地已读取发送字节”与“服务端 `308 Range` 已确认字节”，且仅在收到 `200/201` 且解析到非空 `video_id` 时才判定上传完成；
//! - **防重复视频重试状态机**：网络抖动、`429`、`5xx` 采用有界指数退避并尊重 `Retry-After`；重试前先发送 `PUT bytes */total` 查询服务端偏移，若最末分块实际已被接收则直接恢复 `video_id`，严禁盲目新建会话产生重复视频；`401` 强制刷新一次 OAuth 令牌，永久 `4xx` 立即失败。

use std::future::Future;
use std::io::SeekFrom;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use hyper::client::connect::Connect;
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE, RETRY_AFTER};
use hyper::{Body, Client, Method, Request};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{debug, info, warn};

use super::oauth::{fetch_access_token, load_authenticator};
use crate::config::{VideoExtensions, YoutubeRecoveryMode};
use crate::errors::AppError;
use crate::utils::redact::{
    redact_secrets, redact_youtube_session_uri, register_youtube_session_uri,
};

/// YouTube Data API v3 视频可恢复上传默认入口端点。
pub const YOUTUBE_RESUMABLE_INIT_URL: &str =
    "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status";

/// YouTube Resumable Upload 协议要求的非末块最小对齐粒度（256 KiB）。
pub const YOUTUBE_CHUNK_GRANULARITY: usize = 256 * 1024;

/// 流式发送单帧读取缓冲大小（64 KiB，控制低配 VPS 内存峰值）。
const STREAM_FRAME_BYTES: usize = 64 * 1024;

/// 最大可恢复瞬时错误重试次数。
const MAX_TRANSIENT_RETRIES: u32 = 5;

/// 单次分块 HTTP 请求超时上限。
const CHUNK_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// 上传实时双轨进度快照。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UploadProgress {
    /// 本地已读取并写入连接发送队列的字节数
    pub local_sent_bytes: u64,
    /// YouTube 服务端通过 `308 Resume Incomplete` `Range` 头确认落盘的字节数
    pub server_confirmed_bytes: u64,
    /// 源文件总字节数
    pub total_bytes: u64,
}

impl UploadProgress {
    /// 计算本地发送百分比（0.0 ~ 100.0）。
    #[must_use]
    pub fn local_sent_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            ((self.local_sent_bytes as f64 / self.total_bytes as f64) * 100.0).clamp(0.0, 100.0)
        }
    }

    /// 计算服务端确认百分比（0.0 ~ 100.0）。
    #[must_use]
    pub fn server_confirmed_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            ((self.server_confirmed_bytes as f64 / self.total_bytes as f64) * 100.0)
                .clamp(0.0, 100.0)
        }
    }
}

/// 源文件身份指纹（防止上传过程中原文件被外部重写或截断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFileIdentity {
    /// 初始文件大小（字节）
    pub size: u64,
    /// 初始修改时间（UNIX 纳秒）
    pub modified_nanos: Option<u128>,
}

impl SourceFileIdentity {
    /// 捕获普通视频文件的初始大小与修改时间戳。
    pub fn capture(path: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| AppError::FileIo {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(AppError::invalid_argument("上传目标不是普通视频文件").into());
        }
        if meta.len() == 0 {
            return Err(AppError::invalid_argument("上传视频文件大小为 0 字节").into());
        }
        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());

        Ok(Self {
            size: meta.len(),
            modified_nanos,
        })
    }

    /// 在每个分块上传前重新校验源文件未被修改、替换或截断。
    pub fn verify_unchanged(&self, path: &Path) -> Result<()> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let meta = std::fs::symlink_metadata(path).map_err(|_| AppError::SourceFileChanged {
            filename: filename.clone(),
        })?;

        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(AppError::SourceFileChanged { filename }.into());
        }

        let cur_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());

        if meta.len() != self.size || cur_mtime != self.modified_nanos {
            return Err(AppError::SourceFileChanged { filename }.into());
        }

        Ok(())
    }
}

/// 单次可恢复上传运行参数。
#[derive(Debug, Clone)]
pub struct ResumableUploadOptions {
    /// 上传初始化端点（生产环境使用 [`YOUTUBE_RESUMABLE_INIT_URL`]）
    pub init_endpoint: String,
    /// 分块大小（字节，必须为 `256 KiB` 的整数倍）
    pub chunk_size_bytes: usize,
    /// 可选的重启恢复既有 `session_uri`
    pub existing_session_uri: Option<String>,
    /// 重启恢复模式（决定既有会话未完成时是续传还是重传）
    pub recovery_mode: YoutubeRecoveryMode,
    /// 基础退避间隔（默认 2 秒，单元测试可缩短）
    pub base_backoff: Duration,
}

impl Default for ResumableUploadOptions {
    fn default() -> Self {
        Self {
            init_endpoint: YOUTUBE_RESUMABLE_INIT_URL.to_string(),
            chunk_size_bytes: 10 * 1024 * 1024,
            existing_session_uri: None,
            recovery_mode: YoutubeRecoveryMode::None,
            base_backoff: Duration::from_secs(2),
        }
    }
}

#[derive(Serialize)]
struct VideoInsertMetadata {
    snippet: VideoInsertSnippet,
    status: VideoInsertStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoInsertSnippet {
    title: String,
    description: String,
    category_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoInsertStatus {
    privacy_status: String,
    self_declared_made_for_kids: bool,
}

#[derive(Deserialize)]
struct VideoInsertResponse {
    id: Option<String>,
}

/// YouTube 视频上传器（对外高层入口）。
pub struct YoutubeUploader {
    token_file: std::path::PathBuf,
    chunk_size_bytes: usize,
}

impl YoutubeUploader {
    /// 创建上传器实例。
    ///
    /// `chunk_size_bytes` 会自动向上取整对齐到 `256 KiB` 整数倍（配置层亦已强校验）。
    #[must_use]
    pub fn new(token_file: std::path::PathBuf, chunk_size_bytes: usize) -> Self {
        let aligned = align_chunk_size(chunk_size_bytes);
        Self {
            token_file,
            chunk_size_bytes: aligned,
        }
    }

    /// Query an existing upload session without sending video data or creating a new session.
    pub async fn verify_session(&self, session_uri: &str, total_bytes: u64) -> Result<Option<String>> {
        let auth = load_authenticator(&self.token_file).await?;
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        let client = Client::builder().build::<_, Body>(https);
        let mut token = fetch_access_token(&auth, false).await?;
        for attempt in 0..2 {
            match query_session_status(&client, session_uri, &token, total_bytes).await {
                SessionQueryOutcome::Completed(video_id) => return Ok(Some(video_id)),
                SessionQueryOutcome::Incomplete(_) => return Ok(None),
                SessionQueryOutcome::Unauthorized if attempt == 0 => {
                    token = fetch_access_token(&auth, true).await?;
                }
                SessionQueryOutcome::Unauthorized => anyhow::bail!("YouTube OAuth authorization failed"),
                SessionQueryOutcome::Expired => anyhow::bail!("YouTube upload session expired; verify in YouTube Studio"),
                SessionQueryOutcome::Transient { reason, .. }
                | SessionQueryOutcome::Permanent(reason) => anyhow::bail!("{}", redact_secrets(&reason)),
            }
        }
        anyhow::bail!("YouTube upload session could not be verified")
    }

    /// 执行私享视频可恢复上传，返回服务端确认的 `video_id`。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload<ProgFn, SessFn, SessFut>(
        &self,
        file_path: &Path,
        title: &str,
        existing_session_uri: Option<String>,
        recovery_mode: YoutubeRecoveryMode,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
        progress_callback: ProgFn,
        session_callback: SessFn,
    ) -> Result<String>
    where
        ProgFn: FnMut(UploadProgress) + Send,
        SessFn: FnMut(String) -> SessFut + Send,
        SessFut: Future<Output = Result<()>> + Send,
    {
        let auth = std::sync::Arc::new(load_authenticator(&self.token_file).await?);

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        let client = Client::builder().build::<_, Body>(https);

        let opts = ResumableUploadOptions {
            init_endpoint: YOUTUBE_RESUMABLE_INIT_URL.to_string(),
            chunk_size_bytes: self.chunk_size_bytes,
            existing_session_uri,
            recovery_mode,
            base_backoff: Duration::from_secs(2),
        };

        execute_resumable_upload(
            &client,
            file_path,
            title,
            opts,
            move |force_refresh| {
                let auth_clone = std::sync::Arc::clone(&auth);
                async move { fetch_access_token(&auth_clone, force_refresh).await }
            },
            cancel_rx,
            progress_callback,
            session_callback,
        )
        .await
    }
}

/// 将任意分块字节数规范化为合法的 `256 KiB` 正整数倍。
#[must_use]
pub fn align_chunk_size(raw_bytes: usize) -> usize {
    let clamped = raw_bytes.max(YOUTUBE_CHUNK_GRANULARITY);
    let rem = clamped % YOUTUBE_CHUNK_GRANULARITY;
    if rem == 0 {
        clamped
    } else {
        clamped + (YOUTUBE_CHUNK_GRANULARITY - rem)
    }
}

/// 解析 `308 Resume Incomplete` 响应中的 `Range` 头（形如 `bytes=0-10485759`），返回下一待传偏移字节。
///
/// 若 `308` 响应未携带 `Range` 头，根据 Google Resumable Upload 规范表示服务端已确认 `0` 字节。
pub fn parse_confirmed_offset_from_range(range_header: Option<&str>, total_bytes: u64) -> Result<u64> {
    let Some(raw) = range_header else {
        return Ok(0);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }

    let without_unit = trimmed
        .strip_prefix("bytes=")
        .or_else(|| trimmed.strip_prefix("bytes "))
        .unwrap_or(trimmed);

    let Some((_start_str, end_str)) = without_unit.split_once('-') else {
        anyhow::bail!("Malformed 308 Range header");
    };

    let end_inclusive: u64 = end_str
        .trim()
        .parse()
        .context("Invalid end byte in 308 Range header")?;
    let next_offset = end_inclusive
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Range end byte overflow"))?;

    if next_offset > total_bytes {
        anyhow::bail!("Server confirmed offset ({next_offset}) exceeds file size ({total_bytes})");
    }

    Ok(next_offset)
}

#[derive(Debug)]
enum SessionQueryOutcome {
    Completed(String),
    Incomplete(u64),
    Unauthorized,
    Expired,
    Transient {
        retry_after: Option<Duration>,
        reason: String,
    },
    Permanent(String),
}

#[derive(Debug)]
enum ChunkPutOutcome {
    Completed(String),
    Incomplete(u64),
    Unauthorized,
    Expired,
    Transient {
        retry_after: Option<Duration>,
        reason: String,
    },
    Permanent(String),
}

/// 协议核心执行引擎（支持注入自定义 HTTP Connector 与 Token 提供器，供离线协议级单元测试验证）。
#[allow(clippy::too_many_arguments)]
pub async fn execute_resumable_upload<C, TokenFn, TokenFut, ProgFn, SessFn, SessFut>(
    client: &Client<C, Body>,
    file_path: &Path,
    title: &str,
    opts: ResumableUploadOptions,
    mut token_provider: TokenFn,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    mut progress_callback: ProgFn,
    mut session_callback: SessFn,
) -> Result<String>
where
    C: Connect + Clone + Send + Sync + 'static,
    TokenFn: FnMut(bool) -> TokenFut + Send,
    TokenFut: Future<Output = Result<String>> + Send,
    ProgFn: FnMut(UploadProgress) + Send,
    SessFn: FnMut(String) -> SessFut + Send,
    SessFut: Future<Output = Result<()>> + Send,
{
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video")
        .to_string();

    let source_id = SourceFileIdentity::capture(file_path)?;
    let total_bytes = source_id.size;
    let chunk_size = align_chunk_size(opts.chunk_size_bytes);

    // Keep the local filename intact, but omit its video extension in YouTube metadata.
    let upload_title = if VideoExtensions::is_video(title) {
        title
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .filter(|stem| !stem.is_empty())
            .unwrap_or(title)
    } else {
        title
    };
    let title_truncated: String = upload_title.chars().take(95).collect();
    let mut access_token = token_provider(false).await?;
    let mut refreshed_after_401 = false;
    let mut session_restarts: u32 = 0;
    let mut transient_retries: u32 = 0;
    let mut full_confirmation_retries: u32 = 0;
    let mut no_progress_retries: u32 = 0;

    let mut session_uri_opt = opts.existing_session_uri.clone();
    let mut need_status_query = session_uri_opt.is_some();
    let mut is_initial_recovery_query = session_uri_opt.is_some();
    let mut confirmed_offset: u64 = 0;

    progress_callback(UploadProgress {
        local_sent_bytes: 0,
        server_confirmed_bytes: 0,
        total_bytes,
    });

    loop {
        if *cancel_rx.borrow() {
            return Err(AppError::YoutubeUploadCancelled {
                filename: filename.clone(),
            }
            .into());
        }

        // 每次网络交互前校验原文件未被改动
        source_id.verify_unchanged(file_path)?;

        // 1. 若尚无上传会话，先发起 POST 创建 Resumable Session
        let session_uri = match &session_uri_opt {
            Some(uri) => uri.clone(),
            None => {
                match initiate_resumable_session(
                    client,
                    &opts.init_endpoint,
                    &access_token,
                    &title_truncated,
                    total_bytes,
                )
                .await
                {
                    Ok(new_uri) => {
                        register_youtube_session_uri(&new_uri);
                        // The session must be durable before the first video byte is sent.
                        session_callback(new_uri.clone()).await?;
                        info!(
                            filename = %filename,
                            session = %redact_youtube_session_uri(&new_uri),
                            chunk_mb = chunk_size / (1024 * 1024),
                            "Initiated YouTube resumable upload session"
                        );
                        session_uri_opt = Some(new_uri.clone());
                        confirmed_offset = 0;
                        full_confirmation_retries = 0;
                        need_status_query = false;
                        refreshed_after_401 = false;
                        new_uri
                    }
                    Err(InitSessionError::Unauthorized) if !refreshed_after_401 => {
                        refreshed_after_401 = true;
                        access_token = token_provider(true).await?;
                        continue;
                    }
                    Err(InitSessionError::Unauthorized) => {
                        return Err(AppError::YoutubeApiError {
                            message: "YouTube OAuth 授权失败（刷新令牌后仍返回 401）".to_string(),
                        }
                        .into());
                    }
                    Err(InitSessionError::Transient {
                        retry_after,
                        reason,
                    }) => {
                        transient_retries += 1;
                        if transient_retries > MAX_TRANSIENT_RETRIES {
                            return Err(AppError::YoutubeApiError {
                                message: format!("创建上传会话连续超时或失败: {reason}"),
                            }
                            .into());
                        }
                        wait_backoff_or_cancel(
                            retry_after,
                            opts.base_backoff,
                            transient_retries,
                            &mut cancel_rx,
                            &filename,
                        )
                        .await?;
                        continue;
                    }
                    Err(InitSessionError::Permanent(msg)) => {
                        return Err(AppError::YoutubeApiError { message: msg }.into());
                    }
                }
            }
        };

        // 2. 重试前或恢复启动时：先查询既有会话的已确认偏移，严防盲目重传造成重复视频
        if need_status_query {
            match query_session_status(client, &session_uri, &access_token, total_bytes).await {
                SessionQueryOutcome::Completed(video_id) => {
                    progress_callback(UploadProgress {
                        local_sent_bytes: total_bytes,
                        server_confirmed_bytes: total_bytes,
                        total_bytes,
                    });
                    info!(
                        filename = %filename,
                        video_id = %video_id,
                        "Recovered completed YouTube video_id via session status query"
                    );
                    return Ok(video_id);
                }
                SessionQueryOutcome::Incomplete(server_offset) => {
                    need_status_query = false;
                    refreshed_after_401 = false;

                    if is_initial_recovery_query
                        && opts.recovery_mode == YoutubeRecoveryMode::Requeue
                        && server_offset < total_bytes
                    {
                        // requeue 策略：确认旧会话未完成后，丢弃半截会话并从头新建上传
                        is_initial_recovery_query = false;
                        session_uri_opt = None;
                        confirmed_offset = 0;
                        continue;
                    }

                    is_initial_recovery_query = false;
                    if server_offset > confirmed_offset {
                        no_progress_retries = 0;
                    }
                    confirmed_offset = server_offset;
                    if server_offset >= total_bytes {
                        full_confirmation_retries += 1;
                        if full_confirmation_retries > MAX_TRANSIENT_RETRIES {
                            return Err(AppError::YoutubeUploadStateUnconfirmed {
                                filename: filename.clone(),
                            }
                            .into());
                        }
                        wait_backoff_or_cancel(
                            None,
                            opts.base_backoff,
                            full_confirmation_retries,
                            &mut cancel_rx,
                            &filename,
                        )
                        .await?;
                    } else {
                        full_confirmation_retries = 0;
                    }
                    progress_callback(UploadProgress {
                        local_sent_bytes: confirmed_offset,
                        server_confirmed_bytes: confirmed_offset,
                        total_bytes,
                    });
                }
                SessionQueryOutcome::Unauthorized if !refreshed_after_401 => {
                    refreshed_after_401 = true;
                    access_token = token_provider(true).await?;
                    continue;
                }
                SessionQueryOutcome::Unauthorized => {
                    return Err(AppError::YoutubeApiError {
                        message: "查询上传进度时 OAuth 鉴权失败（401）".to_string(),
                    }
                    .into());
                }
                SessionQueryOutcome::Expired => {
                    if confirmed_offset >= total_bytes
                        || (is_initial_recovery_query
                            && opts.recovery_mode == YoutubeRecoveryMode::Resume)
                    {
                        return Err(AppError::YoutubeUploadStateUnconfirmed {
                            filename: filename.clone(),
                        }
                        .into());
                    }
                    is_initial_recovery_query = false;
                    need_status_query = false;
                    if session_restarts < 1 {
                        session_restarts += 1;
                        warn!(
                            filename = %filename,
                            "Resumable upload session expired (404/410); starting one fresh session"
                        );
                        session_uri_opt = None;
                        confirmed_offset = 0;
                        continue;
                    }
                    return Err(AppError::YoutubeApiError {
                        message: "YouTube 可恢复上传会话已失效且无法重新建立".to_string(),
                    }
                    .into());
                }
                SessionQueryOutcome::Transient {
                    retry_after,
                    reason,
                } => {
                    transient_retries += 1;
                    if transient_retries > MAX_TRANSIENT_RETRIES {
                        if confirmed_offset >= total_bytes {
                            return Err(AppError::YoutubeUploadStateUnconfirmed {
                                filename: filename.clone(),
                            }
                            .into());
                        }
                        return Err(AppError::YoutubeApiError {
                            message: format!("查询上传会话状态重试耗尽: {reason}"),
                        }
                        .into());
                    }
                    wait_backoff_or_cancel(
                        retry_after,
                        opts.base_backoff,
                        transient_retries,
                        &mut cancel_rx,
                        &filename,
                    )
                    .await?;
                    continue;
                }
                SessionQueryOutcome::Permanent(msg) => {
                    if confirmed_offset >= total_bytes {
                        return Err(AppError::YoutubeUploadStateUnconfirmed {
                            filename: filename.clone(),
                        }
                        .into());
                    }
                    return Err(AppError::YoutubeApiError { message: msg }.into());
                }
            }
        }

        if confirmed_offset >= total_bytes {
            // 所有字节均已发送但尚未拿到最终 video_id，触发一次状态查询回收完成响应
            need_status_query = true;
            continue;
        }

        // 3. 发送下一个分块（非末块恒为 chunk_size 即 256 KiB 整数倍）
        let remaining = total_bytes - confirmed_offset;
        let chunk_len = remaining.min(chunk_size as u64) as usize;

        debug!(
            filename = %filename,
            offset = confirmed_offset,
            chunk_len = chunk_len,
            total_bytes = total_bytes,
            "Uploading resumable video chunk"
        );

        let put_outcome = upload_single_chunk(
            client,
            file_path,
            &source_id,
            &session_uri,
            &access_token,
            confirmed_offset,
            chunk_len,
            total_bytes,
            &mut cancel_rx,
            &mut progress_callback,
        )
        .await?;

        match put_outcome {
            ChunkPutOutcome::Completed(video_id) => {
                progress_callback(UploadProgress {
                    local_sent_bytes: total_bytes,
                    server_confirmed_bytes: total_bytes,
                    total_bytes,
                });
                info!(filename = %filename, video_id = %video_id, "YouTube resumable upload succeeded");
                return Ok(video_id);
            }
            ChunkPutOutcome::Incomplete(new_confirmed) => {
                refreshed_after_401 = false;
                if new_confirmed <= confirmed_offset {
                    no_progress_retries += 1;
                    if no_progress_retries > MAX_TRANSIENT_RETRIES {
                        return Err(AppError::YoutubeUploadStateUnconfirmed {
                            filename: filename.clone(),
                        }
                        .into());
                    }
                    wait_backoff_or_cancel(
                        None,
                        opts.base_backoff,
                        no_progress_retries,
                        &mut cancel_rx,
                        &filename,
                    )
                    .await?;
                    need_status_query = true;
                } else {
                    no_progress_retries = 0;
                    transient_retries = 0;
                    confirmed_offset = new_confirmed;
                    progress_callback(UploadProgress {
                        local_sent_bytes: confirmed_offset,
                        server_confirmed_bytes: confirmed_offset,
                        total_bytes,
                    });
                }
            }
            ChunkPutOutcome::Unauthorized if !refreshed_after_401 => {
                refreshed_after_401 = true;
                access_token = token_provider(true).await?;
                need_status_query = true;
            }
            ChunkPutOutcome::Unauthorized => {
                return Err(AppError::YoutubeApiError {
                    message: "分块上传期间 OAuth 令牌失效且刷新后仍被拒绝（401）".to_string(),
                }
                .into());
            }
            ChunkPutOutcome::Expired => {
                if session_restarts < 1 {
                    session_restarts += 1;
                    warn!(filename = %filename, "Upload session expired mid-upload; restarting once");
                    session_uri_opt = None;
                    confirmed_offset = 0;
                    continue;
                }
                return Err(AppError::YoutubeApiError {
                    message: "上传会话中途过期（404/410）".to_string(),
                }
                .into());
            }
            ChunkPutOutcome::Transient {
                retry_after,
                reason,
            } => {
                transient_retries += 1;
                if transient_retries > MAX_TRANSIENT_RETRIES {
                    return Err(AppError::YoutubeUploadStateUnconfirmed {
                        filename: filename.clone(),
                    }
                    .into());
                }
                warn!(
                    filename = %filename,
                    attempt = transient_retries,
                    reason = %redact_secrets(&reason),
                    "Transient error during YouTube chunk upload; will query status before retry"
                );
                wait_backoff_or_cancel(
                    retry_after,
                    opts.base_backoff,
                    transient_retries,
                    &mut cancel_rx,
                    &filename,
                )
                .await?;
                // 关键规则：重试发送数据前必须先向既有 session 查询服务端实际确认偏移！
                need_status_query = true;
            }
            ChunkPutOutcome::Permanent(msg) => {
                return Err(AppError::YoutubeApiError { message: msg }.into());
            }
        }
    }
}

enum InitSessionError {
    Unauthorized,
    Transient {
        retry_after: Option<Duration>,
        reason: String,
    },
    Permanent(String),
}

async fn initiate_resumable_session<C>(
    client: &Client<C, Body>,
    init_endpoint: &str,
    access_token: &str,
    title: &str,
    total_bytes: u64,
) -> std::result::Result<String, InitSessionError>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let payload = VideoInsertMetadata {
        snippet: VideoInsertSnippet {
            title: title.to_string(),
            description: String::new(),
            category_id: "22".to_string(),
        },
        status: VideoInsertStatus {
            privacy_status: "private".to_string(),
            self_declared_made_for_kids: false,
        },
    };

    let json_bytes = serde_json::to_vec(&payload)
        .map_err(|e| InitSessionError::Permanent(format!("构造视频元数据失败: {e}")))?;

    let req = Request::builder()
        .method(Method::POST)
        .uri(init_endpoint)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_TYPE, "application/json; charset=UTF-8")
        .header("X-Upload-Content-Length", total_bytes.to_string())
        .header("X-Upload-Content-Type", "video/*")
        .body(Body::from(json_bytes))
        .map_err(|e| InitSessionError::Permanent(redact_secrets(&e.to_string())))?;

    let resp = match tokio::time::timeout(Duration::from_secs(60), client.request(req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(InitSessionError::Transient {
                retry_after: None,
                reason: redact_secrets(&e.to_string()),
            });
        }
        Err(_) => {
            return Err(InitSessionError::Transient {
                retry_after: None,
                reason: "初始化上传会话请求超时".to_string(),
            });
        }
    };

    let code = resp.status().as_u16();
    let retry_after = extract_retry_after(resp.headers());

    if (200..300).contains(&code) {
        let loc = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                InitSessionError::Permanent("YouTube 未在响应头中返回可恢复上传 Location".to_string())
            })?;
        return Ok(loc.to_string());
    }

    let body_text = read_bounded_body_text(resp.into_body()).await;
    if code == 401 {
        Err(InitSessionError::Unauthorized)
    } else if is_transient_http_status(code) {
        Err(InitSessionError::Transient {
            retry_after,
            reason: format!("HTTP {code}"),
        })
    } else {
        Err(InitSessionError::Permanent(format_youtube_http_error(
            code, &body_text,
        )))
    }
}

async fn query_session_status<C>(
    client: &Client<C, Body>,
    session_uri: &str,
    access_token: &str,
    total_bytes: u64,
) -> SessionQueryOutcome
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let req = match Request::builder()
        .method(Method::PUT)
        .uri(session_uri)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_LENGTH, "0")
        .header(CONTENT_RANGE, format!("bytes */{total_bytes}"))
        .body(Body::empty())
    {
        Ok(r) => r,
        Err(e) => return SessionQueryOutcome::Permanent(redact_secrets(&e.to_string())),
    };

    let resp = match tokio::time::timeout(Duration::from_secs(60), client.request(req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return SessionQueryOutcome::Transient {
                retry_after: None,
                reason: redact_secrets(&e.to_string()),
            };
        }
        Err(_) => {
            return SessionQueryOutcome::Transient {
                retry_after: None,
                reason: "会话进度查询超时".to_string(),
            };
        }
    };

    let code = resp.status().as_u16();
    let retry_after = extract_retry_after(resp.headers());
    let range_hdr = resp
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let body_text = read_bounded_body_text(resp.into_body()).await;

    match code {
        200 | 201 => match parse_video_id_from_json(&body_text) {
            Ok(vid) => SessionQueryOutcome::Completed(vid),
            Err(msg) => SessionQueryOutcome::Permanent(msg),
        },
        308 => match parse_confirmed_offset_from_range(range_hdr.as_deref(), total_bytes) {
            Ok(offset) => SessionQueryOutcome::Incomplete(offset),
            Err(e) => SessionQueryOutcome::Permanent(redact_secrets(&e.to_string())),
        },
        401 => SessionQueryOutcome::Unauthorized,
        404 | 410 => SessionQueryOutcome::Expired,
        c if is_transient_http_status(c) => SessionQueryOutcome::Transient {
            retry_after,
            reason: format!("HTTP {c}"),
        },
        c => SessionQueryOutcome::Permanent(format_youtube_http_error(c, &body_text)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_single_chunk<C, ProgFn>(
    client: &Client<C, Body>,
    file_path: &Path,
    source_id: &SourceFileIdentity,
    session_uri: &str,
    access_token: &str,
    offset: u64,
    chunk_len: usize,
    total_bytes: u64,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    progress_callback: &mut ProgFn,
) -> Result<ChunkPutOutcome>
where
    C: Connect + Clone + Send + Sync + 'static,
    ProgFn: FnMut(UploadProgress) + Send,
{
    source_id.verify_unchanged(file_path)?;

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video")
        .to_string();

    let mut file = tokio::fs::File::open(file_path).await.map_err(|e| {
        AppError::FileIo {
            path: file_path.to_path_buf(),
            message: e.to_string(),
        }
    })?;
    file.seek(SeekFrom::Start(offset)).await?;

    let end_inclusive = offset + (chunk_len as u64) - 1;
    let (mut sender, body) = Body::channel();

    let req = Request::builder()
        .method(Method::PUT)
        .uri(session_uri)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_LENGTH, chunk_len.to_string())
        .header(
            CONTENT_RANGE,
            format!("bytes {offset}-{end_inclusive}/{total_bytes}"),
        )
        .body(body)
        .map_err(|e| anyhow::anyhow!("{}", redact_secrets(&e.to_string())))?;

    let request_fut = client.request(req);
    tokio::pin!(request_fut);

    let mut buf = vec![0u8; STREAM_FRAME_BYTES];
    let mut sent_in_chunk: usize = 0;

    // 边读取 64 KiB 切片边推入 HTTP 请求体，同时监听对端提前响应与用户 /stop 取消
    let resp_result = loop {
        if *cancel_rx.borrow() {
            sender.abort();
            return Err(AppError::YoutubeUploadCancelled { filename }.into());
        }

        if sent_in_chunk >= chunk_len {
            // 关闭发送端（EOF），等待服务端返回 308 或 200/201
            drop(sender);
            let awaited = tokio::select! {
                res = &mut request_fut => Some(res),
                _ = cancel_rx.changed() => None,
                _ = tokio::time::sleep(CHUNK_REQUEST_TIMEOUT) => {
                    return Ok(ChunkPutOutcome::Transient {
                        retry_after: None,
                        reason: "等待服务端确认分块响应超时".to_string(),
                    });
                }
            };
            match awaited {
                Some(res) => break res,
                None => return Err(AppError::YoutubeUploadCancelled { filename }.into()),
            }
        }

        let to_read = (chunk_len - sent_in_chunk).min(STREAM_FRAME_BYTES);
        let n = file.read(&mut buf[..to_read]).await?;
        if n == 0 {
            sender.abort();
            return Err(AppError::SourceFileChanged { filename }.into());
        }

        let frame = Bytes::copy_from_slice(&buf[..n]);
        tokio::select! {
            send_res = sender.send_data(frame) => {
                match send_res {
                    Ok(()) => {
                        sent_in_chunk += n;
                        progress_callback(UploadProgress {
                            local_sent_bytes: offset + sent_in_chunk as u64,
                            server_confirmed_bytes: offset,
                            total_bytes,
                        });
                    }
                    Err(_) => {
                        // 服务端已提前返回状态码（如 401/429/503）并关闭接收通道，直接读取响应
                        drop(sender);
                        match tokio::time::timeout(Duration::from_secs(30), &mut request_fut).await {
                            Ok(res) => break res,
                            Err(_) => {
                                return Ok(ChunkPutOutcome::Transient {
                                    retry_after: None,
                                    reason: "连接在分块传输期间中断".to_string(),
                                });
                            }
                        }
                    }
                }
            }
            res = &mut request_fut => {
                break res;
            }
            _ = cancel_rx.changed() => {
                sender.abort();
                return Err(AppError::YoutubeUploadCancelled { filename }.into());
            }
        }
    };

    let resp = match resp_result {
        Ok(r) => r,
        Err(e) => {
            return Ok(ChunkPutOutcome::Transient {
                retry_after: None,
                reason: redact_secrets(&e.to_string()),
            });
        }
    };

    let code = resp.status().as_u16();
    let retry_after = extract_retry_after(resp.headers());
    let range_hdr = resp
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body_text = read_bounded_body_text(resp.into_body()).await;

    Ok(match code {
        200 | 201 => match parse_video_id_from_json(&body_text) {
            Ok(vid) => ChunkPutOutcome::Completed(vid),
            Err(msg) => ChunkPutOutcome::Permanent(msg),
        },
        308 => match parse_confirmed_offset_from_range(range_hdr.as_deref(), total_bytes) {
            Ok(confirmed) => ChunkPutOutcome::Incomplete(confirmed),
            Err(e) => ChunkPutOutcome::Permanent(redact_secrets(&e.to_string())),
        },
        401 => ChunkPutOutcome::Unauthorized,
        404 | 410 => ChunkPutOutcome::Expired,
        c if is_transient_http_status(c) => ChunkPutOutcome::Transient {
            retry_after,
            reason: format!("HTTP {c}"),
        },
        c => ChunkPutOutcome::Permanent(format_youtube_http_error(c, &body_text)),
    })
}

fn is_transient_http_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

fn extract_retry_after(headers: &hyper::HeaderMap) -> Option<Duration> {
    let val = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let secs: u64 = val.parse().ok()?;
    Some(Duration::from_secs(secs.clamp(1, 120)))
}

async fn wait_backoff_or_cancel(
    retry_after: Option<Duration>,
    base_backoff: Duration,
    attempt: u32,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    filename: &str,
) -> Result<()> {
    let shift = attempt.saturating_sub(1).min(5);
    let exp_delay = base_backoff
        .saturating_mul(1u32 << shift)
        .min(Duration::from_secs(60));
    let delay = retry_after.map_or(exp_delay, |ra| ra.max(exp_delay));

    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = cancel_rx.changed() => Err(AppError::YoutubeUploadCancelled {
            filename: filename.to_string(),
        }.into()),
    }
}

async fn read_bounded_body_text(mut body: Body) -> String {
    use hyper::body::HttpBody;
    let mut collected = Vec::new();
    while let Some(Ok(chunk)) = body.data().await {
        let remain = (64 * 1024usize).saturating_sub(collected.len());
        if remain == 0 {
            break;
        }
        let slice = &chunk[..chunk.len().min(remain)];
        collected.extend_from_slice(slice);
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn parse_video_id_from_json(body_text: &str) -> std::result::Result<String, String> {
    let parsed: VideoInsertResponse = serde_json::from_str(body_text).map_err(|_| {
        "YouTube 返回了 200/201，但响应体不是合法的 Video JSON（拒绝误判为成功）".to_string()
    })?;
    let vid = parsed
        .id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "YouTube 响应未包含有效的 video_id（拒绝误判为成功）".to_string())?;
    Ok(vid)
}

fn format_youtube_http_error(code: u16, body_text: &str) -> String {
    let safe_body = redact_secrets(body_text);
    let lower = safe_body.to_ascii_lowercase();
    if code == 403 && (lower.contains("quotaexceeded") || lower.contains("uploadlimitexceeded")) {
        return "YouTube API 配额或每日上传次数已耗尽（HTTP 403），已立即停止重试".to_string();
    }
    if code == 403 {
        return "YouTube API 权限不足或账号未开启上传功能（HTTP 403）".to_string();
    }
    if code == 400 {
        return "YouTube 拒绝上传请求参数或元数据（HTTP 400）".to_string();
    }

    let mut short = String::new();
    for (idx, ch) in safe_body.trim().chars().enumerate() {
        if idx >= 160 {
            short.push('…');
            break;
        }
        short.push(ch);
    }
    if short.is_empty() {
        format!("YouTube API 错误（HTTP {code}）")
    } else {
        format!("YouTube API 错误（HTTP {code}）: {short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Response, Server, StatusCode};
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use tempfile::TempDir;

    #[test]
    fn test_align_chunk_size_and_range_parser() {
        assert_eq!(align_chunk_size(1), 256 * 1024);
        assert_eq!(align_chunk_size(10 * 1024 * 1024), 10 * 1024 * 1024);
        assert_eq!(align_chunk_size(300 * 1024), 512 * 1024);

        assert_eq!(parse_confirmed_offset_from_range(None, 1_000_000).unwrap(), 0);
        assert_eq!(
            parse_confirmed_offset_from_range(Some("bytes=0-262143"), 1_000_000).unwrap(),
            262_144
        );
        assert!(parse_confirmed_offset_from_range(Some("bytes=0-2000000"), 1_000_000).is_err());
    }

    #[tokio::test]
    async fn test_resumable_upload_chunking_status_recovery_and_no_duplicate_video() {
        // 准备 600 KiB 测试视频文件（按 256 KiB 分块：256 KiB + 256 KiB + 88 KiB 共 3 块）
        let tmp = TempDir::new().unwrap();
        let video_path = tmp.path().join("test_live.mp4");
        let total_len = 600 * 1024usize;
        let payload: Vec<u8> = (0..total_len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&video_path, &payload).unwrap();

        let post_sessions = Arc::new(AtomicUsize::new(0));
        let status_queries = Arc::new(AtomicUsize::new(0));
        let fail_second_chunk_once = Arc::new(AtomicUsize::new(0));
        let drop_final_200_once = Arc::new(AtomicUsize::new(0));
        let server_buf = Arc::new(StdMutex::new(Vec::<u8>::new()));

        let post_c = Arc::clone(&post_sessions);
        let query_c = Arc::clone(&status_queries);
        let fail2_c = Arc::clone(&fail_second_chunk_once);
        let drop_final_c = Arc::clone(&drop_final_200_once);
        let buf_c = Arc::clone(&server_buf);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let make_svc = make_service_fn(move |_| {
            let post_c = Arc::clone(&post_c);
            let query_c = Arc::clone(&query_c);
            let fail2_c = Arc::clone(&fail2_c);
            let drop_final_c = Arc::clone(&drop_final_c);
            let buf_c = Arc::clone(&buf_c);

            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    let post_c = Arc::clone(&post_c);
                    let query_c = Arc::clone(&query_c);
                    let fail2_c = Arc::clone(&fail2_c);
                    let drop_final_c = Arc::clone(&drop_final_c);
                    let buf_c = Arc::clone(&buf_c);

                    async move {
                        if req.method() == Method::POST {
                            post_c.fetch_add(1, Ordering::SeqCst);
                            let loc = format!("http://{addr}/upload?upload_id=fake_sess_token_999");
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header(LOCATION, loc)
                                    .body(Body::empty())
                                    .unwrap(),
                            );
                        }

                        let cr = req
                            .headers()
                            .get(CONTENT_RANGE)
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_string();

                        // 处理重试前的状态查询 PUT bytes */total
                        if cr.starts_with("bytes */") {
                            query_c.fetch_add(1, Ordering::SeqCst);
                            let cur_len = buf_c.lock().unwrap().len();
                            if cur_len == total_len {
                                return Ok(Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Body::from(r#"{"id":"yt_vid_recovered_ok"}"#))
                                    .unwrap());
                            }
                            let mut builder = Response::builder().status(308);
                            if cur_len > 0 {
                                builder =
                                    builder.header(RANGE, format!("bytes=0-{}", cur_len - 1));
                            }
                            return Ok(builder.body(Body::empty()).unwrap());
                        }

                        // 解析实际数据块 PUT bytes start-end/total
                        let range_part = cr.strip_prefix("bytes ").unwrap();
                        let (span, _) = range_part.split_once('/').unwrap();
                        let (start_s, _end_s) = span.split_once('-').unwrap();
                        let start_offset: usize = start_s.parse().unwrap();

                        let whole = hyper::body::to_bytes(req.into_body()).await.unwrap();

                        // 模拟第 2 个分块首次遇到 503 + Retry-After: 1
                        if start_offset == 256 * 1024 && fail2_c.fetch_add(1, Ordering::SeqCst) == 0
                        {
                            return Ok(Response::builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .header(RETRY_AFTER, "0")
                                .body(Body::from("backend transient error"))
                                .unwrap());
                        }

                        let cur_len = {
                            let mut guard = buf_c.lock().unwrap();
                            assert_eq!(start_offset, guard.len());
                            guard.extend_from_slice(&whole);
                            guard.len()
                        };

                        if cur_len < total_len {
                            // 验证所有非末块大小必须是 256 KiB 的整数倍
                            assert_eq!(whole.len() % YOUTUBE_CHUNK_GRANULARITY, 0);
                            return Ok(Response::builder()
                                .status(308)
                                .header(RANGE, format!("bytes=0-{}", cur_len - 1))
                                .body(Body::empty())
                                .unwrap());
                        }

                        // 模拟最末块服务端实际上已落盘，但响应网络闪断（返回 502）
                        if drop_final_c.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Ok(Response::builder()
                                .status(StatusCode::BAD_GATEWAY)
                                .body(Body::from("gateway glitch after final chunk"))
                                .unwrap());
                        }

                        Ok(Response::builder()
                            .status(StatusCode::CREATED)
                            .body(Body::from(r#"{"id":"yt_vid_recovered_ok"}"#))
                            .unwrap())
                    }
                }))
            }
        });

        let server = Server::from_tcp(listener).unwrap().serve(make_svc);
        let srv_handle = tokio::spawn(server);

        let client = Client::new();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let mut confirmed_events = Vec::new();

        let opts = ResumableUploadOptions {
            init_endpoint: format!("http://{addr}/init"),
            chunk_size_bytes: 256 * 1024,
            existing_session_uri: None,
            recovery_mode: YoutubeRecoveryMode::None,
            base_backoff: Duration::from_millis(10),
        };

        let vid = execute_resumable_upload(
            &client,
            &video_path,
            "测试无重复可恢复上传",
            opts,
            |_| async { Ok("fake_oauth_access_token".to_string()) },
            cancel_rx,
            |prog| {
                confirmed_events.push((prog.local_sent_bytes, prog.server_confirmed_bytes));
            },
            |_sess| async { Ok(()) },
        )
        .await
        .unwrap();

        srv_handle.abort();

        assert_eq!(vid, "yt_vid_recovered_ok");
        // 核心断言：全程只创建了 1 次 POST 会话（绝未产生重复视频），且通过 PUT bytes */total 查询恢复了完成状态！
        assert_eq!(post_sessions.load(Ordering::SeqCst), 1);
        assert!(status_queries.load(Ordering::SeqCst) >= 2);
        assert_eq!(*server_buf.lock().unwrap(), payload);
        assert_eq!(
            confirmed_events.last().copied(),
            Some((total_len as u64, total_len as u64))
        );
    }
}
