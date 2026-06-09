#!/bin/bash
set -euo pipefail

# 颜色输出
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
    echo -e "${RED}❌ 请以 root 用户运行此脚本！${NC}"
    exit 1
fi

# 2. 创建并进入工作目录
BOT_DIR="/root/video_bot"
echo -e "${GREEN}[1/6] 正在创建工作目录: ${BOT_DIR}${NC}"
mkdir -p "${BOT_DIR}"
cd "${BOT_DIR}"

# 3. 系统依赖
echo -e "${GREEN}[2/6] 检查系统依赖...${NC}"
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
    echo -e "${RED}❌ 未知操作系统，请手动安装 Python3 和 FFmpeg${NC}"
    exit 1
fi

${PKG_INSTALL} curl python3 python3-pip python3-venv || true

# 检查 Python3 与 FFmpeg
if ! command -v python3 >/dev/null 2>&1; then
    echo -e "${RED}❌ Python3 安装失败${NC}"; exit 1
fi
if ! command -v ffmpeg >/dev/null 2>&1; then
    echo -e "${YELLOW}未检测到 FFmpeg，正在安装...${NC}"
    ${PKG_INSTALL} ffmpeg
fi
if ! command -v ffprobe >/dev/null 2>&1; then
    echo -e "${RED}❌ 未检测到 ffprobe，请确保 FFmpeg 完整${NC}"; exit 1
fi

# 4. Python 虚拟环境
echo -e "${GREEN}[3/6] 初始化 Python 虚拟环境...${NC}"
python3 -m venv yt
source yt/bin/activate
pip install --upgrade pip
pip install 'python-telegram-bot>=21,<23' 'google-api-python-client>=2,<3' 'google-auth-oauthlib>=1,<2' 'google-auth>=2,<3'

# 5. 拉取源码
echo -e "${GREEN}[4/6] 拉取源码文件...${NC}"
BOT_MAIN_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/bot_main.py"
GET_JSON_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/get%20josn.py"
MODULE_URL_BASE="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/video_bot_mod"

# 下载 bot_main.py
curl -fSL -o bot_main.py "${BOT_MAIN_URL}"

# 下载 video_bot_mod 模块
BOT_CORE_DIR="${BOT_DIR}/video_bot_mod"
mkdir -p "${BOT_CORE_DIR}"
for file in __init__.py actions.py auth.py config.py handlers.py media_utils.py task_manager.py ui.py youtube_upload.py; do
    curl -fSL -o "${BOT_CORE_DIR}/${file}" "${MODULE_URL_BASE}/${file}"
done

# 下载 get_json.py
curl -fSL -o get_json.py "${GET_JSON_URL}"

# 编译检查
python3 -m py_compile bot_main.py "${BOT_CORE_DIR}"/*.py

# 6. 配置 .env
echo -e "${GREEN}[5/6] 配置 .env 文件...${NC}"
read -r -p "请输入 Bot Token: " USER_TOKEN
read -r -p "请输入 Admin ID (纯数字): " USER_ADMIN_ID
read -r -p "请输入视频根目录 [默认 /storage512/bilivego/download]: " USER_BASE_DIR
read -r -p "请输入 RTMP 推流地址 [可留空]: " USER_RTMP_URL

USER_BASE_DIR=${USER_BASE_DIR:-/storage512/bilivego/download}
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
    printf 'MERGE_MIN_DURATION_RATIO=0.95\n'
    printf 'MERGE_MIN_SIZE_RATIO=0.30\n'
} > "${ENV_FILE}"
chmod 600 "${ENV_FILE}"
echo -e "${GREEN}.env 已生成${NC}"

# 7. YouTube token 提示
echo -e "${YELLOW}请确保将 token.json 上传到 ${BOT_DIR} 目录${NC}"
read -r -p "是否等待检测 token.json？[y/N]: " WAIT_TOKEN
if [[ "${WAIT_TOKEN}" =~ ^[Yy]$ ]]; then
    while true; do
        if [ -f "${BOT_DIR}/token.json" ]; then
            echo -e "${GREEN}成功检测到 token.json！${NC}"; break
        else
            read -r -p "传输完成后按回车重新检测: " _dummy
        fi
    done
fi

# 8. 注册 Systemd 服务
echo -e "${GREEN}[6/6] 注册 Systemd 服务...${NC}"
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

echo -e "${GREEN}部署完成，机器人已启动！${NC}"