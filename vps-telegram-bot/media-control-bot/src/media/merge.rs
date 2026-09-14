//! 合并结果校验逻辑。
//!
//! 对应 Python 版本的 `validate_merged_file` 和 `format_merge_check` 函数。
//! 根据 FFmpeg 退出状态和输出文件是否有效，判断合并是否完成。

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
    #[allow(dead_code)]
    pub input_size: u64,
    /// 输出大小（字节）
    pub output_size: u64,
    /// 时长比例（output / input）
    pub duration_ratio: f64,
    /// 大小比例（output / input）
    pub size_ratio: f64,
    /// 时长校验是否通过
    #[allow(dead_code)]
    pub duration_ok: bool,
    /// 大小校验是否通过
    #[allow(dead_code)]
    pub size_ok: bool,
}

/// 验证合并结果文件是否合格。
///
/// 校验规则：
/// 1. FFmpeg 退出码为 0
/// 2. 输出文件存在且大小 > 0
/// 3. 输出大小 >= 输入总大小 × `min_size_ratio`
///
/// # Arguments
///
/// * `output_path` - 输出文件路径
/// * `exit_code` - FFmpeg 退出码（`None` 表示被取消）
/// * `input_total_duration` - 输入文件总时长（秒）
/// * `input_total_size` - 输入文件总大小（字节）
/// * `min_size_ratio` - 输出文件大小最低比例（如 0.8）
///
/// # Returns
///
/// `(is_success, details)` 元组。
pub async fn validate_merged_file(
    output_path: &Path,
    exit_code: Option<i32>,
    input_total_duration: f64,
    input_total_size: u64,
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

    // 时长仅用于结果展示，不参与合并成功判定。
    let duration_ok = true;
    let size_ok = input_total_size == 0
        || output_size >= (input_total_size as f64 * min_size_ratio) as u64;

    let duration_ratio = if input_total_duration > 0.0 {
        output_duration / input_total_duration
    } else {
        0.0
    };

    let size_ratio = if input_total_size > 0 {
        output_size as f64 / input_total_size as f64
    } else {
        0.0
    };

    let is_success = exit_code == Some(0) && output_size > 0 && size_ok;

    let details = MergeCheckDetails {
        input_duration: input_total_duration,
        output_duration,
        input_size: input_total_size,
        output_size,
        duration_ratio,
        size_ratio,
        duration_ok,
        size_ok,
    };

    (is_success, details)
}

/// 将合并校验详情格式化为 Telegram 消息文本。
///
/// 对应 Python 版本的 `format_merge_check` 函数。
#[must_use]
pub fn format_merge_check(details: &MergeCheckDetails) -> String {
    let duration_ratio_pct = details.duration_ratio * 100.0;
    let size_ratio_pct = details.size_ratio * 100.0;
    let output_size_mb = details.output_size as f64 / (1024.0 * 1024.0);

    format!(
        "⏱️ 输入总时长: `{}`\n\
         ⏱️ 输出时长: `{}` ({:.1}%)\n\
         📦 输出大小: `{:.2}MB` ({:.1}%)",
        format_duration(details.input_duration),
        format_duration(details.output_duration),
        duration_ratio_pct,
        output_size_mb,
        size_ratio_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let text = format_merge_check(&details);
        assert!(text.contains("01:00:00"));
        assert!(text.contains("980.00MB"));
    }
}
