"""Telegram 菜单、分页与确认界面。"""

import logging
import math
import os
from typing import List

from telegram import InlineKeyboardButton, InlineKeyboardMarkup, Update
from telegram.ext import ContextTypes

from .auth import is_admin
from .config import BASE_DIR, ITEMS_PER_PAGE, VIDEO_EXTENSIONS
from .media_utils import assert_path_inside_base, get_formatted_file_size, safe_join
from .task_manager import get_active_task, get_youtube_upload_count

logger = logging.getLogger(__name__)

async def render_main_menu(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update):
        return

    keyboard = [
        [InlineKeyboardButton("📂 浏览远程文件", callback_data="init_browse")],
        [
            InlineKeyboardButton("📡 RTMP 单路推流", callback_data="init_stream"),
            InlineKeyboardButton("☁️ YouTube 上传", callback_data="init_youtube"),
        ],
        [InlineKeyboardButton("✂️ 智能视频合并", callback_data="init_concat")],
        [InlineKeyboardButton("🔄 批量转码 MP4", callback_data="init_convert")],
        [InlineKeyboardButton("🗑️ 批量删除文件", callback_data="init_delete")],
    ]

    active_name = context.user_data.get("active_task_name") if get_active_task(context) else None
    busy_text = f"\n🔒 当前独占任务: `{active_name}`" if active_name else ""
    upload_count = get_youtube_upload_count(context)
    upload_text = f"\n☁️ YouTube 上传/排队: `{upload_count}`" if upload_count else ""
    text = (
        f"=== 🎬 VPS 多媒体主控面板 ===\n"
        f"根目录: `{BASE_DIR}`\n"
        f"💡 提示: /uploads 查看上传，/stop 中断运行任务"
        f"{busy_text}"
        f"{upload_text}"
    )

    if update.callback_query:
        await update.callback_query.edit_message_text(text, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode="Markdown")
    else:
        await update.message.reply_text(text, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode="Markdown")

async def render_file_selector(update: Update, context: ContextTypes.DEFAULT_TYPE, action_type: str, page: int):
    query = update.callback_query

    current_dir = context.user_data.get("current_dir", BASE_DIR)
    try:
        current_dir = assert_path_inside_base(current_dir)
    except ValueError:
        current_dir = BASE_DIR
        context.user_data["current_dir"] = BASE_DIR

    if not os.path.exists(current_dir):
        await query.edit_message_text("❌ 目录不存在！")
        return

    all_items = os.listdir(current_dir)
    dirs = []
    files = []
    for item in all_items:
        try:
            item_path = safe_join(current_dir, item)
        except ValueError:
            logger.warning("跳过越界路径: %s", item)
            continue

        if os.path.isdir(item_path):
            dirs.append(item)
        elif os.path.isfile(item_path) and item.lower().endswith(VIDEO_EXTENSIONS):
            files.append(item)

    dirs = sorted(dirs)
    files = sorted(files)

    items = dirs + files
    context.user_data["current_files"] = items

    is_multi_select = action_type in ["youtube", "concat", "convert", "delete"]
    selected_indices = context.user_data.setdefault(f"selected_{action_type}", set())

    total_pages = max(1, math.ceil(len(items) / ITEMS_PER_PAGE))
    page = max(0, min(page, total_pages - 1))
    start_idx = page * ITEMS_PER_PAGE
    current_page_items = items[start_idx:start_idx + ITEMS_PER_PAGE]

    keyboard = []
    for i, item_name in enumerate(current_page_items):
        real_idx = start_idx + i
        item_path = safe_join(current_dir, item_name)

        if os.path.isdir(item_path):
            btn_text = f"📁 {item_name}"
            callback_data = f"enterdir_{action_type}_{real_idx}"
        else:
            size_str = get_formatted_file_size(item_path)
            if is_multi_select:
                checkbox = "✅ " if real_idx in selected_indices else "⬜️ "
                btn_text = f"{checkbox}[{size_str}] {item_name}"
                callback_data = f"toggle_{action_type}_{real_idx}_{page}"
            else:
                btn_text = f"[{size_str}] {item_name}"
                callback_data = f"execsingle_{action_type}_{real_idx}"

        keyboard.append([InlineKeyboardButton(btn_text, callback_data=callback_data)])

    nav_buttons = []
    if page > 0:
        nav_buttons.append(InlineKeyboardButton("⬅️ 上一页", callback_data=f"menu_{action_type}_{page - 1}"))
    if page < total_pages - 1:
        nav_buttons.append(InlineKeyboardButton("➡️ 下一页", callback_data=f"menu_{action_type}_{page + 1}"))
    if nav_buttons:
        keyboard.append(nav_buttons)

    if os.path.realpath(current_dir) != os.path.realpath(BASE_DIR):
        keyboard.append([InlineKeyboardButton("⬆️ 返回上一级目录", callback_data=f"updir_{action_type}")])

    if is_multi_select and selected_indices:
        keyboard.append([InlineKeyboardButton(f"▶️ 确认执行 ({len(selected_indices)} 个文件)", callback_data=f"execbatch_{action_type}")])

    keyboard.append([InlineKeyboardButton("🔙 返回主菜单", callback_data="menu_main")])

    action_name_map = {
        "browse": "浏览与查看详情",
        "stream": "推流",
        "youtube": "上传 YT",
        "concat": "合并",
        "convert": "转码 MP4",
        "delete": "删除",
    }

    rel_path = os.path.relpath(current_dir, BASE_DIR)
    display_path = "🏠" if rel_path == "." else f"🏠/{rel_path}"
    header = f"📂 路径: `{display_path}`\n👉 模式: [{action_name_map.get(action_type, action_type.upper())}] (页 {page + 1}/{total_pages})"

    if get_active_task(context):
        header += f"\n🔒 独占任务: `{context.user_data.get('active_task_name', '任务')}`"

    upload_count = get_youtube_upload_count(context)
    if upload_count:
        header += f"\n☁️ YouTube 上传/排队: `{upload_count}`"

    if not items:
        header += "\n\n⚠️ 当前目录下既无子文件夹也无视频文件。"

    await query.edit_message_text(header, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode="Markdown")

async def render_delete_confirmation(update: Update, context: ContextTypes.DEFAULT_TYPE, target_files: List[str]):
    query = update.callback_query
    context.user_data["pending_delete_files"] = target_files

    preview_count = 12
    preview_lines = [f"• `{os.path.basename(path)}`" for path in target_files[:preview_count]]
    if len(target_files) > preview_count:
        preview_lines.append(f"... 其余 {len(target_files) - preview_count} 个文件未展开")

    keyboard = [
        [
            InlineKeyboardButton("✅ 确认删除", callback_data="confirm_delete"),
            InlineKeyboardButton("取消", callback_data="cancel_delete"),
        ]
    ]
    await query.edit_message_text(
        "⚠️ **二次确认：即将永久删除以下文件**\n\n"
        + "\n".join(preview_lines)
        + "\n\n删除后不可恢复。",
        reply_markup=InlineKeyboardMarkup(keyboard),
        parse_mode="Markdown",
    )
