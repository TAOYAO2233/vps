//! 字符串格式化工具。
//!
//! 提供时长格式化、经过时间格式化、智能文件命名、上传列表格式化等工具函数。
//! 对应 Python 版本的 `format_duration`、`format_elapsed`、`smart_rename` 等函数。

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::core::state::UploadTask;

/// 匹配文件名中的日期时间模式（如 `2026-06-07` 或 `2026-06-07 20-00-00`）
static DATE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\d{4}[-_.]\d{2}[-_.]\d{2}(?:[ _-]\d{2}[-_.:]\d{2}[-_.:]\d{2})?").unwrap()
});

/// 匹配开头的日期时间括号，如 `[2026-06-07 20-00-00]`
static BRACKET_DATE_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\[\d[^\]]*\]\s*").unwrap());

/// 将秒数格式化为 `HH:MM:SS` 字符串。
///
/// 对应 Python 版本的 `format_duration` 函数。
///
/// # Arguments
///
/// * `seconds` - 时长（秒）
///
/// # Returns
///
/// 格式化后的时长字符串，如 `"01:23:45"`，若时长 <= 0 则返回 `"未知"`。
#[must_use]
pub fn format_duration(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "未知".to_string();
    }
    let total = seconds as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// 将秒数格式化为紧凑的经过时间字符串。
///
/// 对应 Python 版本的 `format_elapsed` 函数。
///
/// # Examples
///
/// - `65` → `"1m05s"`
/// - `3661` → `"1h01m01s"`
/// - `45` → `"45s"`
#[must_use]
pub fn format_elapsed(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// 将 `Instant` 格式化为经过时间字符串。
#[must_use]
#[allow(dead_code)]
pub fn format_elapsed_instant(instant: &Instant) -> String {
    format_elapsed(instant.elapsed().as_secs_f64())
}

/// 根据第一个文件名智能生成合并输出文件名。
///
/// 对应 Python 版本的 `smart_rename` 函数。
/// 尝试从文件名中提取日期时间信息，生成有意义的输出文件名。
///
/// # Arguments
///
/// * `first_file` - 第一个输入文件的路径
///
/// # Returns
///
/// 建议的输出文件名（含扩展名）。
#[must_use]
pub fn smart_rename(first_file: &Path) -> String {
    let base_name = first_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("merged");
    let ext = first_file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();

    // 提取日期时间字符串
    let date_str = if let Some(m) = DATE_REGEX.find(base_name) {
        m.as_str().to_string()
    } else {
        chrono::Local::now()
            .format("Merged_%Y%m%d_%H%M%S")
            .to_string()
    };

    // 清理开头的日期时间括号，保留后面的标题
    let title_part = BRACKET_DATE_REGEX.replace(base_name, "").to_string();
    let title_part = if title_part == base_name {
        // 没有括号格式，移除日期字符串本身
        let cleaned = base_name.replace(&date_str, "");
        let cleaned = cleaned.trim_start_matches(|c: char| "-_. ".contains(c));
        cleaned.to_string()
    } else {
        title_part
    };

    let output_name = if title_part.is_empty() {
        format!("{date_str}_merged{ext}")
    } else {
        format!("{date_str}_{title_part}{ext}")
    };

    // 清理连续下划线
    output_name.replace("__", "_")
}

/// 格式化 YouTube 上传任务列表为 Telegram 消息文本。
///
/// 对应 Python 版本的 `cmd_uploads` 中的格式化逻辑。
#[must_use]
pub fn format_upload_list(uploads: &HashMap<String, UploadTask>) -> String {
    let mut lines = vec![format!("📤 **YouTube 上传任务** ({} 个)\n", uploads.len())];

    for (idx, task) in uploads.values().enumerate() {
        let progress_text = format!(" {:.1}%", task.progress);
        lines.push(format!(
            "{}. `{}`\n   状态: {}{}\n   已运行: `{}`",
            idx + 1,
            task.filename,
            task.status,
            progress_text,
            format_elapsed(task.elapsed_secs()),
        ));
    }

    lines.push("\n发送 /stop 可停止当前所有任务。".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0.0), "未知");
        assert_eq!(format_duration(-1.0), "未知");
        assert_eq!(format_duration(3661.0), "01:01:01");
        assert_eq!(format_duration(3600.0), "01:00:00");
        assert_eq!(format_duration(59.0), "00:00:59");
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(45.0), "45s");
        assert_eq!(format_elapsed(65.0), "1m05s");
        assert_eq!(format_elapsed(3661.0), "1h01m01s");
        assert_eq!(format_elapsed(0.0), "0s");
    }

    #[test]
    fn test_smart_rename_with_date() {
        let path = Path::new("/tmp/[2026-06-07 20-00-00] 直播录像.mp4");
        let result = smart_rename(path);
        assert!(result.contains("2026"));
        assert!(!result.contains("__"));
    }

    #[test]
    fn test_smart_rename_no_date() {
        let path = Path::new("/tmp/random_video.mkv");
        let result = smart_rename(path);
        assert!(result.contains("Merged_") || result.contains("random"));
    }
}
