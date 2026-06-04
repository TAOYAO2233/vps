#更新日志：
#2024-06-01 v2.0.0
#2024-06-XX v2.1.0 - RTMP推流改为多视频队列模式：可多选视频、自定义播放顺序后按序推流
#2024-06-XX v2.2.0 - 接入 YouTube Live API: 自动创建活动、绑定推流、智能状态切换 (Transition)
import os
import re
import math
import time
import asyncio
import logging
from datetime import datetime

from telegram import Update, InlineKeyboardButton, InlineKeyboardMarkup
from telegram.ext import Application, CommandHandler, CallbackQueryHandler, ContextTypes

from googleapiclient.discovery import build
from googleapiclient.http import MediaFileUpload
from google.oauth2.credentials import Credentials

# ================= 配置区域 =================
BOT_TOKEN = "8672414310:****************"  # 替换为真实的 Bot Token
ADMIN_ID = 0000000000               # 替换为真实的 Telegram User ID (纯数字)
BASE_DIR = "/storage512/bilivego/download"  # 修改为基础根目录

ITEMS_PER_PAGE = 8

# YouTube OAuth 配置 (已更新为最高权限，需删除旧 token.json 重新授权)
YOUTUBE_SCOPES = ['https://www.googleapis.com/auth/youtube']
TOKEN_FILE = "token.json"

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)
# ===========================================

# --- 核心工具函数 ---

def is_admin(update: Update) -> bool:
    return update.effective_user.id == ADMIN_ID

def get_formatted_file_size(filepath: str) -> str:
    """智能获取文件大小：小于1GB显示MB，否则显示GB"""
    try:
        size_bytes = os.path.getsize(filepath)
        size_mb = size_bytes / (1024 ** 2)
        if size_mb >= 1024:
            size_gb = size_bytes / (1024 ** 3)
            return f"{size_gb:.2f}GB"
        else:
            return f"{size_mb:.2f}MB"
    except OSError:
        return "0.00MB"

async def get_video_duration(filepath: str) -> float:
    cmd = ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1", filepath]
    process = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    stdout, _ = await process.communicate()
    try:
        return float(stdout.decode().strip())
    except ValueError:
        return 0.0

def build_progress_bar(percent: float, length: int = 20) -> str:
    filled = int(math.floor((percent / 100.0) * length))
    bar = '█' * filled + '░' * (length - filled)
    return f"[{bar}] {percent:5.1f}%"

def smart_rename(first_file_path: str) -> str:
    base_name = os.path.splitext(os.path.basename(first_file_path))[0]
    ext = os.path.splitext(first_file_path)[1]
    
    date_match = re.search(r'\d{4}[-_.]?\d{2}[-_.]?\d{2}', base_name)
    date_str = date_match.group(0) if date_match else f"Merged_{datetime.now().strftime('%Y%m%d')}"
    
    title_part = re.sub(r'^\[\d[^\]]*\]', '', base_name)
    if title_part == base_name:
        title_part = base_name.replace(date_str, '')
        title_part = re.sub(r'^[-_.]+', '', title_part)
        
    if title_part:
        output_name = f"{date_str}_{title_part}{ext}"
    else:
        output_name = f"{date_str}_merged{ext}"
        
    return output_name.replace('__', '_')

# --- YouTube Live API 专用方法 ---

def create_yt_live_sync(youtube, title: str):
    """同步：创建 YouTube 直播活动与流，并绑定"""
    # 1. 创建直播活动 Broadcast
    broadcast_body = {
        "snippet": {
            "title": title,
            "scheduledStartTime": datetime.utcnow().isoformat() + "Z"
        },
        "status": {
            "privacyStatus": "unlisted",  # 默认创建为 unlisted（不公开），可根据需要改为 public
            "selfDeclaredMadeForKids": False
        },
        "contentDetails": {
            "enableAutoStart": False, # 手动 Transition 开启
            "enableAutoStop": False,  # 防止多视频队列切换期间 YouTube 自动断播
            "monitorStream": {"enableMonitorStream": False}
        }
    }
    broadcast = youtube.liveBroadcasts().insert(part="snippet,status,contentDetails", body=broadcast_body).execute()
    broadcast_id = broadcast["id"]

    # 2. 创建直播推流 Stream
    stream_body = {
        "snippet": {"title": f"Stream for {title}"},
        "cdn": {
            "frameRate": "variable",
            "ingestionType": "rtmp",
            "resolution": "variable"
        }
    }
    stream = youtube.liveStreams().insert(part="snippet,cdn", body=stream_body).execute()
    stream_id = stream["id"]
    
    # 获取动态的推流地址
    ingestion_info = stream["cdn"]["ingestionInfo"]
    rtmp_url = f"{ingestion_info['ingestionAddress']}/{ingestion_info['streamName']}"

    # 3. 绑定活动与推流
    youtube.liveBroadcasts().bind(part="id,contentDetails", id=broadcast_id, streamId=stream_id).execute()
    
    return broadcast_id, stream_id, rtmp_url

def complete_yt_live_sync(youtube, broadcast_id: str):
    """同步：将直播活动状态设为结束"""
    try:
        youtube.liveBroadcasts().transition(broadcastStatus="complete", id=broadcast_id, part="id,status").execute()
    except Exception as e:
        logger.error(f"结束直播失败: {e}")

async def auto_transition_to_live(youtube, broadcast_id: str, stream_id: str, context: ContextTypes.DEFAULT_TYPE):
    """异步：在后台轮询流状态，一旦检测到 active 即自动调用 transition 开启直播"""
    loop = asyncio.get_running_loop()
    try:
        for _ in range(30):  # 最多等 150 秒
            if context.user_data.get('cancel_flag'):
                break
            
            resp = await loop.run_in_executor(
                None, 
                lambda: youtube.liveStreams().list(part="status", id=stream_id).execute()
            )
            status = resp.get("items", [{}])[0].get("status", {}).get("streamStatus", "")
            
            if status == "active":
                await loop.run_in_executor(
                    None,
                    lambda: youtube.liveBroadcasts().transition(
                        broadcastStatus="live",
                        id=broadcast_id,
                        part="id,status"
                    ).execute()
                )
                logger.info(f"Broadcast {broadcast_id} 成功开启直播！")
                return
            
            await asyncio.sleep(5)
    except Exception as e:
        logger.error(f"侦测或切换 Live 状态异常: {e}")

# --- 核心系统指令：强制停止 ---

async def cmd_stop(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update): return
    context.user_data['cancel_flag'] = True
    process = context.user_data.get('current_process')
    if process and process.returncode is None:
        try:
            process.terminate() 
        except Exception as e:
            logger.warning(f"终止进程异常: {e}")
    await update.message.reply_text("🛑 **已接收停止指令！**\n正在强制中断当前运行的任务...", parse_mode='Markdown')

# --- 业务逻辑层 (Actions) ---

async def action_browse(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    query = update.callback_query
    try:
        size_str = get_formatted_file_size(file_path)
        duration = await get_video_duration(file_path)
        mtime = os.path.getmtime(file_path)
        mtime_str = datetime.fromtimestamp(mtime).strftime('%Y-%m-%d %H:%M:%S')
        
        h = int(duration // 3600)
        m = int((duration % 3600) // 60)
        s = int(duration % 60)
        dur_str = f"{h:02d}:{m:02d}:{s:02d}" if duration > 0 else "未知或无损流"
        
        filename = os.path.basename(file_path)
        info_text = (
            f"📄 {filename}\n"
            f"━━━━━━━━━━━━\n"
            f"📏 大小: {size_str}\n"
            f"⏱️ 时长: {dur_str}\n"
            f"🕒 修改时间: {mtime_str}"
        )
        await query.answer(info_text, show_alert=True)
    except Exception as e:
        await query.answer(f"❌ 获取文件信息失败: {e}", show_alert=True)


async def action_stream(update: Update, context: ContextTypes.DEFAULT_TYPE, files: list):
    """推流队列：按顺序依次推流多个视频文件 (整合 YouTube Live API)"""
    query = update.callback_query
    context.user_data['cancel_flag'] = False
    total = len(files)

    message = await query.edit_message_text(
        f"⏳ 推流队列就绪，共 *{total}* 个视频，正在初始化 YouTube API...",
        parse_mode='Markdown'
    )

    if not os.path.exists(TOKEN_FILE):
        return await message.edit_text("❌ 缺少 `token.json`。请先配置并完成 YouTube 授权。")

    # ================= 新增：自动创建 YouTube 直播 =================
    try:
        creds = Credentials.from_authorized_user_file(TOKEN_FILE, YOUTUBE_SCOPES)
        youtube = build('youtube', 'v3', credentials=creds)
        
        await message.edit_text("⏳ 正在请求 API 创建 YouTube 直播流与活动...", parse_mode='Markdown')
        loop = asyncio.get_running_loop()
        live_title = f"VPS Auto Queue Live {datetime.now().strftime('%m-%d %H:%M')}"
        
        broadcast_id, stream_id, rtmp_url = await loop.run_in_executor(None, create_yt_live_sync, youtube, live_title)
        
        await message.edit_text(
            f"✅ *YouTube 直播准备就绪！*\n"
            f"🔗 稍后可在后台或访问观影链接：`https://youtu.be/{broadcast_id}`\n\n"
            f"⏳ 即将开始推送第一段视频...",
            parse_mode='Markdown'
        )
    except Exception as e:
        return await message.edit_text(f"❌ 初始化 YouTube 直播流失败：\n`{str(e)}`", parse_mode='Markdown')
    # ===============================================================

    time_regex = re.compile(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}")
    is_transition_started = False

    for file_idx, file_path in enumerate(files):
        if context.user_data.get('cancel_flag'):
            await message.edit_text(f"🛑 *推流队列已中断*\n已完成 {file_idx}/{total} 个视频", parse_mode='Markdown')
            break

        filename = os.path.basename(file_path)
        size_str = get_formatted_file_size(file_path)

        try:
            await message.edit_text(f"⏳ 正在分析文件 ({file_idx + 1}/{total}):\n`{filename}` ({size_str})...", parse_mode='Markdown')
        except Exception:
            pass

        duration = await get_video_duration(file_path)
        if duration <= 0:
            try:
                await message.edit_text(f"⚠️ 跳过文件 ({file_idx + 1}/{total}):\n`{filename}`\n❌ 无法获取视频时长，已跳过", parse_mode='Markdown')
            except Exception: pass
            await asyncio.sleep(2)
            continue

        cmd = ["ffmpeg", "-re", "-i", file_path, "-c", "copy", "-f", "flv", rtmp_url]
        process = await asyncio.create_subprocess_exec(*cmd, stderr=asyncio.subprocess.PIPE)
        context.user_data['current_process'] = process

        # 在队列启动第一个 FFmpeg 时，在后台发起异步探测，等待数据上传并触发 transition
        if not is_transition_started:
            is_transition_started = True
            asyncio.create_task(auto_transition_to_live(youtube, broadcast_id, stream_id, context))

        last_update_time = time.time()
        last_percent = -1.0

        while True:
            if context.user_data.get('cancel_flag'):
                try:
                    process.terminate()
                except Exception: pass
                break

            line = await process.stderr.readline()
            if not line: break

            match = time_regex.search(line.decode('utf-8', errors='ignore'))
            if match:
                h, m, s = map(int, match.groups())
                current_sec = h * 3600 + m * 60 + s
                percent = (current_sec / duration) * 100
                current_time = time.time()
                
                if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                    bar = build_progress_bar(percent)
                    try:
                        await message.edit_text(
                            f"📡 *推流中* ({file_idx + 1}/{total})\n"
                            f"🔗 ID: `{broadcast_id}`\n"
                            f"`{filename}`\n\n"
                            f"`{bar}`\n"
                            f"⏱️ {current_sec}s / {int(duration)}s",
                            parse_mode='Markdown'
                        )
                        last_update_time = current_time
                        last_percent = int(percent)
                    except Exception: pass

        await process.wait()
        context.user_data['current_process'] = None

        if context.user_data.get('cancel_flag'):
            break

        # 非最后一个时，显示过渡提示
        if file_idx < total - 1:
            try:
                await message.edit_text(f"✅ 第 {file_idx + 1}/{total} 个完成: `{filename}`\n⏳ 正在准备下一个...", parse_mode='Markdown')
            except Exception: pass
            await asyncio.sleep(1)

    # ================= 新增：直播结束后调用结束指令 =================
    if context.user_data.get('cancel_flag'):
        await message.edit_text(f"🛑 *推流已终止*\n正在关闭远端 YouTube 直播流...", parse_mode='Markdown')
    else:
        await message.edit_text(f"✅ *推流队列全部完成！*\n🎉 共推送 {total} 个视频。正在关闭远端直播流...", parse_mode='Markdown')
    
    # 不管是异常中断还是正常结束，都强制完结直播
    await loop.run_in_executor(None, complete_yt_live_sync, youtube, broadcast_id)
    
    if not context.user_data.get('cancel_flag'):
        await message.edit_text(f"✅ *推流队列全部完成！*\n🎉 共推送 {total} 个视频，直播活动已完美结束。", parse_mode='Markdown')
    # ===============================================================

async def action_youtube(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    query = update.callback_query
    context.user_data['cancel_flag'] = False
    message = await query.edit_message_text("🔄 初始化 YouTube API...")

    if not os.path.exists(TOKEN_FILE): return await message.edit_text("❌ 缺少 `token.json`")

    creds = Credentials.from_authorized_user_file(TOKEN_FILE, YOUTUBE_SCOPES)
    youtube = build('youtube', 'v3', credentials=creds)
    filename = os.path.basename(file_path)
    
    body = {
        'snippet': {'title': filename, 'description': '', 'categoryId': '22'},
        'status': {'privacyStatus': 'private', 'selfDeclaredMadeForKids': False},
        'contentDetails': {'contentRating': {'ytRating': 'ytAgeRestricted'}}
    }

    media = MediaFileUpload(file_path, chunksize=10*1024*1024, resumable=True)
    request = youtube.videos().insert(part='snippet,status,contentDetails', body=body, media_body=media)

    last_update_time = time.time()
    last_percent = -1.0
    response = None
    loop = asyncio.get_event_loop()
    try:
        while response is None:
            if context.user_data.get('cancel_flag'):
                return await message.edit_text(f"🛑 **YouTube 上传已手动终止**:\n`{filename}`", parse_mode='Markdown')

            status, chunk_response = await loop.run_in_executor(None, request.next_chunk)

            if chunk_response is not None:
                response = chunk_response
                break

            if status:
                percent = status.progress() * 100
                current_time = time.time()
                if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                    bar = build_progress_bar(percent)
                    try:
                        await message.edit_text(f"☁️ **上传 YouTube** (私享|18+):\n`{filename}`\n\n`{bar}`", parse_mode='Markdown')
                        last_update_time = current_time
                        last_percent = int(percent)
                    except Exception: pass
        await message.edit_text(f"✅ 上传成功！\n📺 `https://youtu.be/{response.get('id')}`", parse_mode='Markdown')
    except Exception as e:
        await message.edit_text(f"❌ 上传异常:\n`{str(e)}`", parse_mode='Markdown')

async def action_concat(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_merge: list):
    query = update.callback_query
    context.user_data['cancel_flag'] = False
    
    if len(files_to_merge) < 2: return await query.answer("❌ 至少需要选择 2 个文件！", show_alert=True)
    
    await query.edit_message_text("⏳ 正在构建合并队列...")
    
    # 将临时文件和输出文件放在第一个视频所在的目录
    work_dir = os.path.dirname(files_to_merge[0])
    list_file_path = os.path.join(work_dir, "concat_list.txt")
    
    with open(list_file_path, 'w', encoding='utf-8') as f:
        for file in files_to_merge: f.write(f"file '{file}'\n")
            
    output_filename = smart_rename(files_to_merge[0])
    output_path = os.path.join(work_dir, output_filename)
    
    await query.edit_message_text(f"✂️ **正在无损拼接...**\n输出文件:\n`{output_filename}`", parse_mode='Markdown')
    
    cmd = ["ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", list_file_path, "-c", "copy", output_path]
    process = await asyncio.create_subprocess_exec(*cmd)
    context.user_data['current_process'] = process
    
    await process.wait()
    context.user_data['current_process'] = None
    if os.path.exists(list_file_path): os.remove(list_file_path)
    
    if context.user_data.get('cancel_flag'):
        await query.edit_message_text("🛑 **合并任务已手动终止。**", parse_mode='Markdown')
    elif process.returncode == 0:
        await query.edit_message_text(f"✅ **合并完成!**\n\n📁 新文件: `{output_filename}`", parse_mode='Markdown')
    else:
        await query.edit_message_text("❌ 合并失败，请检查文件编码或格式兼容性。")

async def action_convert(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_convert: list):
    query = update.callback_query
    context.user_data['cancel_flag'] = False
    total = len(files_to_convert)
    message = await query.edit_message_text(f"🔄 准备转换 {total} 个文件...")
    
    success_count = 0
    time_regex = re.compile(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}")
    
    for idx, file_path in enumerate(files_to_convert):
        if context.user_data.get('cancel_flag'):
            return await message.edit_text("🛑 **批量转换已手动终止。**", parse_mode='Markdown')

        base_name = os.path.splitext(file_path)[0]
        output_path = f"{base_name}.mp4"
        filename = os.path.basename(file_path)
        
        duration = await get_video_duration(file_path)
        await message.edit_text(f"🔄 **正在转换** ({idx+1}/{total}):\n`{filename}`\n-> `.mp4`\n⏳ 获取进度中...", parse_mode='Markdown')
        
        cmd = ["ffmpeg", "-y", "-i", file_path, "-c", "copy", "-movflags", "+faststart", output_path]
        process = await asyncio.create_subprocess_exec(*cmd, stderr=asyncio.subprocess.PIPE)
        context.user_data['current_process'] = process
        
        last_update_time = time.time()
        last_percent = -1.0
        
        while True:
            if context.user_data.get('cancel_flag'): break
            line = await process.stderr.readline()
            if not line: break 
            
            if duration > 0:
                match = time_regex.search(line.decode('utf-8', errors='ignore'))
                if match:
                    h, m, s = map(int, match.groups())
                    current_sec = h * 3600 + m * 60 + s
                    percent = (current_sec / duration) * 100
                    current_time = time.time()
                    
                    if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                        bar = build_progress_bar(percent)
                        try:
                            await message.edit_text(
                                f"🔄 **正在转换** ({idx+1}/{total}):\n`{filename}`\n\n`{bar}`\n⏱️ {current_sec}s / {int(duration)}s", 
                                parse_mode='Markdown'
                            )
                            last_update_time = current_time
                            last_percent = int(percent)
                        except Exception: pass
        
        await process.wait()
        context.user_data['current_process'] = None
        
        if context.user_data.get('cancel_flag'):
            return await message.edit_text("🛑 **批量转换已手动终止。**", parse_mode='Markdown')
            
        if process.returncode == 0: success_count += 1
            
    await message.edit_text(f"✅ **批量转换完成!**\n成功转换 {success_count}/{total} 个文件。", parse_mode='Markdown')

async def action_delete(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_delete: list):
    query = update.callback_query
    deleted = 0
    for f in files_to_delete:
        try:
            os.remove(f)
            deleted += 1
        except Exception: pass
    await query.edit_message_text(f"🗑️ **清理完成!**\n成功删除了 {deleted} 个文件。", parse_mode='Markdown')

# --- UI 与路由分发层 ---

async def render_main_menu(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update): return
    keyboard = [
        [InlineKeyboardButton("📂 浏览远程文件", callback_data="init_browse")],
        [InlineKeyboardButton("📡 RTMP 推流队列", callback_data="init_stream"),
         InlineKeyboardButton("☁️ YouTube 上传", callback_data="init_youtube")],
        [InlineKeyboardButton("✂️ 智能视频合并", callback_data="init_concat")],
        [InlineKeyboardButton("🔄 批量转码 MP4", callback_data="init_convert")],
        [InlineKeyboardButton("🗑️ 批量删除文件", callback_data="init_delete")]
    ]
    text = f"=== 🎬 VPS 多媒体主控面板 ===\n根目录: `{BASE_DIR}`\n💡 提示: 任意时候可发送 /stop 中断运行任务"
    if update.callback_query:
        await update.callback_query.edit_message_text(text, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode='Markdown')
    else:
        await update.message.reply_text(text, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode='Markdown')

async def render_stream_sort_view(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """渲染推流队列排序页面：显示已选视频列表，支持上移/下移/移除，确认后按序推流"""
    query = update.callback_query
    queue = context.user_data.get('stream_queue', [])

    if not queue:
        await query.answer("❌ 推流队列为空，请重新选择文件！", show_alert=True)
        await render_file_selector(update, context, 'stream', 0)
        return

    nums = ['①', '②', '③', '④', '⑤', '⑥', '⑦', '⑧', '⑨', '⑩',
            '⑪', '⑫', '⑬', '⑭', '⑮', '⑯', '⑰', '⑱', '⑲', '⑳']
    n = len(queue)

    lines = [
        "📋 *推流队列排序*",
        f"共 *{n}* 个视频，将按以下顺序自动在 YouTube 直播中推流。",
        "点击 ⬆️⬇️ 调整顺序，🗑️ 移除视频：",
        "━━━━━━━━━━━━"
    ]
    for i, fp in enumerate(queue):
        num = nums[i] if i < len(nums) else f"{i + 1}."
        size_str = get_formatted_file_size(fp)
        filename = os.path.basename(fp)
        lines.append(f"{num} `[{size_str}]` {filename}")

    keyboard = []
    for i in range(n):
        num = nums[i] if i < len(nums) else f"{i + 1}."
        row = []
        if i > 0:
            row.append(InlineKeyboardButton(f"⬆️ {num}", callback_data=f"ssort_up_{i}"))
        else:
            row.append(InlineKeyboardButton(f"🔝 {num}", callback_data="ssort_noop"))
        if i < n - 1:
            row.append(InlineKeyboardButton(f"⬇️ {num}", callback_data=f"ssort_down_{i}"))
        else:
            row.append(InlineKeyboardButton(f"🔚 {num}", callback_data="ssort_noop"))
        row.append(InlineKeyboardButton(f"🗑️ {num}", callback_data=f"ssort_rm_{i}"))
        keyboard.append(row)

    keyboard.append([InlineKeyboardButton(f"▶️ 启动自动直播流程 ({n} 个视频)", callback_data="ssort_start")])
    keyboard.append([InlineKeyboardButton("🔙 返回重新选择文件", callback_data="ssort_back")])

    await query.edit_message_text(
        "\n".join(lines),
        reply_markup=InlineKeyboardMarkup(keyboard),
        parse_mode='Markdown'
    )


async def render_file_selector(update: Update, context: ContextTypes.DEFAULT_TYPE, action_type: str, page: int):
    query = update.callback_query
    
    current_dir = context.user_data.get('current_dir', BASE_DIR)
    
    if not os.path.exists(current_dir): return await query.edit_message_text("❌ 目录不存在！")
    
    all_items = os.listdir(current_dir)
    dirs = sorted([d for d in all_items if os.path.isdir(os.path.join(current_dir, d))])
    files = sorted([f for f in all_items if os.path.isfile(os.path.join(current_dir, f)) and f.lower().endswith(('.mp4', '.mkv', '.flv', '.ts'))])
    
    items = dirs + files
    context.user_data['current_files'] = items
    
    is_multi_select = action_type in ['concat', 'convert', 'delete', 'stream']
    selected_indices = context.user_data.setdefault(f'selected_{action_type}', set())

    total_pages = max(1, math.ceil(len(items) / ITEMS_PER_PAGE))
    start_idx = page * ITEMS_PER_PAGE
    current_page_items = items[start_idx : start_idx + ITEMS_PER_PAGE]

    keyboard = []
    for i, item_name in enumerate(current_page_items):
        real_idx = start_idx + i  
        item_path = os.path.join(current_dir, item_name)
        
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
    if page > 0: nav_buttons.append(InlineKeyboardButton("⬅️ 上一页", callback_data=f"menu_{action_type}_{page-1}"))
    if page < total_pages - 1: nav_buttons.append(InlineKeyboardButton("➡️ 下一页", callback_data=f"menu_{action_type}_{page+1}"))
    if nav_buttons: keyboard.append(nav_buttons)
    
    if current_dir != BASE_DIR:
        keyboard.append([InlineKeyboardButton("⬆️ 返回上一级目录", callback_data=f"updir_{action_type}")])
    
    if is_multi_select and selected_indices:
        if action_type == 'stream':
            keyboard.append([InlineKeyboardButton(
                f"📋 确认并进入排序 ({len(selected_indices)} 个视频)",
                callback_data=f"execbatch_{action_type}"
            )])
        else:
            keyboard.append([InlineKeyboardButton(f"▶️ 确认执行 ({len(selected_indices)} 个文件)", callback_data=f"execbatch_{action_type}")])
        
    keyboard.append([InlineKeyboardButton("🔙 返回主菜单", callback_data="menu_main")])
    
    action_name_map = {
        "browse": "浏览与查看详情",
        "stream": "YouTube API 队列推流",
        "youtube": "上传 YT",
        "concat": "合并",
        "convert": "转码 MP4",
        "delete": "删除"
    }
    
    display_path = current_dir.replace(BASE_DIR, '🏠')
    header = f"📂 路径: `{display_path}`\n👉 模式: [{action_name_map.get(action_type, action_type.upper())}] (页 {page+1}/{total_pages})"

    if action_type == 'stream':
        header += "\n💡 勾选完毕后点击确认，将自动在远端开启新直播并按序推流"
    
    if not items:
        header += "\n\n⚠️ 当前目录下既无子文件夹也无视频文件。"
        
    await query.edit_message_text(header, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode='Markdown')

async def callback_router(update: Update, context: ContextTypes.DEFAULT_TYPE):
    if not is_admin(update): return
    query = update.callback_query
    data = query.data

    if data == "menu_main":
        await render_main_menu(update, context)
        
    elif data.startswith("init_"):
        action = data.split('_')[1]
        context.user_data['current_dir'] = BASE_DIR
        context.user_data[f'selected_{action}'] = set()
        await render_file_selector(update, context, action, 0)

    elif data.startswith("menu_"):
        _, action, page = data.split('_')
        await render_file_selector(update, context, action, int(page))
        
    elif data.startswith("enterdir_"):
        _, action, idx = data.split('_')
        item_name = context.user_data['current_files'][int(idx)]
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        context.user_data['current_dir'] = os.path.join(current_dir, item_name)
        context.user_data[f'selected_{action}'] = set()
        await render_file_selector(update, context, action, 0)
        
    elif data.startswith("updir_"):
        _, action = data.split('_')
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        if current_dir != BASE_DIR:
            context.user_data['current_dir'] = os.path.dirname(current_dir)
        context.user_data[f'selected_{action}'] = set()
        await render_file_selector(update, context, action, 0)
        
    elif data.startswith("toggle_"):
        _, action, idx, page = data.split('_')
        idx = int(idx)
        selected = context.user_data.get(f'selected_{action}', set())
        if idx in selected: selected.remove(idx)
        else: selected.add(idx)
        await render_file_selector(update, context, action, int(page))
        
    elif data.startswith("execsingle_"):
        _, action, idx = data.split('_')
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        filename = context.user_data['current_files'][int(idx)]
        filepath = os.path.join(current_dir, filename)
        
        if action == "browse":
            asyncio.create_task(action_browse(update, context, filepath))
        else:
            await query.answer() 
            if action == "youtube": asyncio.create_task(action_youtube(update, context, filepath))

    elif data.startswith("execbatch_"):
        _, action = data.split('_')
        selected_indices = context.user_data.get(f'selected_{action}', set())
        if not selected_indices:
            return await query.answer("❌ 请先选择至少一个文件！", show_alert=True)
            
        await query.answer()
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        cached_files = context.user_data['current_files']
        target_files = [os.path.join(current_dir, cached_files[i]) for i in sorted(selected_indices)]
        
        if action == "stream":
            context.user_data['stream_queue'] = target_files
            await render_stream_sort_view(update, context)
        elif action == "concat": asyncio.create_task(action_concat(update, context, target_files))
        elif action == "convert": asyncio.create_task(action_convert(update, context, target_files))
        elif action == "delete": asyncio.create_task(action_delete(update, context, target_files))

    elif data.startswith("ssort_"):
        parts = data.split("_")
        ssort_action = parts[1]
        queue = context.user_data.get('stream_queue', [])

        if ssort_action == "noop":
            await query.answer("⚠️ 已在边界位置，无法继续移动。")

        elif ssort_action == "up":
            idx = int(parts[2])
            if 0 < idx < len(queue):
                queue[idx], queue[idx - 1] = queue[idx - 1], queue[idx]
                context.user_data['stream_queue'] = queue
            await render_stream_sort_view(update, context)

        elif ssort_action == "down":
            idx = int(parts[2])
            if 0 <= idx < len(queue) - 1:
                queue[idx], queue[idx + 1] = queue[idx + 1], queue[idx]
                context.user_data['stream_queue'] = queue
            await render_stream_sort_view(update, context)

        elif ssort_action == "rm":
            idx = int(parts[2])
            if 0 <= idx < len(queue):
                queue.pop(idx)
                context.user_data['stream_queue'] = queue
            if not queue:
                await query.answer("⚠️ 队列已清空，请重新选择文件", show_alert=True)
                context.user_data[f'selected_stream'] = set()
                await render_file_selector(update, context, 'stream', 0)
            else:
                await render_stream_sort_view(update, context)

        elif ssort_action == "start":
            if not queue:
                await query.answer("❌ 推流队列为空！", show_alert=True)
                return
            await query.answer()
            asyncio.create_task(action_stream(update, context, list(queue)))

        elif ssort_action == "back":
            await render_file_selector(update, context, 'stream', 0)

def main():
    app = Application.builder().token(BOT_TOKEN).build()
    
    app.add_handler(CommandHandler("start", render_main_menu))
    app.add_handler(CommandHandler("stop", cmd_stop))
    app.add_handler(CallbackQueryHandler(callback_router))
    
    logger.info("系统初始化完成，全局探测与浏览功能挂载完毕...")
    app.run_polling()

if __name__ == '__main__':
    main()