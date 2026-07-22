//! 进度条生成与计算工具。
//!
//! 提供 Telegram 消息中使用的文本进度条，对应 Python 版本的 `build_progress_bar`。

/// 文本进度条生成器。
pub struct ProgressBar {
    /// 进度条总长度（字符数）
    length: usize,
    /// 填充字符（已完成部分）
    filled_char: char,
    /// 空白字符（未完成部分）
    empty_char: char,
}

impl Default for ProgressBar {
    fn default() -> Self {
        Self {
            length: 20,
            filled_char: '█',
            empty_char: '░',
        }
    }
}

impl ProgressBar {
    /// 创建自定义进度条。
    #[must_use]
    #[allow(dead_code)]
    pub fn new(length: usize) -> Self {
        Self {
            length,
            ..Default::default()
        }
    }

    /// 根据百分比渲染进度条字符串。
    ///
    /// # Arguments
    ///
    /// * `percent` - 进度百分比（0.0 ~ 100.0，超出范围自动夹紧）
    ///
    /// # Returns
    ///
    /// 格式为 `[████████░░░░░░░░░░░░]  40.0%` 的字符串。
    #[must_use]
    pub fn render(&self, percent: f64) -> String {
        let percent = percent.clamp(0.0, 100.0);
        let filled = ((percent / 100.0) * self.length as f64).floor() as usize;
        let empty = self.length - filled;

        let bar: String = std::iter::repeat(self.filled_char)
            .take(filled)
            .chain(std::iter::repeat(self.empty_char).take(empty))
            .collect();

        format!("[{bar}] {percent:5.1}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar_zero() {
        let bar = ProgressBar::default();
        let result = bar.render(0.0);
        assert!(result.contains("░░░░░░░░░░░░░░░░░░░░"));
        assert!(result.contains("  0.0%"));
    }

    #[test]
    fn test_progress_bar_full() {
        let bar = ProgressBar::default();
        let result = bar.render(100.0);
        assert!(result.contains("████████████████████"));
        assert!(result.contains("100.0%"));
    }

    #[test]
    fn test_progress_bar_half() {
        let bar = ProgressBar::default();
        let result = bar.render(50.0);
        assert!(result.contains("██████████░░░░░░░░░░"));
        assert!(result.contains(" 50.0%"));
    }

    #[test]
    fn test_progress_bar_clamp() {
        let bar = ProgressBar::default();
        // 超出范围应被夹紧
        let result_over = bar.render(150.0);
        let result_under = bar.render(-10.0);
        assert!(result_over.contains("100.0%"));
        assert!(result_under.contains("  0.0%"));
    }
}
