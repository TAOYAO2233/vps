#!/bin/bash

# ================= 配置区域 =================
TG_BOT_TOKEN="*************************************"   # 添加机器人token
TG_CHAT_ID="***********"                                         # 添加用户ID
LOG_FILE="/etc/XrayR/access.log"                                # log文件位置
# 聚合时间窗口（秒）
BATCH_INTERVAL=10
# ===========================================

# 初始化状态变量
BUFFER=""
LAST_SEND_TIME=$(date +%s)

# 发送函数：将缓冲区内容一次性推送到 Telegram
flush_buffer() {
    local content="$1"
    if [ -n "$content" ]; then
        # 构建消息头尾
        local message="<b>📊 XrayR 访问审计 (5s聚合)</b>%0A"
        message+="--------------------------------------------------%0A"
        message+="$content"

        # 发送请求 (放入后台运行，避免阻塞主循环)
        curl -s -X POST "https://api.telegram.org/bot$TG_BOT_TOKEN/sendMessage" \
            -d chat_id="$TG_CHAT_ID" \
            -d text="$message" \
            -d parse_mode="HTML" > /dev/null &
    fi
}

echo "正在启动 XrayR 聚合监控 (每 ${BATCH_INTERVAL} 秒刷新)..."

# 使用 tail -F 跟踪日志，grep 过滤
# 注意：必须在循环内部使用 read -t 超时机制来实现定时检查
tail -F -n 0 "$LOG_FILE" | grep --line-buffered "accepted" | while true; do
    
    # ================= 1. 读取与缓冲逻辑 =================
    # read -t 1 表示：尝试读取一行，如果1秒内没有数据，则超时返回非0状态
    # 这让我们可以每秒都有机会检查时间，而不是无限期卡在等待日志上
    if read -r -t 1 line; then
        # --- 纯 Bash 解析 (比 awk 更快，减少每行产生的进程开销) ---
        # 原始格式: 2026/02/10 21:32:44 IP:Port accepted tcp:domain:port [Protocol...]
        
        # 利用 read 将行拆分为数组
        read -r date time src status dest proto rest <<< "$line"

        # 1. 提取 IP (去除端口)
        ip=${src%:*}

        # 2. 提取域名 (去除 tcp: 和 :端口)
        # dest 通常格式为 tcp:google.com:443
        temp_domain=${dest#*:}   # 去除前缀 (tcp:)
        domain=${temp_domain%:*} # 去除后缀 (:443)

        # 3. 提取协议
        # proto 通常格式为 [Shadowsocks_...]
        temp_proto=${proto//[\[\]]/} # 去除方括号
        protocol=${temp_proto%%_*}   # 只保留下划线前部分 (Shadowsocks)

        # 4. 格式化单行条目
        # 使用 code 标签等宽显示，%0A 代表换行
        entry="<code>$time</code> | <code>$ip</code> | $domain | $protocol"

        # 5. 追加到缓冲区
        if [ -z "$BUFFER" ]; then
            BUFFER="$entry"
        else
            BUFFER="${BUFFER}%0A${entry}"
        fi
    fi

    # ================= 2. 计时与发送逻辑 =================
    CURRENT_TIME=$(date +%s)
    TIME_DIFF=$((CURRENT_TIME - LAST_SEND_TIME))

    # 检查是否满足发送条件：(时间到了) 并且 (缓冲区里有东西)
    if [ "$TIME_DIFF" -ge "$BATCH_INTERVAL" ]; then
        if [ -n "$BUFFER" ]; then
            echo "[$(date)] 发送聚合日志..."
            flush_buffer "$BUFFER"
            
            # 清空缓冲区
            BUFFER=""
        fi
        # 更新最后检查时间 (无论是否发送，都重置计时周期)
        LAST_SEND_TIME=$CURRENT_TIME
    fi
    
done