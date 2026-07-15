import logging
import httpx
import asyncio
from typing import List, Dict, Any
from telegram import Update, InlineKeyboardButton, InlineKeyboardMarkup
from telegram.ext import (
    ApplicationBuilder, 
    ContextTypes, 
    CommandHandler, 
    CallbackQueryHandler, 
    MessageHandler, 
    filters,
    Defaults
)

# ================= 配置区 =================
# 1. Bililive-go API 地址
API_BASE_URL = "http://127.0.0.1:9000/api"
# 2. Telegram Bot Token (从 @BotFather 获取)
TELEGRAM_TOKEN = ""
# 3. 管理员用户 ID 列表（只有在此列表中的 ID 才能操作）
ADMIN_IDS = [] 
# 4. 状态检测频率（单位：秒）
POLLING_INTERVAL = 30
# ==========================================

# 日志配置
logging.basicConfig(
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s', 
    level=logging.INFO
)

# ----------------- 权限控制装饰器 -----------------
def admin_only(func):
    """验证用户 ID 是否在管理员白名单中"""
    async def wrapper(update: Update, context: ContextTypes.DEFAULT_TYPE, *args, **kwargs):
        user = update.effective_user
        if not user or user.id not in ADMIN_IDS:
            logging.warning(f"非法访问尝试: User {user.id if user else 'Unknown'}")
            if update.message:
                await update.message.reply_text("⛔️ **权限拒绝**：您未被授权操作此机器人。")
            return
        return await func(update, context, *args, **kwargs)
    return wrapper

# ----------------- API 交互层 -----------------
class BiliLiveClient:
    """封装与 Bililive-go API 的交互逻辑"""
    
    @staticmethod
    async def request(method: str, path: str, json_data: Any = None):
        async with httpx.AsyncClient(timeout=10.0) as client:
            url = f"{API_BASE_URL}{path}"
            try:
                if method == "GET":
                    r = await client.get(url)
                elif method == "POST":
                    r = await client.post(url, json=json_data)
                elif method == "PUT":
                    r = await client.put(url, json=json_data)
                elif method == "DELETE":
                    r = await client.delete(url)
                r.raise_for_status()
                return r.json()
            except Exception as e:
                logging.error(f"API 请求失败 [{method} {path}]: {e}")
                return None

    @classmethod
    async def sync_config(cls):
        """执行配置持久化，确保修改写入 config.yaml"""
        return await cls.request("PUT", "/config")

    @classmethod
    async def get_lives(cls) -> List[Dict]:
        res = await cls.request("GET", "/lives")
        return res if res is not None else []

    @classmethod
    async def add_live(cls, url: str):
        # API 接收的是数组形式的配置
        payload = [{"url": url, "listen": True}]
        await cls.request("POST", "/lives", json_data=payload)
        await cls.sync_config() # 立即持久化

    @classmethod
    async def delete_live(cls, live_id: str):
        await cls.request("DELETE", f"/lives/{live_id}")
        await cls.sync_config() # 立即持久化

    @classmethod
    async def control_task(cls, action: str, live_id: str):
        # action: start 或 stop
        res = await cls.request("GET", f"/lives/{live_id}/{action}")
        await cls.sync_config()
        return res

# ----------------- 交互界面逻辑 -----------------

async def build_main_keyboard():
    """构建主菜单：显示所有任务及其状态"""
    lives = await BiliLiveClient.get_lives()
    keyboard = []
    for item in lives:
        # 状态指示符：🔴录制中，🟢监听中，⚪空闲
        status_icon = "🔴" if item.get('recording') else ("🟢" if item.get('listening') else "⚪")
        host_name = item.get('host_name', '未知主播')
        room_name = item.get('room_name', '无标题')
        btn_text = f"{status_icon} {host_name} | {room_name[:12]}..."
        keyboard.append([InlineKeyboardButton(btn_text, callback_data=f"view_{item['id']}")])
    
    keyboard.append([InlineKeyboardButton("🔄 刷新状态", callback_data="refresh_main")])
    return InlineKeyboardMarkup(keyboard)

async def build_detail_keyboard(live_id: str):
    """构建单个直播间的控制菜单"""
    lives = await BiliLiveClient.get_lives()
    target = next((l for l in lives if l['id'] == live_id), None)
    
    if not target:
        return "⚠️ 该任务已不存在。", None

    status_desc = "🔴 录制中" if target['recording'] else ("🟢 正在监听" if target['listening'] else "⚪ 停止中")
    info_text = (
        f"👤 **主播**: {target['host_name']}\n"
        f"📺 **房间**: {target['room_name']}\n"
        f"📊 **当前状态**: {status_desc}\n"
        f"🔗 **链接**: {target['live_url']}"
    )

    keyboard = [
        [
            InlineKeyboardButton("▶️ 开启监听", callback_data=f"op_start_{live_id}"),
            InlineKeyboardButton("⏹️ 停止录制", callback_data=f"op_stop_{live_id}")
        ],
        [
            InlineKeyboardButton("🗑️ 删除任务", callback_data=f"conf_del_{live_id}"),
            InlineKeyboardButton("🔙 返回主列表", callback_data="refresh_main")
        ]
    ]
    return info_text, InlineKeyboardMarkup(keyboard)

# ----------------- 消息与指令处理器 -----------------

@admin_only
async def cmd_start(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """响应 /start 指令"""
    markup = await build_main_keyboard()
    await update.message.reply_text(
        "🛠 **Bililive-go 控制面板**\n发送直播间 URL 即可直接添加任务。",
        reply_markup=markup,
        parse_mode='Markdown'
    )

@admin_only
async def handle_callback(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """处理按钮点击回调"""
    query = update.callback_query
    data = query.data
    await query.answer()

    if data == "refresh_main":
        await query.edit_message_reply_markup(reply_markup=await build_main_keyboard())

    elif data.startswith("view_"):
        l_id = data.split("_")[1]
        text, markup = await build_detail_keyboard(l_id)
        await query.edit_message_text(text, reply_markup=markup, parse_mode='Markdown')

    elif data.startswith("op_"):
        # 处理 start/stop 操作
        _, action, l_id = data.split("_")
        await BiliLiveClient.control_task(action, l_id)
        text, markup = await build_detail_keyboard(l_id)
        await query.edit_message_text(text, reply_markup=markup, parse_mode='Markdown')

    elif data.startswith("conf_del_"):
        l_id = data.split("_")[2]
        await BiliLiveClient.delete_live(l_id)
        await query.edit_message_text("✅ 任务已删除并同步配置。", reply_markup=await build_main_keyboard())

@admin_only
async def handle_url_input(update: Update, context: ContextTypes.DEFAULT_TYPE):
    """处理用户发送的文本，尝试识别为 URL 并添加"""
    url = update.message.text.strip()
    if "http" in url:
        sent_msg = await update.message.reply_text("⌛️ 正在解析并添加任务...")
        await BiliLiveClient.add_live(url)
        await sent_msg.edit_text(f"✅ 任务添加成功！\nURL: {url}", reply_markup=await build_main_keyboard())
    else:
        await update.message.reply_text("❌ 请发送有效的直播间 URL。")

# ----------------- 状态监测轮询任务 -----------------
async def monitor_status_job(context: ContextTypes.DEFAULT_TYPE):
    """后台任务：监控开播状态并推送消息"""
    lives = await BiliLiveClient.get_lives()
    # 在 context.bot_data 中存储上一次的录制状态字典 {id: bool}
    if "last_rec_state" not in context.bot_data:
        context.bot_data["last_rec_state"] = {}
    
    last_state = context.bot_data["last_rec_state"]
    
    for live in lives:
        l_id = live['id']
        is_recording = live.get('recording', False)
        
        # 状态转变检测：从 False -> True (开始录制)
        if l_id in last_state and not last_state[l_id] and is_recording:
            notify_text = (
                f"🚨 **开播提醒**\n\n"
                f"👤 **主播**: {live['host_name']}\n"
                f"🎬 **正在录制**: {live['room_name']}\n"
                f"🔗 [点击进入直播间]({live['live_url']})"
            )
            for admin_id in ADMIN_IDS:
                try:
                    await context.bot.send_message(chat_id=admin_id, text=notify_text, parse_mode='Markdown')
                except Exception as e:
                    logging.error(f"推送消息失败 to {admin_id}: {e}")
        
        # 更新状态快照
        last_state[l_id] = is_recording
    
    context.bot_data["last_rec_state"] = last_state

# ----------------- 主程序入口 -----------------
def main():
    # 设置默认解析模式为 Markdown
    defaults = Defaults(parse_mode='Markdown')
    app = ApplicationBuilder().token(TELEGRAM_TOKEN).defaults(defaults).build()

    # 注册后台轮询任务
    job_queue = app.job_queue
    job_queue.run_repeating(monitor_status_job, interval=POLLING_INTERVAL, first=5)

    # 注册指令处理器
    app.add_handler(CommandHandler("start", cmd_start))
    # 注册按钮回调处理器
    app.add_handler(CallbackQueryHandler(handle_callback))
    # 注册文本消息处理器（用于添加 URL）
    app.add_handler(MessageHandler(filters.TEXT & (~filters.COMMAND), handle_url_input))

    print(f"Bot 已启动，管理员 ID: {ADMIN_IDS}")
    app.run_polling()

if __name__ == "__main__":
    main()