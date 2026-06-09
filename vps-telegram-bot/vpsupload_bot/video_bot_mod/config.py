"""配置读取与运行前校验。"""

import logging
import os

# 项目根目录：video_bot/ 的上一级。保持 .env、token.json 仍在 bot_main.py 同级目录。
PACKAGE_DIR = os.path.dirname(os.path.abspath(__file__))
APP_DIR = os.path.abspath(os.getenv("APP_DIR", os.path.dirname(PACKAGE_DIR)).strip())

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

# 合并结果校验阈值：时长为主，大小为辅。
MERGE_MIN_DURATION_RATIO = get_float_env("MERGE_MIN_DURATION_RATIO", 0.95)
MERGE_MIN_SIZE_RATIO = get_float_env("MERGE_MIN_SIZE_RATIO", 0.30)

logging.basicConfig(level=logging.INFO, format="%(asctime)s - %(levelname)s - %(message)s")
logger = logging.getLogger(__name__)

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
