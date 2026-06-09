"""配置读取、常量与统一日志初始化。"""

import logging
import os
from typing import Tuple

# 项目根目录：video_bot_mod/ 的上一级。保持 .env、token.json 仍在 bot_main.py 同级目录。
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


def get_csv_extensions_env(name: str, default: str) -> Tuple[str, ...]:
    """读取逗号分隔的视频扩展名，自动补点号并转小写。"""
    raw_value = os.getenv(name, default).strip()
    extensions = []
    for item in raw_value.split(","):
        ext = item.strip().lower()
        if not ext:
            continue
        if not ext.startswith("."):
            ext = f".{ext}"
        extensions.append(ext)

    if not extensions:
        raise RuntimeError(f"环境变量 {name} 至少需要包含一个有效扩展名")

    return tuple(dict.fromkeys(extensions))


BOT_TOKEN = os.getenv("BOT_TOKEN", "").strip()
ADMIN_ID = get_int_env("ADMIN_ID", 0)
BASE_DIR = os.path.abspath(os.getenv("BASE_DIR", "/storage512/bilivego/download").strip())
RTMP_URL = os.getenv("RTMP_URL", "").strip()

ITEMS_PER_PAGE = get_int_env("ITEMS_PER_PAGE", 8)
VIDEO_EXTENSIONS = get_csv_extensions_env("VIDEO_EXTENSIONS", ".mp4,.mkv,.flv,.ts")
FFPROBE_TIMEOUT_SECONDS = get_float_env("FFPROBE_TIMEOUT_SECONDS", 30.0)

# Telegram UI 常量
ACTION_NAME_MAP = {
    "browse": "浏览与查看详情",
    "stream": "推流",
    "youtube": "上传 YT",
    "concat": "合并",
    "convert": "转码 MP4",
    "delete": "删除",
}

# YouTube OAuth 配置
YOUTUBE_SCOPES = ["https://www.googleapis.com/auth/youtube.upload"]
TOKEN_FILE = os.getenv("TOKEN_FILE", os.path.join(APP_DIR, "token.json")).strip()
YOUTUBE_MAX_CONCURRENT_UPLOADS = get_int_env("YOUTUBE_MAX_CONCURRENT_UPLOADS", 2)
YOUTUBE_UPLOAD_CHUNK_MB = get_int_env("YOUTUBE_UPLOAD_CHUNK_MB", 10)
YOUTUBE_UPLOAD_QUEUE_FILE = os.getenv(
    "YOUTUBE_UPLOAD_QUEUE_FILE",
    os.path.join(APP_DIR, "youtube_upload_queue.json"),
).strip()

# 合并结果校验阈值：时长为主，大小为辅。
MERGE_MIN_DURATION_RATIO = get_float_env("MERGE_MIN_DURATION_RATIO", 0.95)
MERGE_MIN_SIZE_RATIO = get_float_env("MERGE_MIN_SIZE_RATIO", 0.30)

LOG_LEVEL = os.getenv("LOG_LEVEL", "INFO").strip().upper()
LOG_FORMAT = os.getenv("LOG_FORMAT", "%(asctime)s - %(levelname)s - %(name)s - %(message)s")


def setup_logging() -> None:
    """统一初始化日志，供 bot_main.py 在启动阶段调用。"""
    level = getattr(logging, LOG_LEVEL, logging.INFO)
    logging.basicConfig(level=level, format=LOG_FORMAT, force=True)


setup_logging()
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

    if FFPROBE_TIMEOUT_SECONDS <= 0:
        raise RuntimeError("FFPROBE_TIMEOUT_SECONDS 必须大于 0")

    if YOUTUBE_MAX_CONCURRENT_UPLOADS <= 0:
        raise RuntimeError("YOUTUBE_MAX_CONCURRENT_UPLOADS 必须大于 0")

    if YOUTUBE_UPLOAD_CHUNK_MB <= 0:
        raise RuntimeError("YOUTUBE_UPLOAD_CHUNK_MB 必须大于 0")

    if not os.path.isdir(BASE_DIR):
        logger.warning("BASE_DIR 目录不存在或不可访问: %s", BASE_DIR)

    queue_dir = os.path.dirname(os.path.abspath(YOUTUBE_UPLOAD_QUEUE_FILE))
    if queue_dir and not os.path.isdir(queue_dir):
        logger.warning("YouTube 队列文件目录不存在: %s", queue_dir)
