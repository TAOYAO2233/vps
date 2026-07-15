#!/bin/bash

# 退出脚本如果发生任何错误
set -e

# 定义颜色输出
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # 无颜色

echo -e "${BLUE}==============================================${NC}"
echo -e "${BLUE}    Bililive-go Rust 机器人一键部署脚本       ${NC}"
echo -e "${BLUE}==============================================${NC}"

# 1. 创建并进入部署总文件夹
TARGET_DIR="bilive_bot"
if [ -d "$TARGET_DIR" ]; then
    echo -e "${YELLOW}警告: 文件夹 $TARGET_DIR 已存在。${NC}"
    read -p "是否覆盖/重新下载该目录下的内容？(y/n): " confirm
    if [[ "$confirm" =~ ^[Yy]$ ]]; then
        echo -e "${YELLOW}正在删除旧的 $TARGET_DIR 文件夹...${NC}"
        rm -rf "$TARGET_DIR"
    else
        echo -e "${RED}部署已取消。${NC}"
        exit 1
    fi
fi

echo -e "${GREEN}正在创建文件夹: $TARGET_DIR...${NC}"
mkdir -p "$TARGET_DIR"
cd "$TARGET_DIR"

# 2. 克隆整个仓库代码
GITHUB_REPO="https://github.com/TAOYAO2233/vps.git" # 请替换为 

if [ "$GITHUB_REPO" = "***" ] || [ -z "$GITHUB_REPO" ]; then
    echo -e "${RED}错误: 请先用文本编辑器打开本脚本，将 GITHUB_REPO=\"***\" 中的 *** 替换为您真实的 GitHub 仓库链接！${NC}"
    exit 1
fi

echo -e "${GREEN}正在从 GitHub 克隆仓库...${NC}"
git clone "$GITHUB_REPO" temp_repo

# 将 RUST 目录下的内容移动到当前 bilive_bot 根目录下，并清理临时克隆文件夹
# 这样可以保证 .env、Cargo.toml 以及编译后的二进制文件都在 bilive_bot 根目录下！
if [ -d "temp_repo/vps-telegram-bot/bilive_bot/RUST" ]; then
    echo -e "${GREEN}检测到 RUST 目录，正在提取 Rust 项目文件...${NC}"
    cp -r temp_repo/vps-telegram-bot/bilive_bot/RUST/. ./
    rm -rf temp_repo
else
    echo -e "${RED}错误: 未在仓库中找到 vps-telegram-bot/bilive_bot/RUST 路径，请检查仓库结构！${NC}"
    rm -rf temp_repo
    exit 1
fi

# 3. 自动引导创建 .env 文件
echo -e "${BLUE}==============================================${NC}"
echo -e "${YELLOW}           正在配置环境变量 (.env)            ${NC}"
echo -e "${BLUE}==============================================${NC}"

read -p "请输入 Bililive-go API 地址 [默认: http://127.0.0.1:9000/api]: " api_base_url
api_base_url=${api_base_url:-"http://127.0.0.1:9000/api"}

read -p "请输入 Telegram Bot Token (必填): " telegram_token
while [ -z "$telegram_token" ]; do
    read -p "Token 不能为空，请重新输入 Telegram Bot Token: " telegram_token
done

read -p "请输入管理员用户 ID 列表 (多个ID用逗号隔开，例如: 123456,789012): " admin_ids
while [ -z "$admin_ids" ]; do
    read -p "管理员 ID 不能为空，请重新输入: " admin_ids
done

read -p "请输入状态检测频率(秒) [默认: 30]: " polling_interval
polling_interval=${polling_interval:-"30"}

read -p "请输入日志级别 [默认: info]: " rust_log
rust_log=${rust_log:-"info"}

# 写入配置文件到当前 bilive_bot 目录
cat <<EOF > .env
# Bililive-go API 地址
API_BASE_URL=${api_base_url}
# Telegram Bot Token
TELOXIDE_TOKEN=${telegram_token}
# 管理员用户 ID 列表，用英文逗号分隔
ADMIN_IDS=${admin_ids}
# 状态检测频率（单位：秒）
POLLING_INTERVAL=${polling_interval}
# 日志级别
RUST_LOG=${rust_log}
EOF

echo -e "${GREEN}.env 配置文件创建成功！${NC}"

# 4. 检测并安装 Rust 环境（若未安装）
if ! command -v cargo &> /dev/null; then
    echo -e "${YELLOW}未检测到 Rust 环境，正在通过 rustup 安装...${NC}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # 载入环境变量
    source "$HOME/.cargo/env"
    echo -e "${GREEN}Rust 环境安装成功！${NC}"
else
    echo -e "${GREEN}检测到已安装 Rust/Cargo 环境。${NC}"
fi

# 5. 编译项目
echo -e "${GREEN}开始编译 Rust 项目 (Release 模式)，请稍候...${NC}"
cargo build --release

# 6. 创建 Systemd 一键托管服务
echo -e "${BLUE}==============================================${NC}"
echo -e "${YELLOW}         正在创建 Systemd 系统守护服务        ${NC}"
echo -e "${BLUE}==============================================${NC}"

SERVICE_NAME="bilive_bot"
CURRENT_DIR=$(pwd)
USER_NAME=$(whoami)

# 生成 service 文件内容
sudo cat <<EOF > /etc/systemd/system/${SERVICE_NAME}.service
[Unit]
Description=Bililive-go Telegram Bot Service
After=network.target

[Service]
Type=simple
User=${USER_NAME}
WorkingDirectory=${CURRENT_DIR}
ExecStart=${CURRENT_DIR}/target/release/bl_bot_rust
Restart=always
RestartSec=5
StandardOutput=syslog
StandardError=syslog
SyslogIdentifier=${SERVICE_NAME}

[Install]
WantedBy=multi-user.target
EOF

# 启动并使能服务
echo -e "${GREEN}正在启动 ${SERVICE_NAME} 服务并设置开机自启...${NC}"
sudo systemctl daemon-reload
sudo systemctl enable ${SERVICE_NAME}
sudo systemctl start ${SERVICE_NAME}

echo -e "${BLUE}==============================================${NC}"
echo -e "${GREEN}🎉 部署完成！${NC}"
echo -e "${GREEN}当前服务运行状态：${NC}"
sudo systemctl status ${SERVICE_NAME} --no-pager
echo -e "${BLUE}==============================================${NC}"