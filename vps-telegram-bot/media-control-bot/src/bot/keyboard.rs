//! InlineKeyboard 构建工具。
//!
//! 提供各种场景下的键盘布局构建函数，对应 Python 版本中散布在各渲染函数里的
//! `InlineKeyboardMarkup` 构建代码。集中管理键盘布局，便于维护和测试。

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// 构建主菜单键盘。
#[must_use]
pub fn main_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "📂 浏览远程文件",
            "init_browse",
        )],
        vec![
            InlineKeyboardButton::callback("📡 RTMP 单路推流", "init_stream"),
            InlineKeyboardButton::callback("☁️ YouTube 上传", "init_youtube"),
        ],
        vec![InlineKeyboardButton::callback(
            "✂️ 智能视频合并",
            "init_concat",
        )],
        vec![InlineKeyboardButton::callback(
            "🔄 批量转码 MP4",
            "init_convert",
        )],
        vec![InlineKeyboardButton::callback(
            "🗑️ 批量删除文件",
            "init_delete",
        )],
    ])
}

/// 构建文件选择器键盘。
///
/// # Arguments
///
/// * `items` - 当前页的文件/目录列表，每项包含 (显示名, callback_data)
/// * `page` - 当前页码（0-indexed）
/// * `total_pages` - 总页数
/// * `action` - 操作类型字符串
/// * `is_at_base` - 是否在根目录
/// * `selected_count` - 已选中文件数量（0 表示不显示确认按钮）
#[must_use]
pub fn file_selector_keyboard(
    items: &[(String, String)],
    page: usize,
    total_pages: usize,
    action: &str,
    is_at_base: bool,
    selected_count: usize,
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = items
        .iter()
        .map(|(text, data)| vec![InlineKeyboardButton::callback(text.clone(), data.clone())])
        .collect();

    // 翻页按钮
    let mut nav_row = Vec::new();
    if page > 0 {
        nav_row.push(InlineKeyboardButton::callback(
            "⬅️ 上一页",
            format!("menu_{action}_{}", page - 1),
        ));
    }
    if page + 1 < total_pages {
        nav_row.push(InlineKeyboardButton::callback(
            "➡️ 下一页",
            format!("menu_{action}_{}", page + 1),
        ));
    }
    if !nav_row.is_empty() {
        rows.push(nav_row);
    }

    // 返回上级目录按钮
    if !is_at_base {
        rows.push(vec![InlineKeyboardButton::callback(
            "⬆️ 返回上一级目录",
            format!("updir_{action}"),
        )]);
    }

    // 确认执行按钮（多选模式）
    if selected_count > 0 {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("▶️ 确认执行 ({selected_count} 个文件)"),
            format!("execbatch_{action}"),
        )]);
    }

    // 返回主菜单
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 返回主菜单",
        "menu_main",
    )]);

    InlineKeyboardMarkup::new(rows)
}

/// 构建删除确认键盘。
#[must_use]
pub fn delete_confirmation_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ 确认删除", "confirm_delete"),
        InlineKeyboardButton::callback("取消", "cancel_delete"),
    ]])
}

/// 构建“返回主菜单”单按鈕键盘（用于错误提示等场景）。
#[must_use]
#[allow(dead_code)]
pub fn back_to_main_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🔙 返回主菜单",
        "menu_main",
    )]])
}
