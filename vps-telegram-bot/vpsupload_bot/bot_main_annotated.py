"""
====================================================================
 📁 VPS 交互式多媒体主控 Telegram Bot
====================================================================
 更新日志：
 - 2024-06-01 v2.0.0 核心重构，支持动态目录浏览
 - 2024-xx-xx v2.4.0 优化智能合并(TS容错防静默失败) + 修复完整时分秒重命名
 
 核心功能：
 1. 动态文件系统浏览 (支持无限层级下钻)
 2. B站等平台直播录像实时推流 (RTMP)
 3. YouTube 断点续传私享上传
 4. 智能无损合并 (极速拼接 + TS流容错双重保险，防丢帧死机)
 5. MP4 批量转码 (封装 faststart 优化秒开流媒体)
====================================================================
"""

import os            # 用于文件系统操作 (获取大小、路径拼接、删除文件等)
import re            # 用于正则表达式操作 (提取文件名中的时间戳)
import math          # 用于数学计算 (分页计算、进度条比例计算)
import time          # 用于获取当前时间戳 (控制进度条刷新频率)
import asyncio       # 异步 I/O 核心库 (非阻塞执行 ffmpeg 等耗时任务)
import logging       # 日志模块 (在控制台输出运行状态和报错)
from datetime import datetime # 日期时间处理 (获取修改时间、上传时间等)

# 导入 Telegram Bot API 的相关核心组件
from telegram import Update, InlineKeyboardButton, InlineKeyboardMarkup
from telegram.ext import Application, CommandHandler, CallbackQueryHandler, ContextTypes

# 导入 Google YouTube Data API v3 相关组件
from googleapiclient.discovery import build
from googleapiclient.http import MediaFileUpload
from google.oauth2.credentials import Credentials

# ================= ⚙️ 核心配置区域 =================
BOT_TOKEN = "8672414310:****************"  # 【必填】通过 @BotFather 获取的真实 Bot Token
ADMIN_ID = 0000000000               # 【必填】你的 Telegram User ID (纯数字)，用于阻挡陌生人越权操作
BASE_DIR = "/storage512/bilivego/download"  # 【必填】机器人在 VPS 上的“根目录”，建议配置为录播软件的下载目录
RTMP_URL = "rtmp://a.rtmp.youtube.com/live2/****-5cat-****-a7se-****" # 【选填】单路推流默认的 RTMP 地址

ITEMS_PER_PAGE = 8 # 文件列表每页显示的条目数 (防止消息过长被 TG 限制)

# YouTube OAuth 鉴权配置
YOUTUBE_SCOPES = ['https://www.googleapis.com/auth/youtube.upload'] # 申请 YouTube 上传权限的范围
TOKEN_FILE = "token.json" # 本地生成的包含 Refresh Token 的授权文件路径

# 配置全局日志输出格式
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)
# ===========================================


# ================= 🛠️ 核心工具函数 =================

def is_admin(update: Update) -> bool:
    """权限拦截器：判断当前发送指令/点击按钮的用户，是否是配置文件中的 ADMIN_ID"""
    return update.effective_user.id == ADMIN_ID

def get_formatted_file_size(filepath: str) -> str:
    """智能获取文件大小：小于 1024MB 显示 MB，否则换算为 GB"""
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
    """
    异步调用外部工具 ffprobe 获取视频的精确时长 (秒)。
    如果不使用异步，会导致整个 Bot 失去响应直到检测完毕。
    """
    cmd = ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1", filepath]
    process = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    stdout, _ = await process.communicate() # 获取标准输出结果
    try:
        return float(stdout.decode().strip())
    except ValueError:
        return 0.0 # 部分无损流或者损坏的文件可能获取不到时长，返回 0.0

def build_progress_bar(percent: float, length: int = 20) -> str:
    """根据百分比生成纯文本格式的 UI 进度条 (例如: [██████░░░░] 60.0%)"""
    filled = int(math.floor((percent / 100.0) * length))
    bar = '█' * filled + '░' * (length - filled)
    return f"[{bar}] {percent:5.1f}%"

def smart_rename(first_file_path: str) -> str:
    """
    智能重命名逻辑 (用于合并文件命名)：
    从第一个被勾选的视频文件名中，用正则表达式提取完整的日期和时间 (YYYY-MM-DD HH-MM-SS)，
    并将多余的括号前缀剔除，生成干净的新文件名。
    """
    base_name = os.path.splitext(os.path.basename(first_file_path))[0] # 去除后缀的纯文件名
    ext = os.path.splitext(first_file_path)[1] # 获取后缀 (如 .flv)
    
    # 正则表达式：匹配 `2026-06-07` 或连带时间 `2026-06-07 20:45:30` 等格式
    date_match = re.search(r'\d{4}[-_.]\d{2}[-_.]\d{2}(?:[ _-]\d{2}[-_.:]\d{2}[-_.:]\d{2})?', base_name)
    # 如果没匹配到时间，就用当前系统时间生成一个
    date_str = date_match.group(0) if date_match else f"Merged_{datetime.now().strftime('%Y%m%d_%H%M%S')}"
    
    # 正则表达式：剔除文件名前方类似 `[2026-06-07 20-00-00]` 这种带中括号的录播姬前缀
    title_part = re.sub(r'^\[\d[^\]]*\]\s*', '', base_name)
    
    # 如果剔除前缀失败，尝试使用笨办法：直接将匹配到的时间字符串从标题中替换掉
    if title_part == base_name:
        title_part = base_name.replace(date_str, '')
        title_part = re.sub(r'^[-_.\s]+', '', title_part) # 清理开头多余的横杠和空格
        
    if title_part:
        output_name = f"{date_str}_{title_part}{ext}"
    else:
        output_name = f"{date_str}_merged{ext}"
        
    return output_name.replace('__', '_') # 修复可能出现的双下划线


# ================= 🛑 全局控制指令 =================

async def cmd_stop(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """
    /stop 指令处理函数。
    随时强制中断当前正在后台异步运行的转码、推流、合并或上传任务。
    """
    if not is_admin(update): return
    # 1. 设置一个取消标识符，各个循环中的任务如果读到它为 True，就会主动 break 退出循环
    context.user_data['cancel_flag'] = True
    
    # 2. 如果底层还有 ffmpeg 子进程在运行，直接向系统发送 terminate (SIGTERM) 信号强制击杀
    process = context.user_data.get('current_process')
    if process and process.returncode is None: # None 代表进程仍在运行
        try:
            process.terminate() 
        except Exception as e:
            logger.warning(f"终止进程异常: {e}")
            
    await update.message.reply_text("🛑 **已接收停止指令！**\n正在强制中断当前运行的任务...", parse_mode='Markdown')


# ================= ⚙️ 业务逻辑层 (具体的执行功能) =================

async def action_browse(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    """【单选动作】获取文件详情信息，并以弹窗 (Alert) 形式展示在手机屏幕上"""
    query = update.callback_query
    try:
        size_str = get_formatted_file_size(file_path)
        duration = await get_video_duration(file_path)
        mtime = os.path.getmtime(file_path) # 获取文件最后的修改时间戳
        mtime_str = datetime.fromtimestamp(mtime).strftime('%Y-%m-%d %H:%M:%S')
        
        # 换算秒数为 时:分:秒 格式
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
        await query.answer(info_text, show_alert=True) # show_alert=True 强制居中弹窗显示
    except Exception as e:
        await query.answer(f"❌ 获取文件信息失败: {e}", show_alert=True)


async def action_stream(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    """【单选动作】调用 FFmpeg 将视频流无损实时推向 RTMP 服务器"""
    query = update.callback_query
    context.user_data['cancel_flag'] = False 
    
    size_str = get_formatted_file_size(file_path)
    message = await query.edit_message_text(f"⏳ 正在分析推流文件: `{os.path.basename(file_path)}` ({size_str})...", parse_mode='Markdown')

    # 需要先拿到总时长，才能在稍后计算并显示推送进度条
    duration = await get_video_duration(file_path)
    if duration <= 0: return await message.edit_text("❌ 无法获取视频时长，推流终止。")

    # 构建 ffmpeg 命令: -re (按真实帧率读取，推流必备) -c copy (不转码，极低CPU占用)
    cmd = ["ffmpeg", "-re", "-i", file_path, "-c", "copy", "-f", "flv", RTMP_URL]
    process = await asyncio.create_subprocess_exec(*cmd, stderr=asyncio.subprocess.PIPE)
    context.user_data['current_process'] = process 

    # 正则表达式，用于从 ffmpeg 的报错输出流中实时抓取当前处理到的时间 "time=HH:MM:SS"
    time_regex = re.compile(r"time=(\d{2}):(\d{2}):(\d{2})\.\d{2}")
    last_update_time = time.time()
    last_percent = -1.0

    while True:
        if context.user_data.get('cancel_flag'): break # 监听 /stop 中断信号

        line = await process.stderr.readline() # 逐行读取 ffmpeg 的输出
        if not line: break
        
        match = time_regex.search(line.decode('utf-8', errors='ignore'))
        if match:
            h, m, s = map(int, match.groups())
            current_sec = h * 3600 + m * 60 + s
            percent = (current_sec / duration) * 100
            current_time = time.time()
            
            # 限制刷新频率：电报 API 限制非常严格，仅当进度增加 >= 1% 且距上次刷新超 2 秒时才请求修改消息
            if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                bar = build_progress_bar(percent)
                try:
                    await message.edit_text(f"📡 **推流中**: `{os.path.basename(file_path)}`\n\n`{bar}`\n⏱️ {current_sec}s / {int(duration)}s", parse_mode='Markdown')
                    last_update_time = current_time
                    last_percent = int(percent)
                except Exception: pass # 忽略可能出现的网络超时异常，保证任务继续
                
    await process.wait() # 等待 ffmpeg 进程结束
    context.user_data['current_process'] = None
    
    if context.user_data.get('cancel_flag'):
        await message.edit_text(f"🛑 **推流已手动终止**:\n`{os.path.basename(file_path)}`", parse_mode='Markdown')
    else:
        await message.edit_text(f"✅ **推流结束**:\n`{os.path.basename(file_path)}`", parse_mode='Markdown')


async def action_youtube(update: Update, context: ContextTypes.DEFAULT_TYPE, file_path: str):
    """【单选动作】利用官方 API 将视频断点续传至 YouTube"""
    query = update.callback_query
    context.user_data['cancel_flag'] = False
    message = await query.edit_message_text("🔄 初始化 YouTube API...")

    if not os.path.exists(TOKEN_FILE): return await message.edit_text("❌ 缺少 `token.json` 授权文件")

    # 从 token.json 中加载凭据
    creds = Credentials.from_authorized_user_file(TOKEN_FILE, YOUTUBE_SCOPES)
    youtube = build('youtube', 'v3', credentials=creds)
    filename = os.path.basename(file_path)
    
    # 构造上传的元数据 (标题、私享、关闭儿童声明、开启 18+ 限制)
    body = {
        'snippet': {'title': filename, 'description': '', 'categoryId': '22'},
        'status': {'privacyStatus': 'private', 'selfDeclaredMadeForKids': False},
        'contentDetails': {'contentRating': {'ytRating': 'ytAgeRestricted'}}
    }

    # 以每次 10MB 的分块进行断点续传 (Resumable)
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

            # 因为 Google API 库是同步阻塞的，所以必须把它扔到 asyncio 的线程池里运行，否则会卡死整个 Bot
            status, chunk_response = await loop.run_in_executor(None, request.next_chunk)

            if chunk_response is not None:
                response = chunk_response # response 获取到值意味着全部上传完毕
                break

            if status:
                percent = status.progress() * 100
                current_time = time.time()
                # 同样限制刷新频率防止报错
                if (percent - last_percent >= 1.0) and (current_time - last_update_time >= 2.0):
                    bar = build_progress_bar(percent)
                    try:
                        await message.edit_text(f"☁️ **上传 YouTube** (私享|18+):\n`{filename}`\n\n`{bar}`", parse_mode='Markdown')
                        last_update_time = current_time
                        last_percent = int(percent)
                    except Exception: pass
        
        # 上传成功：展示包含当前时间等丰富详情的通知
        upload_time = datetime.now().strftime('%Y-%m-%d %H:%M:%S')
        success_text = (
            f"✅ **上传成功！**\n"
            f"🎬 视频名称: `{filename}`\n"
            f"🕒 上传时间: `{upload_time}`\n"
            f"📺 观看链接: `https://youtu.be/{response.get('id')}`"
        )
        await message.edit_text(success_text, parse_mode='Markdown')
    except Exception as e:
        await message.edit_text(f"❌ 上传异常:\n`{str(e)}`", parse_mode='Markdown')


async def action_concat(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_merge: list):
    """
    【批量多选动作】智能防静默视频合并机制。
    流程：
    1. 尝试使用 concat 直接无损拼接。
    2. 计算生成的文件体积。如果生成体积大幅缩水 (说明 ffmpeg 遇到时间戳断层并偷偷退出合并，即"静默失败")。
    3. 拦截静默失败，自动触发备选方案：将所有输入文件分别单独转码为 .ts 格式（免疫时间轴错误），然后拼接 TS 文件。
    """
    query = update.callback_query
    context.user_data['cancel_flag'] = False
    
    if len(files_to_merge) < 2: 
        return await query.answer("❌ 至少需要选择 2 个文件！", show_alert=True)
    
    # 记录原始所有的视频文件总大小，用于事后的体积校验
    total_input_size = sum(os.path.getsize(f) for f in files_to_merge)
    work_dir = os.path.dirname(files_to_merge[0])
    
    # 通过智能命名函数获取拼接后的文件名，并强制将其后缀改成 .mp4
    # MP4 容器在处理网络流合并时，比 FLV 有着好得多的容错性和兼容性
    base_smart_name = smart_rename(files_to_merge[0])
    output_filename = os.path.splitext(base_smart_name)[0] + ".mp4"
    output_path = os.path.join(work_dir, output_filename)
    
    # ================= 第一阶段：尝试极速直连拼接 =================
    await query.edit_message_text("⏳ **正在尝试极速直连拼接...**", parse_mode='Markdown')
    
    # 构建 ffmpeg concat demuxer 所需的 txt 文本列表
    list_file_path = os.path.join(work_dir, "concat_list.txt")
    with open(list_file_path, 'w', encoding='utf-8') as f:
        for file in files_to_merge: f.write(f"file '{file}'\n")
            
    # 参数解释：-movflags +faststart 表示将 mp4 的头信息 (moov atom) 移动到文件最前面，便于将来秒开边下边播
    cmd = ["ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", list_file_path, "-c", "copy", "-movflags", "+faststart", output_path]
    process = await asyncio.create_subprocess_exec(*cmd)
    context.user_data['current_process'] = process
    await process.wait() # 阻塞当前协程，直到极速拼接完成
    
    if os.path.exists(list_file_path): os.remove(list_file_path) # 用完就删掉临时 txt 列表
    
    if context.user_data.get('cancel_flag'):
        if os.path.exists(output_path): os.remove(output_path)
        return await query.edit_message_text("🛑 **合并任务已手动终止。**", parse_mode='Markdown')
        
    # --- 核心防死机拦截器：体积校验，检测是否发生静默失败 ---
    output_size = os.path.getsize(output_path) if os.path.exists(output_path) else 0
    # 逻辑：即使 ffmpeg 返回代码为 0 (声称成功)，但如果合并出来的文件大小还不到原始文件总大小的 70%，
    # 必定是它中途遭遇时间轴错误罢工了。判定为失败 (is_success = False)。
    is_success = process.returncode == 0 and output_size >= (total_input_size * 0.7)

    # ================= 第二阶段：如果失败，触发流媒体救星 (TS 容错机制) =================
    if not is_success:
        await query.edit_message_text(
            "⚠️ **直连拼接失败或检测到时间戳断层 (丢帧)！**\n"
            "正在触发 `.ts` 容错处理机制，请耐心等待...", parse_mode='Markdown'
        )
        
        ts_files = []
        for idx, file_path in enumerate(files_to_merge):
            if context.user_data.get('cancel_flag'): break
            
            ts_path = os.path.join(work_dir, f"temp_merge_fallback_{idx}.ts")
            ts_files.append(ts_path)
            
            # 将切片视频无损抽出封装为纯 TS 流 (TS 是广电标准流媒体格式，极其耐操，不惧时间戳跳变)
            cmd_ts = ["ffmpeg", "-y", "-i", file_path, "-c", "copy", ts_path]
            proc_ts = await asyncio.create_subprocess_exec(*cmd_ts)
            context.user_data['current_process'] = proc_ts  
            await proc_ts.wait()
            
        if context.user_data.get('cancel_flag'):
            for ts in ts_files:
                if os.path.exists(ts): os.remove(ts)
            if os.path.exists(output_path): os.remove(output_path)
            return await query.edit_message_text("🛑 **合并任务已手动终止。**", parse_mode='Markdown')
            
        # 为生成的多个 ts 临时文件构建新的 txt 列表
        list_file_path_ts = os.path.join(work_dir, "concat_list_ts.txt")
        with open(list_file_path_ts, 'w', encoding='utf-8') as f_ts:
            for ts in ts_files: f_ts.write(f"file '{ts}'\n")
        
        await query.edit_message_text(f"✂️ **TS 容错转换完成，正在进行最终拼接...**\n输出文件:\n`{output_filename}`", parse_mode='Markdown')
        
        # 再次执行合并，将干净的 TS 拼接回 MP4
        cmd_concat_ts = ["ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", list_file_path_ts, "-c", "copy", "-movflags", "+faststart", output_path]
        process_ts = await asyncio.create_subprocess_exec(*cmd_concat_ts)
        context.user_data['current_process'] = process_ts
        await process_ts.wait()
        
        # 打扫战场：清理全部 TS 临时碎片和列表
        if os.path.exists(list_file_path_ts): os.remove(list_file_path_ts)
        for ts in ts_files:
            if os.path.exists(ts): os.remove(ts)
            
        # 再次执行严格把关：对最终的结果也进行体积校验
        final_size = os.path.getsize(output_path) if os.path.exists(output_path) else 0
        is_success = process_ts.returncode == 0 and final_size >= (total_input_size * 0.7)

    # ================= 最终消息响应阶段 =================
    context.user_data['current_process'] = None
    
    if context.user_data.get('cancel_flag'):
        if os.path.exists(output_path): os.remove(output_path)
        await query.edit_message_text("🛑 **合并任务已手动终止。**", parse_mode='Markdown')
    elif is_success:
        await query.edit_message_text(f"✅ **合并完成!**\n\n📁 新文件: `{output_filename}`", parse_mode='Markdown')
    else:
        await query.edit_message_text("❌ **合并彻底失败**\n两段视频的编码或分辨率可能严重不一致，导致容错机制也无法处理。建议先单文件转码后再试！", parse_mode='Markdown')


async def action_convert(update: Update, context: ContextTypes.DEFAULT_TYPE, files_to_convert: list):
    """【批量多选动作】将非 MP4 文件批量洗帧提取，无损封转成极佳兼容性的 faststart MP4"""
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
        
        # 核心命令行：不转码音画编码 (-c copy)，但改变了外部盒子容器格式 (-movflags +faststart)
        cmd = ["ffmpeg", "-y", "-i", file_path, "-c", "copy", "-movflags", "+faststart", output_path]
        process = await asyncio.create_subprocess_exec(*cmd, stderr=asyncio.subprocess.PIPE)
        context.user_data['current_process'] = process
        
        last_update_time = time.time()
        last_percent = -1.0
        
        # 抓取并计算进度的循环结构，同推流功能原理一致
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
    """【批量多选动作】物理删除服务器上的源文件以释放硬盘空间"""
    query = update.callback_query
    deleted = 0
    for f in files_to_delete:
        try:
            os.remove(f)
            deleted += 1
        except Exception: pass
    await query.edit_message_text(f"🗑️ **清理完成!**\n成功删除了 {deleted} 个文件。", parse_mode='Markdown')


# ================= 🖥️ UI 渲染层与回调路由分发层 =================

async def render_main_menu(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """渲染机器人启动后的主控菜单键盘面板"""
    if not is_admin(update): return
    # 定义内联键盘的布局 (格式为行列结构)
    # 以 init_ 开头的回调数据，代表将要进入某种业务模式，并初始化当前目录回 BASE_DIR
    keyboard = [
        [InlineKeyboardButton("📂 浏览远程文件", callback_data="init_browse")],
        [InlineKeyboardButton("📡 RTMP 单路推流", callback_data="init_stream"),
         InlineKeyboardButton("☁️ YouTube 上传", callback_data="init_youtube")],
        [InlineKeyboardButton("✂️ 智能视频合并", callback_data="init_concat")],
        [InlineKeyboardButton("🔄 批量转码 MP4", callback_data="init_convert")],
        [InlineKeyboardButton("🗑️ 批量删除文件", callback_data="init_delete")]
    ]
    text = f"=== 🎬 VPS 多媒体主控面板 ===\n根目录: `{BASE_DIR}`\n💡 提示: 任意时候可发送 /stop 中断运行任务"
    
    # 判断是用户点击回调触发的，还是通过发送 /start 指令触发的
    if update.callback_query:
        await update.callback_query.edit_message_text(text, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode='Markdown')
    else:
        await update.message.reply_text(text, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode='Markdown')

async def render_file_selector(update: Update, context: ContextTypes.DEFAULT_TYPE, action_type: str, page: int):
    """
    核心 UI 组件：动态文件选择器。负责生成文件列表、处理翻页、渲染多选勾选框。
    这是系统内实现文件系统穿梭 (上下层文件夹跳转) 的底层驱动引擎。
    """
    query = update.callback_query
    
    # 提取保存在内存状态中的当前所处路径，如果没有，就默认回到基准根目录
    current_dir = context.user_data.get('current_dir', BASE_DIR)
    
    if not os.path.exists(current_dir): return await query.edit_message_text("❌ 目录不存在！")
    
    # 探测系统文件，将文件夹 (目录) 和 视频文件 分离
    all_items = os.listdir(current_dir)
    dirs = sorted([d for d in all_items if os.path.isdir(os.path.join(current_dir, d))])
    files = sorted([f for f in all_items if os.path.isfile(os.path.join(current_dir, f)) and f.lower().endswith(('.mp4', '.mkv', '.flv', '.ts'))])
    
    # 将文件夹优先排在顶部展示
    items = dirs + files
    context.user_data['current_files'] = items # 统一缓存名称到内存，方便后面的索引提取
    
    # 判定当前业务类型是单选还是多选
    is_multi_select = action_type in ['concat', 'convert', 'delete']
    selected_indices = context.user_data.setdefault(f'selected_{action_type}', set())

    # 计算分页核心逻辑
    total_pages = max(1, math.ceil(len(items) / ITEMS_PER_PAGE))
    start_idx = page * ITEMS_PER_PAGE
    current_page_items = items[start_idx : start_idx + ITEMS_PER_PAGE]

    keyboard = []
    # 动态渲染当前页面的所有按钮列表
    for i, item_name in enumerate(current_page_items):
        real_idx = start_idx + i  # 计算该文件在整个目录列表里的绝对索引
        item_path = os.path.join(current_dir, item_name)
        
        if os.path.isdir(item_path):
            # 渲染出子文件夹按钮 (携带 📁 图标)
            btn_text = f"📁 {item_name}"
            callback_data = f"enterdir_{action_type}_{real_idx}" # 指令: 进去！
        else:
            # 渲染出视频文件按钮 (附带大小提示)
            size_str = get_formatted_file_size(item_path)
            if is_multi_select:
                checkbox = "✅ " if real_idx in selected_indices else "⬜️ "
                btn_text = f"{checkbox}[{size_str}] {item_name}"
                callback_data = f"toggle_{action_type}_{real_idx}_{page}" # 指令: 切换勾选状态
            else:
                btn_text = f"[{size_str}] {item_name}"
                callback_data = f"execsingle_{action_type}_{real_idx}" # 指令: 立刻触发执行动作
            
        keyboard.append([InlineKeyboardButton(btn_text, callback_data=callback_data)])
    
    # 翻页控制按钮组
    nav_buttons = []
    if page > 0: nav_buttons.append(InlineKeyboardButton("⬅️ 上一页", callback_data=f"menu_{action_type}_{page-1}"))
    if page < total_pages - 1: nav_buttons.append(InlineKeyboardButton("➡️ 下一页", callback_data=f"menu_{action_type}_{page+1}"))
    if nav_buttons: keyboard.append(nav_buttons)
    
    # 层级控制按钮：只有当所处目录不是根目录时，才允许返回上级
    if current_dir != BASE_DIR:
        keyboard.append([InlineKeyboardButton("⬆️ 返回上一级目录", callback_data=f"updir_{action_type}")])
    
    # 多选执行按钮：当勾选筐内至少有一个被选中的项目时，才渲染执行按钮
    if is_multi_select and selected_indices:
        keyboard.append([InlineKeyboardButton(f"▶️ 确认执行 ({len(selected_indices)} 个文件)", callback_data=f"execbatch_{action_type}")])
        
    keyboard.append([InlineKeyboardButton("🔙 返回主菜单", callback_data="menu_main")])
    
    action_name_map = {
        "browse": "浏览与查看详情",
        "stream": "推流",
        "youtube": "上传 YT",
        "concat": "合并",
        "convert": "转码 MP4",
        "delete": "删除"
    }
    
    # 替换长长的根目录绝对路径为优雅的 🏠，避免手机端换行折叠太难看
    display_path = current_dir.replace(BASE_DIR, '🏠')
    header = f"📂 路径: `{display_path}`\n👉 模式: [{action_name_map.get(action_type, action_type.upper())}] (页 {page+1}/{total_pages})"
    
    if not items:
        header += "\n\n⚠️ 当前目录下既无子文件夹也无视频文件。"
        
    await query.edit_message_text(header, reply_markup=InlineKeyboardMarkup(keyboard), parse_mode='Markdown')

async def callback_router(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """
    大脑枢纽路由：根据不同按钮绑定的 callback_data，解析并下发任务到对应的处理函数。
    它采用 split('_') 解析字符串约定。
    """
    if not is_admin(update): return
    query = update.callback_query
    data = query.data

    if data == "menu_main":
        await render_main_menu(update, context)
        
    elif data.startswith("init_"):
        # 从主菜单初始化进入：强制重置工作路径和勾选缓存集合
        action = data.split('_')[1]
        context.user_data['current_dir'] = BASE_DIR
        context.user_data[f'selected_{action}'] = set()
        await render_file_selector(update, context, action, 0)

    elif data.startswith("menu_"):
        # 执行翻页操作，仅变换 page 变量，重新渲染 UI
        _, action, page = data.split('_')
        await render_file_selector(update, context, action, int(page))
        
    elif data.startswith("enterdir_"):
        # 进入子文件夹：通过缓存索引定位到文件夹名，并执行路径拼接，最后清空历史选择避免越界
        _, action, idx = data.split('_')
        item_name = context.user_data['current_files'][int(idx)]
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        context.user_data['current_dir'] = os.path.join(current_dir, item_name)
        context.user_data[f'selected_{action}'] = set() 
        await render_file_selector(update, context, action, 0)
        
    elif data.startswith("updir_"):
        # 返回上级文件夹：调用 os.path.dirname 智能回退
        _, action = data.split('_')
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        if current_dir != BASE_DIR:
            context.user_data['current_dir'] = os.path.dirname(current_dir)
        context.user_data[f'selected_{action}'] = set()
        await render_file_selector(update, context, action, 0)
        
    elif data.startswith("toggle_"):
        # 多选框逻辑：在 Python 的 set() 集合里动态增加或移除选中的数组索引
        _, action, idx, page = data.split('_')
        idx = int(idx)
        selected = context.user_data.get(f'selected_{action}', set())
        if idx in selected: selected.remove(idx)
        else: selected.add(idx)
        await render_file_selector(update, context, action, int(page))
        
    elif data.startswith("execsingle_"):
        # 单文件命令分发器
        _, action, idx = data.split('_')
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        filename = context.user_data['current_files'][int(idx)]
        filepath = os.path.join(current_dir, filename)
        
        if action == "browse":
            asyncio.create_task(action_browse(update, context, filepath))
        else:
            await query.answer() 
            # 采用后台独立协程任务分发，确保不阻塞 Telegram Bot 主线程接收其他用户指令
            if action == "stream": asyncio.create_task(action_stream(update, context, filepath))
            elif action == "youtube": asyncio.create_task(action_youtube(update, context, filepath))

    elif data.startswith("execbatch_"):
        # 多选批量命令分发器
        _, action = data.split('_')
        selected_indices = context.user_data.get(f'selected_{action}', set())
        if not selected_indices:
            return await query.answer("❌ 请先选择至少一个文件！", show_alert=True)
            
        await query.answer()
        current_dir = context.user_data.get('current_dir', BASE_DIR)
        cached_files = context.user_data['current_files']
        # 根据选中的数字索引，转换成完整的绝对文件路径列表对象，传送给底层的 action 函数
        target_files = [os.path.join(current_dir, cached_files[i]) for i in sorted(selected_indices)]
        
        if action == "concat": asyncio.create_task(action_concat(update, context, target_files))
        elif action == "convert": asyncio.create_task(action_convert(update, context, target_files))
        elif action == "delete": asyncio.create_task(action_delete(update, context, target_files))


# ================= 🚀 入口启动模块 =================

def main():
    """Bot 生命起点：初始化 Application 对象，挂载所有命令处理器并进入轮询模式"""
    app = Application.builder().token(BOT_TOKEN).build()
    
    app.add_handler(CommandHandler("start", render_main_menu))
    app.add_handler(CommandHandler("stop", cmd_stop))
    app.add_handler(CallbackQueryHandler(callback_router)) # 接管所有内联键盘按钮的点击事件
    
    logger.info("系统初始化完成，多媒体主控平台已启动并挂载在后台...")
    app.run_polling() # 开始与 Telegram 服务器建立长连接监听请求

if __name__ == '__main__':
    main()