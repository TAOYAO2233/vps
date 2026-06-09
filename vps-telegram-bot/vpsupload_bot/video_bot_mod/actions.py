"""媒体业务动作：浏览、推流、合并、转码、删除。"""

import asyncio
import logging
import os
import re
import time
from datetime import datetime
from typing import List

from telegram import Update
from telegram.ext import ContextTypes

from .config import RTMP_URL
from .media_utils import (
    assert_path_inside_base,
    build_progress_bar,
    format_duration,
    format_merge_check,
    get_formatted_file_size,
    get_video_duration,
    remove_if_exists,
    smart_rename,
    unique_path,
    validate_merged_file,
    write_concat_list,
)

logger = logging.getLogger(__name__)

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
