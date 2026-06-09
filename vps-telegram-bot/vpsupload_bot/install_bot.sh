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

# 4. 初始化 Python 虚拟环境并安装环境库
echo -e "${GREEN}[3/6] 正在初始化 Python 虚拟环境并安装运行依赖...${NC}"
python3 -m venv yt
# shellcheck source=/dev/null
source yt/bin/activate

pip install --upgrade pip
pip install 'python-telegram-bot>=21,<23' 'google-api-python-client>=2,<3' 'google-auth-oauthlib>=1,<2' 'google-auth>=2,<3'

# 5. 准备源码文件：优先使用安装脚本同目录下的优化版，缺失时再从 GitHub 拉取
echo -e "${GREEN}[4/6] 正在准备脚本源码...${NC}"
BOT_MAIN_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/bot_main.py"
GET_JSON_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/get%20josn.py"

if [ -s "${SCRIPT_DIR}/bot_main_optimized.py" ]; then
    cp "${SCRIPT_DIR}/bot_main_optimized.py" bot_main.py
elif [ -s "${SCRIPT_DIR}/bot_main.py" ]; then
    cp "${SCRIPT_DIR}/bot_main.py" bot_main.py
else
    echo -e "${YELLOW}未在安装脚本同目录找到 bot_main_optimized.py，改为从 GitHub 下载。请确认仓库中已更新优化版。${NC}"
    curl -fSL -o bot_main.py "${BOT_MAIN_URL}"
fi

if [ -s "${SCRIPT_DIR}/get_json.py" ]; then
    cp "${SCRIPT_DIR}/get_json.py" get_json.py
else
    curl -fSL -o get_json.py "${GET_JSON_URL}"
fi

if [ ! -s "bot_main.py" ] || [ ! -s "get_json.py" ]; then
    echo -e "${RED}❌ 错误：源码文件准备失败或为空！请检查文件是否存在或 VPS 与 GitHub 的连通性。${NC}"
    exit 1
fi

# 6. 交互式配置：写入 .env，不再 sed 修改 Python 源码
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
    printf 'MERGE_MIN_DURATION_RATIO=0.95\n'
    printf 'MERGE_MIN_SIZE_RATIO=0.30\n'
} > "${ENV_FILE}"
chmod 600 "${ENV_FILE}"

echo -e "${GREEN}.env 配置文件已生成：${ENV_FILE}${NC}"
echo -e "${YELLOW}注意：Bot Token、Admin ID、RTMP 地址现在只保存在 .env，不会写入 bot_main.py。${NC}"

# 7. 阻断检测：YouTube OAuth 凭据逻辑验证
echo -e "${YELLOW}==================================================================${NC}"
echo -e "${YELLOW}💡 提示与操作指引：YouTube API 授权认证${NC}"
echo -e "${YELLOW}==================================================================${NC}"
echo -e "1. 请下载托管在您 GitHub 上的 ${BLUE}get_json.py${NC} 并在您的${GREEN}【本地个人电脑】${NC}上运行。"
echo -e "2. 运行前确保本地有从 Google Cloud Console 导出的 ${BLUE}client_secrets.json${NC}。"
echo -e "3. 在本地完成浏览器鉴权解锁后，本地会生成授权文件 ${GREEN}token.json${NC}。"
echo -e "4. 请通过 SFTP/SCP 图形化工具，将 ${GREEN}token.json${NC} 传到 VPS 的 ${BOT_DIR}/ 目录下。"
echo -e "5. 如果暂时不使用 YouTube 上传功能，也可以之后再补传 token.json。"
echo -e "${YELLOW}==================================================================${NC}"

read -r -p "是否现在等待检测 token.json？[y/N]: " WAIT_TOKEN
if [[ "${WAIT_TOKEN}" =~ ^[Yy]$ ]]; then
    while true; do
        if [ -f "${BOT_DIR}/token.json" ]; then
            echo -e "${GREEN}🎉 完美！成功检测到 token.json 认证凭据。${NC}"
            break
        else
            echo -e "${RED}🚨 尚未在目录中检测到 token.json 凭据文件。${NC}"
            read -r -p "请在传输完成后按 [Enter] 键触发重新检测（或按 Ctrl+C 终止退出）：" _dummy
        fi
    done
else
    echo -e "${YELLOW}已跳过 token.json 等待。YouTube 上传功能会在 token.json 补齐后可用。${NC}"
fi

# 8. 写入 Systemd 服务单元
echo -e "${GREEN}[6/6] 正在向系统注册守护进程服务 (Systemd)...${NC}"
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
echo -e "${GREEN}🎉 恭喜！多媒体控制机器人已成功部署并于后台运行。${NC}"
echo -e "配置文件：        ${BLUE}${ENV_FILE}${NC}"
echo -e "实时查看运行日志： ${BLUE}journalctl -u videobot -f${NC}"
echo -e "停止服务：        ${BLUE}systemctl stop videobot${NC}"
echo -e "重启服务：        ${BLUE}systemctl restart videobot${NC}"
echo -e "修改配置后：      ${BLUE}nano ${ENV_FILE} && systemctl restart videobot${NC}"
echo -e "${GREEN}==================================================${NC}"
