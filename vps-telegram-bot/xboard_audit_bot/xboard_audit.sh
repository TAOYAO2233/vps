#!/bin/bash

# 定义颜色常量
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# GitHub 托管的 Python 审计脚本地址
GITHUB_RAW_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/xboard_audit_bot/xboard_audit.py"

clear
echo -e "${BLUE}====================================================${NC}"
echo -e "${GREEN}    Xboard 节点网络访问双机集中审计一键脚本 (云端同步版)${NC}"
echo -e "${YELLOW}    系统支持: Ubuntu 20.04+ (兼容当前系统 Python 3.8)${NC}"
echo -e "${BLUE}====================================================${NC}"

# 基础公共检查与权限放开
init_public_dir() {
    echo -e "${YELLOW}📂 正在初始化系统公共接收缓冲区 /home/xboard_log/...${NC}"
    mkdir -p /home/xboard_log/
    chmod 777 /home/xboard_log/
}

# 管道流重建
start_journal_tunnel() {
    echo -e "${YELLOW}🚀 正在建立 journalctl -> /home/xboard_log/xboard.log 实时导流管道...${NC}"
    pkill -f "journalctl -u xboard-node" > /dev/null 2>&1
    nohup journalctl -u xboard-node -f --no-pager > /home/xboard_log/xboard.log 2>&1 &
    sleep 1
    if ps ax | grep -v grep | grep "journalctl -u xboard-node" > /dev/null; then
        echo -e "${GREEN}✅ 实时日志导流管道建立成功！${NC}"
    else
        echo -e "${RED}❌ 警告：未检测到 xboard-node 服务运行，导流管道已置于后台挂起监控。${NC}"
    fi
}

# 1. 设置本机为主控
set_as_master() {
    init_public_dir
    touch /home/xboard_log/xboard_vps_b.log
    chmod 666 /home/xboard_log/xboard_vps_b.log
    
    # 引导输入参数
    echo -e "\n${BLUE}--- 请配置 Telegram 机器人联动参数 ---${NC}"
    read -p "请输入你的 Telegram Bot Token: " TG_TOKEN
    read -p "请输入接收者 Chat ID (多个用逗号隔开，例如 6053576171,5525443144): " TG_IDS
    
    if [ -z "$TG_TOKEN" ] || [ -z "$TG_IDS" ]; then
        echo -e "${RED}❌ 错误：Token 或 Chat ID 不能为空，配置失败！${NC}"
        exit 1
    fi
    
    # 将逗号隔开的输入转换为 Python 格式的列表元素
    PYTHON_CHAT_IDS=$(echo "$TG_IDS" | sed 's/,/, /g')
    
    # 开启 rsyslog 的 UDP 514 接收
    echo -e "${YELLOW}📡 正在配置主控端 rsyslog 514/UDP 接收流与分流隔离规则...${NC}"
    sed -i 's/^#module(load="imudp")/module(load="imudp")/g' /etc/rsyslog.conf
    sed -i 's/^#input(type="imudp" port="514")/input(type="imudp" port="514")/g' /etc/rsyslog.conf
    
    cat << 'RECVEOF' > /etc/rsyslog.d/xboard-recv.conf
if $msg contains 'xboard-node-b' or $rawmsg contains 'xboard-node-b' then {
    action(type="omfile" file="/home/xboard_log/xboard_vps_b.log")
    stop
}
RECVEOF
    
    systemctl restart rsyslog
    start_journal_tunnel
    
    # 从 GitHub 实时拉取你托管的代码
    echo -e "${YELLOW}📥 正在从 GitHub 远程安全获取最新版审计代码...${NC}"
    mkdir -p /root/xboard_log
    curl -sS -o /root/xboard_log/xboard_monitor.py "$GITHUB_RAW_URL"
    
    if [ ! -f "/root/xboard_log/xboard_monitor.py" ] || [ ! -s "/root/xboard_log/xboard_monitor.py" ]; then
        echo -e "${RED}❌ 错误：从 GitHub 下载源码失败！请检查 VPS 的网络连接性。${NC}"
        exit 1
    fi
    
    # 💡 核心替换逻辑：动态修改代码中的 Token 和 Chat 数组
    echo -e "${YELLOW}🔧 正在热注入局部安全变量 (Token / Chat IDs)...${NC}"
    sed -i "s/TG_BOT_TOKEN = \".*\"/TG_BOT_TOKEN = \"$TG_TOKEN\"/g" /root/xboard_log/xboard_monitor.py
    sed -i "s/TG_CHAT_IDS = \[.*\]/TG_CHAT_IDS = \[$PYTHON_CHAT_IDS\]/g" /root/xboard_log/xboard_monitor.py
    
    # 构建隔离虚拟环境
    echo -e "${YELLOW}📦 正在自动化隔离并架设系统原生的 Python 3.8 独立虚拟环境...${NC}"
    apt-get update > /dev/null 2>&1
    apt-get install python3-venv python3-dev -y > /dev/null 2>&1
    rm -rf /root/xboard_log/xboard_env
    python3 -m venv /root/xboard_log/xboard_env
    /root/xboard_log/xboard_env/bin/pip install --upgrade pip > /dev/null
    /root/xboard_log/xboard_env/bin/pip install python-telegram-bot > /dev/null
    
    # 建立后台守护
    echo -e "${YELLOW}⚙️  正在建立 Systemd 自动化系统后台守护进程...${NC}"
    cat << 'SYSTEMEOF' > /etc/systemd/system/xboard-audit.service
[Unit]
Description=Xboard Log全量网络审计Telegram机器人(集中管理版)
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=/root/xboard_log
ExecStart=/root/xboard_log/xboard_env/bin/python3 /root/xboard_log/xboard_monitor.py
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
SYSTEMEOF
    
    systemctl daemon-reload
    systemctl enable --now xboard-audit
    systemctl restart xboard-audit
    
    echo -e "${GREEN}=========================================${NC}"
    echo -e "${GREEN}🎉 本机【主控中心】一键配置并从 GitHub 同步成功！${NC}"
    echo -e "${YELLOW}💡 提示：请确保主控端的云面板/防火墙放行了 514/UDP 端口。${NC}"
    echo -e "${GREEN}=========================================${NC}"
    exit 0
}

# 2. 设置本机为客户端
set_as_client() {
    init_public_dir
    
    echo -e "\n${BLUE}--- 请配置日志网络远程推送参数 ---${NC}"
    read -p "请输入主控机 (VPS-A) 的公网 IP: " MASTER_IP
    if [ -z "$MASTER_IP" ]; then
        echo -e "${RED}❌ 错误：主控机 IP 不能为空，配置失败！${NC}"
        exit 1
    fi
    
    start_journal_tunnel
    
    echo -e "${YELLOW}📨 正在向系统植入 rsyslog 网络远程投递规则...${NC}"
    cat << LOGEOF > /etc/rsyslog.d/xboard-forward.conf
module(load="imfile")

input(type="imfile"
      File="/home/xboard_log/xboard.log"
      Tag="xboard-node-b"
      Severity="info"
      Facility="local7")

local7.* @${MASTER_IP}:514
LOGEOF
    
    echo -e "${YELLOW}🔒 正在破除系统的 rsyslog 跨目录降权阻锁...${NC}"
    sed -i 's/^\$PrivDropToUser/# \$PrivDropToUser/g' /etc/rsyslog.conf
    sed -i 's/^\$PrivDropToGroup/# \$PrivDropToGroup/g' /etc/rsyslog.conf
    
    systemctl restart rsyslog
    
    echo -e "${GREEN}=========================================${NC}"
    echo -e "${GREEN}✅ 本机【客户端推送流】一键配置成功！${NC}"
    echo -e "${YELLOW}💡 日志已开始不间断打向主控机: ${MASTER_IP}${NC}"
    echo -e "${GREEN}=========================================${NC}"
    exit 0
}

# 菜单选择逻辑
echo -e "${BLUE}请选择当前 VPS 的部署角色:${NC}"
echo -e "  ${GREEN}1.${NC} 设置本机为 ${GREEN}[主控机 (VPS-A)]${NC} (从 GitHub 自动拉取源码并热填入变量)"
echo -e "  ${GREEN}2.${NC} 设置本机为 ${GREEN}[客户端 (VPS-B)]${NC} (仅外发推送日志给主控)"
echo -e "  ${RED}3.${NC} 退出安装"
echo -e "${BLUE}────────────────────────────────────────────────────${NC}"
read -p "请输入对应数字 [1-3]: " CHOSEN

case $CHOSEN in
    1)
        set_as_master
        ;;
    2)
        set_as_client
        ;;
    3)
        echo -e "${YELLOW}已取消安装。${NC}"
        exit 0
        ;;
    *)
        echo -e "${RED}❌ 错误输入，请输入 1、2 或 3。${NC}"
        exit 1
        ;;
esac