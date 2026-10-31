//! 无损合并结果严格校验与报告格式化模块。
//!
//! 严格对齐交接文档 §3.3 与 §3.4：
//! 1. FFmpeg 退出码必须为 `0`；
//! 2. 输出文件存在且大小 `> 0`；
//! 3. FFprobe 必须成功解析输出容器与音视频流，且输出时长有效（`> 0`），绝不因 FFprobe 失败跳过校验；
//! 4. 输出时长与期望保留时长匹配，且总片尾截除时长不超过 `max_loss_seconds`；
//! 5. 文件体积比仅作辅助异常提示指标，不单独否决流与时长均完整的无损合并产物。

use std::path::Path;

use crate::media::ffprobe::{probe_media_cancellable, MediaProbeInfo};
use crate::utils::format::{escape_html, format_duration, format_elapsed};
use crate::utils::redact::redact_secrets;

/// 最终生效的无损合并策略。
#[derive(Debug, Clone, PartialEq)]
pub enum MergeStrategy {
    /// 阶段 1：直接无损 concat demuxer 拼接
    DirectConcat,
    /// 阶段 2A：逐分段标准无损重封装为 MPEG-TS 后拼接
    TsRemux,
    /// 阶段 2B：启用 `-fflags +discardcorrupt+genpts` 容错无损重封装为 MPEG-TS 后拼接
    TsRemuxCorruptTolerant,
    /// 阶段 2C：对损坏尾部分段执行有界片尾无损截除（`-t` 置于 `-i` 之前）重封装后拼接
    TsRemuxBoundedTailTrim {
        /// 被截除片尾的分段明细
        trims: Vec<SegmentTrimRecord>,
    },
}

impl MergeStrategy {
    /// 返回策略的中文说明文本。
    #[must_use]
    pub fn display_label(&self) -> String {
        match self {
            Self::DirectConcat => "阶段 1：直接无损流复制拼接 (-f concat -c copy)".to_string(),
            Self::TsRemux => "阶段 2：MPEG-TS 中间层无损重封装拼接".to_string(),
            Self::TsRemuxCorruptTolerant => {
                "阶段 2：MPEG-TS 容错重建时间戳无损重封装 (+discardcorrupt+genpts)".to_string()
            }
            Self::TsRemuxBoundedTailTrim { trims } => {
                let total_trim: u64 = trims.iter().map(|t| t.trimmed_seconds).sum();
                format!("阶段 2：有界片尾无损截除重封装（共截除 {total_trim}s 损坏尾部）")
            }
        }
    }
}

/// 单个分段的有界片尾无损截除记录。
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentTrimRecord {
    /// 1-based 分段序号
    pub segment_index: usize,
    /// 分段文件名
    pub filename: String,
    /// 原分段探测时长（秒）
    pub original_duration: f64,
    /// 截除的片尾秒数
    pub trimmed_seconds: u64,
}

/// 严格合并校验详情。
#[derive(Debug, Clone)]
pub struct MergeCheckDetails {
    /// 输入分段原始总时长（秒）
    pub input_duration: f64,
    /// 期望保留总时长（扣除有界片尾截除后的秒数）
    pub expected_retained_duration: f64,
    /// 实际输出时长（秒）
    pub output_duration: f64,
    /// 总截除秒数
    pub total_trimmed_seconds: u64,
    /// 输入总大小（字节）
    pub input_size: u64,
    /// 输出大小（字节）
    pub output_size: u64,
    /// 输出时长占期望保留时长比例
    pub duration_ratio: f64,
    /// 输出大小占输入总大小比例（辅助参考）
    pub size_ratio: f64,
    /// FFprobe 是否成功解析输出容器与完整音视频流且 `output_duration > 0`
    pub probe_streams_ok: bool,
    /// 保留时长与截除上限校验是否通过
    pub duration_ok: bool,
    /// 辅助体积比指标是否达到参考线（仅提示，不单独否决）
    pub size_ok: bool,
    /// 若校验未通过，给出简明原因
    pub failure_reason: Option<String>,
}

/// 纯函数校验合并结果是否满足全部强制完整性条件（便于无外部进程单元测试）。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn evaluate_merge_strict(
    exit_code: Option<i32>,
    input_total_duration: f64,
    input_total_size: u64,
    input_has_audio: bool,
    output_size: u64,
    output_probe: Option<&MediaProbeInfo>,
    total_trimmed_seconds: u64,
    max_loss_seconds: u64,
    min_duration_ratio: f64,
    min_size_ratio: f64,
) -> (bool, MergeCheckDetails) {
    let expected_retained_duration =
        (input_total_duration - total_trimmed_seconds as f64).max(0.0);

    let output_duration = output_probe.map_or(0.0, |p| p.duration_secs);

    let probe_streams_ok = match output_probe {
        Some(p) => {
            p.has_video()
                && (!input_has_audio || p.has_audio())
                && p.duration_secs.is_finite()
                && p.duration_secs > 0.0
        }
        None => false,
    };

    let duration_ratio = if expected_retained_duration > 0.0 {
        output_duration / expected_retained_duration
    } else if output_duration > 0.0 {
        1.0
    } else {
        0.0
    };

    let size_ratio = if input_total_size > 0 {
        output_size as f64 / input_total_size as f64
    } else {
        0.0
    };

    let trim_within_limit = total_trimmed_seconds <= max_loss_seconds;
    let retained_ok = if expected_retained_duration > 0.0 {
        duration_ratio >= min_duration_ratio
            && output_duration <= expected_retained_duration * 1.05 + 2.0
    } else {
        output_duration > 0.0
    };
    let duration_ok = probe_streams_ok && trim_within_limit && retained_ok;

    // 体积比仅作为辅助参考指标
    let size_ok =
        input_total_size == 0 || output_size >= (input_total_size as f64 * min_size_ratio) as u64;

    let mut failure_reason = None;
    if exit_code != Some(0) {
        failure_reason = Some(format!("FFmpeg 退出码非零 ({exit_code:?})"));
    } else if output_size == 0 {
        failure_reason = Some("合并输出文件不存在或大小为 0".to_string());
    } else if !probe_streams_ok {
        failure_reason = Some(
            "FFprobe 无法解析输出容器、缺失音视频流或输出时长无效 (<=0)".to_string(),
        );
    } else if !trim_within_limit {
        failure_reason = Some(format!(
            "片尾累计截除时长 ({total_trimmed_seconds}s) 超出允许上限 ({max_loss_seconds}s)"
        ));
    } else if !retained_ok {
        failure_reason = Some(format!(
            "输出时长 ({:.1}s) 超出期望保留时长 ({:.1}s) 的合理范围（最低 {:.0}%）",
            output_duration,
            expected_retained_duration,
            min_duration_ratio * 100.0
        ));
    }

    let is_success = failure_reason.is_none();

    let details = MergeCheckDetails {
        input_duration: input_total_duration,
        expected_retained_duration,
        output_duration,
        total_trimmed_seconds,
        input_size: input_total_size,
        output_size,
        duration_ratio,
        size_ratio,
        probe_streams_ok,
        duration_ok,
        size_ok,
        failure_reason,
    };

    (is_success, details)
}

/// 对合并临时产物执行文件大小读取与带超时、响应取消信号的 `ffprobe` 校验。
#[allow(clippy::too_many_arguments)]
pub async fn validate_merged_output(
    output_path: &Path,
    exit_code: Option<i32>,
    input_total_duration: f64,
    input_total_size: u64,
    input_has_audio: bool,
    total_trimmed_seconds: u64,
    max_loss_seconds: u64,
    min_duration_ratio: f64,
    min_size_ratio: f64,
    ffprobe_timeout_secs: u64,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<(bool, MergeCheckDetails)> {
    if *cancel_rx.borrow() {
        return Err(crate::errors::AppError::Cancelled.into());
    }

    let output_size = std::fs::symlink_metadata(output_path)
        .ok()
        .filter(|m| m.file_type().is_file())
        .map_or(0, |m| m.len());

    let probe_opt = if exit_code == Some(0) && output_size > 0 {
        match probe_media_cancellable(output_path, ffprobe_timeout_secs, cancel_rx).await {
            Ok(p) => Some(p),
            Err(e) if e.downcast_ref::<crate::errors::AppError>() == Some(&crate::errors::AppError::Cancelled) => {
                return Err(e);
            }
            Err(_) => None,
        }
    } else {
        None
    };

    Ok(evaluate_merge_strict(
        exit_code,
        input_total_duration,
        input_total_size,
        input_has_audio,
        output_size,
        probe_opt.as_ref(),
        total_trimmed_seconds,
        max_loss_seconds,
        min_duration_ratio,
        min_size_ratio,
    ))
}

/// 将合并校验指标格式化为 Telegram HTML 块。
#[must_use]
pub fn format_merge_check(details: &MergeCheckDetails) -> String {
    let duration_ratio_pct = details.duration_ratio * 100.0;
    let size_ratio_pct = details.size_ratio * 100.0;
    let input_size_mb = details.input_size as f64 / (1024.0 * 1024.0);
    let output_size_mb = details.output_size as f64 / (1024.0 * 1024.0);

    let duration_note = if details.duration_ok {
        ""
    } else {
        " ⚠️ 时长或媒体流未达标"
    };
    let size_note = if details.size_ok {
        ""
    } else {
        " ℹ️ 辅助提示：封装后体积占比较低（以音视频流与时长为准）"
    };

    let trim_line = if details.total_trimmed_seconds > 0 {
        format!(
            "\n✂️ 片尾无损截除: <code>{}s</code>（期望保留 <code>{}</code>）",
            details.total_trimmed_seconds,
            format_duration(details.expected_retained_duration)
        )
    } else {
        String::new()
    };

    format!(
        "⏱️ 输入总时长: <code>{}</code>{trim_line}\n\
         ⏱️ 输出时长: <code>{}</code> ({duration_ratio_pct:.1}%){duration_note}\n\
         📦 输出大小: <code>{output_size_mb:.2}MB</code> / 输入 <code>{input_size_mb:.2}MB</code> ({size_ratio_pct:.1}%){size_note}",
        format_duration(details.input_duration),
        format_duration(details.output_duration),
    )
}

/// 格式化无损合并成功的最终 Telegram HTML 报告。
#[must_use]
pub fn format_merge_success_report(
    output_filename: &str,
    strategy: &MergeStrategy,
    details: &MergeCheckDetails,
    elapsed_secs: f64,
) -> String {
    let mut text = format!(
        "✅ <b>无损合并成功！</b>\n\
         ━━━━━━━━━━━━\n\
         📄 输出文件: <code>{}</code>\n\
         🛠️ 合并策略: {}\n\
         {}\n\
         ⏱️ 耗时: <code>{}</code>",
        escape_html(output_filename),
        escape_html(&strategy.display_label()),
        format_merge_check(details),
        format_elapsed(elapsed_secs),
    );

    if let MergeStrategy::TsRemuxBoundedTailTrim { trims } = strategy {
        if !trims.is_empty() {
            text.push_str("\n\n🔍 <b>分段片尾修复明细</b>:");
            for t in trims {
                text.push_str(&format!(
                    "\n• 分段 #{}: <code>{}</code> 截除尾部 <code>{}s</code>（原长 <code>{}</code>）",
                    t.segment_index,
                    escape_html(&t.filename),
                    t.trimmed_seconds,
                    format_duration(t.original_duration),
                ));
            }
        }
    }

    text.push_str("\n\n💡 原始分段文件完好保留，全程采用 <code>-c copy</code> 零重编码。");
    text
}

/// 当保守默认 `MERGE_ALLOW_AUTO_TRIM=false` 时，若探测到有界片尾截除可修复损坏分段，生成待确认报告（不自动丢失内容、不产出文件）。
#[must_use]
pub fn format_merge_candidate_trim_prompt(
    trims: &[SegmentTrimRecord],
    candidate_details: &MergeCheckDetails,
    max_loss_seconds: u64,
) -> String {
    let mut trim_lines = String::new();
    for t in trims {
        trim_lines.push_str(&format!(
            "\n• 分段 #{}: <code>{}</code> 需截除损坏片尾 <code>{}s</code>（原长 <code>{}</code>）",
            t.segment_index,
            escape_html(&t.filename),
            t.trimmed_seconds,
            format_duration(t.original_duration),
        ));
    }

    format!(
        "⚠️ <b>检测到分段尾部损坏（未自动截除内容）</b>\n\
         ━━━━━━━━━━━━\n\
         常规直接拼接与标准/容错 TS 无损重封装均未能完整修复，但在临时目录中预探测到<b>有界片尾无损截除方案</b>可成功合并（上限 <code>{max_loss_seconds}s</code>）：\
         {trim_lines}\n\n\
         📊 <b>预探测合并指标</b>:\n\
         {}\n\n\
         🛡️ 当前处于保守策略（<code>MERGE_ALLOW_AUTO_TRIM=false</code>），Bot <b>未自动丢弃任何视频内容</b>，已清理全部临时文件且未生成残损成品。\n\
         如需启用上限内自动无损片尾截除，请在 <code>.env</code> 中确认设置 <code>MERGE_ALLOW_AUTO_TRIM=true</code> 后重试。",
        format_merge_check(candidate_details)
    )
}

/// 格式化不可恢复的无损合并失败报告（不产生残损成品，明确指出疑似损坏分段与时间点）。
#[must_use]
pub fn format_merge_failure_report(
    stage_name: &str,
    suspected_segment: Option<&str>,
    approx_time_secs: Option<f64>,
    reason: &str,
    stderr_summary: &str,
) -> String {
    let seg_line = match suspected_segment {
        Some(seg) => format!("\n📍 疑似异常分段: <code>{}</code>", escape_html(seg)),
        None => String::new(),
    };
    let pos_line = match approx_time_secs {
        Some(secs) if secs > 0.0 => format!("\n⏱️ 大致异常位置: <code>{}</code>", format_duration(secs)),
        _ => String::new(),
    };

    let safe_reason = escape_html(&redact_secrets(reason));
    let safe_stderr = redact_secrets(stderr_summary);
    let mut short_stderr = String::new();
    for (idx, ch) in safe_stderr.chars().enumerate() {
        if idx >= 450 {
            short_stderr.push('…');
            break;
        }
        short_stderr.push(ch);
    }

    let stderr_block = if short_stderr.trim().is_empty() {
        String::new()
    } else {
        format!("\n🧾 摘要诊断:\n<pre>{}</pre>", escape_html(short_stderr.trim()))
    };

    format!(
        "❌ <b>无损合并失败（已放弃产出）</b>\n\
         ━━━━━━━━━━━━\n\
         🛑 失败阶段: {}\
         {seg_line}\
         {pos_line}\n\
         ⚠️ 原因: {safe_reason}\
         {stderr_block}\n\n\
         🧹 所有临时中间文件已清理，原始分段未被修改，本系统绝不启用重编码或输出残损文件。",
        escape_html(stage_name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ffprobe::{AudioStreamInfo, VideoStreamInfo};

    fn make_valid_probe(duration_secs: f64, with_audio: bool) -> MediaProbeInfo {
        MediaProbeInfo {
            format_name: "mov,mp4,m4a".to_string(),
            duration_secs,
            bit_rate_bps: Some(5_000_000),
            video_streams: vec![VideoStreamInfo {
                codec_name: "h264".to_string(),
                profile: Some("High".to_string()),
                width: 1920,
                height: 1080,
                pix_fmt: Some("yuv420p".to_string()),
                r_frame_rate: Some("60/1".to_string()),
            }],
            audio_streams: if with_audio {
                vec![AudioStreamInfo {
                    codec_name: "aac".to_string(),
                    sample_rate: Some(48000),
                    channels: Some(2),
                    channel_layout: Some("stereo".to_string()),
                }]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn test_strict_validation_never_skips_when_ffprobe_fails() {
        // FFprobe 失败（None）或时长 <= 0 必须判定为失败，绝不跳过放行！
        let (ok_none, _) = evaluate_merge_strict(
            Some(0),
            3600.0,
            1_000_000,
            true,
            980_000,
            None,
            0,
            3,
            0.95,
            0.80,
        );
        assert!(!ok_none);

        let zero_dur_probe = make_valid_probe(0.0, true);
        let (ok_zero, _) = evaluate_merge_strict(
            Some(0),
            3600.0,
            1_000_000,
            true,
            980_000,
            Some(&zero_dur_probe),
            0,
            3,
            0.95,
            0.80,
        );
        assert!(!ok_zero);
    }

    #[test]
    fn test_strict_validation_rejects_missing_audio_or_short_duration_or_excess_trim() {
        // 输入有音频而输出丢了音频轨：必须失败
        let no_audio_probe = make_valid_probe(3595.0, false);
        let (ok_no_audio, _) = evaluate_merge_strict(
            Some(0),
            3600.0,
            1_000_000,
            true,
            980_000,
            Some(&no_audio_probe),
            0,
            3,
            0.95,
            0.80,
        );
        assert!(!ok_no_audio);

        // 输出时长不足：必须失败
        let short_probe = make_valid_probe(1800.0, true);
        let (ok_short, _) = evaluate_merge_strict(
            Some(0),
            3600.0,
            1_000_000,
            true,
            980_000,
            Some(&short_probe),
            0,
            3,
            0.95,
            0.80,
        );
        assert!(!ok_short);

        // 截除时长超过上限（例如 5s > 3s）：必须失败
        let trimmed_probe = make_valid_probe(3595.0, true);
        let (ok_excess_trim, _) = evaluate_merge_strict(
            Some(0),
            3600.0,
            1_000_000,
            true,
            980_000,
            Some(&trimmed_probe),
            5,
            3,
            0.95,
            0.80,
        );
        assert!(!ok_excess_trim);
    }

    #[test]
    fn test_size_ratio_is_auxiliary_and_does_not_alone_fail_valid_merge() {
        // TS -> MP4 去除了 188B 包填充开销，体积变为输入的 75%（低于 0.80），但音视频流与时长完全正常
        let valid_probe = make_valid_probe(3598.0, true);
        let (ok, details) = evaluate_merge_strict(
            Some(0),
            3600.0,
            1_000_000,
            true,
            750_000,
            Some(&valid_probe),
            2,
            3,
            0.95,
            0.80,
        );
        assert!(ok);
        assert!(details.duration_ok);
        assert!(!details.size_ok);
        let formatted = format_merge_check(&details);
        assert!(formatted.contains("辅助提示"));
    }
}
