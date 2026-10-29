"""Telegram 命令与 CallbackQuery 路由。"""

import asyncio
import logging
import os

from telegram import Update
from telegram.ext import ContextTypes

from .actions import action_browse, action_concat, action_convert, action_delete, action_stream
from .auth import is_admin
from .config import BASE_DIR
from .media_utils import assert_path_inside_base, safe_join
from .task_manager import get_active_task, get_youtube_upload_count, get_youtube_uploads, start_long_task
from .ui import render_delete_confirmation, render_file_selector, render_main_menu
from .youtube_upload import start_youtube_uploads

logger = logging.getLogger(__name__)

async def cmd_stop(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update):
        return

    active_task = get_active_task(context)
    process = context.user_data.get("current_process")
    uploads = get_youtube_uploads(context)

    if not active_task and not process and not uploads:
        await update.message.reply_text("\u2139\ufe0f \u5f53\u524d\u6ca1\u6709\u6b63\u5728\u8fd0\u884c\u7684\u4efb\u52a1\u3002")
        return

    context.user_data["cancel_flag"] = True

    if process and process.returncode is None:
        try:
            process.terminate()
        except ProcessLookupError:
            pass
        except Exception as exc:
            logger.warning("Failed to terminate process: %s", exc)

    for info in list(uploads.values()):
        cancel_event = info.get("cancel_event")
        if isinstance(cancel_event, asyncio.Event):
            cancel_event.set()

    parts = []
    if active_task or process:
        parts.append("\u6b63\u5728\u4e2d\u65ad\u5f53\u524d\u72ec\u5360\u4efb\u52a1")
    if uploads:
        parts.append(f"\u5df2\u6807\u8bb0 {len(uploads)} \u4e2a YouTube \u4e0a\u4f20\u4efb\u52a1\u4e3a\u53d6\u6d88")

    await update.message.reply_text(
        "\U0001f6d1 **\u5df2\u63a5\u6536\u505c\u6b62\u6307\u4ee4\uff01**\n" + "\n".join(parts) + "\n\nYouTube \u4e0a\u4f20\u4f1a\u5728\u5f53\u524d chunk \u8fd4\u56de\u540e\u505c\u6b62\u3002",
        parse_mode="Markdown",
    )

async def callback_router(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update):
        return

    query = update.callback_query
    data = query.data

    if data == "menu_main":
        await render_main_menu(update, context)

    elif data == "cancel_delete":
        context.user_data.pop("pending_delete_files", None)
        await query.answer("已取消删除。")
        await render_file_selector(update, context, "delete", 0)

    elif data == "confirm_delete":
        pending_files = context.user_data.get("pending_delete_files", [])
        if not pending_files:
            await query.answer("没有待删除文件，请重新选择。", show_alert=True)
            return

        await start_long_task(
            update,
            context,
            "删除文件",
            lambda: action_delete(update, context, pending_files),
        )

    elif data.startswith("init_"):
        action = data.split("_")[1]
        context.user_data["current_dir"] = BASE_DIR
        context.user_data[f"selected_{action}"] = set()
        context.user_data.pop("pending_delete_files", None)
        await render_file_selector(update, context, action, 0)

    elif data.startswith("menu_"):
        _, action, page = data.split("_")
        await render_file_selector(update, context, action, int(page))

    elif data.startswith("enterdir_"):
        _, action, idx = data.split("_")
        item_name = context.user_data["current_files"][int(idx)]
        current_dir = context.user_data.get("current_dir", BASE_DIR)
        context.user_data["current_dir"] = safe_join(current_dir, item_name)
        context.user_data[f"selected_{action}"] = set()
        context.user_data.pop("pending_delete_files", None)
        await render_file_selector(update, context, action, 0)

    elif data.startswith("updir_"):
        _, action = data.split("_")
        current_dir = assert_path_inside_base(context.user_data.get("current_dir", BASE_DIR))
        if os.path.realpath(current_dir) != os.path.realpath(BASE_DIR):
            context.user_data["current_dir"] = assert_path_inside_base(os.path.dirname(current_dir))
        context.user_data[f"selected_{action}"] = set()
        context.user_data.pop("pending_delete_files", None)
        await render_file_selector(update, context, action, 0)

    elif data.startswith("toggle_"):
        _, action, idx, page = data.split("_")
        idx = int(idx)
        selected = context.user_data.get(f"selected_{action}", set())
        if idx in selected:
            selected.remove(idx)
        else:
            selected.add(idx)
        context.user_data[f"selected_{action}"] = selected
        context.user_data.pop("pending_delete_files", None)
        await render_file_selector(update, context, action, int(page))

    elif data.startswith("execsingle_"):
        _, action, idx = data.split("_")
        current_dir = context.user_data.get("current_dir", BASE_DIR)
        filename = context.user_data["current_files"][int(idx)]
        filepath = safe_join(current_dir, filename)

        if action == "browse":
            await action_browse(update, context, filepath)
        elif action == "stream":
            await start_long_task(update, context, "RTMP 推流", lambda: action_stream(update, context, filepath))
        elif action == "youtube":
            await start_youtube_uploads(update, context, [filepath])

    elif data.startswith("execbatch_"):
        _, action = data.split("_")
        selected_indices = context.user_data.get(f"selected_{action}", set())
        if not selected_indices:
            await query.answer("❌ 请先选择至少一个文件！", show_alert=True)
            return

        current_dir = context.user_data.get("current_dir", BASE_DIR)
        cached_files = context.user_data["current_files"]
        target_files = [safe_join(current_dir, cached_files[i]) for i in sorted(selected_indices)]
        target_files = [path for path in target_files if os.path.isfile(path)]

        if not target_files:
            await query.answer("❌ 没有可执行的文件，请重新选择。", show_alert=True)
            return

        if action == "delete":
            if get_active_task(context):
                active_name = context.user_data.get("active_task_name", "运行中任务")
                await query.answer(f"已有任务正在运行：{active_name}\n请先发送 /stop 或等待完成。", show_alert=True)
                return
            upload_count = get_youtube_upload_count(context)
            if upload_count:
                await query.answer(f"当前有 {upload_count} 个 YouTube 上传任务，请先 /stop 或等待完成后再删除。", show_alert=True)
                return
            await query.answer()
            await render_delete_confirmation(update, context, target_files)
        elif action == "youtube":
            await start_youtube_uploads(update, context, target_files)
        elif action == "concat":
            await start_long_task(update, context, "视频合并", lambda: action_concat(update, context, target_files))
        elif action == "convert":
            await start_long_task(update, context, "批量转码", lambda: action_convert(update, context, target_files))
