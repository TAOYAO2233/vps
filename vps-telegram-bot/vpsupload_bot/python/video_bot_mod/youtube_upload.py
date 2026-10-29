"""YouTube 上传命令、上传任务实现与重启恢复。"""

import asyncio
import logging
import os
import time
from datetime import datetime
from types import SimpleNamespace
from typing import Any, Dict, List, Optional

from google.oauth2.credentials import Credentials
from googleapiclient.discovery import build
from googleapiclient.http import MediaFileUpload
from telegram import Update
from telegram.ext import ContextTypes

from .auth import is_admin
from .config import (
    ADMIN_ID,
    TOKEN_FILE,
    YOUTUBE_MAX_CONCURRENT_UPLOADS,
    YOUTUBE_SCOPES,
    YOUTUBE_UPLOAD_CHUNK_MB,
)
from .media_utils import assert_path_inside_base, build_progress_bar, format_elapsed
from .task_manager import (
    get_active_task,
    get_youtube_semaphore,
    get_youtube_uploads,
    load_persisted_youtube_uploads,
    persist_youtube_upload_task,
    remove_persisted_youtube_upload_task,
    update_persisted_youtube_upload_task,
)

logger = logging.getLogger(__name__)


async def cmd_uploads(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update):
        return

    uploads = get_youtube_uploads(context)
    if not uploads:
        await update.message.reply_text("\u2139\ufe0f \u5f53\u524d\u6ca1\u6709 YouTube \u4e0a\u4f20\u4efb\u52a1\u3002")
        return

    lines = [f"\U0001f4e4 **YouTube \u4e0a\u4f20\u4efb\u52a1** ({len(uploads)} \u4e2a)\n"]
    now = time.time()
    for idx, info in enumerate(uploads.values(), start=1):
        filename = str(info.get("filename", "unknown"))
        status = str(info.get("status", "\u6392\u961f\u4e2d"))
        progress = info.get("progress")
        created_at = float(info.get("created_at", now))
        progress_text = ""
        if isinstance(progress, (int, float)):
            progress_text = f" {float(progress):.1f}%"
        lines.append(
            f"{idx}. `{filename}`\n"
            f"   \u72b6\u6001: {status}{progress_text}\n"
            f"   \u5df2\u8fd0\u884c: `{format_elapsed(now - created_at)}`"
        )

    lines.append("\n\u53d1\u9001 /stop \u53ef\u505c\u6b62\u5f53\u524d\u6240\u6709\u4efb\u52a1\u3002")
    await update.message.reply_text("\n".join(lines), parse_mode="Markdown")


async def upload_youtube_file(
    context: ContextTypes.DEFAULT_TYPE,
    message,
    file_path: str,
    task_id: str,
    cancel_event: asyncio.Event,
):
    file_path = assert_path_inside_base(file_path)
    filename = os.path.basename(file_path)

    def set_upload_info(status: str, progress: Optional[float] = None) -> None:
        uploads = context.user_data.get("youtube_uploads", {})
        global_uploads = context.bot_data.get("restored_youtube_uploads", {})
        info = uploads.get(task_id) or global_uploads.get(task_id)
        if not info:
            return
        info["status"] = status
        if progress is not None:
            info["progress"] = progress
        try:
            update_persisted_youtube_upload_task(task_id, status=status, progress=info.get("progress"))
        except Exception as exc:
            logger.warning("更新 YouTube 上传队列记录失败: %s", exc)

    try:
        set_upload_info("\u6392\u961f\u4e2d", 0.0)
        await message.edit_text(
            f"\u23f3 **\u5df2\u52a0\u5165 YouTube \u4e0a\u4f20\u961f\u5217**\n`{filename}`\n"
            f"\u5e76\u53d1\u4e0a\u9650: `{YOUTUBE_MAX_CONCURRENT_UPLOADS}`\n"
            "\u53d1\u9001 /uploads \u67e5\u770b\uff0c/stop \u505c\u6b62\u3002",
            parse_mode="Markdown",
        )

        semaphore = get_youtube_semaphore(context)
        async with semaphore:
            if cancel_event.is_set():
                set_upload_info("\u5df2\u53d6\u6d88", 0.0)
                await message.edit_text(f"\U0001f6d1 **YouTube \u4e0a\u4f20\u5df2\u53d6\u6d88**:\n`{filename}`", parse_mode="Markdown")
                return

            set_upload_info("\u521d\u59cb\u5316", 0.0)
            await message.edit_text(f"\U0001f504 \u521d\u59cb\u5316 YouTube API...\n`{filename}`", parse_mode="Markdown")

            if not os.path.exists(TOKEN_FILE):
                set_upload_info("\u5931\u8d25", 0.0)
                await message.edit_text(f"\u274c \u7f3a\u5c11 `{TOKEN_FILE}`", parse_mode="Markdown")
                return

            creds = Credentials.from_authorized_user_file(TOKEN_FILE, YOUTUBE_SCOPES)
            youtube = build("youtube", "v3", credentials=creds)

            body = {
                "snippet": {"title": os.path.splitext(filename)[0][:95], "description": "", "categoryId": "22"},
                "status": {"privacyStatus": "private", "selfDeclaredMadeForKids": False},
            }

            media = MediaFileUpload(
                file_path,
                chunksize=YOUTUBE_UPLOAD_CHUNK_MB * 1024 * 1024,
                resumable=True,
            )
            request = youtube.videos().insert(part="snippet,status", body=body, media_body=media)

            last_update_time = time.time()
            last_percent = -1.0
            response = None
            loop = asyncio.get_event_loop()
            set_upload_info("\u4e0a\u4f20\u4e2d", 0.0)

            while response is None:
                if cancel_event.is_set():
                    set_upload_info("\u5df2\u53d6\u6d88")
                    await message.edit_text(f"\U0001f6d1 **YouTube \u4e0a\u4f20\u5df2\u624b\u52a8\u7ec8\u6b62**:\n`{filename}`", parse_mode="Markdown")
                    return

                status, chunk_response = await loop.run_in_executor(None, request.next_chunk)

                if chunk_response is not None:
                    response = chunk_response
                    break

                if status:
                    percent = status.progress() * 100
                    set_upload_info("\u4e0a\u4f20\u4e2d", percent)
                    current_time = time.time()
                    if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                        bar = build_progress_bar(percent)
                        try:
                            await message.edit_text(
                                f"\u2601\ufe0f **\u4e0a\u4f20 YouTube** (\u79c1\u4eab):\n`{filename}`\n\n`{bar}`",
                                parse_mode="Markdown",
                            )
                            last_update_time = current_time
                            last_percent = int(percent)
                        except Exception:
                            pass

            upload_time = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
            video_id = response.get("id") if response else ""
            set_upload_info("\u5b8c\u6210", 100.0)
            success_text = (
                f"\u2705 **\u4e0a\u4f20\u6210\u529f\uff01**\n"
                f"\U0001f3ac \u89c6\u9891\u540d\u79f0: `{filename}`\n"
                f"\U0001f552 \u4e0a\u4f20\u65f6\u95f4: `{upload_time}`\n"
                f"\U0001f4fa \u89c2\u770b\u94fe\u63a5: `https://youtu.be/{video_id}`\n"
                f"\U0001f6e0\ufe0f Studio: `https://studio.youtube.com/video/{video_id}/edit`"
            )
            await message.edit_text(success_text, parse_mode="Markdown")
    except asyncio.CancelledError:
        set_upload_info("\u5df2\u53d6\u6d88")
        try:
            await message.edit_text(f"\U0001f6d1 **YouTube \u4e0a\u4f20\u5df2\u53d6\u6d88**:\n`{filename}`", parse_mode="Markdown")
        except Exception:
            pass
        raise
    except Exception as exc:
        set_upload_info("\u5931\u8d25")
        await message.edit_text(f"\u274c \u4e0a\u4f20\u5f02\u5e38:\n`{str(exc)}`", parse_mode="Markdown")
    finally:
        uploads = context.user_data.get("youtube_uploads", {})
        uploads.pop(task_id, None)
        context.bot_data.get("restored_youtube_uploads", {}).pop(task_id, None)
        try:
            remove_persisted_youtube_upload_task(task_id)
        except Exception as exc:
            logger.warning("清理 YouTube 上传队列记录失败: %s", exc)


async def start_youtube_uploads(update: Update, context: ContextTypes.DEFAULT_TYPE, file_paths: List[str]) -> bool:
    query = update.callback_query
    active_task = get_active_task(context)
    if active_task:
        active_name = context.user_data.get("active_task_name", "\u8fd0\u884c\u4e2d\u4efb\u52a1")
        await query.answer(f"\u5df2\u6709\u72ec\u5360\u4efb\u52a1\u6b63\u5728\u8fd0\u884c\uff1a{active_name}\n\u8bf7\u5148 /stop \u6216\u7b49\u5f85\u5b8c\u6210\u540e\u518d\u4e0a\u4f20\u3002", show_alert=True)
        return False

    target_files = []
    for file_path in file_paths:
        safe_path = assert_path_inside_base(file_path)
        if os.path.isfile(safe_path):
            target_files.append(safe_path)

    if not target_files:
        await query.answer("\u274c \u6ca1\u6709\u53ef\u4e0a\u4f20\u7684\u89c6\u9891\u6587\u4ef6\u3002", show_alert=True)
        return False

    if not os.path.exists(TOKEN_FILE):
        await query.answer(f"\u274c \u7f3a\u5c11 {TOKEN_FILE}\uff0c\u65e0\u6cd5\u4e0a\u4f20 YouTube\u3002", show_alert=True)
        return False

    await query.answer()
    uploads = context.user_data.setdefault("youtube_uploads", {})
    created = []
    now_ms = int(time.time() * 1000)

    for idx, file_path in enumerate(target_files):
        filename = os.path.basename(file_path)
        task_id = f"yt_{now_ms}_{idx}_{abs(hash(file_path)) % 100000}"
        cancel_event = asyncio.Event()
        progress_message = await query.message.reply_text(
            f"\u23f3 \u521b\u5efa YouTube \u4e0a\u4f20\u4efb\u52a1:\n`{filename}`",
            parse_mode="Markdown",
        )
        info: Dict[str, Any] = {
            "filename": filename,
            "path": file_path,
            "chat_id": update.effective_chat.id if update.effective_chat else ADMIN_ID,
            "user_id": update.effective_user.id if update.effective_user else ADMIN_ID,
            "cancel_event": cancel_event,
            "created_at": time.time(),
            "status": "\u6392\u961f\u4e2d",
            "progress": 0.0,
            "task": None,
        }
        uploads[task_id] = info
        try:
            persist_youtube_upload_task(task_id, info)
        except Exception as exc:
            logger.warning("写入 YouTube 上传队列记录失败: %s", exc)

        task = asyncio.create_task(
            upload_youtube_file(context, progress_message, file_path, task_id, cancel_event),
            name=f"youtube:{filename}",
        )
        info["task"] = task
        created.append(filename)

    context.user_data["selected_youtube"] = set()

    try:
        await query.edit_message_text(
            f"\u2705 \u5df2\u542f\u52a8 `{len(created)}` \u4e2a YouTube \u4e0a\u4f20\u4efb\u52a1\u3002\n"
            f"\u5e76\u53d1\u4e0a\u9650: `{YOUTUBE_MAX_CONCURRENT_UPLOADS}`\n"
            "\u53d1\u9001 /uploads \u67e5\u770b\u4efb\u52a1\uff0c/stop \u505c\u6b62\u6240\u6709\u4efb\u52a1\u3002",
            parse_mode="Markdown",
        )
    except Exception:
        pass

    return True


async def restore_persisted_youtube_uploads(application) -> None:
    """Restore unfinished YouTube upload tasks after Bot restart."""
    records = load_persisted_youtube_uploads()
    if not records:
        return

    restored_uploads = application.bot_data.setdefault("restored_youtube_uploads", {})
    logger.info("发现 %s 个待恢复 YouTube 上传任务", len(records))

    for old_task_id, record in list(records.items()):
        file_path = str(record.get("path", ""))
        try:
            file_path = assert_path_inside_base(file_path)
        except Exception as exc:
            logger.warning("跳过非法恢复路径: task=%s path=%s error=%s", old_task_id, file_path, exc)
            remove_persisted_youtube_upload_task(old_task_id)
            continue

        if not os.path.isfile(file_path):
            logger.warning("跳过不存在的恢复文件: task=%s path=%s", old_task_id, file_path)
            remove_persisted_youtube_upload_task(old_task_id)
            continue

        chat_id = int(record.get("chat_id") or record.get("user_id") or ADMIN_ID)
        filename = os.path.basename(file_path)
        task_id = f"restored_{int(time.time() * 1000)}_{abs(hash(file_path)) % 100000}"
        cancel_event = asyncio.Event()

        try:
            message = await application.bot.send_message(
                chat_id=chat_id,
                text=(
                    "\u267b\ufe0f **\u68c0\u6d4b\u5230\u670d\u52a1\u91cd\u542f\uff0c\u6b63\u5728\u6062\u590d YouTube \u4e0a\u4f20\u4efb\u52a1**\n"
                    f"`{filename}`"
                ),
                parse_mode="Markdown",
            )
        except Exception as exc:
            logger.warning("发送恢复提示失败，跳过任务: task=%s chat_id=%s error=%s", old_task_id, chat_id, exc)
            continue

        info: Dict[str, Any] = {
            "filename": filename,
            "path": file_path,
            "chat_id": chat_id,
            "user_id": int(record.get("user_id") or ADMIN_ID),
            "cancel_event": cancel_event,
            "created_at": float(record.get("created_at") or time.time()),
            "status": "\u6062\u590d\u4e2d",
            "progress": float(record.get("progress") or 0.0),
            "task": None,
        }
        restored_uploads[task_id] = info
        remove_persisted_youtube_upload_task(old_task_id)
        persist_youtube_upload_task(task_id, info)

        fake_context = SimpleNamespace(user_data={"youtube_uploads": {}}, bot_data=application.bot_data)
        task = asyncio.create_task(
            upload_youtube_file(fake_context, message, file_path, task_id, cancel_event),
            name=f"youtube:restore:{filename}",
        )
        info["task"] = task
