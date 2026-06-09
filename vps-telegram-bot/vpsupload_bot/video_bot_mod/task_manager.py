"""长任务、独占任务、YouTube 上传池与队列持久化管理。"""

import asyncio
import json
import logging
import os
import tempfile
import time
from typing import Any, Awaitable, Callable, Dict, Optional

from telegram import Update
from telegram.ext import ContextTypes

from .config import YOUTUBE_MAX_CONCURRENT_UPLOADS, YOUTUBE_UPLOAD_QUEUE_FILE

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


def _clean_finished_uploads(uploads: Dict[str, Dict[str, Any]]) -> None:
    stale_task_ids = []
    for task_id, info in uploads.items():
        task = info.get("task")
        if isinstance(task, asyncio.Task) and task.done():
            stale_task_ids.append(task_id)

    for task_id in stale_task_ids:
        uploads.pop(task_id, None)


def get_youtube_uploads(context: ContextTypes.DEFAULT_TYPE) -> Dict[str, Dict[str, Any]]:
    """Return active/queued YouTube uploads, including restored global uploads."""
    user_uploads = context.user_data.setdefault("youtube_uploads", {})
    global_uploads = context.bot_data.setdefault("restored_youtube_uploads", {})

    _clean_finished_uploads(user_uploads)
    _clean_finished_uploads(global_uploads)

    if not global_uploads:
        return user_uploads

    combined: Dict[str, Dict[str, Any]] = {}
    combined.update(global_uploads)
    combined.update(user_uploads)
    return combined


def get_youtube_upload_count(context: ContextTypes.DEFAULT_TYPE) -> int:
    return len(get_youtube_uploads(context))


def get_youtube_semaphore(context: ContextTypes.DEFAULT_TYPE) -> asyncio.Semaphore:
    semaphore = context.bot_data.get("youtube_upload_semaphore")
    if semaphore is None:
        semaphore = asyncio.Semaphore(YOUTUBE_MAX_CONCURRENT_UPLOADS)
        context.bot_data["youtube_upload_semaphore"] = semaphore
    return semaphore


def _sanitize_upload_info(info: Dict[str, Any]) -> Dict[str, Any]:
    allowed_keys = {
        "filename",
        "path",
        "chat_id",
        "user_id",
        "created_at",
        "status",
        "progress",
    }
    sanitized = {key: info.get(key) for key in allowed_keys if key in info}
    sanitized["updated_at"] = time.time()
    return sanitized


def load_persisted_youtube_uploads() -> Dict[str, Dict[str, Any]]:
    """Load persisted YouTube upload queue from JSON. Corrupt files are preserved as .bad."""
    if not os.path.exists(YOUTUBE_UPLOAD_QUEUE_FILE):
        return {}

    try:
        with open(YOUTUBE_UPLOAD_QUEUE_FILE, "r", encoding="utf-8") as queue_file:
            data = json.load(queue_file)
        if isinstance(data, dict):
            return {str(task_id): dict(info) for task_id, info in data.items() if isinstance(info, dict)}
    except Exception as exc:
        logger.exception("读取 YouTube 上传队列失败: %s", exc)
        bad_path = f"{YOUTUBE_UPLOAD_QUEUE_FILE}.bad"
        try:
            os.replace(YOUTUBE_UPLOAD_QUEUE_FILE, bad_path)
            logger.warning("已将损坏队列文件移动到: %s", bad_path)
        except OSError:
            pass

    return {}


def save_persisted_youtube_uploads(records: Dict[str, Dict[str, Any]]) -> None:
    """Atomically save persisted YouTube upload queue."""
    queue_dir = os.path.dirname(os.path.abspath(YOUTUBE_UPLOAD_QUEUE_FILE))
    os.makedirs(queue_dir, exist_ok=True)

    fd, temp_path = tempfile.mkstemp(prefix=".youtube_upload_queue_", suffix=".json", dir=queue_dir)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as temp_file:
            json.dump(records, temp_file, ensure_ascii=False, indent=2, sort_keys=True)
        os.replace(temp_path, YOUTUBE_UPLOAD_QUEUE_FILE)
    except Exception:
        try:
            os.remove(temp_path)
        except OSError:
            pass
        raise


def persist_youtube_upload_task(task_id: str, info: Dict[str, Any]) -> None:
    records = load_persisted_youtube_uploads()
    records[task_id] = _sanitize_upload_info(info)
    save_persisted_youtube_uploads(records)


def update_persisted_youtube_upload_task(task_id: str, **updates: Any) -> None:
    records = load_persisted_youtube_uploads()
    if task_id not in records:
        return
    records[task_id].update({key: value for key, value in updates.items() if key not in {"task", "cancel_event"}})
    records[task_id]["updated_at"] = time.time()
    save_persisted_youtube_uploads(records)


def remove_persisted_youtube_upload_task(task_id: str) -> None:
    records = load_persisted_youtube_uploads()
    if task_id in records:
        records.pop(task_id, None)
        save_persisted_youtube_uploads(records)


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
