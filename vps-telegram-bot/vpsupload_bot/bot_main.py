# 更新日志：
# 2024-06-01 v2.0.0
# 2024-xx-xx v2.4.0 优化合并逻辑(TS容错) + 修复智能命名保留完整时分秒
# 2026-06-08 v2.5.0 .env 配置 + 任务锁 + 删除二次确认 + 输出避让命名 + 合并时长/体积双校验
# 2026-06-09 v2.5.1 YouTube 上传池：支持多个视频并发上传，独占任务仍互斥
# 2026-06-09 v2.5.1-modular 按功能拆分模块，入口文件仅负责启动与注册 handler

import logging

from telegram.ext import Application, CallbackQueryHandler, CommandHandler

from video_bot_mod.config import BASE_DIR, BOT_TOKEN, ENV_FILE, validate_config
from video_bot_mod.handlers import callback_router, cmd_stop
from video_bot_mod.ui import render_main_menu
from video_bot_mod.youtube_upload import cmd_uploads

logger = logging.getLogger(__name__)


def main() -> None:
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
