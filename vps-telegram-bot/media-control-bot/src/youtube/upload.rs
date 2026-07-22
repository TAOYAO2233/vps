//! YouTube 视频上传逻辑。
//!
//! 封装 YouTube Data API v3 的分块上传（resumable upload）流程。
//! 对应 Python 版本的 `upload_youtube_file` 中的 API 调用部分。
//!
//! ## 版本说明
//!
//! 使用 `google-youtube3 v5` 的 `VideoSnippet` / `VideoStatus` 类型（非 `Snippet`/`Status`）。

use std::io::{Read, Seek, SeekFrom, Result as IoResult};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use google_youtube3::api::{Video, VideoSnippet, VideoStatus};
use tracing::{debug, info};

use super::api::build_youtube_hub;

/// 带上传进度回调的 Reader 包装器。
/// 必须同时实现 `Read` 和 `Seek`，因为 google-youtube3 的 upload_resumable 要求 `R: Read + Seek`。
pub struct ProgressReader<R: Read + Seek, F: FnMut(f64)> {
    pub inner: R,
    pub total: u64,
    pub current: u64,
    pub callback: F,
}

// ✅ 补全声明包含 new 方法的 impl 块
impl<R: Read + Seek, F: FnMut(f64)> ProgressReader<R, F> {
    pub fn new(inner: R, total: u64, callback: F) -> Self {
        Self {
            inner,
            total,
            current: 0,
            callback,
        }
    }
}

impl<R: Read + Seek, F: FnMut(f64)> Read for ProgressReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.current += n as u64;
            if self.total > 0 {
                let percent = (self.current as f64 / self.total as f64) * 100.0;
                (self.callback)(percent.min(100.0));
            }
        }
        Ok(n)
    }
}

impl<R: Read + Seek, F: FnMut(f64)> Seek for ProgressReader<R, F> {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        let new_pos = self.inner.seek(pos)?;
        self.current = new_pos; // 保持游标与当前读取字节同步
        Ok(new_pos)
    }
}

/// YouTube 视频上传器。
pub struct YoutubeUploader {
    token_file: PathBuf,
    #[allow(dead_code)]
    chunk_size: usize,
}

impl YoutubeUploader {
    /// 创建上传器实例。
    ///
    /// # Arguments
    ///
    /// * `token_file` - OAuth2 token.json 文件路径
    /// * `chunk_size` - 上传分块大小（字节）
    #[must_use]
    pub fn new(token_file: PathBuf, chunk_size: usize) -> Self {
        Self {
            token_file,
            chunk_size,
        }
    }

    /// 上传视频文件到 YouTube（私享）。
    ///
    /// # Arguments
    ///
    /// * `file_path` - 视频文件路径
    /// * `title` - 视频标题（最多 100 字符）
    /// * `progress_callback` - 进度回调函数（接收 0.0~100.0 的进度百分比）
    ///
    /// # Returns
    ///
    /// 上传成功后的 YouTube 视频 ID。
    ///
    /// # Errors
    ///
    /// 若上传失败，返回错误。
    pub async fn upload(
        &self,
        file_path: &Path,
        title: &str,
        progress_callback: impl FnMut(f64),
    ) -> Result<String> {
        let hub = build_youtube_hub(&self.token_file)
            .await
            .context("Failed to build YouTube API client")?;

        // 截断标题至 95 字符（YouTube 限制 100 字符）
        let title_truncated: String = title.chars().take(95).collect();

        let video = Video {
            snippet: Some(VideoSnippet {
                title: Some(title_truncated.clone()),
                description: Some(String::new()),
                category_id: Some("22".to_string()), // People & Blogs
                ..Default::default()
            }),
            status: Some(VideoStatus {
                privacy_status: Some("private".to_string()),
                self_declared_made_for_kids: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let file = std::fs::File::open(file_path)
            .with_context(|| format!("Failed to open file: {}", file_path.display()))?;
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        debug!(
            title = %title_truncated,
            file_size = file_size,
            "Starting YouTube upload"
        );

        // ✅ 现在可以正常调用 ProgressReader::new 构造函数了
        let reader = ProgressReader::new(file, file_size, progress_callback);

        let (_response, video_result) = hub
            .videos()
            .insert(video)
            .upload_resumable(reader, "video/*".parse().unwrap())
            .await
            .context("YouTube upload API call failed")?;

        let video_id = video_result
            .id
            .ok_or_else(|| anyhow::anyhow!("YouTube API returned no video ID"))?;

        info!(video_id = %video_id, title = %title_truncated, "YouTube upload completed");

        Ok(video_id)
    }
}