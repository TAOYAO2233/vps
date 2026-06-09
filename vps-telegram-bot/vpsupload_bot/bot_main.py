# 更新日志：
# 2024-06-01 v2.0.0
# 2024-xx-xx v2.4.0 优化合并逻辑(TS容错) + 修复智能命名保留完整时分秒
# 2026-06-08 v2.5.0 .env 配置 + 任务锁 + 删除二次确认 + 输出避让命名 + 合并时长/体积双校验
# 2026-06-09 v2.5.1 YouTube 上传池：支持多个视频并发上传，独占任务仍互斥
import os
import re
import math
import time
import asyncio
import logging
from datetime import datetime
from typing import Any, Awaitable, Callable, Dict, List, Optional, Tuple

from telegram import Update, InlineKeyboardButton, InlineKeyboardMarkup
from telegram.ext import Application, CommandHandler, CallbackQueryHandler, ContextTypes

from googleapiclient.discovery import build
from googleapiclient.http import MediaFileUpload
from google.oauth2.credentials import Credentials

# ================= 配置区域 =================
APP_DIR = os.path.dirname(os.path.abspath(__file__))


def load_env_file(env_path: str) -> None:
    """轻量读取 .env，避免额外依赖 python-dotenv。系统环境变量优先级更高。"""
    if not os.path.exists(env_path):
        return

    with open(env_path, "r", encoding="utf-8") as env_file:
        for raw_line in env_file:
            line = raw_line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue

            key, value = line.split("=", 1)
            key = key.strip()
            value = value.strip()

            if not key or key in os.environ:
                continue

            if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
                value = value[1:-1]

            os.environ[key] = value


ENV_FILE = os.getenv("ENV_FILE", os.path.join(APP_DIR, ".env"))
load_env_file(ENV_FILE)


def get_int_env(name: str, default: int) -> int:
    raw_value = os.getenv(name, str(default)).strip()
    try:
        return int(raw_value)
    except ValueError as exc:
        raise RuntimeError(f"环境变量 {name} 必须是整数，当前值: {raw_value!r}") from exc


def get_float_env(name: str, default: float) -> float:
    raw_value = os.getenv(name, str(default)).strip()
    try:
        return float(raw_value)
    except ValueError as exc:
        raise RuntimeError(f"环境变量 {name} 必须是数字，当前值: {raw_value!r}") from exc


BOT_TOKEN = os.getenv("BOT_TOKEN", "").strip()
ADMIN_ID = get_int_env("ADMIN_ID", 0)
BASE_DIR = os.path.abspath(os.getenv("BASE_DIR", "/storage512/bilivego/download").strip())
RTMP_URL = os.getenv("RTMP_URL", "").strip()

ITEMS_PER_PAGE = get_int_env("ITEMS_PER_PAGE", 8)
VIDEO_EXTENSIONS = (".mp4", ".mkv", ".flv", ".ts")

# YouTube OAuth 配置
YOUTUBE_SCOPES = ["https://www.googleapis.com/auth/youtube.upload"]
TOKEN_FILE = os.getenv("TOKEN_FILE", os.path.join(APP_DIR, "token.json")).strip()
YOUTUBE_MAX_CONCURRENT_UPLOADS = get_int_env("YOUTUBE_MAX_CONCURRENT_UPLOADS", 2)
YOUTUBE_UPLOAD_CHUNK_MB = get_int_env("YOUTUBE_UPLOAD_CHUNK_MB", 10)

# 合并结果校验阈值：时长为主，大小为辅。大小阈值故意低于旧版 70%，避免因容器/码率差异误判。
MERGE_MIN_DURATION_RATIO = get_float_env("MERGE_MIN_DURATION_RATIO", 0.95)
MERGE_MIN_SIZE_RATIO = get_float_env("MERGE_MIN_SIZE_RATIO", 0.30)

logging.basicConfig(level=logging.INFO, format="%(asctime)s - %(levelname)s - %(message)s")
logger = logging.getLogger(__name__)
# ===========================================

# --- 核心工具函数 ---


def validate_config() -> None:
    missing = []
    if not BOT_TOKEN:
        missing.append("BOT_TOKEN")
    if not ADMIN_ID:
        missing.append("ADMIN_ID")

    if missing:
        names = ", ".join(missing)
        raise RuntimeError(
            f"缺少必要配置: {names}。请在 {ENV_FILE} 或 systemd EnvironmentFile 中配置。"
        )

    if ITEMS_PER_PAGE <= 0:
        raise RuntimeError("ITEMS_PER_PAGE 必须大于 0")

    if YOUTUBE_MAX_CONCURRENT_UPLOADS <= 0:
        raise RuntimeError("YOUTUBE_MAX_CONCURRENT_UPLOADS 必须大于 0")

    if YOUTUBE_UPLOAD_CHUNK_MB <= 0:
        raise RuntimeError("YOUTUBE_UPLOAD_CHUNK_MB 必须大于 0")

    if not os.path.isdir(BASE_DIR):
        logger.warning("BASE_DIR 目录不存在或不可访问: %s", BASE_DIR)


def is_admin(update: Update) -> bool:
    return bool(update.effective_user and update.effective_user.id == ADMIN_ID)


def assert_path_inside_base(path: str) -> str:
    """确保所有文件操作都限制在 BASE_DIR 内，避免状态异常导致路径越界。"""
    base = os.path.realpath(BASE_DIR)
    target = os.path.realpath(path)
    if target == base or target.startswith(base + os.sep):
        return target
    raise ValueError(f"非法路径，已超出 BASE_DIR: {target}")


def safe_join(parent: str, child: str) -> str:
    return assert_path_inside_base(os.path.join(parent, child))


def remove_if_exists(path: str) -> None:
    try:
        if path and os.path.exists(path):
            os.remove(path)
    except OSError as exc:
        logger.warning("清理文件失败: %s, %s", path, exc)


def unique_path(path: str) -> str:
    """如果目标文件已存在，自动生成 xxx_1.ext、xxx_2.ext，避免 ffmpeg 覆盖旧文件。"""
    path = os.path.abspath(path)
    if not os.path.exists(path):
        return path

    root, ext = os.path.splitext(path)
    index = 1
    while True:
        candidate = f"{root}_{index}{ext}"
        if not os.path.exists(candidate):
            return candidate
        index += 1


def get_formatted_file_size(filepath: str) -> str:
    """智能获取文件大小：小于 1GB 显示 MB，否则显示 GB。"""
    try:
        size_bytes = os.path.getsize(filepath)
        size_mb = size_bytes / (1024 ** 2)
        if size_mb >= 1024:
            size_gb = size_bytes / (1024 ** 3)
            return f"{size_gb:.2f}GB"
        return f"{size_mb:.2f}MB"
    except OSError:
        return "0.00MB"


def format_duration(seconds: float) -> str:
    if seconds <= 0:
        return "未知"
    seconds_int = int(seconds)
    h = seconds_int // 3600
    m = (seconds_int % 3600) // 60
    s = seconds_int % 60
    return f"{h:02d}:{m:02d}:{s:02d}"


async def get_video_duration(filepath: str) -> float:
    cmd = [
        "ffprobe",
        "-v", "error",
        "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1",
        filepath,
    ]
    process = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    stdout, _ = await process.communicate()
    try:
        return float(stdout.decode().strip())
    except ValueError:
        return 0.0


def build_progress_bar(percent: float, length: int = 20) -> str:
    percent = max(0.0, min(100.0, percent))
    filled = int(math.floor((percent / 100.0) * length))
    bar = "█" * filled + "░" * (length - filled)
    return f"[{bar}] {percent:5.1f}%"


def smart_rename(first_file_path: str) -> str:
    base_name = os.path.splitext(os.path.basename(first_file_path))[0]
    ext = os.path.splitext(first_file_path)[1]

    # 增强正则：匹配日期(YYYY-MM-DD) 以及其后的时间(HH-MM-SS)，保留完整的录播时间标签
    date_match = re.search(r"\d{4}[-_.]\d{2}[-_.]\d{2}(?:[ _-]\d{2}[-_.:]\d{2}[-_.:]\d{2})?", base_name)
    date_str = date_match.group(0) if date_match else f"Merged_{datetime.now().strftime('%Y%m%d_%H%M%S')}"

    # 清理掉开头的日期时间括号 [2026-06-07 20-00-00]，保留后面的标题
    title_part = re.sub(r"^\[\d[^\]]*\]\s*", "", base_name)

    if title_part == base_name:
        title_part = base_name.replace(date_str, "")
        title_part = re.sub(r"^[-_.\s]+", "", title_part)

    if title_part:
        output_name = f"{date_str}_{title_part}{ext}"
    else:
        output_name = f"{date_str}_merged{ext}"

    return output_name.replace("__", "_")


def write_concat_list(list_file_path: str, file_paths: List[str]) -> None:
    """写入 ffmpeg concat list，并转义单引号和反斜杠。"""
    with open(list_file_path, "w", encoding="utf-8") as list_file:
        for file_path in file_paths:
            escaped = file_path.replace("\\", "\\\\").replace("'", "\\'")
            list_file.write(f"file '{escaped}'\n")


async def validate_merged_file(
    output_path: str,
    returncode: Optional[int],
    input_total_duration: float,
    input_total_size: int,
) -> Tuple[bool, Dict[str, float]]:
    """合并成功判断：ffmpeg 退出码 + 输出存在 + 时长达标 + 大小辅助校验。"""
    output_size = os.path.getsize(output_path) if os.path.exists(output_path) else 0
    output_duration = await get_video_duration(output_path) if output_size > 0 else 0.0

    duration_ok = True
    if input_total_duration > 0:
        duration_ok = output_duration >= input_total_duration * MERGE_MIN_DURATION_RATIO

    size_ok = True
    if input_total_size > 0:
        size_ok = output_size >= input_total_size * MERGE_MIN_SIZE_RATIO

    is_success = bool(returncode == 0 and output_size > 0 and duration_ok and size_ok)
    details = {
        "input_duration": input_total_duration,
        "output_duration": output_duration,
        "input_size": float(input_total_size),
        "output_size": float(output_size),
        "duration_ratio": (output_duration / input_total_duration) if input_total_duration > 0 else 0.0,
        "size_ratio": (output_size / input_total_size) if input_total_size > 0 else 0.0,
        "duration_ok": 1.0 if duration_ok else 0.0,
        "size_ok": 1.0 if size_ok else 0.0,
    }
    return is_success, details


def format_merge_check(details: Dict[str, float]) -> str:
    input_duration = details.get("input_duration", 0.0)
    output_duration = details.get("output_duration", 0.0)
    duration_ratio = details.get("duration_ratio", 0.0) * 100
    size_ratio = details.get("size_ratio", 0.0) * 100
    output_size = int(details.get("output_size", 0.0))

    return (
        f"⏱️ 输入总时长: `{format_duration(input_duration)}`\n"
        f"⏱️ 输出时长: `{format_duration(output_duration)}` ({duration_ratio:.1f}%)\n"
        f"📦 输出大小: `{output_size / (1024 ** 2):.2f}MB` ({size_ratio:.1f}%)"
    )


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


def format_elapsed(seconds: float) -> str:
    seconds_int = max(0, int(seconds))
    h = seconds_int // 3600
    m = (seconds_int % 3600) // 60
    s = seconds_int % 60
    if h:
        return f"{h}h{m:02d}m{s:02d}s"
    if m:
        return f"{m}m{s:02d}s"
    return f"{s}s"


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


# --- 核心系统指令：强制停止 ---


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


# --- 业务逻辑层 (Actions) ---


async def action_browse(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    query = update.callback_query
    try:
        file_path = assert_path_inside_base(file_path)
        size_str = get_formatted_file_size(file_path)
        duration = await get_video_duration(file_path)
        mtime = os.path.getmtime(file_path)
        mtime_str = datetime.fromtimestamp(mtime).strftime("%Y-%m-%d %H:%M:%S")

        dur_str = format_duration(duration) if duration > 0 else "未知或无损流"
        filename = os.path.basename(file_path)
        info_text = (
            f"📄 {filename}\n"
            f"━━━━━━━━━━━━\n"
            f"📏 大小: {size_str}\n"
            f"⏱️ 时长: {dur_str}\n"
            f"🕒 修改时间: {mtime_str}"
        )
        await query.answer(info_text, show_alert=True)
    except Exception as exc:
        await query.answer(f"❌ 获取文件信息失败: {exc}", show_alert=True)


async def action_stream(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    query = update.callback_query
    context.user_data["cancel_flag"] = False

    file_path = assert_path_inside_base(file_path)
    filename = os.path.basename(file_path)
    size_str = get_formatted_file_size(file_path)

    if not RTMP_URL:
        await query.edit_message_text("❌ RTMP_URL 未配置。请在 `.env` 中添加 `RTMP_URL=你的推流地址`。", parse_mode="Markdown")
        return

    message = await query.edit_message_text(
        f"⏳ 正在分析推流文件: `{filename}` ({size_str})...",
        parse_mode="Markdown",
    )

    duration = await get_video_duration(file_path)
    if context.user_data.get("cancel_flag"):
        await message.edit_text(f"🛑 **推流已手动终止**:\n`{filename}`", parse_mode="Markdown")
        return

    if duration <= 0:
        await message.edit_text("❌ 无法获取视频时长，推流终止。")
        return

    cmd = ["ffmpeg", "-re", "-i", file_path, "-c", "copy", "-f", "flv", RTMP_URL]
    process = await asyncio.create_subprocess_exec(*cmd, stderr=asyncio.subprocess.PIPE)
    context.user_data["current_process"] = process

    time_regex = re.compile(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}")
    last_update_time = time.time()
    last_percent = -1.0

    try:
        while True:
            if context.user_data.get("cancel_flag"):
                if process.returncode is None:
                    process.terminate()
                break

            line = await process.stderr.readline()
            if not line:
                break

            match = time_regex.search(line.decode("utf-8", errors="ignore"))
            if match:
                h, m, s = map(int, match.groups())
                current_sec = h * 3600 + m * 60 + s
                percent = (current_sec / duration) * 100
                current_time = time.time()
                if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                    bar = build_progress_bar(percent)
                    try:
                        await message.edit_text(
                            f"📡 **推流中**: `{filename}`\n\n`{bar}`\n⏱️ {current_sec}s / {int(duration)}s",
                            parse_mode="Markdown",
                        )
                        last_update_time = current_time
                        last_percent = int(percent)
                    except Exception:
                        pass

        await process.wait()
    finally:
        context.user_data["current_process"] = None

    if context.user_data.get("cancel_flag"):
        await message.edit_text(f"🛑 **推流已手动终止**:\n`{filename}`", parse_mode="Markdown")
    elif process.returncode == 0:
        await message.edit_text(f"✅ **推流结束**:\n`{filename}`", parse_mode="Markdown")
    else:
        await message.edit_text(f"❌ **推流异常结束**:\n`{filename}`\n退出码: `{process.returncode}`", parse_mode="Markdown")


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
        info = uploads.get(task_id)
        if not info:
            return
        info["status"] = status
        if progress is not None:
            info["progress"] = progress

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
    uploads = get_youtube_uploads(context)
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
            "cancel_event": cancel_event,
            "created_at": time.time(),
            "status": "\u6392\u961f\u4e2d",
            "progress": 0.0,
            "task": None,
        }
        uploads[task_id] = info
        task = asyncio.create_task(
            upload_youtube_file(context, progress_message, file_path, task_id, cancel_event),
            name=f"youtube:{filename}",
        )
        info["task"] = task
        created.append(filename)

    context.user_data["youtube_uploads"] = uploads
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


async def action_concat(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_merge: List[str]):
    query = update.callback_query
    context.user_data["cancel_flag"] = False

    files_to_merge = [assert_path_inside_base(path) for path in files_to_merge]
    if len(files_to_merge) < 2:
        await query.answer("❌ 至少需要选择 2 个文件！", show_alert=True)
        return

    work_dir = os.path.dirname(files_to_merge[0])
    run_id = f"{os.getpid()}_{int(time.time() * 1000)}"
    list_file_path = os.path.join(work_dir, f".concat_list_{run_id}.txt")
    list_file_path_ts = os.path.join(work_dir, f".concat_list_ts_{run_id}.txt")
    ts_files: List[str] = []

    # 强制输出 MP4 格式，修复 FLV 合并后的时间轴乱序问题，并自动避让同名文件。
    base_smart_name = smart_rename(files_to_merge[0])
    output_filename = os.path.splitext(base_smart_name)[0] + ".mp4"
    output_path = unique_path(os.path.join(work_dir, output_filename))
    output_filename = os.path.basename(output_path)

    message = await query.edit_message_text(
        f"⏳ **正在分析 {len(files_to_merge)} 个文件的大小和时长...**\n输出文件:\n`{output_filename}`",
        parse_mode="Markdown",
    )

    total_input_size = sum(os.path.getsize(path) for path in files_to_merge)
    input_total_duration = 0.0
    for path in files_to_merge:
        if context.user_data.get("cancel_flag"):
            await message.edit_text("🛑 **合并任务已手动终止。**", parse_mode="Markdown")
            return
        input_total_duration += await get_video_duration(path)

    try:
        # ================= 第一阶段：尝试极速直连拼接 =================
        await message.edit_text(
            f"⏳ **正在尝试极速直连拼接...**\n输出文件:\n`{output_filename}`",
            parse_mode="Markdown",
        )

        write_concat_list(list_file_path, files_to_merge)
        cmd = [
            "ffmpeg",
            "-y",
            "-f", "concat",
            "-safe", "0",
            "-i", list_file_path,
            "-c", "copy",
            "-movflags", "+faststart",
            output_path,
        ]
        process = await asyncio.create_subprocess_exec(*cmd)
        context.user_data["current_process"] = process
        await process.wait()
        context.user_data["current_process"] = None
        remove_if_exists(list_file_path)

        if context.user_data.get("cancel_flag"):
            remove_if_exists(output_path)
            await message.edit_text("🛑 **合并任务已手动终止。**", parse_mode="Markdown")
            return

        is_success, check = await validate_merged_file(
            output_path,
            process.returncode,
            input_total_duration,
            total_input_size,
        )

        # ================= 第二阶段：如果失败，触发 TS 容错机制 =================
        if not is_success:
            await message.edit_text(
                "⚠️ **直连拼接未通过时长/体积校验！**\n"
                f"{format_merge_check(check)}\n\n"
                "正在触发 `.ts` 容错处理机制，请耐心等待...",
                parse_mode="Markdown",
            )

            ts_convert_ok = True
            for idx, file_path in enumerate(files_to_merge):
                if context.user_data.get("cancel_flag"):
                    break

                ts_path = os.path.join(work_dir, f".temp_merge_fallback_{run_id}_{idx}.ts")
                ts_files.append(ts_path)

                await message.edit_text(
                    f"⚙️ **TS 容错转换中** ({idx + 1}/{len(files_to_merge)})\n"
                    f"`{os.path.basename(file_path)}`",
                    parse_mode="Markdown",
                )

                # 将视频无损封转为 TS 格式
                cmd_ts = ["ffmpeg", "-y", "-i", file_path, "-c", "copy", "-f", "mpegts", ts_path]
                proc_ts = await asyncio.create_subprocess_exec(*cmd_ts)
                context.user_data["current_process"] = proc_ts
                await proc_ts.wait()
                context.user_data["current_process"] = None

                if proc_ts.returncode != 0 or not os.path.exists(ts_path) or os.path.getsize(ts_path) <= 0:
                    ts_convert_ok = False
                    break

            if context.user_data.get("cancel_flag"):
                for ts in ts_files:
                    remove_if_exists(ts)
                remove_if_exists(output_path)
                await message.edit_text("🛑 **合并任务已手动终止。**", parse_mode="Markdown")
                return

            if not ts_convert_ok:
                for ts in ts_files:
                    remove_if_exists(ts)
                remove_if_exists(output_path)
                await message.edit_text(
                    "❌ **TS 容错转换失败**\n部分片段无法无损封装为 TS，建议先单文件转码后再试。",
                    parse_mode="Markdown",
                )
                return

            write_concat_list(list_file_path_ts, ts_files)

            await message.edit_text(
                f"✂️ **TS 容错转换完成，正在进行最终拼接...**\n输出文件:\n`{output_filename}`",
                parse_mode="Markdown",
            )

            cmd_concat_ts = [
                "ffmpeg",
                "-y",
                "-f", "concat",
                "-safe", "0",
                "-i", list_file_path_ts,
                "-c", "copy",
                "-movflags", "+faststart",
                output_path,
            ]
            process_ts = await asyncio.create_subprocess_exec(*cmd_concat_ts)
            context.user_data["current_process"] = process_ts
            await process_ts.wait()
            context.user_data["current_process"] = None

            is_success, check = await validate_merged_file(
                output_path,
                process_ts.returncode,
                input_total_duration,
                total_input_size,
            )

        # ================= 最终结果输出 =================
        if context.user_data.get("cancel_flag"):
            remove_if_exists(output_path)
            await message.edit_text("🛑 **合并任务已手动终止。**", parse_mode="Markdown")
        elif is_success:
            await message.edit_text(
                f"✅ **合并完成!**\n\n📁 新文件: `{output_filename}`\n{format_merge_check(check)}",
                parse_mode="Markdown",
            )
        else:
            await message.edit_text(
                "❌ **合并彻底失败**\n"
                f"{format_merge_check(check)}\n\n"
                "输出文件未通过时长/体积校验。两段视频的编码、分辨率或时间戳可能严重不一致，建议先单文件转码后再试。",
                parse_mode="Markdown",
            )
    finally:
        context.user_data["current_process"] = None
        remove_if_exists(list_file_path)
        remove_if_exists(list_file_path_ts)
        for ts in ts_files:
            remove_if_exists(ts)


async def action_convert(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_convert: List[str]):
    query = update.callback_query
    context.user_data["cancel_flag"] = False
    files_to_convert = [assert_path_inside_base(path) for path in files_to_convert]
    total = len(files_to_convert)
    message = await query.edit_message_text(f"🔄 准备转换 {total} 个文件...")

    success_count = 0
    time_regex = re.compile(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}")

    for idx, file_path in enumerate(files_to_convert):
        if context.user_data.get("cancel_flag"):
            await message.edit_text("🛑 **批量转换已手动终止。**", parse_mode="Markdown")
            return

        base_name = os.path.splitext(file_path)[0]
        output_path = unique_path(f"{base_name}.mp4")
        output_filename = os.path.basename(output_path)
        filename = os.path.basename(file_path)

        duration = await get_video_duration(file_path)
        if context.user_data.get("cancel_flag"):
            await message.edit_text("🛑 **批量转换已手动终止。**", parse_mode="Markdown")
            return

        await message.edit_text(
            f"🔄 **正在转换** ({idx + 1}/{total}):\n`{filename}`\n-> `{output_filename}`\n⏳ 获取进度中...",
            parse_mode="Markdown",
        )

        cmd = ["ffmpeg", "-y", "-i", file_path, "-c", "copy", "-movflags", "+faststart", output_path]
        process = await asyncio.create_subprocess_exec(*cmd, stderr=asyncio.subprocess.PIPE)
        context.user_data["current_process"] = process

        last_update_time = time.time()
        last_percent = -1.0

        try:
            while True:
                if context.user_data.get("cancel_flag"):
                    if process.returncode is None:
                        process.terminate()
                    break

                line = await process.stderr.readline()
                if not line:
                    break

                if duration > 0:
                    match = time_regex.search(line.decode("utf-8", errors="ignore"))
                    if match:
                        h, m, s = map(int, match.groups())
                        current_sec = h * 3600 + m * 60 + s
                        percent = (current_sec / duration) * 100
                        current_time = time.time()

                        if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                            bar = build_progress_bar(percent)
                            try:
                                await message.edit_text(
                                    f"🔄 **正在转换** ({idx + 1}/{total}):\n`{filename}`\n\n`{bar}`\n⏱️ {current_sec}s / {int(duration)}s",
                                    parse_mode="Markdown",
                                )
                                last_update_time = current_time
                                last_percent = int(percent)
                            except Exception:
                                pass

            await process.wait()
        finally:
            context.user_data["current_process"] = None

        if context.user_data.get("cancel_flag"):
            remove_if_exists(output_path)
            await message.edit_text("🛑 **批量转换已手动终止。**", parse_mode="Markdown")
            return

        if process.returncode == 0 and os.path.exists(output_path) and os.path.getsize(output_path) > 0:
            success_count += 1
        else:
            remove_if_exists(output_path)

    await message.edit_text(
        f"✅ **批量转换完成!**\n成功转换 {success_count}/{total} 个文件。\n同名输出已自动避让。",
        parse_mode="Markdown",
    )


async def action_delete(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_delete: List[str]):
    query = update.callback_query
    deleted = 0
    failed = 0

    try:
        for file_path in files_to_delete:
            if context.user_data.get("cancel_flag"):
                break
            try:
                file_path = assert_path_inside_base(file_path)
                if os.path.isfile(file_path):
                    os.remove(file_path)
                    deleted += 1
                else:
                    failed += 1
            except Exception as exc:
                logger.warning("删除失败: %s, %s", file_path, exc)
                failed += 1

        if context.user_data.get("cancel_flag"):
            await query.edit_message_text(f"🛑 **删除任务已手动终止。**\n已删除 {deleted} 个文件。", parse_mode="Markdown")
        else:
            await query.edit_message_text(
                f"🗑️ **清理完成!**\n成功删除 {deleted} 个文件，失败 {failed} 个。",
                parse_mode="Markdown",
            )
    finally:
        context.user_data.pop("pending_delete_files", None)


# --- UI 与路由分发层 ---


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


def main():
    validate_config()
    app = Application.builder().token(BOT_TOKEN).build()

    app.add_handler(CommandHandler("start", render_main_menu))
    app.add_handler(CommandHandler("stop", cmd_stop))
    app.add_handler(CommandHandler("uploads", cmd_uploads))
    app.add_handler(CallbackQueryHandler(callback_router))

    logger.info("系统初始化完成，全局探测与浏览功能挂载完毕...")
    logger.info("BASE_DIR=%s, ENV_FILE=%s", BASE_DIR, ENV_FILE)
    app.run_polling()


if __name__ == "__main__":
    main()
