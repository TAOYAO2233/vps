//! 日志与错误消息敏感信息脱敏模块。
//!
//! 实现双层防护策略：
//! 1. **源头白名单与显式脱敏**：提供 [`redact_rtmp_target`]、[`sanitize_user_error`] 等辅助函数，
//!    避免在业务层构造含凭据的日志字段或向 Telegram 用户直接暴露底层原始错误文本。
//! 2. **统一输出边界兜底**：提供实现 [`tracing_subscriber::fmt::MakeWriter`] 的
//!    [`RedactingMakeWriter`]，在字节流按行/按空白边界重组后执行运行时秘密字典匹配与格式正则兜底，
//!    覆盖 `pretty`、`json`、多行异常链、第三方库调试日志以及跨写入块（chunked write）场景。

use std::io::{self, Write};
use std::sync::{Arc, Mutex, RwLock};

use once_cell::sync::Lazy;
use regex::Regex;
use tracing_subscriber::fmt::MakeWriter;
use url::Url;

/// 统一的脱敏占位符（不保留秘密前后任何字符）。
pub const REDACTED: &str = "[REDACTED]";

/// 无换行超长行的最大缓冲字节数（超过后在空白边界安全分段刷新）。
const MAX_BUFFER_WITHOUT_NEWLINE: usize = 64 * 1024;

/// 超长行分段刷新时保留的尾部回退窗口字节数。
const OVERLAP_HOLDBACK_BYTES: usize = 2048;

/// 派生秘密片段（如路径末段流密钥、查询令牌）允许注册的最小长度，防止误伤极短公共词。
const MIN_DERIVED_SECRET_LEN: usize = 6;

/// 全局运行时秘密注册表（按长度降序排列，优先匹配完整长秘密）。
static GLOBAL_SECRETS: Lazy<RwLock<Vec<String>>> = Lazy::new(|| RwLock::new(Vec::new()));

/// 全局标准输出脱敏写入器（供 `main.rs` 停机时刷新残余缓冲）。
static GLOBAL_STDOUT_WRITER: Lazy<RedactingMakeWriter<io::Stdout>> =
    Lazy::new(|| RedactingMakeWriter::new(io::stdout()));

// ── 格式兜底正则表达式 ─────────────────────────────────────────────────────────

/// 匹配 Telegram Bot API URL：`https://api.telegram.org/bot<token>/...` 或 `/file/bot<token>/...`
static TG_BOT_URL_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(https?://api\.telegram\.org/(?:file/)?bot)[^/\s\x22'<>\\]+").unwrap()
});

/// 匹配独立的 Telegram Bot Token（形如 `123456789:AA...`）
static TG_BOT_TOKEN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d{6,15}:[A-Za-z0-9_-]{25,80}\b").unwrap());

/// 匹配完整 RTMP/RTMPS/RTMPE/RTMPT/RTMPTE URL
static RTMP_URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\brtmp(?:s|e|t|te)?://[^\s\x22'<>\\]+").unwrap());

/// 匹配 YouTube 可恢复上传会话 URI
static YOUTUBE_SESSION_URI_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)https?://(?:[a-z0-9-]+\.)*googleapis\.com/upload/youtube/v3/videos\?[^\s\x22'<>\\]+",
    )
    .unwrap()
});

/// 匹配独立的 `upload_id=...` 查询参数
static UPLOAD_ID_PARAM_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\bupload_id=)[^&\s\x22'<>\\]+").unwrap());

/// 匹配 `Authorization: Bearer ...` 或 `authorization=Bearer ...`
static AUTH_HEADER_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(authorization\s*[:=]\s*(?:bearer\s+)?)[^\s\x22',;\\\}\]]+").unwrap()
});

/// 匹配独立的 `Bearer <token>` 片段
static BEARER_TOKEN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\bbearer\s+)[A-Za-z0-9._~+/=-]{12,}").unwrap());

/// 匹配 Google OAuth2 Access Token（`ya29....`）
static GOOGLE_ACCESS_TOKEN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bya29\.[A-Za-z0-9._-]{10,}\b").unwrap());

/// 匹配 Google OAuth2 Refresh Token（`1//...`）
static GOOGLE_REFRESH_TOKEN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b1//[A-Za-z0-9._-]{16,}\b").unwrap());

/// 匹配 Google OAuth2 Client Secret（`GOCSPX-...`）
static GOOGLE_CLIENT_SECRET_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bGOCSPX-[A-Za-z0-9._-]{10,}\b").unwrap());

/// 匹配 JSON（含转义 JSON）中的敏感键值字段
static JSON_SECRET_FIELD_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\\*")(access_token|refresh_token|client_secret|bot_token|stream_key|rtmp_url|session_uri)(\\*"\s*:\s*\\*")([^"\\]+)(\\*")"#,
    )
    .unwrap()
});

/// 匹配 URL 查询串或表单中的敏感键值参数
static QUERY_SECRET_PARAM_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(access_token|refresh_token|client_secret|stream_key|bot_token)=([^&\s\x22'<>\\]+)",
    )
    .unwrap()
});

// ── 运行时秘密注册接口 ─────────────────────────────────────────────────────────

/// 向指定秘密列表中插入秘密及其 JSON 转义变体（保持按长度降序且去重）。
fn insert_secret_variants(list: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == REDACTED {
        return;
    }

    let mut candidates = vec![trimmed.to_string()];
    if let Ok(json_quoted) = serde_json::to_string(trimmed) {
        if json_quoted.len() >= 2 {
            let escaped = &json_quoted[1..json_quoted.len() - 1];
            if escaped != trimmed && !escaped.is_empty() {
                candidates.push(escaped.to_string());
            }
        }
    }

    for candidate in candidates {
        if !list.iter().any(|existing| existing == &candidate) {
            list.push(candidate);
        }
    }

    // 较长字符串优先替换，防止子串先行替换导致残余尾巴泄露
    list.sort_by_key(|b| std::cmp::Reverse(b.len()));
}

/// 注册单个精确秘密值（如 `BOT_TOKEN`、`client_secret`、`refresh_token`、`access_token`）。
pub fn register_secret(secret: &str) {
    if secret.trim().is_empty() {
        return;
    }
    if let Ok(mut guard) = GLOBAL_SECRETS.write() {
        insert_secret_variants(&mut guard, secret);
    }
}

/// 注册 RTMP/RTMPS 地址及其包含的用户名、密码、流密钥与查询参数。
pub fn register_rtmp_secret(rtmp_url: &str) {
    let trimmed = rtmp_url.trim();
    if trimmed.is_empty() {
        return;
    }

    if let Ok(mut guard) = GLOBAL_SECRETS.write() {
        insert_secret_variants(&mut guard, trimmed);

        if let Ok(parsed) = Url::parse(trimmed) {
            if let Some(password) = parsed.password() {
                if !password.is_empty() {
                    insert_secret_variants(&mut guard, password);
                }
            }
            let username = parsed.username();
            if username.len() >= MIN_DERIVED_SECRET_LEN {
                insert_secret_variants(&mut guard, username);
            }
            if let Some(segments) = parsed.path_segments() {
                let seg_vec: Vec<&str> = segments.filter(|s| !s.is_empty()).collect();
                // 常见结构为 /app/stream_key，流密钥在末段（或第 2 段起）
                for (idx, seg) in seg_vec.iter().enumerate() {
                    let is_generic_app = matches!(
                        seg.to_ascii_lowercase().as_str(),
                        "live" | "app" | "rtmp" | "stream" | "publish" | "live2"
                    );
                    if !is_generic_app
                        && (idx >= 1 || seg.len() >= MIN_DERIVED_SECRET_LEN)
                        && seg.len() >= 4
                    {
                        insert_secret_variants(&mut guard, seg);
                    }
                }
            }
            if let Some(query) = parsed.query() {
                if query.len() >= 4 {
                    insert_secret_variants(&mut guard, query);
                }
                for (_, val) in parsed.query_pairs() {
                    if val.len() >= 4 {
                        insert_secret_variants(&mut guard, &val);
                    }
                }
            }
        }
    }
}

/// 注册 YouTube 可恢复上传会话 URI 及其 `upload_id` 令牌。
pub fn register_youtube_session_uri(session_uri: &str) {
    let trimmed = session_uri.trim();
    if trimmed.is_empty() {
        return;
    }

    if let Ok(mut guard) = GLOBAL_SECRETS.write() {
        insert_secret_variants(&mut guard, trimmed);
        if let Ok(parsed) = Url::parse(trimmed) {
            for (key, val) in parsed.query_pairs() {
                if key.eq_ignore_ascii_case("upload_id") && !val.is_empty() {
                    insert_secret_variants(&mut guard, &val);
                }
            }
        }
    }
}

/// 从环境变量预注册敏感配置（供日志系统初始化前最早调用）。
pub fn preload_secrets_from_env() {
    if let Ok(token) = std::env::var("BOT_TOKEN") {
        register_secret(&token);
    }
    if let Ok(rtmp) = std::env::var("RTMP_URL") {
        register_rtmp_secret(&rtmp);
    }
}

// ── 脱敏与安全展示函数 ─────────────────────────────────────────────────────────

/// 提取 RTMP/RTMPS 地址的安全诊断标识（仅保留 `协议://主机[:端口]/[REDACTED]`）。
///
/// 不输出用户名、密码、应用路径、推流密钥或查询参数。
#[must_use]
pub fn redact_rtmp_target(rtmp_url: &str) -> String {
    let trimmed = rtmp_url.trim();
    if trimmed.is_empty() {
        return "(未配置)".to_string();
    }

    match Url::parse(trimmed) {
        Ok(parsed) => {
            let scheme = parsed.scheme();
            match parsed.host_str() {
                Some(host) => match parsed.port() {
                    Some(port) => format!("{scheme}://{host}:{port}/{REDACTED}"),
                    None => format!("{scheme}://{host}/{REDACTED}"),
                },
                None => REDACTED.to_string(),
            }
        }
        Err(_) => REDACTED.to_string(),
    }
}

/// 提取 YouTube 可恢复上传会话 URI 的安全诊断标识（剥离 `upload_id` 等全部查询参数）。
#[must_use]
pub fn redact_youtube_session_uri(session_uri: &str) -> String {
    let trimmed = session_uri.trim();
    if trimmed.is_empty() {
        return "(未分配)".to_string();
    }

    match Url::parse(trimmed) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => {
                let port_part = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
                format!(
                    "{}://{}{}{}?upload_id={}",
                    parsed.scheme(),
                    host,
                    port_part,
                    parsed.path(),
                    REDACTED
                )
            }
            None => REDACTED.to_string(),
        },
        Err(_) => REDACTED.to_string(),
    }
}

/// 使用给定的运行时秘密切片和内置正则规则对文本执行完整脱敏。
#[must_use]
pub fn redact_with_secrets(input: &str, runtime_secrets: &[String]) -> String {
    if input.is_empty() {
        return String::new();
    }

    let mut out = input.to_string();

    // 1. 先按模式将结构化敏感 URL 规范替换为安全形式，避免后续产生半截残片
    out = TG_BOT_URL_REGEX
        .replace_all(&out, format!("${{1}}{REDACTED}"))
        .into_owned();

    out = RTMP_URL_REGEX
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            redact_rtmp_target(&caps[0])
        })
        .into_owned();

    out = YOUTUBE_SESSION_URI_REGEX
        .replace_all(
            &out,
            format!("https://www.googleapis.com/upload/youtube/v3/videos?upload_id={REDACTED}"),
        )
        .into_owned();

    // 2. 精确匹配运行时注册的秘密值（统一替换为 [REDACTED]）
    for secret in runtime_secrets {
        if !secret.is_empty() && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }

    // 3. 格式兜底正则规则
    out = TG_BOT_TOKEN_REGEX.replace_all(&out, REDACTED).into_owned();
    out = UPLOAD_ID_PARAM_REGEX
        .replace_all(&out, format!("${{1}}{REDACTED}"))
        .into_owned();
    out = AUTH_HEADER_REGEX
        .replace_all(&out, format!("${{1}}{REDACTED}"))
        .into_owned();
    out = BEARER_TOKEN_REGEX
        .replace_all(&out, format!("${{1}}{REDACTED}"))
        .into_owned();
    out = GOOGLE_ACCESS_TOKEN_REGEX
        .replace_all(&out, REDACTED)
        .into_owned();
    out = GOOGLE_REFRESH_TOKEN_REGEX
        .replace_all(&out, REDACTED)
        .into_owned();
    out = GOOGLE_CLIENT_SECRET_REGEX
        .replace_all(&out, REDACTED)
        .into_owned();
    out = JSON_SECRET_FIELD_REGEX
        .replace_all(&out, format!("${{1}}${{2}}${{3}}{REDACTED}${{5}}"))
        .into_owned();
    out = QUERY_SECRET_PARAM_REGEX
        .replace_all(&out, format!("${{1}}={REDACTED}"))
        .into_owned();

    out
}

/// 使用全局运行时秘密注册表对字符串执行脱敏。
#[must_use]
pub fn redact_secrets(input: &str) -> String {
    let snapshot: Vec<String> = GLOBAL_SECRETS
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    redact_with_secrets(input, &snapshot)
}

/// 将应用层错误映射为适合向 Telegram 用户展示的安全短消息。
///
/// 不直接暴露原始异常链、完整本地绝对路径、HTTP 请求 URL 或任何凭据。
#[must_use]
pub fn sanitize_user_error(err: &anyhow::Error) -> String {
    use crate::errors::AppError;

    if let Some(app_err) = err.downcast_ref::<AppError>() {
        return match app_err {
            AppError::PermissionDenied { .. } => "权限不足，仅管理员可执行此操作。".to_string(),
            AppError::PathTraversal { .. } => "安全校验拒绝：目标路径超出允许的媒体目录范围。".to_string(),
            AppError::TaskStopping => {
                "正在等待已发出停止请求的任务退出并回收资源，请收到“已停止”确认后再启动新任务。".to_string()
            }
            AppError::TaskAlreadyRunning { task_name } => {
                format!("已有任务正在运行：{task_name}。请先发送 /stop 或等待完成。")
            }
            AppError::YoutubeUploadBlocking { count } => {
                format!("当前有 {count} 个 YouTube 上传任务在运行/排队，请先 /stop 或等待完成。")
            }
            AppError::YoutubeQueueFull {
                current,
                requested,
                max_total,
            } => {
                format!(
                    "上传队列已满：当前 {current} 个任务，本次请求 {requested} 个，上限为 {max_total} 个。"
                )
            }
            AppError::TaskNotFound { .. } => "未找到对应的任务。".to_string(),
            AppError::FileNotFound { .. } => "目标文件不存在或已被移除，请刷新文件列表。".to_string(),
            AppError::DirectoryNotFound { .. } => "目标目录不存在或不可访问，请刷新菜单。".to_string(),
            AppError::FileIo { .. } | AppError::Io(_) => {
                "文件系统读写失败，请检查磁盘状态与权限。".to_string()
            }
            AppError::InsufficientDiskSpace {
                required_bytes,
                available_bytes,
            } => {
                let req_gb = *required_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                let avail_gb = *available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                format!(
                    "目标磁盘剩余空间不足：预计需要约 {req_gb:.2}GB（含安全余量），当前可用 {avail_gb:.2}GB。"
                )
            }
            AppError::StorageMountUnavailable { .. } => {
                "存储挂载点不可用或未正常挂载，已拒绝写入任务。".to_string()
            }
            AppError::StaleMenuSnapshot => {
                "菜单列表已过期或目录内容已变化，请使用刷新后的最新列表重试。".to_string()
            }
            AppError::SourceFileChanged { filename } => {
                format!("源文件在操作期间发生变化（{filename}），已安全中止。")
            }
            AppError::StreamIncompatible { reason } => {
                format!("媒体流参数不兼容：{reason}")
            }
            AppError::FfmpegFailed { exit_code, .. } => {
                format!("FFmpeg 处理失败（退出码 {exit_code}），详情请查看服务端日志。")
            }
            AppError::FfprobeFailed { .. } => {
                "无法解析媒体文件元数据（FFprobe 超时或数据损坏）。".to_string()
            }
            AppError::MergeValidationFailed { .. } => {
                "合并输出未通过完整性校验，已清理临时文件。".to_string()
            }
            AppError::ConcatInsufficientFiles { count } => {
                format!("合并至少需要选择 2 个文件（当前仅 {count} 个）。")
            }
            AppError::YoutubeTokenMissing { .. } => {
                "未找到 YouTube OAuth 授权凭证文件，请先在服务端配置 token.json。".to_string()
            }
            AppError::YoutubeApiError { message } => {
                let clean = redact_secrets(message);
                let brief: String = clean.chars().take(120).collect();
                format!("YouTube 接口调用失败：{brief}")
            }
            AppError::YoutubeUploadCancelled { .. } | AppError::Cancelled => {
                "操作已由用户取消。".to_string()
            }
            AppError::YoutubeUploadStateUnconfirmed { filename } => {
                format!(
                    "服务端状态未确认（{filename}）：全部字节已发送但 YouTube 持续返回 308 未给出最终 video_id，已停止轮询并保留会话信息供后续核实。"
                )
            }
            AppError::RtmpUrlNotConfigured => {
                "尚未配置 RTMP 推流地址，请在服务端 .env 中配置。".to_string()
            }
            AppError::RtmpStreamFailed { exit_code } => {
                format!("RTMP 推流异常中断（退出码 {exit_code}）。")
            }
            AppError::ConfigError { .. } => "服务端配置错误，请管理员检查日志。".to_string(),
            AppError::TelegramError(_) => "Telegram 消息通信暂时异常，请稍后重试。".to_string(),
            AppError::InvalidArgument { message } => {
                let clean = redact_secrets(message);
                clean.chars().take(120).collect()
            }
        };
    }

    // 非 AppError 的兜底归类（绝不直接透传底层错误链原文）
    let redacted_full = redact_secrets(&format!("{err:#}"));
    let lower = redacted_full.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "操作超时，请检查网络或媒体文件状态后重试。".to_string()
    } else if lower.contains("permission denied") {
        "服务端文件权限不足，请检查运行账号权限。".to_string()
    } else if lower.contains("no space left") {
        "磁盘剩余空间不足，操作已中止。".to_string()
    } else {
        "操作未能完成，请稍后重试或查看服务端脱敏日志排障。".to_string()
    }
}

// ── 统一流式脱敏 Writer 实现 ───────────────────────────────────────────────────

/// 底层流式缓冲脱敏状态机，支持跨 `write()` 拆块重组后脱敏。
struct RedactingStreamState<W: Write> {
    inner: W,
    pending: Vec<u8>,
    custom_secrets: Option<Arc<RwLock<Vec<String>>>>,
}

impl<W: Write> RedactingStreamState<W> {
    fn new(inner: W, custom_secrets: Option<Arc<RwLock<Vec<String>>>>) -> Self {
        Self {
            inner,
            pending: Vec::with_capacity(4096),
            custom_secrets,
        }
    }

    fn current_secrets(&self) -> Vec<String> {
        if let Some(custom) = &self.custom_secrets {
            custom.read().map(|g| g.clone()).unwrap_or_default()
        } else {
            GLOBAL_SECRETS.read().map(|g| g.clone()).unwrap_or_default()
        }
    }

    fn redact_chunk_bytes(&self, bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let secrets = self.current_secrets();
        redact_with_secrets(&text, &secrets)
    }

    fn push_and_flush_complete_lines(&mut self, buf: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(buf);

        // 优先在最后一个换行符处切分：完整行立即脱敏写出，未完结行保留等待下一块
        if let Some(last_nl) = self.pending.iter().rposition(|&b| b == b'\n') {
            let chunk: Vec<u8> = self.pending.drain(..=last_nl).collect();
            let redacted = self.redact_chunk_bytes(&chunk);
            self.inner.write_all(redacted.as_bytes())?;
            return Ok(());
        }

        // 防御异常超长无换行输出：在安全空白边界处切分，保留尾部重叠窗口以防截断秘密
        if self.pending.len() > MAX_BUFFER_WITHOUT_NEWLINE {
            let target_cut = self.pending.len().saturating_sub(OVERLAP_HOLDBACK_BYTES);
            let cut_idx = self.pending[..target_cut]
                .iter()
                .rposition(|b| matches!(*b, b' ' | b'\t' | b'\r'))
                .map(|idx| idx + 1)
                .unwrap_or_else(|| utf8_floor_boundary(&self.pending, target_cut));

            if cut_idx > 0 {
                let chunk: Vec<u8> = self.pending.drain(..cut_idx).collect();
                let redacted = self.redact_chunk_bytes(&chunk);
                self.inner.write_all(redacted.as_bytes())?;
            }
        }

        Ok(())
    }

    fn flush_all(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            let chunk = std::mem::take(&mut self.pending);
            let redacted = self.redact_chunk_bytes(&chunk);
            self.inner.write_all(redacted.as_bytes())?;
        }
        self.inner.flush()
    }
}

/// 寻找不超过 `index` 的合法 UTF-8 字符起始边界。
fn utf8_floor_boundary(bytes: &[u8], index: usize) -> usize {
    let mut idx = index.min(bytes.len());
    while idx > 0 && (bytes[idx] & 0b1100_0000) == 0b1000_0000 {
        idx -= 1;
    }
    idx
}

/// 面向 [`tracing_subscriber::fmt`] 的统一脱敏 `MakeWriter` 工厂。
pub struct RedactingMakeWriter<W: Write> {
    state: Arc<Mutex<RedactingStreamState<W>>>,
}

impl<W: Write> Clone for RedactingMakeWriter<W> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<W: Write> RedactingMakeWriter<W> {
    /// 使用全局秘密注册表包装底层输出流。
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self {
            state: Arc::new(Mutex::new(RedactingStreamState::new(inner, None))),
        }
    }

    /// 使用独立的秘密列表包装底层输出流（便于隔离单元测试）。
    #[must_use]
    #[allow(dead_code)]
    pub fn with_secret_store(inner: W, secrets: Arc<RwLock<Vec<String>>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(RedactingStreamState::new(inner, Some(secrets)))),
        }
    }

    /// 强制刷新所有残余未换行字节到底层输出流。
    pub fn flush_remaining(&self) -> io::Result<()> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| io::Error::other("RedactingMakeWriter mutex poisoned"))?;
        guard.flush_all()
    }
}

/// 返回全局共享的标准输出脱敏 `MakeWriter`。
#[must_use]
pub fn global_stdout_make_writer() -> RedactingMakeWriter<io::Stdout> {
    GLOBAL_STDOUT_WRITER.clone()
}

/// 刷新全局脱敏标准输出中的残余缓冲。
pub fn flush_global_stdout() {
    let _ = GLOBAL_STDOUT_WRITER.flush_remaining();
}

/// 单次写入句柄，实现 [`std::io::Write`]。
pub struct RedactingWriter<W: Write> {
    state: Arc<Mutex<RedactingStreamState<W>>>,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| io::Error::other("RedactingWriter mutex poisoned"))?;
        guard.push_and_flush_complete_lines(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| io::Error::other("RedactingWriter mutex poisoned"))?;
        guard.inner.flush()
    }
}

impl<'a, W: Write + 'static> MakeWriter<'a> for RedactingMakeWriter<W> {
    type Writer = RedactingWriter<W>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            state: Arc::clone(&self.state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn into_string(&self) -> String {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes).unwrap()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_redact_rtmp_target_preserves_only_scheme_and_host() {
        let url = "rtmps://user01:super_secret_pw@push.example.invalid:1935/live/fake_key_998877?token=abc123";
        let safe = redact_rtmp_target(url);
        assert_eq!(safe, "rtmps://push.example.invalid:1935/[REDACTED]");
        assert!(!safe.contains("user01"));
        assert!(!safe.contains("super_secret_pw"));
        assert!(!safe.contains("fake_key_998877"));
        assert!(!safe.contains("abc123"));
    }

    #[test]
    fn test_redact_all_secret_categories_and_keep_diagnostics() {
        // 全部使用虚构测试凭据
        let fake_bot_token = "123456789:AAFakeUnitTestBotTokenValue0123456789";
        let fake_rtmp = "rtmp://ingest.example.invalid/live/fake_stream_key_xyz987";
        let fake_client_secret = "GOCSPX-FakeClientSecretForTest999";
        let fake_refresh = "1//0gFakeRefreshTokenForUnitTestingOnly12345";
        let fake_access = "ya29.a0FakeAccessTokenValueForTestingOnly98765";
        let fake_session =
            "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&upload_id=ADPycdFakeSessionId12345";

        let mut store = Vec::new();
        insert_secret_variants(&mut store, fake_bot_token);
        insert_secret_variants(&mut store, fake_rtmp);
        insert_secret_variants(&mut store, "fake_stream_key_xyz987");
        insert_secret_variants(&mut store, fake_client_secret);
        insert_secret_variants(&mut store, fake_refresh);
        insert_secret_variants(&mut store, fake_access);
        insert_secret_variants(&mut store, fake_session);

        let raw_log = format!(
            "file=2026-10-29_live_part1.flv exit_code=1\n\
             tg_err=error sending request for url (https://api.telegram.org/bot{fake_bot_token}/getUpdates): 409 Conflict\n\
             ffmpeg_stderr=[rtmp @ 0x55] Cannot open connection {fake_rtmp} key=fake_stream_key_xyz987\n\
             oauth={{\"client_secret\":\"{fake_client_secret}\",\"refresh_token\":\"{fake_refresh}\"}}\n\
             header=Authorization: Bearer {fake_access}\n\
             session={fake_session}"
        );

        let redacted = redact_with_secrets(&raw_log, &store);

        // 任何虚构秘密原文均不得出现
        assert!(!redacted.contains(fake_bot_token));
        assert!(!redacted.contains("AAFakeUnitTest"));
        assert!(!redacted.contains("fake_stream_key_xyz987"));
        assert!(!redacted.contains(fake_client_secret));
        assert!(!redacted.contains(fake_refresh));
        assert!(!redacted.contains(fake_access));
        assert!(!redacted.contains("ADPycdFakeSessionId12345"));
        assert!(redacted.contains(REDACTED));

        // 普通诊断信息（文件名、409 状态、退出码）必须保留
        assert!(redacted.contains("2026-10-29_live_part1.flv"));
        assert!(redacted.contains("exit_code=1"));
        assert!(redacted.contains("409 Conflict"));
    }

    #[test]
    fn test_chunked_writes_across_secret_boundaries_pretty_and_json() {
        let fake_token = "987654321:AAFakeChunkedWriteTokenValueABCDE12345";
        let secrets = Arc::new(RwLock::new(Vec::new()));
        insert_secret_variants(&mut secrets.write().unwrap(), fake_token);

        let sink = SharedBuffer::default();
        let make_writer = RedactingMakeWriter::with_secret_store(sink.clone(), secrets);
        let mut writer = make_writer.make_writer();

        // 模拟 JSON 日志行被拆成极碎的 3 字节块逐次写入（横跨 token 中间）
        let json_event = format!(
            "{{\"level\":\"DEBUG\",\"message\":\"Polling https://api.telegram.org/bot{fake_token}/getUpdates failed\",\"code\":409}}\n"
        );
        for chunk in json_event.as_bytes().chunks(3) {
            writer.write_all(chunk).unwrap();
        }
        make_writer.flush_remaining().unwrap();

        let output = sink.into_string();
        assert!(!output.contains(fake_token));
        assert!(!output.contains("AAFakeChunkedWrite"));
        assert!(output.contains(REDACTED));
        assert!(output.contains("\"code\":409"));
    }
}
