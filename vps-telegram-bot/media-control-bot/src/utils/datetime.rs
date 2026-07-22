//! 时间处理工具。
//!
//! 提供时间戳格式化等工具函数。

use chrono::Local;

/// 获取当前本地时间的格式化字符串。
///
/// # Arguments
///
/// * `fmt` - 时间格式字符串（chrono 格式）
///
/// # Returns
///
/// 格式化后的时间字符串。
#[must_use]
#[allow(dead_code)]
pub fn now_formatted(fmt: &str) -> String {
    Local::now().format(fmt).to_string()
}

/// 获取当前时间的标准显示格式（`YYYY-MM-DD HH:MM:SS`）。
#[must_use]
#[allow(dead_code)]
pub fn now_display() -> String {
    now_formatted("%Y-%m-%d %H:%M:%S")
}

/// 获取当前时间的文件名安全格式（`YYYYMMDD_HHMMSS`）。
#[must_use]
#[allow(dead_code)]
pub fn now_filename_safe() -> String {
    now_formatted("%Y%m%d_%H%M%S")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_display_format() {
        let s = now_display();
        // 格式应为 YYYY-MM-DD HH:MM:SS，长度 19
        assert_eq!(s.len(), 19);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }

    #[test]
    fn test_now_filename_safe_format() {
        let s = now_filename_safe();
        // 格式应为 YYYYMMDD_HHMMSS，长度 15
        assert_eq!(s.len(), 15);
        assert_eq!(&s[8..9], "_");
    }
}
