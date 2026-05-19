#!/bin/bash

# 默认路径
base_path="${1:-/home/tdl/tdl-export}" # 默认路径为 /home/tdl/tdl-export
success_log="success.log"
error_log="error.log"

# 当前日期时间（格式化为 yyyy-mm-dd hh:mm:ss）
timestamp() {
  date +"%Y-%m-%d %H:%M:%S"
}

# 初始化日志文件
> "$success_log"  # 清空成功日志
> "$error_log"    # 清空失败日志

# 处理单个文件的函数
process_file() {
  local file="$1"

  # 顺序执行 tdl forward 命令
  if tdl forward --from "$file" --to https://t.me/taoyao2233 --desc --reconnect-timeout 0 --delay 5s --pool 0; then
    echo "$(timestamp) - Successfully forwarded: $file" >> "$success_log"
  else
    echo "$(timestamp) - Failed to forward: $file" >> "$error_log"
  fi

  # 删除已处理的文件
  if rm -f "$file"; then
    echo "$(timestamp) - Deleted: $file" >> "$success_log"
  else
    echo "$(timestamp) - Failed to delete: $file" >> "$error_log"
  fi
}

# 处理文件
counter=1
while true; do
  # 构造文件名
  file="${base_path}${counter}.json"

  if [ -f "$file" ]; then
    process_file "$file"
  else
    # 文件不存在，结束循环
    echo "$(timestamp) - No more files to process. Stopping at file: $file" >> "$success_log"
    break
  fi

  # 增加计数器
  counter=$((counter + 1))
done

echo "Processing complete. Check $success_log and $error_log for details."
