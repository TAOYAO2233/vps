pub(crate) use crate::models::number;

pub(crate) fn short(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    if text.encode_utf16().count() <= limit {
        return text.into();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        used += ch.len_utf16();
        if used > limit.saturating_sub(1) {
            break;
        }
        result.push(ch);
    }
    result.push('…');
    result
}
pub(crate) fn field(v: &serde_json::Value, key: &str) -> String {
    match v.get(key) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "暂无数据".into(),
    }
}
pub(crate) fn optional(s: Option<&str>) -> &str {
    s.filter(|s| !s.is_empty()).unwrap_or("暂无数据")
}
pub(crate) fn size_text(mut n: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut i = 0;
    while n >= 1024.0 && i < units.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", n, units[i])
}

pub(crate) const MESSAGE_LIMIT: usize = 3900;
const STREAM_PAGE_LIMIT: usize = 3200;

pub(crate) fn split_pages(text: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut current = String::new();
    let mut length = 0;
    for ch in text.chars() {
        if length + ch.len_utf16() > STREAM_PAGE_LIMIT {
            pages.push(std::mem::take(&mut current));
            length = 0;
        }
        length += ch.len_utf16();
        current.push(ch);
    }
    if !current.is_empty() {
        pages.push(current);
    }
    if pages.is_empty() {
        pages.push("暂无数据".into());
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_utf16_limit_and_unicode_boundaries() {
        assert_eq!(short("abc", 0), "");
        assert_eq!(short("abc", 1), "…");
        assert_eq!(short("😀中文", 3), "😀…");
        assert_eq!(short("中文", 2), "中文");
    }

    #[test]
    fn pagination_preserves_all_text_without_exceeding_telegram_budget() {
        let text = "😀中文\n".repeat(2000);
        let pages = split_pages(&text);
        assert!(pages.len() > 1);
        assert_eq!(pages.concat(), text);
        assert!(pages.iter().all(|page| page.encode_utf16().count() <= 3200));
        assert_eq!(split_pages(""), vec!["暂无数据"]);
    }
}
