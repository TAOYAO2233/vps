"""媒体文件、路径、安全与格式化工具。"""

import asyncio
import logging
import math
import os
import re
from datetime import datetime
from typing import Dict, List, Optional, Tuple

from .config import BASE_DIR, FFPROBE_TIMEOUT_SECONDS, MERGE_MIN_DURATION_RATIO, MERGE_MIN_SIZE_RATIO

logger = logging.getLogger(__name__)

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
    """Use ffprobe to read media duration. Return 0.0 and log details on failure."""
    cmd = [
        "ffprobe",
        "-v", "error",
        "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1",
        filepath,
    ]
    process = None
    try:
        process = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(
            process.communicate(),
            timeout=FFPROBE_TIMEOUT_SECONDS,
        )
    except asyncio.TimeoutError:
        if process and process.returncode is None:
            process.kill()
            await process.communicate()
        logger.warning("ffprobe 超时: file=%s timeout=%ss", filepath, FFPROBE_TIMEOUT_SECONDS)
        return 0.0
    except FileNotFoundError:
        logger.exception("ffprobe 未安装或不可执行，无法分析文件时长: %s", filepath)
        return 0.0
    except Exception as exc:
        logger.exception("ffprobe 执行异常: file=%s error=%s", filepath, exc)
        return 0.0

    stderr_text = stderr.decode("utf-8", errors="ignore").strip()
    stdout_text = stdout.decode("utf-8", errors="ignore").strip()

    if process.returncode != 0:
        logger.warning(
            "ffprobe 分析失败: file=%s returncode=%s stderr=%s",
            filepath,
            process.returncode,
            stderr_text[:500],
        )
        return 0.0

    try:
        return float(stdout_text)
    except ValueError:
        logger.warning(
            "ffprobe 输出无法解析为时长: file=%s stdout=%r stderr=%s",
            filepath,
            stdout_text,
            stderr_text[:500],
        )
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
