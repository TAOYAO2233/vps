//! 合并结果校验逻辑。
//!
//! 对应 Python 版本的 `validate_merged_file` 和 `format_merge_check` 函数。
//! 采用「时长 + 体积」双指标校验：两者都达标（或时长不可用）时才判定成功。

use std::path::Path;

use crate::media::ffprobe::get_video_duration;
use crate::utils::format::format_duration;

/// 合并校验详情。
#[derive(Debug, Clone)]
pub struct MergeCheckDetails {
    /// 输入总时长（秒）
    pub input_duration: f64,
    /// 输出时长（秒）
    pub output_duration: f64,
    /// 输入总大小（字节）
    pub input_size: u64,
    /// 输出大小（字节）
    pub output_size: u64,
    /// 时长比例（output / input）
    pub duration_ratio: f64,
    /// 大小比例（output / input）
    pub size_ratio: f64,
    /// 时长校验是否通过
    pub duration_ok: bool,
    /// 大小校验是否通过
    pub size_ok: bool,
    /// 时长是否真正参与判定（输入或输出时长未知时为 `false`）
    pub duration_checked: bool,
}

/// 根据已知的输入/输出时长与体积判定合并是否合格（纯计算，便于单元测试）。
///
/// 校验规则：
/// 1. FFmpeg 退出码为 `0`
/// 2. 输出文件大小大于 `0`
/// 3. 输出大小 >= 输入总大小 × `min_size_ratio`
/// 4. 能取得输入与输出时长时：输出时长 >= 输入总时长 × `min_duration_ratio`
///
/// 第 4 条在时长不可用（输入时长未知，或 ffprobe 读取输出失败）时自动跳过，
/// 并通过 [`MergeCheckDetails::duration_checked`] 标注，避免把无法校验的情况误判为失败。
#[must_use]
pub fn evaluate_merge(
    exit_code: Option<i32>,
    input_duration: f64,
    output_duration: f64,
    input_size: u64,
    output_size: u64,
    min_duration_ratio: f64,
    min_size_ratio: f64,
) -> (bool, MergeCheckDetails) {
    let duration_ratio = if input_duration > 0.0 {
        output_duration / input_duration
    } else {
        0.0
    };
    let size_ratio = if input_size > 0 {
        output_size as f64 / input_size as f64
    } else {
        0.0
    };

    let duration_checked = input_duration > 0.0 && output_duration > 0.0;
    let duration_ok = !duration_checked || duration_ratio >= min_duration_ratio;
    let size_ok = input_size == 0 || output_size >= (input_size as f64 * min_size_ratio) as u64;

    let is_success = exit_code == Some(0) && output_size > 0 && size_ok && duration_ok;

    let details = MergeCheckDetails {
        input_duration,
        output_duration,
        input_size,
        output_size,
        duration_ratio,
        size_ratio,
        duration_ok,
        size_ok,
        duration_checked,
    };

    (is_success, details)
}

/// 读取输出文件的元数据与时长，并验证合并结果是否合格。
///
/// # Arguments
///
/// * `output_path` - 输出文件路径
/// * `exit_code` - FFmpeg 退出码（`None` 表示被取消）
/// * `input_total_duration` - 输入文件总时长（秒）
/// * `input_total_size` - 输入文件总大小（字节）
/// * `min_duration_ratio` - 输出时长最低比例（如 0.95）
/// * `min_size_ratio` - 输出文件大小最低比例（如 0.80）
///
/// # Returns
///
/// `(is_success, details)` 元组。
pub async fn validate_merged_file(
    output_path: &Path,
    exit_code: Option<i32>,
    input_total_duration: f64,
    input_total_size: u64,
    min_duration_ratio: f64,
    min_size_ratio: f64,
) -> (bool, MergeCheckDetails) {
    let output_size = if output_path.exists() {
        std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    let output_duration = if output_size > 0 {
        get_video_duration(output_path).await.unwrap_or(0.0)
    } else {
        0.0
    };

    evaluate_merge(
        exit_code,
        input_total_duration,
        output_duration,
        input_total_size,
        output_size,
        min_duration_ratio,
        min_size_ratio,
    )
}

/// 将合并校验详情格式化为 Telegram HTML 消息文本。
///
/// 对应 Python 版本的 `format_merge_check` 函数。
#[must_use]
pub fn format_merge_check(details: &MergeCheckDetails) -> String {
    let duration_ratio_pct = details.duration_ratio * 100.0;
    let size_ratio_pct = details.size_ratio * 100.0;
    let input_size_mb = details.input_size as f64 / (1024.0 * 1024.0);
    let output_size_mb = details.output_size as f64 / (1024.0 * 1024.0);

    let duration_note = if details.duration_checked {
        if details.duration_ok {
            ""
        } else {
            " ⚠️ 时长不足"
        }
    } else {
        "（时长不可用，未校验）"
    };
    let size_note = if details.size_ok { "" } else { " ⚠️ 体积不足" };

    format!(
        "⏱️ 输入总时长: <code>{}</code>\n\
         ⏱️ 输出时长: <code>{}</code> ({duration_ratio_pct:.1}%){duration_note}\n\
         📦 输出大小: <code>{output_size_mb:.2}MB</code> / 输入 <code>{input_size_mb:.2}MB</code> ({size_ratio_pct:.1}%){size_note}",
        format_duration(details.input_duration),
        format_duration(details.output_duration),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(input_duration: f64, output_duration: f64) -> MergeCheckDetails {
        let (_, details) = evaluate_merge(
            Some(0),
            input_duration,
            output_duration,
            1000,
            900,
            0.95,
            0.80,
        );
        details
    }

    #[test]
    fn test_evaluate_merge_success() {
        let (ok, details) = evaluate_merge(Some(0), 3600.0, 3590.0, 1000, 900, 0.95, 0.80);
        assert!(ok);
        assert!(details.duration_ok);
        assert!(details.size_ok);
        assert!(details.duration_checked);
    }

    #[test]
    fn test_evaluate_merge_duration_too_short() {
        let (ok, details) = evaluate_merge(Some(0), 3600.0, 1800.0, 1000, 900, 0.95, 0.80);
        assert!(!ok);
        assert!(!details.duration_ok);
        assert!(details.size_ok);
    }

    #[test]
    fn test_evaluate_merge_size_too_small() {
        let (ok, details) = evaluate_merge(Some(0), 3600.0, 3590.0, 1000, 100, 0.95, 0.80);
        assert!(!ok);
        assert!(details.duration_ok);
        assert!(!details.size_ok);
    }

    #[test]
    fn test_evaluate_merge_duration_unknown_is_skipped() {
        let (ok, details) = evaluate_merge(Some(0), 0.0, 0.0, 1000, 900, 0.95, 0.80);
        assert!(ok);
        assert!(!details.duration_checked);
    }

    #[test]
    fn test_evaluate_merge_failed_exit_code() {
        let (ok, _) = evaluate_merge(Some(1), 3600.0, 3590.0, 1000, 900, 0.95, 0.80);
        assert!(!ok);
    }

    #[test]
    fn test_format_merge_check() {
        let details = MergeCheckDetails {
            input_duration: 3600.0,
            output_duration: 3580.0,
            input_size: 1024 * 1024 * 1000,
            output_size: 1024 * 1024 * 980,
            duration_ratio: 3580.0 / 3600.0,
            size_ratio: 980.0 / 1000.0,
            duration_ok: true,
            size_ok: true,
            duration_checked: true,
        };
        let text = format_merge_check(&details);
        assert!(text.contains("01:00:00"));
        assert!(text.contains("980.00MB"));
        assert!(!text.contains('`'));
    }

    #[test]
    fn test_format_merge_check_warns_on_short_duration() {
        let text = format_merge_check(&details(3600.0, 1800.0));
        assert!(text.contains("⚠️ 时长不足"));
    }
}
