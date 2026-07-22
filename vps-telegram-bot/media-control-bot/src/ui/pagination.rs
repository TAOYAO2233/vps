//! 分页逻辑计算。
//!
//! 提供通用的分页计算工具，对应 Python 版本中的 `math.ceil` 分页逻辑。

/// 分页器。
#[derive(Debug, Clone)]
pub struct Paginator {
    /// 总条目数
    pub total: usize,
    /// 每页条目数
    pub per_page: usize,
    /// 当前页码（0-indexed）
    pub current_page: usize,
}

impl Paginator {
    /// 创建分页器。
    ///
    /// # Arguments
    ///
    /// * `total` - 总条目数
    /// * `per_page` - 每页条目数
    /// * `current_page` - 请求的页码（0-indexed，会自动夹紧到合法范围）
    #[must_use]
    pub fn new(total: usize, per_page: usize, current_page: usize) -> Self {
        let total_pages = Self::calc_total_pages(total, per_page);
        let current_page = if total_pages == 0 {
            0
        } else {
            current_page.min(total_pages - 1)
        };
        Self {
            total,
            per_page,
            current_page,
        }
    }

    /// 计算总页数。
    #[must_use]
    pub fn total_pages(&self) -> usize {
        Self::calc_total_pages(self.total, self.per_page)
    }

    /// 返回当前页的起始索引（包含）。
    #[must_use]
    pub fn start_index(&self) -> usize {
        self.current_page * self.per_page
    }

    /// 返回当前页的结束索引（不包含）。
    #[must_use]
    pub fn end_index(&self) -> usize {
        ((self.current_page + 1) * self.per_page).min(self.total)
    }

    /// 返回当前页的条目切片范围。
    #[must_use]
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start_index()..self.end_index()
    }

    /// 是否有上一页。
    #[must_use]
    #[allow(dead_code)]
    pub fn has_prev(&self) -> bool {
        self.current_page > 0
    }

    /// 是否有下一页。
    #[must_use]
    #[allow(dead_code)]
    pub fn has_next(&self) -> bool {
        self.current_page + 1 < self.total_pages()
    }

    fn calc_total_pages(total: usize, per_page: usize) -> usize {
        if per_page == 0 {
            return 1;
        }
        total.div_ceil(per_page).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paginator_basic() {
        let p = Paginator::new(25, 8, 0);
        assert_eq!(p.total_pages(), 4); // ceil(25/8) = 4
        assert_eq!(p.start_index(), 0);
        assert_eq!(p.end_index(), 8);
        assert!(!p.has_prev());
        assert!(p.has_next());
    }

    #[test]
    fn test_paginator_last_page() {
        let p = Paginator::new(25, 8, 3);
        assert_eq!(p.start_index(), 24);
        assert_eq!(p.end_index(), 25);
        assert!(p.has_prev());
        assert!(!p.has_next());
    }

    #[test]
    fn test_paginator_clamp() {
        // 请求超出范围的页码应被夹紧
        let p = Paginator::new(10, 8, 999);
        assert_eq!(p.current_page, 1); // 只有 2 页，最大页码为 1
    }

    #[test]
    fn test_paginator_empty() {
        let p = Paginator::new(0, 8, 0);
        assert_eq!(p.total_pages(), 1);
        assert_eq!(p.range(), 0..0);
    }
}
