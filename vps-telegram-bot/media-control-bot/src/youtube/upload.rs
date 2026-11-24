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
    /// 可选的入队/恢复时捕获的源文件身份快照，防止排队期间文件被替换
    pub expected_identity: Option<SourceFileIdentity>,
}

impl Default for ResumableUploadOptions {
    fn default() -> Self {
        Self {
            init_endpoint: YOUTUBE_RESUMABLE_INIT_URL.to_string(),
            chunk_size_bytes: 10 * 1024 * 1024,
            existing_session_uri: None,
            recovery_mode: YoutubeRecoveryMode::None,
            base_backoff: Duration::from_secs(2),
            expected_identity: None,
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
        register_youtube_session_uri(session_uri);
        validate_session_uri(session_uri, YOUTUBE_RESUMABLE_INIT_URL)?;
        let auth = load_authenticator(&self.token_file).await?;
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(60))
            .build::<_, Body>(https);
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
                | SessionQueryOutcome::FinalResponseUnconfirmed(reason)
                | SessionQueryOutcome::Permanent(reason) => anyhow::bail!("{}", redact_secrets(&reason)),
            }
        }
        anyhow::bail!("YouTube upload session could not be verified")
    }

    /// 执行私享视频可恢复上传，返回服务端确认的 `video_id`。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload<ProgFn, SessFn, SessFut, FinalFn, FinalFut>(
        &self,
        file_path: &Path,
        title: &str,
        existing_session_uri: Option<String>,
        expected_identity: Option<SourceFileIdentity>,
        recovery_mode: YoutubeRecoveryMode,
        existing_completion_uncertain: bool,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        progress_callback: ProgFn,
        session_callback: SessFn,
        final_chunk_callback: FinalFn,
    ) -> Result<String>
    where
        ProgFn: FnMut(UploadProgress) + Send,
        SessFn: FnMut(String) -> SessFut + Send,
        SessFut: Future<Output = Result<()>> + Send,
        FinalFn: FnMut() -> FinalFut + Send,
        FinalFut: Future<Output = Result<()>> + Send,
    {
        let filename = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("video");
        let auth = std::sync::Arc::new(
            oauth_or_cancel(load_authenticator(&self.token_file), &mut cancel_rx, filename).await?
        );

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
            expected_identity,
        };

        execute_resumable_upload_with_final_hook(
            &client,
            file_path,
            title,
            opts,
            existing_completion_uncertain,
            move |force_refresh| {
                let auth_clone = std::sync::Arc::clone(&auth);
                async move { fetch_access_token(&auth_clone, force_refresh).await }
            },
            cancel_rx,
            progress_callback,
            session_callback,
            final_chunk_callback,
        )
        .await
    }
}

/// 等待取消通道真正变为 `true`；若发送端被释放则永不误触发取消。
async fn wait_for_cancel(cancel_rx: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *cancel_rx.borrow_and_update() {
            return;
        }
        if cancel_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// 校验可恢复上传 `session_uri` 的协议与域名合法性，防止向非 Google 主机泄露 OAuth Bearer Token。
///
/// 当 `expected_init_endpoint` 为 Google 官方 HTTPS 端点时，强制要求 `session_uri` 使用 `https` 且域名为
/// `googleapis.com` 或 `*.googleapis.com`；本地测试 mock 端点则要求同源（scheme/host/port 一致）。
fn validate_session_uri(session_uri: &str, expected_init_endpoint: &str) -> Result<()> {
    let parsed = url::Url::parse(session_uri)
        .map_err(|_| AppError::YoutubeApiError {
            message: "无效的 YouTube 上传会话 URI".to_string(),
        })?;
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| AppError::YoutubeApiError {
            message: "YouTube 上传会话 URI 缺少有效主机名".to_string(),
        })?;

    if let Ok(expected) = url::Url::parse(expected_init_endpoint) {
        let expected_host = expected.host_str().map(str::to_ascii_lowercase).unwrap_or_default();
        let is_google_endpoint = expected.scheme() == "https"
            && (expected_host == "googleapis.com" || expected_host.ends_with(".googleapis.com"));
        if !is_google_endpoint {
            // 测试用本地 mock server：校验同源
            if parsed.scheme() == expected.scheme()
                && host == expected_host
                && parsed.port_or_known_default() == expected.port_or_known_default()
            {
                return Ok(());
            }
            return Err(AppError::YoutubeApiError {
                message: "上传会话 URI 与目标端点不同源，已拒绝携带令牌访问".to_string(),
            }
            .into());
        }
    }

    if parsed.scheme() != "https" || !(host == "googleapis.com" || host.ends_with(".googleapis.com")) {
        return Err(AppError::YoutubeApiError {
            message: "拒绝向非 *.googleapis.com HTTPS 域名发送 YouTube 鉴权令牌".to_string(),
        }
        .into());
    }

    Ok(())
}

/// 清理并按 UTF-16 码元上限截断 YouTube 视频标题（过滤 `<`、`>` 及控制字符，拒绝纯空白）。
fn sanitize_youtube_title(raw_title: &str) -> String {
    let without_ext = if VideoExtensions::is_video(raw_title) {
        raw_title
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .filter(|stem| !stem.trim().is_empty())
            .unwrap_or(raw_title)
    } else {
        raw_title
    };

    let cleaned: String = without_ext
        .chars()
        .map(|c| {
            if c == '<' || c == '>' || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return "Untitled Video".to_string();
    }

    let mut out = String::new();
    let mut utf16_units = 0usize;
    for ch in trimmed.chars() {
        let units = ch.len_utf16();
        if utf16_units + units > 95 {
            break;
        }
        out.push(ch);
        utf16_units += units;
    }
    let final_title = out.trim();
    if final_title.is_empty() {
        "Untitled Video".to_string()
    } else {
        final_title.to_string()
    }
}

/// 取消尚未完成的上传会话时，尽力发送 `DELETE <session_uri>` 释放服务端残留会话（限时 3 秒）。
async fn best_effort_delete_session<C>(
    client: &Client<C, Body>,
    session_uri: &str,
    access_token: &str,
) where
    C: Connect + Clone + Send + Sync + 'static,
{
    if let Ok(req) = Request::builder()
        .method(Method::DELETE)
        .uri(session_uri)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_LENGTH, "0")
        .body(Body::empty())
    {
        let _ = tokio::time::timeout(Duration::from_secs(3), client.request(req)).await;
    }
}

/// 将任意分块字节数规范化为合法的 `256 KiB` 正整数倍（防止极大值溢出）。
#[must_use]
pub fn align_chunk_size(raw_bytes: usize) -> usize {
    let clamped = raw_bytes.max(YOUTUBE_CHUNK_GRANULARITY);
    let rem = clamped % YOUTUBE_CHUNK_GRANULARITY;
    if rem == 0 {
        clamped
    } else {
        clamped
            .checked_add(YOUTUBE_CHUNK_GRANULARITY - rem)
            .unwrap_or(usize::MAX - (usize::MAX % YOUTUBE_CHUNK_GRANULARITY))
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

    let lower_prefix = trimmed.get(..6).map(str::to_ascii_lowercase);
    let without_unit = match lower_prefix.as_deref() {
        Some("bytes=") | Some("bytes ") => &trimmed[6..],
        _ => anyhow::bail!("Malformed 308 Range header unit"),
    };

    let Some((start_str, end_str)) = without_unit.split_once('-') else {
        anyhow::bail!("Malformed 308 Range header");
    };

    let start_byte: u64 = start_str
        .trim()
        .parse()
        .context("Invalid start byte in 308 Range header")?;
    if start_byte != 0 {
        anyhow::bail!("Unexpected non-zero start byte ({start_byte}) in 308 Range header");
    }

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
    FinalResponseUnconfirmed(String),
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
    FinalResponseUnconfirmed(String),
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
    token_provider: TokenFn,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    progress_callback: ProgFn,
    session_callback: SessFn,
) -> Result<String>
where
    C: Connect + Clone + Send + Sync + 'static,
    TokenFn: FnMut(bool) -> TokenFut + Send,
    TokenFut: Future<Output = Result<String>> + Send,
    ProgFn: FnMut(UploadProgress) + Send,
    SessFn: FnMut(String) -> SessFut + Send,
    SessFut: Future<Output = Result<()>> + Send,
{
    execute_resumable_upload_with_final_hook(
        client,
        file_path,
        title,
        opts,
        false,
        token_provider,
        cancel_rx,
        progress_callback,
        session_callback,
        || async { Ok(()) },
    )
    .await
}

/// 带末块发送前持久化钩子的协议执行引擎：在发送末块前先通知上层将“可能已完成”状态落盘，防止崩溃重启后 `requeue` 重复成片。
#[allow(clippy::too_many_arguments)]
async fn execute_resumable_upload_with_final_hook<
    C,
    TokenFn,
    TokenFut,
    ProgFn,
    SessFn,
    SessFut,
    FinalFn,
    FinalFut,
>(
    client: &Client<C, Body>,
    file_path: &Path,
    title: &str,
    opts: ResumableUploadOptions,
    existing_completion_uncertain: bool,
    mut token_provider: TokenFn,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    mut progress_callback: ProgFn,
    mut session_callback: SessFn,
    mut final_chunk_callback: FinalFn,
) -> Result<String>
where
    C: Connect + Clone + Send + Sync + 'static,
    TokenFn: FnMut(bool) -> TokenFut + Send,
    TokenFut: Future<Output = Result<String>> + Send,
    ProgFn: FnMut(UploadProgress) + Send,
    SessFn: FnMut(String) -> SessFut + Send,
    SessFut: Future<Output = Result<()>> + Send,
    FinalFn: FnMut() -> FinalFut + Send,
    FinalFut: Future<Output = Result<()>> + Send,
{
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video")
        .to_string();

    let source_id = match opts.expected_identity {
        Some(expected) => {
            expected.verify_unchanged(file_path)?;
            expected
        }
        None => SourceFileIdentity::capture(file_path)?,
    };
    let total_bytes = source_id.size;
    let chunk_size = align_chunk_size(opts.chunk_size_bytes);

    // Keep the local filename intact, sanitize forbidden chars/whitespace and bound UTF-16 units.
    let title_truncated = sanitize_youtube_title(title);
    let mut access_token = oauth_or_cancel(token_provider(false), &mut cancel_rx, &filename).await?;
    let mut refreshed_after_401 = false;
    let mut session_restarts: u32 = 0;
    let mut transient_retries: u32 = 0;
    let mut full_confirmation_retries: u32 = 0;
    let mut no_progress_retries: u32 = 0;

    if let Some(ref existing_uri) = opts.existing_session_uri {
        register_youtube_session_uri(existing_uri);
        validate_session_uri(existing_uri, &opts.init_endpoint)?;
    }
    let mut session_uri_opt = opts.existing_session_uri.clone();
    let mut need_status_query = session_uri_opt.is_some();
    let mut is_initial_recovery_query = session_uri_opt.is_some();
    let mut confirmed_offset: u64 = 0;
    // Once completion may have happened, never replace this session automatically.
    let mut completion_uncertain = existing_completion_uncertain;

    progress_callback(UploadProgress {
        local_sent_bytes: 0,
        server_confirmed_bytes: 0,
        total_bytes,
    });

    loop {
        if *cancel_rx.borrow_and_update() {
            if completion_uncertain || confirmed_offset >= total_bytes {
                return Err(AppError::YoutubeUploadStateUnconfirmed {
                    filename: filename.clone(),
                }
                .into());
            }
            if let Some(ref uri) = session_uri_opt {
                best_effort_delete_session(client, uri, &access_token).await;
            }
            return Err(AppError::YoutubeUploadCancelled {
                filename: filename.clone(),
            }
            .into());
        }

        // 每次网络交互前校验原文件未被改动
        source_id.verify_unchanged(file_path)
            .map_err(|e| preserve_completion_error(e, completion_uncertain, &filename))?;

        // 1. 若尚无上传会话，先发起 POST 创建 Resumable Session
        let session_uri = match &session_uri_opt {
            Some(uri) => uri.clone(),
            None => {
                let init = tokio::select! {
                    result = initiate_resumable_session(
                        client,
                        &opts.init_endpoint,
                        &access_token,
                        &title_truncated,
                        total_bytes,
                    ) => result,
                    () = wait_for_cancel(&mut cancel_rx) => return Err(AppError::YoutubeUploadCancelled { filename: filename.clone() }.into()),
                };
                match init {
                    Ok(new_uri) => {
                        register_youtube_session_uri(&new_uri);
                        validate_session_uri(&new_uri, &opts.init_endpoint)?;
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
                        access_token = oauth_or_cancel(token_provider(true), &mut cancel_rx, &filename).await?;
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
                            false,
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
            let query = tokio::select! {
                result = query_session_status(client, &session_uri, &access_token, total_bytes) => result,
                () = wait_for_cancel(&mut cancel_rx) => {
                    if completion_uncertain || confirmed_offset >= total_bytes {
                        return Err(AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into());
                    }
                    best_effort_delete_session(client, &session_uri, &access_token).await;
                    return Err(AppError::YoutubeUploadCancelled { filename: filename.clone() }.into());
                }
            };
            match query {
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
                SessionQueryOutcome::FinalResponseUnconfirmed(reason) => {
                    completion_uncertain = true;
                    is_initial_recovery_query = false;
                    transient_retries += 1;
                    warn!(filename = %filename, reason = %redact_secrets(&reason), "Completion response could not be read; retaining original session");
                    if transient_retries > MAX_TRANSIENT_RETRIES {
                        return Err(AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into());
                    }
                    wait_backoff_or_cancel(None, opts.base_backoff, transient_retries, &mut cancel_rx, &filename, completion_uncertain).await?;
                    continue;
                }
                SessionQueryOutcome::Incomplete(server_offset) => {
                    need_status_query = false;

                    if is_initial_recovery_query
                        && opts.recovery_mode == YoutubeRecoveryMode::Requeue
                        && !completion_uncertain
                        && server_offset < total_bytes
                    {
                        // requeue 策略：确认旧会话未完成后，丢弃半截会话并从头新建上传
                        is_initial_recovery_query = false;
                        refreshed_after_401 = false;
                        session_uri_opt = None;
                        confirmed_offset = 0;
                        continue;
                    }

                    is_initial_recovery_query = false;
                    // 仅当服务端确认偏移真正前进时才重置 401 刷新预算，防止查询 308 与分块 401 死循环
                    if server_offset > confirmed_offset {
                        no_progress_retries = 0;
                        refreshed_after_401 = false;
                    }
                    confirmed_offset = server_offset;
                    if server_offset >= total_bytes {
                        completion_uncertain = true;
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
                            completion_uncertain,
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
                    access_token = oauth_or_cancel(token_provider(true), &mut cancel_rx, &filename).await
                        .map_err(|e| preserve_completion_error(e, completion_uncertain, &filename))?;
                    continue;
                }
                SessionQueryOutcome::Unauthorized => {
                    return Err(preserve_completion_error(AppError::YoutubeApiError {
                        message: "查询上传进度时 OAuth 鉴权失败（401）".to_string(),
                    }
                    .into(), completion_uncertain, &filename));
                }
                SessionQueryOutcome::Expired => {
                    if completion_uncertain || confirmed_offset >= total_bytes
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
                        if completion_uncertain || confirmed_offset >= total_bytes {
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
                        completion_uncertain || confirmed_offset >= total_bytes,
                    )
                    .await?;
                    continue;
                }
                SessionQueryOutcome::Permanent(msg) => {
                    if completion_uncertain || confirmed_offset >= total_bytes {
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
        // 一旦准备发送末块，先持久化“可能已完成”标记再发送网络数据，
        // 防止进程在服务端收完末块后崩溃重启时被 requeue 误判并创建重复视频。
        if confirmed_offset + (chunk_len as u64) >= total_bytes && !completion_uncertain {
            completion_uncertain = true;
            final_chunk_callback().await?;
        }

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
        .await
        .map_err(|e| {
            if completion_uncertain {
                warn!(filename = %filename, error = %redact_secrets(&e.to_string()), "Final chunk interrupted or cancelled; retaining session for verification");
                AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into()
            } else {
                e
            }
        })?;

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
            ChunkPutOutcome::FinalResponseUnconfirmed(reason) => {
                completion_uncertain = true;
                need_status_query = true;
                transient_retries += 1;
                warn!(filename = %filename, reason = %redact_secrets(&reason), "Final upload response invalid; querying original session");
                if transient_retries > MAX_TRANSIENT_RETRIES {
                    return Err(AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into());
                }
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
                        completion_uncertain,
                    )
                    .await?;
                    need_status_query = true;
                } else {
                    no_progress_retries = 0;
                    transient_retries = 0;
                    confirmed_offset = new_confirmed;
                    if confirmed_offset >= total_bytes {
                        completion_uncertain = true;
                    }
                    progress_callback(UploadProgress {
                        local_sent_bytes: confirmed_offset,
                        server_confirmed_bytes: confirmed_offset,
                        total_bytes,
                    });
                }
            }
            ChunkPutOutcome::Unauthorized if !refreshed_after_401 => {
                refreshed_after_401 = true;
                access_token = oauth_or_cancel(token_provider(true), &mut cancel_rx, &filename).await
                    .map_err(|e| preserve_completion_error(e, completion_uncertain, &filename))?;
                need_status_query = true;
            }
            ChunkPutOutcome::Unauthorized => {
                return Err(preserve_completion_error(AppError::YoutubeApiError {
                    message: "分块上传期间 OAuth 令牌失效且刷新后仍被拒绝（401）".to_string(),
                }
                .into(), completion_uncertain, &filename));
            }
            ChunkPutOutcome::Expired => {
                if completion_uncertain || confirmed_offset + chunk_len as u64 >= total_bytes {
                    return Err(AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into());
                }
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
                if confirmed_offset + chunk_len as u64 >= total_bytes {
                    completion_uncertain = true;
                }
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
                    completion_uncertain,
                )
                .await?;
                // 关键规则：重试发送数据前必须先向既有 session 查询服务端实际确认偏移！
                need_status_query = true;
            }
            ChunkPutOutcome::Permanent(msg) => {
                if completion_uncertain {
                    return Err(AppError::YoutubeUploadStateUnconfirmed { filename: filename.clone() }.into());
                }
                return Err(AppError::YoutubeApiError { message: msg }.into());
            }
        }
    }
}

fn preserve_completion_error(error: anyhow::Error, uncertain: bool, filename: &str) -> anyhow::Error {
    if uncertain {
        warn!(filename, error = %redact_secrets(&error.to_string()), "Cannot finish verifying potentially completed upload");
        AppError::YoutubeUploadStateUnconfirmed { filename: filename.to_string() }.into()
    } else {
        error
    }
}

async fn oauth_or_cancel<T>(
    operation: impl Future<Output = Result<T>>,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    filename: &str,
) -> Result<T> {
    if *cancel_rx.borrow_and_update() {
        return Err(AppError::YoutubeUploadCancelled { filename: filename.to_string() }.into());
    }
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(60), operation) => match result {
            Ok(result) => result,
            Err(_) => Err(AppError::YoutubeApiError { message: "OAuth 操作超时".to_string() }.into()),
        },
        () = wait_for_cancel(cancel_rx) => Err(AppError::YoutubeUploadCancelled { filename: filename.to_string() }.into()),
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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

    let resp = match tokio::time::timeout_at(deadline, client.request(req)).await {
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

    let body_text = match tokio::time::timeout_at(deadline, read_bounded_body_text(resp.into_body())).await {
        Ok(Ok(text)) => text,
        Ok(Err(reason)) => reason,
        Err(_) => "初始化会话响应体读取超时".to_string(),
    };
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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

    let resp = match tokio::time::timeout_at(deadline, client.request(req)).await {
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

    let body_result = match tokio::time::timeout_at(deadline, read_bounded_body_text(resp.into_body())).await {
        Ok(result) => result,
        Err(_) => Err("会话查询响应体读取超时".to_string()),
    };
    if matches!(code, 200 | 201) {
        return final_query_response(body_result);
    }
    let body_text = body_result.unwrap_or_else(|reason| reason);

    match code {
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

fn final_query_response(body: std::result::Result<String, String>) -> SessionQueryOutcome {
    match body.and_then(|text| parse_video_id_from_json(&text)) {
        Ok(video_id) => SessionQueryOutcome::Completed(video_id),
        Err(reason) => SessionQueryOutcome::FinalResponseUnconfirmed(reason),
    }
}

// A single deadline covers file reads, request body backpressure, response headers
// and response body. Dropping the inner future closes its body sender/connection.
#[allow(clippy::too_many_arguments)]
async fn upload_single_chunk<C, ProgFn>(
    client: &Client<C, Body>, file_path: &Path, source_id: &SourceFileIdentity,
    session_uri: &str, access_token: &str, offset: u64, chunk_len: usize,
    total_bytes: u64, cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    progress_callback: &mut ProgFn,
) -> Result<ChunkPutOutcome>
where
    C: Connect + Clone + Send + Sync + 'static,
    ProgFn: FnMut(UploadProgress) + Send,
{
    let filename = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("video").to_string();
    if *cancel_rx.borrow_and_update() {
        return Err(AppError::YoutubeUploadCancelled { filename }.into());
    }
    let mut inner_cancel = cancel_rx.clone();
    let operation = upload_single_chunk_inner(
        client, file_path, source_id, session_uri, access_token, offset,
        chunk_len, total_bytes, &mut inner_cancel, progress_callback,
    );
    tokio::select! {
        result = tokio::time::timeout(CHUNK_REQUEST_TIMEOUT, operation) => match result {
            Ok(result) => result,
            Err(_) => Ok(ChunkPutOutcome::Transient {
                retry_after: None, reason: "分块请求整体超时（含发送及响应体读取）".to_string(),
            }),
        },
        () = wait_for_cancel(cancel_rx) => Err(AppError::YoutubeUploadCancelled { filename }.into()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_single_chunk_inner<C, ProgFn>(
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
    let opened_meta = file.metadata().await.map_err(|e| AppError::FileIo {
        path: file_path.to_path_buf(),
        message: e.to_string(),
    })?;
    let opened_mtime = opened_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    if !opened_meta.is_file()
        || opened_meta.len() != total_bytes
        || (source_id.modified_nanos.is_some() && opened_mtime != source_id.modified_nanos)
    {
        return Err(AppError::SourceFileChanged { filename }.into());
    }
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
        if *cancel_rx.borrow_and_update() {
            sender.abort();
            return Err(AppError::YoutubeUploadCancelled { filename }.into());
        }

        if sent_in_chunk >= chunk_len {
            // 末块发完前再次核验文件大小未被外部追加扩展
            if offset + (chunk_len as u64) == total_bytes {
                if let Ok(meta) = file.metadata().await {
                    if meta.len() != total_bytes {
                        sender.abort();
                        return Err(AppError::SourceFileChanged { filename }.into());
                    }
                }
            }
            // 关闭发送端（EOF），等待服务端返回 308 或 200/201
            drop(sender);
            let awaited = tokio::select! {
                res = &mut request_fut => Some(res),
                () = wait_for_cancel(cancel_rx) => None,
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
            () = wait_for_cancel(cancel_rx) => {
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
    let body_result = read_bounded_body_text(resp.into_body()).await;
    if matches!(code, 200 | 201) {
        return Ok(final_chunk_response(body_result));
    }
    let body_text = body_result.unwrap_or_else(|reason| reason);

    Ok(match code {
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

fn final_chunk_response(body: std::result::Result<String, String>) -> ChunkPutOutcome {
    match body.and_then(|text| parse_video_id_from_json(&text)) {
        Ok(video_id) => ChunkPutOutcome::Completed(video_id),
        Err(reason) => ChunkPutOutcome::FinalResponseUnconfirmed(reason),
    }
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
    completion_uncertain: bool,
) -> Result<()> {
    let shift = attempt.saturating_sub(1).min(5);
    let exp_delay = base_backoff
        .saturating_mul(1u32 << shift)
        .min(Duration::from_secs(60));
    let delay = retry_after.map_or(exp_delay, |ra| ra.max(exp_delay));

    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        () = wait_for_cancel(cancel_rx) => {
            if completion_uncertain {
                Err(AppError::YoutubeUploadStateUnconfirmed {
                    filename: filename.to_string(),
                }.into())
            } else {
                Err(AppError::YoutubeUploadCancelled {
                    filename: filename.to_string(),
                }.into())
            }
        }
    }
}

async fn read_bounded_body_text(mut body: Body) -> std::result::Result<String, String> {
    use hyper::body::HttpBody;
    let mut collected = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| format!("读取 YouTube 响应体失败: {}", redact_secrets(&e.to_string())))?;
        let remain = (64 * 1024usize).saturating_sub(collected.len());
        if chunk.len() > remain {
            return Err("YouTube 响应体超过 64 KiB 限制".to_string());
        }
        collected.extend_from_slice(&chunk);
    }
    String::from_utf8(collected).map_err(|_| "YouTube 响应体不是有效 UTF-8".to_string())
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

    #[tokio::test]
    async fn body_read_failure_is_not_treated_as_a_complete_json_response() {
        let (mut sender, body) = Body::channel();
        let writer = tokio::spawn(async move {
            sender.send_data(Bytes::from_static(br#"{"id":"video123"}"#)).await.unwrap();
            sender.abort();
        });
        let outcome = read_bounded_body_text(body).await;
        writer.await.unwrap();
        assert!(outcome.is_err());
        assert!(matches!(final_chunk_response(outcome), ChunkPutOutcome::FinalResponseUnconfirmed(_)));
    }

    #[tokio::test]
    async fn oversized_response_body_is_an_error() {
        assert!(read_bounded_body_text(Body::from(vec![b'x'; 65_537])).await.is_err());
    }

    #[tokio::test]
    async fn cancellation_interrupts_stalled_final_response_body() {
        exercise_stalled_body(true).await;
    }

    #[tokio::test]
    async fn whole_chunk_deadline_includes_response_body_reading() {
        exercise_stalled_body(false).await;
    }

    async fn exercise_stalled_body(cancel: bool) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("video.mp4");
        std::fs::write(&path, [7u8; 128]).unwrap();
        let identity = SourceFileIdentity::capture(&path).unwrap();
        let ready = Arc::new(tokio::sync::Notify::new());
        // Hold the response sender open without emitting or closing its body.
        let held_sender = Arc::new(StdMutex::new(None));
        let ready_service = Arc::clone(&ready);
        let held_service = Arc::clone(&held_sender);
        let service = make_service_fn(move |_| {
            let ready = Arc::clone(&ready_service);
            let held = Arc::clone(&held_service);
            async move {
                Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                    let ready = Arc::clone(&ready);
                    let held = Arc::clone(&held);
                    async move {
                        hyper::body::to_bytes(request.into_body()).await.unwrap();
                        let (sender, body) = Body::channel();
                        *held.lock().unwrap() = Some(sender);
                        ready.notify_one();
                        Ok::<_, Infallible>(Response::builder().status(200).body(body).unwrap())
                    }
                }))
            }
        });
        let server = Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
        let url = format!("http://{}/upload", server.local_addr());
        let server_task = tokio::spawn(server);
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let upload_task = tokio::spawn(async move {
            let mut progress = |_| {};
            upload_single_chunk(&Client::new(), &path, &identity, &url, "fake", 0, 128, 128,
                &mut cancel_rx, &mut progress).await
        });
        tokio::time::timeout(Duration::from_secs(2), ready.notified()).await.unwrap();
        tokio::task::yield_now().await;
        if cancel {
            cancel_tx.send(true).unwrap();
        } else {
            tokio::time::pause();
            tokio::time::advance(CHUNK_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
        }
        let result = tokio::time::timeout(Duration::from_secs(2), upload_task).await.unwrap().unwrap();
        if cancel {
            assert!(matches!(result.unwrap_err().downcast_ref::<AppError>(), Some(AppError::YoutubeUploadCancelled { .. })));
        } else {
            assert!(matches!(result.unwrap(), ChunkPutOutcome::Transient { .. }));
            tokio::time::resume();
        }
        server_task.abort();
    }

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
        exercise_final_response(502, "gateway glitch after final chunk", false).await;
    }

    #[tokio::test]
    async fn malformed_final_json_is_recovered_without_creating_a_second_video() {
        exercise_final_response(200, "truncated-json", false).await;
    }

    #[tokio::test]
    async fn missing_final_video_id_is_recovered_from_original_session() {
        exercise_final_response(201, "{}", false).await;
    }

    #[tokio::test]
    async fn expired_session_after_malformed_final_response_is_retained_as_unconfirmed() {
        exercise_final_response(200, "truncated-json", true).await;
    }

    async fn exercise_final_response(final_code: u16, final_body: &'static str, expire_after_final: bool) {
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
                                if expire_after_final {
                                    return Ok(Response::builder().status(404).body(Body::empty()).unwrap());
                                }
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

                        // Final bytes are stored, but the final response may be corrupt/lost.
                        if drop_final_c.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Ok(Response::builder()
                                .status(final_code)
                                .body(Body::from(final_body))
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
            expected_identity: None,
        };

        let outcome = execute_resumable_upload(
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
        .await;

        srv_handle.abort();

        if expire_after_final {
            let error = outcome.unwrap_err();
            assert!(matches!(error.downcast_ref::<AppError>(), Some(AppError::YoutubeUploadStateUnconfirmed { .. })));
        } else {
            assert_eq!(outcome.unwrap(), "yt_vid_recovered_ok");
        }
        // 核心断言：全程只创建了 1 次 POST 会话（绝未产生重复视频），且通过 PUT bytes */total 查询恢复了完成状态！
        assert_eq!(post_sessions.load(Ordering::SeqCst), 1);
        assert!(status_queries.load(Ordering::SeqCst) >= 2);
        assert_eq!(*server_buf.lock().unwrap(), payload);
        if !expire_after_final {
            assert_eq!(confirmed_events.last().copied(), Some((total_len as u64, total_len as u64)));
        }
    }
}
