#!/bin/bash

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
if [ "$EUID" -ne 0 ]; then
    echo -e "${RED}❌ 错误：请以 root 用户运行此脚本！${NC}"
    exit 1
fi

# 2. 创建并进入工作目录
BOT_DIR="/root/video_bot"
echo -e "${GREEN}[1/6] 正在创建工作目录: ${BOT_DIR}${NC}"
mkdir -p "${BOT_DIR}"
cd "${BOT_DIR}" || exit 1

# 3. 检查并安装系统级依赖 (Python3, Pip3, FFmpeg)
echo -e "${GREEN}[2/6] 正在检查并配置系统环境依赖...${NC}"

# 更新包管理器缓存
if [ -f /etc/debian_version ]; then
    apt update -y
    APT_CMD="apt install -y"
elif [ -f /etc/redhat-release ]; then
    yum makecache || dnf makecache
    APT_CMD="dnf install -y"
    # CentOS/Rocky等需要EPEL源才能安装ffmpeg
    if ! command -v ffmpeg &> /dev/null; then
        $APT_CMD epel-release
    fi
else
    echo -e "${RED}❌ 错误：未知的操作系统架构，请手动安装 Python3 和 FFmpeg。${NC}"
    exit 1
fi

# 安装 Python3, venv 和 FFmpeg
if ! command -v python3 &> /dev/null; then
    echo -e "${YELLOW}未检测到 Python3，正在安装...${NC}"
    $APT_CMD python3 python3-pip python3-venv
fi

if ! command -v ffmpeg &> /dev/null; then
    echo -e "${YELLOW}未检测到 FFmpeg/FFprobe，正在安装...${NC}"
    $APT_CMD ffmpeg
fi

# 4. 初始化 Python 虚拟环境并安装环境库
echo -e "${GREEN}[3/6] 正在初始化 Python 虚拟环境并安装运行依赖...${NC}"
python3 -m venv yt
source yt/bin/activate

pip install --upgrade pip
pip install python-telegram-bot google-api-python-client google-auth-oauthlib google-auth

# 5. 下载托管的源码文件
echo -e "${GREEN}[4/6] 正在从 GitHub 仓库拉取最新的脚本源码...${NC}"
BOT_MAIN_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/bot_main.py"
GET_JSON_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/get%20josn.py"

curl -sS -L -o bot_main.py "${BOT_MAIN_URL}"
curl -sS -L -o get_json.py "${GET_JSON_URL}"

if [ ! -f "bot_main.py" ] || [ ! -f "get_json.py" ]; then
    echo -e "${RED}❌ 错误：源码文件下载失败！请检查 VPS 网络或与 GitHub 的连通性。${NC}"
    exit 1
fi

# 6. 交互式交互：配置机器人关键参数
echo -e "${GREEN}[5/6] 正在配置机器人环境参数...${NC}"
read -p "请输入您的 Telegram Bot Token: " USER_TOKEN
read -p "请输入您的 Telegram Admin ID (纯数字): " USER_ADMIN_ID

if [ -z "$USER_TOKEN" ] || [ -z "$USER_ADMIN_ID" ]; then
    echo -e "${RED}❌ 错误：Token 或 Admin ID 不能为空！${NC}"
    exit 1
fi

# 使用 sed 动态替换配置区
sed -i "s/BOT_TOKEN = .*/BOT_TOKEN = \"${USER_TOKEN}\"/g" bot_main.py
sed -i "s/ADMIN_ID = .*/ADMIN_ID = ${USER_ADMIN_ID}/g" bot_main.py

echo -e "${GREEN}配置参数注入成功！${NC}"

# 7. 阻断检测：YouTube OAuth 凭据逻辑验证
echo -e "${YELLOW}==================================================================${NC}"
echo -e "${YELLOW}💡 提示与操作指引：YouTube API 授权认证${NC}"
echo -e "${YELLOW}==================================================================${NC}"
echo -e "1. 请下载托管在您 GitHub 上的 ${BLUE}get_json.py${NC} 并在您的${GREEN}【本地个人电脑】${NC}上运行。"
echo -e "2. 运行前确保本地有从 Google Cloud Console 导出的 ${BLUE}client_secrets.json${NC}。"
echo -e "3. 在本地完成浏览器鉴权解锁后，本地会生成授权文件 ${GREEN}token.json${NC}。"
echo -e "4. 请通过 SFTP/SCP 图形化工具，将 ${GREEN}token.json${NC} 传到 VPS 的 ${BOT_DIR}/ 目录下。"
echo -e "${YELLOW}==================================================================${NC}"

while true; do
    if [ -f "${BOT_DIR}/token.json" ]; then
        echo -e "${GREEN}🎉 完美！成功检测到 token.json 认证凭据。${NC}"
        break
    else
        echo -e "${RED}🚨 警告：尚未在目录中检测到 token.json 凭据文件！机器人目前无法正常启动。${NC}"
        read -p "请在传输完成后按 [Enter] 键触发重新检测（或按 Ctrl+C 终止退出）：" dummy
    fi
done

# 8. 写入 Systemd 服务单元
echo -e "${GREEN}[6/6] 正在向系统注册守护进程服务 (Systemd)...${NC}"
SERVICE_FILE="/etc/systemd/system/videobot.service"

cat <<EOF > "${SERVICE_FILE}"
[Unit]
Description=Telegram VPS Multimedia Control Bot Service
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=${BOT_DIR}
ExecStart=${BOT_DIR}/yt/bin/python3 ${BOT_DIR}/bot_main.py
Restart=always
RestartSec=10
Environment=PYTHONUNBUFFERED=1

[Install]
WantedBy=multi-user.target
EOF

# 激活并启动服务
systemctl daemon-reload
systemctl enable videobot
systemctl start videobot

echo -e "${GREEN}==================================================${NC}"
echo -e "${GREEN}🎉 恭喜！多媒体控制机器人已成功部署并于后台平稳运行。${NC}"
echo -e "实时查看运行日志： ${BLUE}journalctl -u videobot -f${NC}"
echo -e "停止服务：        ${BLUE}systemctl stop videobot${NC}"
echo -e "重启服务：        ${BLUE}systemctl restart videobot${NC}"
echo -e "${GREEN}==================================================${NC}"