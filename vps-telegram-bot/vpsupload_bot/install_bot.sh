#!/bin/bash
set -euo pipefail

# 颜色输出定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}==================================================${NC}"
echo -e "${BLUE}       VPS 多媒体 Telegram Bot 一键部署脚本       ${NC}"
echo -e "${BLUE}==================================================${NC}"

# 1. 权限检查
if [ "${EUID}" -ne 0 ]; then
    echo -e "${RED}❌ 错误：请以 root 用户运行此脚本！${NC}"
    exit 1
fi

# 2. 创建并进入工作目录
BOT_DIR="/root/video_bot"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo -e "${GREEN}[1/6] 正在创建工作目录: ${BOT_DIR}${NC}"
mkdir -p "${BOT_DIR}"
cd "${BOT_DIR}"

# 3. 检查并安装系统级依赖 (Python3, Pip3, FFmpeg)
echo -e "${GREEN}[2/6] 正在检查并配置系统环境依赖...${NC}"

if [ -f /etc/debian_version ]; then
    apt update -y
    PKG_INSTALL="apt install -y"
elif [ -f /etc/redhat-release ]; then
    if command -v dnf >/dev/null 2>&1; then
        dnf makecache || true
        PKG_INSTALL="dnf install -y"
    else
        yum makecache || true
        PKG_INSTALL="yum install -y"
    fi

    if ! command -v ffmpeg >/dev/null 2>&1; then
        ${PKG_INSTALL} epel-release || true
    fi
else
    echo -e "${RED}❌ 错误：未知的操作系统架构，请手动安装 Python3 和 FFmpeg。${NC}"
    exit 1
fi

echo -e "${GREEN}正在安装/确认基础运行包...${NC}"
if [ -f /etc/debian_version ]; then
    ${PKG_INSTALL} curl python3 python3-pip python3-venv
else
    ${PKG_INSTALL} curl python3 python3-pip
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo -e "${RED}❌ 错误：Python3 安装失败，请手动检查包管理器输出。${NC}"
    exit 1
fi

if ! command -v ffmpeg >/dev/null 2>&1; then
    echo -e "${YELLOW}未检测到 FFmpeg，正在安装...${NC}"
    ${PKG_INSTALL} ffmpeg
fi

if ! command -v ffprobe >/dev/null 2>&1; then
    echo -e "${RED}❌ 错误：未检测到 ffprobe。请确认 FFmpeg 套件安装完整。${NC}"
    exit 1
fi

# 4. 初始化 Python 虚拟环境并安装运行依赖
echo -e "${GREEN}[3/6] 正在初始化 Python 虚拟环境并安装运行依赖...${NC}"
python3 -m venv yt
# shellcheck source=/dev/null
source yt/bin/activate

pip install --upgrade pip
pip install 'python-telegram-bot>=21,<23' 'google-api-python-client>=2,<3' 'google-auth-oauthlib>=1,<2' 'google-auth>=2,<3'

# 5. 准备源码文件：支持 GitHub 模块化目录 video_bot_mod/
echo -e "${GREEN}[4/6] 正在准备脚本源码...${NC}"
BOT_MAIN_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/bot_main.py"
GET_JSON_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/get%20josn.py"
MODULE_URL_BASE="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/video_bot_mod"

# 始终从 GitHub 拉取 bot_main.py，确保入口文件与模块版本一致
echo -e "${YELLOW}正在从 GitHub 下载 bot_main.py...${NC}"
curl -fSL -o bot_main.py "${BOT_MAIN_URL}"

# 始终从 GitHub 拉取模块化目录 video_bot_mod/，不依赖安装脚本本地目录
BOT_CORE_DIR="${BOT_DIR}/video_bot_mod"
rm -rf "${BOT_CORE_DIR}"
mkdir -p "${BOT_CORE_DIR}"
echo -e "${YELLOW}正在从 GitHub 下载模块化源码 video_bot_mod/...${NC}"
for file in __init__.py actions.py auth.py config.py handlers.py media_utils.py task_manager.py ui.py youtube_upload.py; do
    curl -fSL -o "${BOT_CORE_DIR}/${file}" "${MODULE_URL_BASE}/${file}"
done

# 拉取 get_json.py
if [ -s "${SCRIPT_DIR}/get_json.py" ]; then
    cp "${SCRIPT_DIR}/get_json.py" get_json.py
else
    curl -fSL -o get_json.py "${GET_JSON_URL}"
fi

# 检查文件完整性
if [ ! -s "bot_main.py" ] || [ ! -s "get_json.py" ]; then
    echo -e "${RED}❌ 错误：源码文件准备失败或为空！请检查文件是否存在或 VPS 与 GitHub 的连通性。${NC}"
    exit 1
fi

# 编译检查
python3 -m py_compile bot_main.py "${BOT_CORE_DIR}"/*.py

# 6. 交互式配置：写入 .env
echo -e "${GREEN}[5/6] 正在配置机器人环境参数...${NC}"
read -r -p "请输入您的 Telegram Bot Token: " USER_TOKEN
read -r -p "请输入您的 Telegram Admin ID (纯数字): " USER_ADMIN_ID
read -r -p "请输入视频根目录 [默认 /storage512/bilivego/download]: " USER_BASE_DIR
read -r -p "请输入 RTMP 推流地址 [可留空，之后可在 .env 修改]: " USER_RTMP_URL

USER_BASE_DIR=${USER_BASE_DIR:-/storage512/bilivego/download}

if [ -z "${USER_TOKEN}" ] || [ -z "${USER_ADMIN_ID}" ]; then
    echo -e "${RED}❌ 错误：Token 或 Admin ID 不能为空！${NC}"
    exit 1
fi

if ! [[ "${USER_ADMIN_ID}" =~ ^[0-9]+$ ]]; then
    echo -e "${RED}❌ 错误：Admin ID 必须是纯数字！${NC}"
    exit 1
fi

ENV_FILE="${BOT_DIR}/.env"
{
    printf 'BOT_TOKEN=%s\n' "${USER_TOKEN}"
    printf 'ADMIN_ID=%s\n' "${USER_ADMIN_ID}"
    printf 'BASE_DIR=%s\n' "${USER_BASE_DIR}"
    printf 'RTMP_URL=%s\n' "${USER_RTMP_URL}"
    printf 'TOKEN_FILE=%s/token.json\n' "${BOT_DIR}"
    printf 'ITEMS_PER_PAGE=8\n'
    printf 'YOUTUBE_MAX_CONCURRENT_UPLOADS=2\n'
    printf 'YOUTUBE_UPLOAD_CHUNK_MB=10\n'
    printf 'YOUTUBE_UPLOAD_QUEUE_FILE=%s/youtube_upload_queue.json\n' "${BOT_DIR}"
    printf 'VIDEO_EXTENSIONS=.mp4,.mkv,.flv,.ts,.webm,.mov\n'
    printf 'FFPROBE_TIMEOUT_SECONDS=30\n'
    printf 'LOG_LEVEL=INFO\n'
    printf 'MERGE_MIN_DURATION_RATIO=0.95\n'
    printf 'MERGE_MIN_SIZE_RATIO=0.30\n'
} > "${ENV_FILE}"
chmod 600 "${ENV_FILE}"

echo -e "${GREEN}.env 配置文件已生成：${ENV_FILE}${NC}"
echo -e "${YELLOW}注意：Bot Token、Admin ID、RTMP 地址现在只保存在 .env，不会写入 bot_main.py。${NC}"

# 7. YouTube OAuth 提示
echo -e "${YELLOW}==================================================================${NC}"
echo -e "${YELLOW}💡 提示与操作指引：YouTube API 授权认证${NC}"
echo -e "${YELLOW}==================================================================${NC}"
echo -e "1. 请下载 get_json.py 并在本地运行完成 OAuth 授权，生成 token.json"
echo -e "2. 将 token.json 上传到 VPS 的 ${BOT_DIR}/ 目录"
echo -e "${YELLOW}==================================================================${NC}"

read -r -p "是否现在等待检测 token.json？[y/N]: " WAIT_TOKEN
if [[ "${WAIT_TOKEN}" =~ ^[Yy]$ ]]; then
    while true; do
        if [ -f "${BOT_DIR}/token.json" ]; then
            echo -e "${GREEN}🎉 成功检测到 token.json！${NC}"
            break
        else
            echo -e "${RED}🚨 尚未检测到 token.json。${NC}"
            read -r -p "传输完成后按 [Enter] 键重新检测（或 Ctrl+C 退出）：" _dummy
        fi
    done
else
    echo -e "${YELLOW}已跳过 token.json 等待，可在之后补传。${NC}"
fi

# 8. 写入 Systemd 服务
echo -e "${GREEN}[6/6] 正在注册 Systemd 服务...${NC}"
SERVICE_FILE="/etc/systemd/system/videobot.service"

cat > "${SERVICE_FILE}" <<EOF
[Unit]
Description=Telegram VPS Multimedia Control Bot Service
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=${BOT_DIR}
Environment=PYTHONUNBUFFERED=1
EnvironmentFile=${ENV_FILE}
ExecStart=${BOT_DIR}/yt/bin/python3 ${BOT_DIR}/bot_main.py
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable videobot
systemctl restart videobot

echo -e "${GREEN}==================================================${NC}"
echo -e "${GREEN}🎉 多媒体控制机器人已成功部署并运行！${NC}"
echo -e "配置文件：        ${BLUE}${ENV_FILE}${NC}"
echo -e "实时查看运行日志： ${BLUE}journalctl -u videobot -f${NC}"
echo -e "停止服务：        ${BLUE}systemctl stop videobot${NC}"
echo -e "重启服务：        ${BLUE}systemctl restart videobot${NC}"
echo -e "修改配置后：      ${BLUE}nano ${ENV_FILE} && systemctl restart videobot${NC}"
echo -e "${GREEN}==================================================${NC}"