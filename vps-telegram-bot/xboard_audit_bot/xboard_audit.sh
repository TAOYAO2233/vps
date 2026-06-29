#!/bin/bash

# 定义颜色常量
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# 定义双版本 Python 审计脚本云端同步地址
URL_PYTHON_38="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/xboard_audit_bot/xboard_audit3.8.py"
URL_PYTHON_HIGH="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/xboard_audit_bot/xboard_audit.py"

clear
echo -e "${BLUE}====================================================${NC}"
echo -e "${GREEN}    Xboard 节点网络访问多机集中审计一键脚本 (稳定守护版)${NC}"
echo -e "${YELLOW}    系统支持: Ubuntu 20.04+ (支持使用 IP 或代号直接添加节点)${NC}"
echo -e "${BLUE}====================================================${NC}"

# 基础公共检查与权限放开
init_public_dir() {
    echo -e "${YELLOW}📂 正在初始化系统公共接收缓冲区 /home/xboard_log/...${NC}"
    mkdir -p /home/xboard_log/
    chmod 777 /home/xboard_log/
}

# 自动检测并安装 rsyslog 依赖
check_rsyslog() {
    if ! command -v rsyslogd &> /dev/null; then
        echo -e "${YELLOW}📦 检测到系统环境缺失 rsyslog 核心日志组件，正在自动补全...${NC}"
        apt-get update > /dev/null 2>&1
        apt-get install rsyslog -y > /dev/null 2>&1
        
        mkdir -p /etc/rsyslog.d
        if [ ! -f "/etc/rsyslog.conf" ] && [ -f "/usr/share/doc/doc/rsyslog/examples/rsyslog.conf" ]; then
            cp /usr/share/doc/rsyslog/examples/rsyslog.conf /etc/rsyslog.conf
        fi
        echo -e "${GREEN}✅ rsyslog 补全成功！${NC}"
    fi
}

# 管道流重建与配置优化 (已改为 Systemd 强力守护模式，永不挂掉)
start_journal_tunnel() {
    if [ -f "/etc/xboard-node/config.yml" ]; then
        if grep -q "log_level:[[:space:]]*warn" /etc/xboard-node/config.yml; then
            echo -e "${YELLOW}⚙️ 检测到内核日志级别为 warn，正在自动修正为 info 以启用全量审计...${NC}"
            sed -i 's/log_level:[[:space:]]*warn/log_level: info/g' /etc/xboard-node/config.yml
            echo -e "${YELLOW}🔄 正在重启 xboard-node 服务以应用新配置...${NC}"
            systemctl restart xboard-node > /dev/null 2>&1
            sleep 1
        else
            echo -e "${GREEN}✅ 检查完毕：内核日志级别已是 info，无需修改。${NC}"
        fi
    fi

    echo -e "${YELLOW}🚀 正在通过 Systemd 构建本地日志导流守护服务 (xboard-tunnel)...${NC}"
    
    # 清理旧的 nohup 残留进程
    pkill -f "journalctl -u xboard-node" > /dev/null 2>&1

    # 直接用 Systemd 动态托管这个导流任务
    cat << 'TUNNELEOF' > /etc/systemd/system/xboard-tunnel.service
[Unit]
Description=Xboard 本地日志重定向导流工具
After=network.target xboard-node.service

[Service]
Type=simple
User=root
# 使用 bash -c 包装重定向，并加入垫底历史日志读取，避免无新日志时闪退
ExecStart=/bin/bash -c '/usr/bin/journalctl -u xboard-node --since "1 hour ago" -f --no-pager > /home/xboard_log/xboard.log'
# 核心控制：无论什么原因挂掉，5秒内必定强制重启
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
TUNNELEOF

    systemctl daemon-reload
    systemctl enable --now xboard-tunnel
    systemctl restart xboard-tunnel
    
    sleep 2
    if systemctl is-active --quiet xboard-tunnel; then
        echo -e "${GREEN}✅ 本地日志强力守护管道架设成功且运行中！${NC}"
    else
        echo -e "${RED}❌ 警告：本地导流守护服务未能正常启动，请检查本地 xboard-node 的 Systemd 服务名是否正确。${NC}"
    fi
}

# 1. 设置本机为主控
set_as_master() {
    init_public_dir
    check_rsyslog
    touch /home/xboard_log/xboard_vps_b.log
    chmod 666 /home/xboard_log/xboard_vps_b.log
    
    echo -e "\n${BLUE}--- 请配置 Telegram 机器人联动参数 ---${NC}"
    read -p "请输入你的 Telegram Bot Token: " TG_TOKEN
    read -p "请输入接收者 Chat ID (多个用逗号隔开，例如 6053576171,5525443144): " TG_IDS
    
    if [ -z "$TG_TOKEN" ] || [ -z "$TG_IDS" ]; then
        echo -e "${RED}❌ 错误：Token 或 Chat ID 不能为空，配置失败！${NC}"
        exit 1
    fi
    
    PYTHON_CHAT_IDS=$(echo "$TG_IDS" | sed 's/,/, /g')
    
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
    
    PY_SUB_VER=$(python3 -c 'import sys; print(sys.version_info.minor)')
    echo -e "${BLUE}[系统嗅探] 当前本地内置 Python 版本为: 3.${PY_SUB_VER}${NC}"
    
    if [ "$PY_SUB_VER" -lt 9 ]; then
        echo -e "${YELLOW}⚠️  自动匹配 3.8 兼容版审计源码...${NC}"
        GITHUB_RAW_URL="$URL_PYTHON_38"
    else
        echo -e "${GREEN}✨ 自动匹配标准版高阶审计源码...${NC}"
        GITHUB_RAW_URL="$URL_PYTHON_HIGH"
    fi
    
    echo -e "${YELLOW}📥 正在从 GitHub 获取最新审计代码...${NC}"
    mkdir -p /root/xboard_log
    curl -sS -o /root/xboard_log/xboard_monitor.py "$GITHUB_RAW_URL"
    
    if [ ! -f "/root/xboard_log/xboard_monitor.py" ] || [ ! -s "/root/xboard_log/xboard_monitor.py" ]; then
        echo -e "${RED}❌ 错误：从 GitHub 下载源码失败！${NC}"
        exit 1
    fi
    
    echo -e "${YELLOW}🔧 正在热注入局部安全变量 (Token / Chat IDs)...${NC}"
    sed -i "s/TG_BOT_TOKEN = \".*\"/TG_BOT_TOKEN = \"$TG_TOKEN\"/g" /root/xboard_log/xboard_monitor.py
    sed -i "s/TG_CHAT_IDS = \[.*\]/TG_CHAT_IDS = \[$PYTHON_CHAT_IDS\]/g" /root/xboard_log/xboard_monitor.py
    
    echo -e "${YELLOW}📦 正在架设系统原生的 Python 3 独立虚拟环境...${NC}"
    apt-get update > /dev/null 2>&1
    apt-get install python3-venv python3-dev -y > /dev/null 2>&1
    rm -rf /root/xboard_log/xboard_env
    python3 -m venv /root/xboard_log/xboard_env
    /root/xboard_log/xboard_env/bin/pip install --upgrade pip > /dev/null
    /root/xboard_log/xboard_env/bin/pip install python-telegram-bot > /dev/null
    
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
    echo -e "${GREEN}🎉 本机【主控中心】一键配置成功！默认支持本地和 B 节点。${NC}"
    echo -e "${YELLOW}💡 后续如需添加更多节点，再次运行本脚本选择选项 3 即可！${NC}"
    echo -e "${GREEN}=========================================${NC}"
    exit 0
}

# 2. 设置本机为客户端
set_as_client() {
    init_public_dir
    check_rsyslog
    
    echo -e "\n${BLUE}--- 请配置客户端网络远程推送参数 ---${NC}"
    read -p "请输入主控机 (VPS-A) 的公网 IP: " MASTER_IP
    read -p "请为当前节点指定标识 (可以直接填当前机公网 IP，也可以起英文代号如 node-c): " USER_INPUT
    
    if [ -z "$MASTER_IP" ] || [ -z "$USER_INPUT" ]; then
        echo -e "${RED}❌ 错误：参数不能为空，配置失败！${NC}"
        exit 1
    fi
    
    if [[ "$USER_INPUT" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        CLEAN_TAG=$(echo "$USER_INPUT" | sed 's/\.//g')
        NODE_DISPLAY="ip-${USER_INPUT//./-}"
    else
        CLEAN_TAG=$(echo "$USER_INPUT" | tr 'A-Z' 'a-z' | sed 's/[^a-z0-9-]//g')
        NODE_DISPLAY="$CLEAN_TAG"
    fi
    
    FULL_TAG="xboard-${CLEAN_TAG}"
    
    start_journal_tunnel
    
    echo -e "${YELLOW}📨 正在向系统植入 rsyslog 网络远程投递规则 (Tag: ${FULL_TAG})...${NC}"
    cat << LOGEOF > /etc/rsyslog.d/xboard-forward.conf
module(load="imfile")

input(type="imfile"
      File="/home/xboard_log/xboard.log"
      Tag="${FULL_TAG}"
      Severity="info"
      Facility="local7")

local7.* @${MASTER_IP}:514
LOGEOF
    
    echo -e "${YELLOW}🔒 正在破除系统的 rsyslog 跨目录降权阻锁...${NC}"
    if [ -f "/etc/rsyslog.conf" ]; then
        sed -i 's/^\$PrivDropToUser/# \$PrivDropToUser/g' /etc/rsyslog.conf
        sed -i 's/^\$PrivDropToGroup/# \$PrivDropToGroup/g' /etc/rsyslog.conf
    fi
    
    systemctl restart rsyslog
    
    echo -e "${GREEN}=========================================${NC}"
    echo -e "${GREEN}✅ 本机【客户端推送流】一键配置成功！${NC}"
    echo -e "${YELLOW}💡 内部识别标识: ${FULL_TAG}${NC}"
    echo -e "${YELLOW}💡 提示：去主控机运行脚本选择 3，直接输入同样的 [ ${USER_INPUT} ] 即可无缝对接！${NC}"
    echo -e "${GREEN}=========================================${NC}"
    exit 0
}

# 3. 💡【双输入自适应核心】在主控机上一键动态追加新监控节点
add_new_node_on_master() {
    if [ ! -f "/root/xboard_log/xboard_monitor.py" ]; then
        echo -e "${RED}❌ 错误：未检测到主控端环境，请先选择选项 1 安装主控中心！${NC}"
        exit 1
    fi
    
    echo -e "\n${BLUE}--- 📡 动态添加新客户端节点 (支持 IP 或代号输入) ---${NC}"
    read -p "请输入您在客户端填写的 IP 或英文代号: " MASTER_INPUT
    
    if [ -z "$MASTER_INPUT" ]; then
        echo -e "${RED}❌ 错误：输入不能为空！${NC}"
        exit 1
    fi
    
    if [[ "$MASTER_INPUT" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        CLEAN_TAG=$(echo "$MASTER_INPUT" | sed 's/\.//g')
        NODE_NAME="vps-${MASTER_INPUT}"
    else
        CLEAN_TAG=$(echo "$MASTER_INPUT" | tr 'A-Z' 'a-z' | sed 's/[^a-z0-9-]//g')
        NODE_NAME="vps-${CLEAN_TAG}"
    fi
    
    FULL_TAG="xboard-${CLEAN_TAG}"
    LOG_FILE_NAME="xboard_vps_${CLEAN_TAG}.log"
    
    echo -e "${YELLOW}⏳ 正在动态追加主控 rsyslog 分流规则...${NC}"
    if grep -q "$FULL_TAG" /etc/rsyslog.d/xboard-recv.conf; then
        echo -e "${YELLOW}ℹ️  rsyslog 中已存在此节点的过滤规则，跳过写入。${NC}"
    else
        touch /home/xboard_log/${LOG_FILE_NAME}
        chmod 666 /home/xboard_log/${LOG_FILE_NAME}
        
        sed -i "1i if \$msg contains '${FULL_TAG}' or \$rawmsg contains '${FULL_TAG}' then {\n    action(type=\"omfile\" file=\"/home/xboard_log/${LOG_FILE_NAME}\")\n    stop\n}" /etc/rsyslog.d/xboard-recv.conf
        systemctl restart rsyslog
        echo -e "${GREEN}✅ 主控层网络分流规则建立完毕！${NC}"
    fi

    echo -e "${YELLOW}⏳ 正在动态无损注入 Python 核心监控数组...${NC}"
    if grep -q "${LOG_FILE_NAME}" /root/xboard_log/xboard_monitor.py; then
        echo -e "${YELLOW}ℹ️  Python 核心中已并联此节点，跳过注入。${NC}"
    else
        python3 -c "
file_path = '/root/xboard_log/xboard_monitor.py'
with open(file_path, 'r', encoding='utf-8') as f: lines = f.readlines()
start = False
for i, l in enumerate(lines):
    if 'LOG_TASKS = [' in l: start = True
    if start and l.strip() == ']':
        lines[i-1] = lines[i-1].rstrip('\r\n') + ',\n'
        lines.insert(i, '    {\"path\": \"/home/xboard_log/${LOG_FILE_NAME}\", \"node_name\": \"${NODE_NAME}\"},\n')
        break
with open(file_path, 'w', encoding='utf-8') as f: f.writelines(lines)
"
        systemctl restart xboard-audit
        echo -e "${GREEN}✅ Python 跨多路高并发并联监听机制注入成功！${NC}"
    fi
    
    echo -e "${GREEN}=========================================${NC}"
    echo -e "${GREEN}🎉 动态追加成功！新节点 [${NODE_NAME}] 已加入实时审计！${NC}"
    echo -e "${YELLOW}💡 提示：您可以使用 journalctl -u xboard-audit -f 实时查看多端合并流. ${NC}"
    echo -e "${GREEN}=========================================${NC}"
    exit 0
}

# 菜单选择逻辑
echo -e "${BLUE}请选择当前 VPS 的操作选项:${NC}"
echo -e "  ${GREEN}1.${NC} 设置本机为 ${GREEN}[主控机 (VPS-A)]${NC} (初次安装主控中心)"
echo -e "  ${GREEN}2.${NC} 设置本机为 ${GREEN}[客户端 (VPS-B/C/D...)]${NC} (配置日志外发投递)"
echo -e "  ${BLUE}3.${NC} 【主控端专用】${BLUE}动态一键追加新的监控客户端节点 (支持输入 IP 或代号)${NC}"
echo -e "  ${RED}4.${NC} 退出"
echo -e "${BLUE}────────────────────────────────────────────────────${NC}"
read -p "请输入对应数字 [1-4]: " CHOSEN

case $CHOSEN in
    1)
        set_as_master
        ;;
    2)
        set_as_client
        ;;
    3)
        add_new_node_on_master
        ;;
    4)
        echo -e "${YELLOW}已取消。${NC}"
        exit 0
        ;;
    *)
        echo -e "${RED}❌ 错误输入，请输入 1-4 之间的数字。${NC}"
        exit 1
        ;;
esac