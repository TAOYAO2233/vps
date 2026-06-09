"""长任务、独占任务与 YouTube 上传池管理。"""

import asyncio
import logging
from typing import Any, Awaitable, Callable, Dict, Optional

from telegram import Update
from telegram.ext import ContextTypes

from .config import YOUTUBE_MAX_CONCURRENT_UPLOADS

logger = logging.getLogger(__name__)

def get_active_task(context: ContextTypes.DEFAULT_TYPE) -> Optional[asyncio.Task]:
    """Exclusive task: stream/concat/convert/delete. YouTube uploads use a separate pool."""
    task = context.user_data.get("active_task")
    if task and not task.done():
        return task

    if task and task.done():
        context.user_data.pop("active_task", None)
        context.user_data.pop("active_task_name", None)

    return None

def get_youtube_uploads(context: ContextTypes.DEFAULT_TYPE) -> Dict[str, Dict[str, Any]]:
    """Return active/queued YouTube upload tasks and clean finished entries."""
    uploads = context.user_data.setdefault("youtube_uploads", {})
    stale_task_ids = []
    for task_id, info in uploads.items():
        task = info.get("task")
        if isinstance(task, asyncio.Task) and task.done():
            stale_task_ids.append(task_id)

    for task_id in stale_task_ids:
        uploads.pop(task_id, None)

    return uploads

def get_youtube_upload_count(context: ContextTypes.DEFAULT_TYPE) -> int:
    return len(get_youtube_uploads(context))

def get_youtube_semaphore(context: ContextTypes.DEFAULT_TYPE) -> asyncio.Semaphore:
    semaphore = context.bot_data.get("youtube_upload_semaphore")
    if semaphore is None:
        semaphore = asyncio.Semaphore(YOUTUBE_MAX_CONCURRENT_UPLOADS)
        context.bot_data["youtube_upload_semaphore"] = semaphore
    return semaphore

async def start_long_task(
    update: Update,
    context: ContextTypes.DEFAULT_TYPE,
    task_name: str,
    task_factory: Callable[[], Awaitable[None]],
) -> bool:
    """Only one exclusive task may run. YouTube uploads are concurrent but block exclusive tasks."""
    query = update.callback_query
    active_task = get_active_task(context)
    if active_task:
        active_name = context.user_data.get("active_task_name", "\u8fd0\u884c\u4e2d\u4efb\u52a1")
        await query.answer(f"\u5df2\u6709\u4efb\u52a1\u6b63\u5728\u8fd0\u884c\uff1a{active_name}\n\u8bf7\u5148\u53d1\u9001 /stop \u6216\u7b49\u5f85\u5b8c\u6210\u3002", show_alert=True)
        return False

    upload_count = get_youtube_upload_count(context)
    if upload_count:
        await query.answer(
            f"\u5f53\u524d\u6709 {upload_count} \u4e2a YouTube \u4e0a\u4f20\u4efb\u52a1\u5728\u8fd0\u884c/\u6392\u961f\u3002\n"
            "\u4e3a\u907f\u514d\u8fb9\u4e0a\u4f20\u8fb9\u5220\u9664/\u8f6c\u7801/\u5408\u5e76\u5bfc\u81f4\u6587\u4ef6\u51b2\u7a81\uff0c\u8bf7\u5148 /stop \u6216\u7b49\u4e0a\u4f20\u5b8c\u6210\u3002",
            show_alert=True,
        )
        return False

    await query.answer()
    context.user_data["cancel_flag"] = False
    context.user_data["active_task_name"] = task_name

    async def runner() -> None:
        try:
            await task_factory()
        except asyncio.CancelledError:
            logger.info("Exclusive task cancelled: %s", task_name)
            raise
        except Exception as exc:
            logger.exception("Exclusive task failed: %s", task_name)
            try:
                await query.message.reply_text(f"\u274c **\u4efb\u52a1\u5f02\u5e38**: `{str(exc)}`", parse_mode="Markdown")
            except Exception:
                pass
        finally:
            context.user_data["current_process"] = None
            context.user_data["active_task"] = None
            context.user_data["active_task_name"] = None
            context.user_data["cancel_flag"] = False

    task = asyncio.create_task(runner(), name=task_name)
    context.user_data["active_task"] = task
    return True
