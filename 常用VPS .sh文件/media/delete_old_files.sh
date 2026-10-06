#!/bin/bash

# 指定多个文件夹路径
TARGET_DIRS=(
  "/storage1024/docker/alist/bililive/videos/斗鱼/QuQu丶太常规/"
  "/storage1024/docker/alist/bililive/videos/斗鱼/玩机器丶Machine/"
  "/storage1024/docker/alist/bililive/videos/斗鱼/超级小桀/"
  
)  # 在此处添加你要处理的文件夹路径

# 指定日志文件路径
LOG_FILE="/root/shjiaoben/delete_log.txt"

# 指定多个关键词
KEYWORDS=("keyword1" "keyword2" "keyword3")  # 将此处的关键词替换为你的关键词

# 获取当前时间
current_time=$(date +"%Y-%m-%d %H:%M:%S")

# 删除多个文件夹中的修改时间超过25小时的文件，并记录到日志
for TARGET_DIR in "${TARGET_DIRS[@]}"; do
  find "$TARGET_DIR" -type f -mmin +1800 -exec sh -c '
    for file do
      echo "$0: 删除文件: $file" >> '"$LOG_FILE"'
      rm -f "$file"
    done
  ' "$current_time" {} +
done

# 检查并删除包含多个关键词的文件
for TARGET_DIR in "${TARGET_DIRS[@]}"; do
  for keyword in "${KEYWORDS[@]}"; do
    find "$TARGET_DIR" -type f -name "*$keyword*" -exec sh -c '
      for file do
        echo "$0: 删除包含关键词 $1 的文件: $file" >> '"$LOG_FILE"'
        rm -f "$file"
      done
    ' "$current_time" "$keyword" {} +
  done
done

# 如果需要删除空文件夹，可以取消注释下面一行
# for TARGET_DIR in "${TARGET_DIRS[@]}"; do
#   find "$TARGET_DIR" -type d -empty -exec rmdir {} \;
# done
