//! FFprobe 媒体元数据探测与无损流兼容性检查。
//!
//! 所有 `ffprobe` 子进程调用均受超时控制（`kill_on_drop(true)` 保证超时自动终止子进程），
//! 并提供纯 JSON 解析与流参数兼容性校验函数供单元测试直接验证。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tracing::warn;

use crate::errors::AppError;
use crate::utils::redact::redact_secrets;

/// 默认 `ffprobe` 超时时间（秒）。
#[allow(dead_code)]
pub const DEFAULT_FFPROBE_TIMEOUT_SECS: u64 = 30;

/// 视频流关键参数摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct VideoStreamInfo {
    /// 编码格式（如 `h264`、`hevc`、`av1`）
    pub codec_name: String,
    /// Profile（如 `High`、`Main`，可能为空）
    pub profile: Option<String>,
    /// 画面宽度（像素）
    pub width: u32,
    /// 画面高度（像素）
    pub height: u32,
    /// 像素格式（如 `yuv420p`）
    pub pix_fmt: Option<String>,
    /// 标称帧率字符串（如 `60/1`，仅作诊断参考）
    pub r_frame_rate: Option<String>,
}

/// 音频流关键参数摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct AudioStreamInfo {
    /// 编码格式（如 `aac`、`mp3`、`opus`）
    pub codec_name: String,
    /// 采样率（Hz，如 `48000`）
    pub sample_rate: Option<u32>,
    /// 声道数（如 `2`）
    pub channels: Option<u32>,
    /// 声道布局（如 `stereo`）
    pub channel_layout: Option<String>,
}

/// 单个媒体文件的完整探测结果。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct MediaProbeInfo {
    /// 容器格式名称（如 `flv`、`mov,mp4,m4a,3gp,3g2,mj2`、`mpegts`）
    pub format_name: String,
    /// 媒体时长（秒；若容器头未记录则为 `0.0`）
    pub duration_secs: f64,
    /// 容器总码率（bps，若可获取）
    pub bit_rate_bps: Option<u64>,
    /// 视频流列表
    pub video_streams: Vec<VideoStreamInfo>,
    /// 音频流列表
    pub audio_streams: Vec<AudioStreamInfo>,
}

impl MediaProbeInfo {
    /// 是否包含至少一路视频流。
    #[must_use]
    pub fn has_video(&self) -> bool {
        !self.video_streams.is_empty()
    }

    /// 是否包含至少一路音频流。
    #[must_use]
    pub fn has_audio(&self) -> bool {
        !self.audio_streams.is_empty()
    }
}

// ── FFprobe JSON 反序列化结构 ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawFfprobeOutput {
    #[serde(default)]
    streams: Vec<RawStream>,
    #[serde(default)]
    format: Option<RawFormat>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    r_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

/// 解析 `ffprobe -print_format json -show_format -show_streams` 输出的 JSON 文本。
pub fn parse_ffprobe_json(json_text: &str) -> Result<MediaProbeInfo> {
    let raw: RawFfprobeOutput =
        serde_json::from_str(json_text).context("Invalid ffprobe JSON output")?;

    let mut video_streams = Vec::new();
    let mut audio_streams = Vec::new();
    let mut max_stream_duration: f64 = 0.0;

    for stream in raw.streams {
        if let Some(dur_str) = &stream.duration {
            if let Ok(d) = dur_str.trim().parse::<f64>() {
                if d.is_finite() && d > max_stream_duration {
                    max_stream_duration = d;
                }
            }
        }

        match stream.codec_type.as_deref() {
            Some("video") => {
                video_streams.push(VideoStreamInfo {
                    codec_name: stream
                        .codec_name
                        .unwrap_or_else(|| "unknown".to_string())
                        .to_ascii_lowercase(),
                    profile: stream.profile,
                    width: stream.width.unwrap_or(0),
                    height: stream.height.unwrap_or(0),
                    pix_fmt: stream.pix_fmt.map(|s| s.to_ascii_lowercase()),
                    r_frame_rate: stream.r_frame_rate,
                });
            }
            Some("audio") => {
                let sample_rate = stream
                    .sample_rate
                    .as_deref()
                    .and_then(|s| s.trim().parse::<u32>().ok());
                audio_streams.push(AudioStreamInfo {
                    codec_name: stream
                        .codec_name
                        .unwrap_or_else(|| "unknown".to_string())
                        .to_ascii_lowercase(),
                    sample_rate,
                    channels: stream.channels,
                    channel_layout: stream.channel_layout,
                });
            }
            _ => {}
        }
    }

    let (format_name, format_duration, bit_rate_bps) = match raw.format {
        Some(fmt) => {
            let fname = fmt.format_name.unwrap_or_else(|| "unknown".to_string());
            let fdur = fmt
                .duration
                .as_deref()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d > 0.0)
                .unwrap_or(0.0);
            let br = fmt
                .bit_rate
                .as_deref()
                .and_then(|s| s.trim().parse::<u64>().ok());
            (fname, fdur, br)
        }
        None => ("unknown".to_string(), 0.0, None),
    };

    let duration_secs = if format_duration > 0.0 {
        format_duration
    } else {
        max_stream_duration
    };

    Ok(MediaProbeInfo {
        format_name,
        duration_secs,
        bit_rate_bps,
        video_streams,
        audio_streams,
    })
}

/// 带超时调用 `ffprobe` 获取媒体容器与音视频流详情。
///
/// 超时后会自动终止并回收 `ffprobe` 子进程，防止僵尸进程堆积。
pub async fn probe_media(path: &Path, timeout_secs: u64) -> Result<MediaProbeInfo> {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    probe_media_cancellable(path, timeout_secs, &rx).await
}

/// 同时监听取消信号与有界超时的 `ffprobe` 媒体探测。
///
/// 收到 `/stop` 取消信号或超时时，立即终止并等待回收 `ffprobe` 子进程后退出。
pub async fn probe_media_cancellable(
    path: &Path,
    timeout_secs: u64,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
) -> Result<MediaProbeInfo> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }

    let mut cmd = tokio::process::Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn ffprobe: {e}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture ffprobe stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture ffprobe stderr"))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stdout.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let remain = (2 * 1024 * 1024usize).saturating_sub(buf.len());
            if remain == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n.min(remain)]);
        }
        buf
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let remain = (64 * 1024usize).saturating_sub(buf.len());
            if remain == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n.min(remain)]);
        }
        buf
    });

    let timeout_dur = Duration::from_secs(timeout_secs.max(1));
    let mut cancel_watch = cancel_rx.clone();

    let status = tokio::select! {
        res = child.wait() => {
            match res {
                Ok(st) => st,
                Err(e) => {
                    stdout_task.abort();
                    stderr_task.abort();
                    warn!(path = ?path, error = %e, "ffprobe execution I/O error");
                    return Err(AppError::FfprobeFailed {
                        path: path.to_path_buf(),
                    }
                    .into());
                }
            }
        }
        _ = cancel_watch.changed() => {
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(AppError::Cancelled.into());
        }
        _ = tokio::time::sleep(timeout_dur) => {
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            warn!(path = ?path, timeout_secs = timeout_secs, "ffprobe timed out and was reaped");
            return Err(AppError::FfprobeFailed {
                path: path.to_path_buf(),
            }
            .into());
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    if !status.success() {
        let stderr_str = redact_secrets(&String::from_utf8_lossy(&stderr_bytes));
        warn!(
            path = ?path,
            code = ?status.code(),
            stderr = %stderr_str.trim(),
            "ffprobe exited with non-zero status"
        );
        return Err(AppError::FfprobeFailed {
            path: path.to_path_buf(),
        }
        .into());
    }

    let stdout_str = String::from_utf8_lossy(&stdout_bytes);
    parse_ffprobe_json(&stdout_str).map_err(|e| {
        warn!(path = ?path, error = %e, "Failed to parse ffprobe JSON");
        AppError::FfprobeFailed {
            path: path.to_path_buf(),
        }
        .into()
    })
}

/// 带指定超时获取视频文件的时长（秒）；若容器未记录时长返回 `0.0`。
#[allow(dead_code)]
pub async fn get_video_duration_with_timeout(path: &Path, timeout_secs: u64) -> Result<f64> {
    let info = probe_media(path, timeout_secs).await?;
    Ok(info.duration_secs)
}

/// 获取视频文件的时长（默认 30 秒超时；若探测失败返回 `0.0`，兼容详情浏览等非阻断场景）。
#[allow(dead_code)]
pub async fn get_video_duration(path: &Path) -> Result<f64> {
    match get_video_duration_with_timeout(path, DEFAULT_FFPROBE_TIMEOUT_SECS).await {
        Ok(secs) => Ok(secs),
        Err(e) => {
            warn!(path = ?path, error = %redact_secrets(&e.to_string()), "get_video_duration fallback to 0.0");
            Ok(0.0)
        }
    }
}

/// 校验所有待合并分段的音视频编码参数是否满足无损 `-c copy` 拼接要求。
///
/// 由于严格禁止重编码，若分段之间的视频编码、分辨率、像素格式或音频编码、采样率、声道数不一致，
/// 直接返回 [`AppError::StreamIncompatible`] 并指明具体不一致的分段文件名与参数。
pub fn check_concat_stream_compatibility(
    segments: &[(PathBuf, MediaProbeInfo)],
) -> std::result::Result<(), AppError> {
    let Some((first_path, first_probe)) = segments.first() else {
        return Err(AppError::ConcatInsufficientFiles { count: 0 });
    };

    let first_name = display_filename(first_path);
    let Some(ref_v) = first_probe.video_streams.first() else {
        return Err(AppError::StreamIncompatible {
            reason: format!("分段 #1 ({first_name}) 不包含有效视频流"),
        });
    };
    let ref_a = first_probe.audio_streams.first();

    for (idx, (seg_path, probe)) in segments.iter().enumerate().skip(1) {
        let seg_no = idx + 1;
        let seg_name = display_filename(seg_path);

        let Some(cur_v) = probe.video_streams.first() else {
            return Err(AppError::StreamIncompatible {
                reason: format!("分段 #{seg_no} ({seg_name}) 不包含有效视频流"),
            });
        };

        if cur_v.codec_name != ref_v.codec_name {
            return Err(AppError::StreamIncompatible {
                reason: format!(
                    "分段 #{seg_no} ({seg_name}) 视频编码为 {}，与分段 #1 ({first_name}) 的 {} 不一致",
                    cur_v.codec_name, ref_v.codec_name
                ),
            });
        }

        if cur_v.width != ref_v.width || cur_v.height != ref_v.height {
            return Err(AppError::StreamIncompatible {
                reason: format!(
                    "分段 #{seg_no} ({seg_name}) 分辨率为 {}x{}，与分段 #1 ({first_name}) 的 {}x{} 不一致",
                    cur_v.width, cur_v.height, ref_v.width, ref_v.height
                ),
            });
        }

        if cur_v.pix_fmt != ref_v.pix_fmt {
            return Err(AppError::StreamIncompatible {
                reason: format!(
                    "分段 #{seg_no} ({seg_name}) 像素格式为 {:?}，与分段 #1 ({first_name}) 的 {:?} 不一致",
                    cur_v.pix_fmt, ref_v.pix_fmt
                ),
            });
        }

        let cur_a = probe.audio_streams.first();
        match (ref_a, cur_a) {
            (Some(_), None) => {
                return Err(AppError::StreamIncompatible {
                    reason: format!(
                        "分段 #{seg_no} ({seg_name}) 缺少音频轨，而分段 #1 ({first_name}) 含有音频轨"
                    ),
                });
            }
            (None, Some(_)) => {
                return Err(AppError::StreamIncompatible {
                    reason: format!(
                        "分段 #{seg_no} ({seg_name}) 含有音频轨，而分段 #1 ({first_name}) 无音频轨"
                    ),
                });
            }
            (Some(ra), Some(ca)) => {
                if ca.codec_name != ra.codec_name {
                    return Err(AppError::StreamIncompatible {
                        reason: format!(
                            "分段 #{seg_no} ({seg_name}) 音频编码为 {}，与分段 #1 ({first_name}) 的 {} 不一致",
                            ca.codec_name, ra.codec_name
                        ),
                    });
                }
                if ca.sample_rate != ra.sample_rate || ca.channels != ra.channels {
                    return Err(AppError::StreamIncompatible {
                        reason: format!(
                            "分段 #{seg_no} ({seg_name}) 音频采样率/声道为 {:?}Hz/{:?}ch，与分段 #1 ({first_name}) 的 {:?}Hz/{:?}ch 不一致",
                            ca.sample_rate, ca.channels, ra.sample_rate, ra.channels
                        ),
                    });
                }
            }
            (None, None) => {}
        }
    }

    Ok(())
}

/// 校验媒体流是否兼容 `-c copy -f flv` 无损 RTMP 直推（禁止隐式重编码）。
pub fn check_rtmp_flv_compatibility(probe: &MediaProbeInfo) -> std::result::Result<(), AppError> {
    const SUPPORTED_FLV_VIDEO_CODECS: &[&str] = &["h264", "hevc", "av1", "vp9", "flv1"];
    const SUPPORTED_FLV_AUDIO_CODECS: &[&str] =
        &["aac", "mp3", "speex", "pcm_alaw", "pcm_mulaw"];

    let Some(video) = probe.video_streams.first() else {
        return Err(AppError::StreamIncompatible {
            reason: "源文件不包含可推流的视频轨".to_string(),
        });
    };

    if !SUPPORTED_FLV_VIDEO_CODECS.contains(&video.codec_name.as_str()) {
        return Err(AppError::StreamIncompatible {
            reason: format!(
                "源视频编码 `{}` 不支持 `-c copy -f flv` 无损直推（本系统严格禁止重编码）",
                video.codec_name
            ),
        });
    }

    if let Some(audio) = probe.audio_streams.first() {
        if !SUPPORTED_FLV_AUDIO_CODECS.contains(&audio.codec_name.as_str()) {
            return Err(AppError::StreamIncompatible {
                reason: format!(
                    "源音频编码 `{}`（常见于 WebM/MKV）无法通过 `-c copy` 封装进 FLV/RTMP（本系统严格禁止重编码）",
                    audio.codec_name
                ),
            });
        }
    }

    Ok(())
}

fn display_filename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_probe_json(v_codec: &str, width: u32, height: u32, a_codec: &str, sr: u32) -> String {
        format!(
            r#"{{
                "streams": [
                    {{
                        "codec_type": "video",
                        "codec_name": "{v_codec}",
                        "profile": "High",
                        "width": {width},
                        "height": {height},
                        "pix_fmt": "yuv420p",
                        "r_frame_rate": "60/1"
                    }},
                    {{
                        "codec_type": "audio",
                        "codec_name": "{a_codec}",
                        "sample_rate": "{sr}",
                        "channels": 2,
                        "channel_layout": "stereo"
                    }}
                ],
                "format": {{
                    "format_name": "flv",
                    "duration": "1800.50",
                    "bit_rate": "6500000"
                }}
            }}"#
        )
    }

    #[test]
    fn test_parse_ffprobe_json_and_rtmp_compat() {
        let info = parse_ffprobe_json(&sample_probe_json("h264", 1920, 1080, "aac", 48000)).unwrap();
        assert!((info.duration_secs - 1800.5).abs() < 1e-6);
        assert_eq!(info.bit_rate_bps, Some(6_500_000));
        assert_eq!(info.video_streams[0].codec_name, "h264");
        assert_eq!(info.audio_streams[0].sample_rate, Some(48000));
        assert!(check_rtmp_flv_compatibility(&info).is_ok());

        // Opus 音频流不兼容 FLV 无损直推，必须被提前拦截而非重编码
        let opus_info =
            parse_ffprobe_json(&sample_probe_json("h264", 1920, 1080, "opus", 48000)).unwrap();
        assert!(check_rtmp_flv_compatibility(&opus_info).is_err());
    }

    #[test]
    fn test_concat_stream_compatibility_checks() {
        let p1 = parse_ffprobe_json(&sample_probe_json("h264", 1920, 1080, "aac", 48000)).unwrap();
        let p2 = parse_ffprobe_json(&sample_probe_json("h264", 1920, 1080, "aac", 48000)).unwrap();
        let p_mismatch_res =
            parse_ffprobe_json(&sample_probe_json("h264", 1280, 720, "aac", 48000)).unwrap();

        let ok_list = vec![
            (PathBuf::from("part1.flv"), p1.clone()),
            (PathBuf::from("part2.flv"), p2),
        ];
        assert!(check_concat_stream_compatibility(&ok_list).is_ok());

        let bad_list = vec![
            (PathBuf::from("part1.flv"), p1),
            (PathBuf::from("part2_720p.flv"), p_mismatch_res),
        ];
        let err = check_concat_stream_compatibility(&bad_list).unwrap_err();
        assert!(err.to_string().contains("part2_720p.flv"));
        assert!(err.to_string().contains("1280x720"));
    }
}
