//! InlineKeyboard 构建工具。
//!
//! 集中管理所有 Telegram 内联键盘布局与 `callback_data` 编码格式（全部严格小于 64 字节上限）。

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
            "✂️ 智能无损合并",
            "init_concat",
        )],
        vec![InlineKeyboardButton::callback(
            "🔄 批量无损转 MP4",
            "init_convert",
        )],
        vec![InlineKeyboardButton::callback(
            "🗑️ 批量删除文件",
            "init_delete",
        )],
    ])
}

/// 构建带快照标识 `snap_id` 的文件选择器键盘。
#[must_use]
pub fn file_selector_keyboard(
    items: &[(String, String)],
    page: usize,
    total_pages: usize,
    action: &str,
    snap_id: &str,
    is_at_base: bool,
    selected_count: usize,
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = items
        .iter()
        .map(|(text, data)| vec![InlineKeyboardButton::callback(text.clone(), data.clone())])
        .collect();

    let mut nav_row = Vec::new();
    if page > 0 {
        nav_row.push(InlineKeyboardButton::callback(
            "⬅️ 上一页",
            format!("page_{action}_{snap_id}_{}", page - 1),
        ));
    }
    if page + 1 < total_pages {
        nav_row.push(InlineKeyboardButton::callback(
            "➡️ 下一页",
            format!("page_{action}_{snap_id}_{}", page + 1),
        ));
    }
    if !nav_row.is_empty() {
        rows.push(nav_row);
    }

    if matches!(action, "youtube" | "concat" | "convert" | "delete") {
        rows.push(vec![InlineKeyboardButton::callback(
            "☑️ 选择本页文件",
            format!("selectpage_{action}_{snap_id}_{page}"),
        )]);
        if selected_count > 0 {
            rows.push(vec![InlineKeyboardButton::callback(
                "清空已选文件",
                format!("clearselection_{action}_{snap_id}"),
            )]);
        }
    }

    if !is_at_base {
        rows.push(vec![InlineKeyboardButton::callback(
            "⬆️ 返回上一级目录",
            format!("updir_{action}_{snap_id}"),
        )]);
    }

    if selected_count > 0 {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("▶️ 确认执行 ({selected_count} 个文件)"),
            format!("execbatch_{action}_{snap_id}"),
        )]);
    }

    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 返回主菜单",
        "menu_main",
    )]);

    InlineKeyboardMarkup::new(rows)
}

/// 构建绑定确认批次令牌 `token` 的二次删除确认键盘。
#[must_use]
pub fn delete_confirmation_keyboard(token: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ 确认永久删除", format!("confirm_del_{token}")),
        InlineKeyboardButton::callback("取消", format!("cancel_del_{token}")),
    ]])
}

/// 构建“返回主菜单”单按钮键盘。
#[must_use]
pub fn back_to_main_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🔙 返回主菜单",
        "menu_main",
    )]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_callback_data_lengths_strictly_within_64_bytes() {
        let snap_id = "0001abcd";
        let kb = file_selector_keyboard(
            &[(
                "video.mp4".into(),
                format!("toggle_youtube_{snap_id}_999_99"),
            )],
            5,
            10,
            "youtube",
            snap_id,
            false,
            3,
        );
        for row in kb.inline_keyboard {
            for btn in row {
                if let teloxide::types::InlineKeyboardButtonKind::CallbackData(data) = btn.kind {
                    assert!(
                        data.len() <= 64,
                        "Callback data exceeded 64 bytes ({}): {data}",
                        data.len()
                    );
                }
            }
        }

        let del_kb = delete_confirmation_keyboard("abcd1234");
        for row in del_kb.inline_keyboard {
            for btn in row {
                if let teloxide::types::InlineKeyboardButtonKind::CallbackData(data) = btn.kind {
                    assert!(data.len() <= 64);
                }
            }
        }
    }
}
