#!/bin/bash

# ================= 配置区域 =================
# 指定要处理的文件夹路径
SOURCE_DIR="/home/docker/alist/biliup/backup"
# 设置最大同时运行的转码任务数
MAX_JOBS=2
# 指定日志文件的路径
LOG_FILE="/home/docker/alist/biliup/backup/conversion.log"
# ===========================================

# 检查日志目录是否存在，不存在则创建
LOG_DIR=$(dirname "$LOG_FILE")
[ ! -d "$LOG_DIR" ] && mkdir -p "$LOG_DIR"

# 记录脚本开始执行的时间
echo "=== 转换任务开始于 $(date) ===" >> "$LOG_FILE"

# 定义转码处理函数
process_video() {
    local flv_file="$1"
    local mp4_file="${flv_file%.flv}.mp4"

    echo "[$BASHPID] 启动转码: $flv_file -> $mp4_file" >> "$LOG_FILE"

    # 使用 nice 降低优先级，避免影响系统其他服务
    # 增加 -y 参数自动覆盖可能存在的损坏文件，-hide_banner 减少日志杂讯
    if nice -n 10 ffmpeg -y -hide_banner -loglevel error -i "$flv_file" -c:v libx264 -c:a aac "$mp4_file" >> "$LOG_FILE" 2>&1; then
        # 【关键优化】安全检查：确保新文件存在且大小大于 0
        if [ -s "$mp4_file" ]; then
            rm "$flv_file"
            echo "[$BASHPID] 成功转换并删除原文件: $flv_file" >> "$LOG_FILE"
        else
            echo "[$BASHPID] 错误：转换后的文件为空或不存在，保留原文件: $flv_file" >> "$LOG_FILE"
        fi
    else
        echo "[$BASHPID] 转码失败: $flv_file" >> "$LOG_FILE"
        # 失败时尝试清理可能产生的垃圾文件
        [ -f "$mp4_file" ] && rm "$mp4_file"
    fi
}

# 检查源目录是否存在
if [ ! -d "$SOURCE_DIR" ]; then
    echo "错误：源目录 $SOURCE_DIR 不存在" >> "$LOG_FILE"
    exit 1
fi

# 启用 nullglob，防止没有 .flv 文件时通配符不展开导致报错
shopt -s nullglob

# 遍历文件夹中的所有 .flv 文件
for flv_file in "$SOURCE_DIR"/*.flv; do
    # 构造输出文件的名称
    mp4_file="${flv_file%.flv}.mp4"

    # 检查是否已经存在目标 mp4 文件（避免重复转换）
    if [ -f "$mp4_file" ]; then
        echo "跳过：目标文件已存在 $mp4_file" >> "$LOG_FILE"
        continue
    fi

    # 检查并发数量
    while [ "$(jobs -r | wc -l)" -ge "$MAX_JOBS" ]; do
        sleep 1
    done

    # 【关键优化】将函数放入后台执行 (&)，实现真正的并发
    process_video "$flv_file" &
done

# 等待所有后台任务完成
wait

# 恢复 shell 选项
shopt -u nullglob

# 记录脚本结束执行的时间
echo "=== 转换任务结束于 $(date) ===" >> "$LOG_FILE"
echo "---------------------------------" >> "$LOG_FILE"