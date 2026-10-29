"""权限相关工具。"""

from telegram import Update

from .config import ADMIN_ID

def is_admin(update: Update) -> bool:
    return bool(update.effective_user and update.effective_user.id == ADMIN_ID)
