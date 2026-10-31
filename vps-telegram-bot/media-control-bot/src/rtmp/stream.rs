//! RTMP 无损直推命令构建与错误分类基础设施。
//!
//! 作为全项目唯一的 RTMP FFmpeg 命令来源（消除与 `actions::stream` 的重复实现）：
//! - 严格保留 `-re` 与 `-c copy -f flv`（**零重编码**）；
//! - 显式映射首路视频轨（`-map 0:v:0`）与首路音频轨（`-map 0:a:0` 或 `-an`），避免字幕/附件流导致 FLV 封装报错；
//! - 配置 `-rw_timeout` 微秒级读写超时，防止远端黑洞连接永久挂死；
//! - 绝不在任何日志或错误中打印原始 `rtmp_url`，统一通过 [`redact_rtmp_target`] 脱敏。

use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;
use tracing::info;

use crate::core::state::SharedState;
use crate::errors::AppError;
use crate::media::ffmpeg::FfmpegProcess;
use crate::media::ffprobe::{check_rtmp_flv_compatibility, MediaProbeInfo};
use crate::utils::redact::{redact_rtmp_target, redact_secrets, register_rtmp_secret};

/// RTMP 推流失败原因安全分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtmpFailureCategory {
    /// 推流鉴权失败、推流码无效或直播间未开启
    AuthOrRejected,
    /// DNS 解析失败、连接被拒绝或无法建立握手
    ConnectFailed,
    /// 推流中途网络断开、对端重置或读写超时
    DisconnectedOrTimeout,
    /// 源容器时间戳错乱或数据包与 FLV 封装不兼容
    MuxingOrTimestampError,
    /// 其他未知原因
    Unknown,
}

impl RtmpFailureCategory {
    /// 返回面向管理员的安全中文说明。
    #[must_use]
    pub const fn user_hint(self) -> &'static str {
        match self {
            Self::AuthOrRejected => {
                "推流服务器拒绝发布（请检查目标平台直播间是否已开播、推流码是否过期或被占用）"
            }
            Self::ConnectFailed => {
                "无法连接到 RTMP 服务器（请检查推流域名、端口或 VPS 出站网络）"
            }
            Self::DisconnectedOrTimeout => {
                "推流过程中网络连接中断或读写超时（可能为上行带宽不足或远端主动断开）"
            }
            Self::MuxingOrTimestampError => {
                "源文件时间戳异常或数据包损坏导致 FLV 封装中断（建议先对该文件执行无损合并/修复）"
            }
            Self::Unknown => "FFmpeg 推流子进程非零退出",
        }
    }
}

/// 根据已脱敏的 `stderr` 摘要分类 RTMP 失败原因。
#[must_use]
pub fn classify_rtmp_failure(stderr_summary: &str, timed_out_by_watchdog: bool) -> RtmpFailureCategory {
    if timed_out_by_watchdog {
        return RtmpFailureCategory::DisconnectedOrTimeout;
    }

    let lower = stderr_summary.to_ascii_lowercase();
    if lower.contains("403")
        || lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("publish failed")
        || lower.contains("netstream.publish.badname")
        || lower.contains("access denied")
    {
        RtmpFailureCategory::AuthOrRejected
    } else if lower.contains("connection refused")
        || lower.contains("name or service not known")
        || lower.contains("could not resolve")
        || lower.contains("failed to connect")
        || lower.contains("network is unreachable")
    {
        RtmpFailureCategory::ConnectFailed
    } else if lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("end of file")
        || lower.contains("i/o error")
    {
        RtmpFailureCategory::DisconnectedOrTimeout
    } else if lower.contains("non-monotonous dts")
        || lower.contains("pts < dts")
        || lower.contains("codec not currently supported in container")
        || lower.contains("invalid data found")
    {
        RtmpFailureCategory::MuxingOrTimestampError
    } else {
        RtmpFailureCategory::Unknown
    }
}

/// 校验配置的 RTMP 目标地址协议合法性（仅允许 `rtmp://` 或 `rtmps://`）。
pub fn validate_rtmp_url(rtmp_url: &str) -> Result<()> {
    let trimmed = rtmp_url.trim();
    if trimmed.is_empty() {
        return Err(AppError::RtmpUrlNotConfigured.into());
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| {
        AppError::ConfigError {
            message: "RTMP_URL 格式无效（需以 rtmp:// 或 rtmps:// 开头）".to_string(),
        }
    })?;

    if !matches!(parsed.scheme(), "rtmp" | "rtmps") || parsed.host_str().is_none() {
        return Err(AppError::ConfigError {
            message: "RTMP_URL 必须使用 rtmp:// 或 rtmps:// 协议并包含有效主机名".to_string(),
        }
        .into());
    }

    Ok(())
}

/// 统一 RTMP 无损推流器。
pub struct RtmpStreamer {
    rtmp_url: String,
    rw_timeout_secs: u64,
}

impl RtmpStreamer {
    /// 创建推流器实例，自动校验 URL 并将其注册进全局脱敏注册表。
    pub fn new(rtmp_url: String, rw_timeout_secs: u64) -> Result<Self> {
        validate_rtmp_url(&rtmp_url)?;
        register_rtmp_secret(&rtmp_url);
        Ok(Self {
            rtmp_url,
            rw_timeout_secs: rw_timeout_secs.max(5),
        })
    }

    /// 返回脱敏后的推流目标展示字符串（如 `rtmp://a.rtmp.youtube.com/[REDACTED]`）。
    #[must_use]
    pub fn redacted_target(&self) -> String {
        redact_rtmp_target(&self.rtmp_url)
    }

    /// 根据源文件探测结果构造无损直推参数列表（严格 `-re` + `-c copy` + `-f flv`，绝不重编码）。
    pub fn build_args(&self, input: &Path, probe: &MediaProbeInfo) -> Result<Vec<OsString>> {
        check_rtmp_flv_compatibility(probe)?;

        let rw_timeout_us = self.rw_timeout_secs.saturating_mul(1_000_000);
        let mut args = vec![
            OsString::from("-re"),
            OsString::from("-i"),
            input.as_os_str().to_os_string(),
            OsString::from("-map"),
            OsString::from("0:v:0"),
        ];

        if probe.has_audio() {
            args.push(OsString::from("-map"));
            args.push(OsString::from("0:a:0"));
        } else {
            args.push(OsString::from("-an"));
        }

        args.push(OsString::from("-c"));
        args.push(OsString::from("copy"));
        args.push(OsString::from("-rw_timeout"));
        args.push(OsString::from(rw_timeout_us.to_string()));
        args.push(OsString::from("-f"));
        args.push(OsString::from("flv"));
        args.push(OsString::from(self.rtmp_url.as_str()));

        Ok(args)
    }

    /// 启动带结构化进度与并发 `stderr` 排空的 RTMP 推流进程。
    pub async fn spawn_process(
        &self,
        input: &Path,
        probe: &MediaProbeInfo,
        state: &SharedState,
    ) -> Result<FfmpegProcess> {
        let args = self.build_args(input, probe)?;
        let proc = FfmpegProcess::spawn(args, probe.duration_secs, state)
            .await
            .map_err(|e| anyhow::anyhow!("{}", redact_secrets(&e.to_string())))?;

        info!(
            filename = ?input.file_name(),
            target = %self.redacted_target(),
            pid = ?proc.pid(),
            duration_secs = probe.duration_secs,
            rw_timeout_secs = self.rw_timeout_secs,
            "RTMP lossless stream spawned"
        );

        Ok(proc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ffprobe::{AudioStreamInfo, VideoStreamInfo};

    fn sample_h264_probe(with_audio: bool) -> MediaProbeInfo {
        MediaProbeInfo {
            format_name: "mp4".into(),
            duration_secs: 600.0,
            bit_rate_bps: Some(4_000_000),
            video_streams: vec![VideoStreamInfo {
                codec_name: "h264".into(),
                profile: Some("High".into()),
                width: 1920,
                height: 1080,
                pix_fmt: Some("yuv420p".into()),
                r_frame_rate: Some("60/1".into()),
            }],
            audio_streams: if with_audio {
                vec![AudioStreamInfo {
                    codec_name: "aac".into(),
                    sample_rate: Some(48000),
                    channels: Some(2),
                    channel_layout: Some("stereo".into()),
                }]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn test_rtmp_streamer_args_and_redaction() {
        let streamer = RtmpStreamer::new(
            "rtmp://push.example.invalid/live2/fake_secret_key_777".into(),
            30,
        )
        .unwrap();

        assert_eq!(
            streamer.redacted_target(),
            "rtmp://push.example.invalid/[REDACTED]"
        );

        let args: Vec<String> = streamer
            .build_args(Path::new("/storage512/test.mp4"), &sample_h264_probe(true))
            .unwrap()
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        assert!(args.contains(&"-re".to_string()));
        assert!(args.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:a:0"]));
        assert!(args.windows(2).any(|w| w == ["-rw_timeout", "30000000"]));
        assert!(args.windows(2).any(|w| w == ["-f", "flv"]));
    }

    #[test]
    fn test_classify_rtmp_failure() {
        assert_eq!(
            classify_rtmp_failure("Server returned 403 Forbidden", false),
            RtmpFailureCategory::AuthOrRejected
        );
        assert_eq!(
            classify_rtmp_failure("av_interleaved_write_frame(): Broken pipe", false),
            RtmpFailureCategory::DisconnectedOrTimeout
        );
        assert_eq!(
            classify_rtmp_failure("", true),
            RtmpFailureCategory::DisconnectedOrTimeout
        );
    }
}
