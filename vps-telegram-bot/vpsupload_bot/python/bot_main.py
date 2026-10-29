# bot_main.py
# 2026-06-09 v2.5.2-modular
# 入口文件：启动 Telegram Bot、注册 handler，并恢复未完成的 YouTube 上传队列

import logging
from telegram.ext import Application, CallbackQueryHandler, CommandHandler

from video_bot_mod.config import BASE_DIR, BOT_TOKEN, ENV_FILE, setup_logging, validate_config
from video_bot_mod.handlers import callback_router, cmd_stop
from video_bot_mod.ui import render_main_menu
from video_bot_mod.youtube_upload import cmd_uploads, restore_persisted_youtube_uploads

setup_logging()
logger = logging.getLogger(__name__)


async def post_init(application) -> None:
    """Bot 启动后恢复本地 JSON 中未完成的 YouTube 上传任务。"""
    await restore_persisted_youtube_uploads(application)


def main() -> None:
    """启动 Telegram Bot。"""
    validate_config()

    app = Application.builder().token(BOT_TOKEN).post_init(post_init).build()

    app.add_handler(CommandHandler("start", render_main_menu))
    app.add_handler(CommandHandler("stop", cmd_stop))
    app.add_handler(CommandHandler("uploads", cmd_uploads))
    app.add_handler(CallbackQueryHandler(callback_router))

    logger.info("系统初始化完成，全局探测与浏览功能挂载完毕...")
    logger.info("BASE_DIR=%s, ENV_FILE=%s", BASE_DIR, ENV_FILE)
    app.run_polling()


if __name__ == "__main__":
    main()
