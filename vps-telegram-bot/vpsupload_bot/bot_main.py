# bot_main.py
# 2026-06-09 v2.5.1-modular
# 入口文件：启动 Telegram Bot 并注册所有 handler

import logging
from telegram.ext import Application, CallbackQueryHandler, CommandHandler

from video_bot_mod.config import BASE_DIR, BOT_TOKEN, ENV_FILE, validate_config
from video_bot_mod.handlers import callback_router, cmd_stop
from video_bot_mod.ui import render_main_menu
from video_bot_mod.youtube_upload import cmd_uploads

# 日志配置
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(levelname)s - %(name)s - %(message)s",
)
logger = logging.getLogger(__name__)


def main() -> None:
    """启动 Telegram Bot"""
    # 配置校验
    validate_config()

    # 初始化 Bot 应用
    app = Application.builder().token(BOT_TOKEN).build()

    # 注册命令
    app.add_handler(CommandHandler("start", render_main_menu))
    app.add_handler(CommandHandler("stop", cmd_stop))
    app.add_handler(CommandHandler("uploads", cmd_uploads))

    # 注册回调查询处理器
    app.add_handler(CallbackQueryHandler(callback_router))

    logger.info("系统初始化完成，全局探测与浏览功能挂载完毕...")
    logger.info("BASE_DIR=%s, ENV_FILE=%s", BASE_DIR, ENV_FILE)

    # 启动轮询
    app.run_polling()


if __name__ == "__main__":
    main()