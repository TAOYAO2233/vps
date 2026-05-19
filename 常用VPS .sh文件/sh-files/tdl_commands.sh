#!/bin/bash

# 默认起始时间（2020-01-01 00:00:00）
default_start="2020-01-01 00:00:00"

# 时间格式转换为时间戳的函数
date_to_timestamp() {
  date -d "$1" +%s
}

# 获取当前时间和时间戳
current_date=$(date "+%Y-%m-%d %H:%M:%S")
current_timestamp=$(date +%s)

# 默认起始时间戳和当前时间作为截止时间戳
start_timestamp=$(date_to_timestamp "$default_start")
end_timestamp=$current_timestamp

# 选择时间
echo "1. 使用默认时间（$default_start 到 $current_date）"
echo "2. 自定义起始时间和截止时间"
echo "3. 导出全部（不限制时间范围）"
echo "请选择时间选项："
read time_choice

case $time_choice in
  1)
    # 使用默认时间
    start_timestamp=$start_timestamp
    end_timestamp=$end_timestamp
    export_all=false
    ;;
  2)
    # 自定义起始时间和截止时间
    echo "请输入起始时间（默认：$default_start）："
    read start_input
    if [ -z "$start_input" ]; then
      start_timestamp=$start_timestamp
    else
      start_timestamp=$(date_to_timestamp "$start_input")
    fi

    echo "请输入截止时间（默认：$current_date）："
    read end_input
    if [ -z "$end_input" ]; then
      end_timestamp=$end_timestamp
    else
      end_timestamp=$(date_to_timestamp "$end_input")
    fi
    export_all=false
    ;;
  3)
    # 导出全部（不限制时间范围）
    export_all=true
    ;;
  *)
    echo "无效选择，退出脚本。"
    exit 1
    ;;
esac

# 直接输入频道ID
echo "请输入频道ID："
read channel_id

# 设置导出文件名
output_file="/home/tdl/tdl-export.json"

if [ "$export_all" = true ]; then
  # 导出全部聊天记录
  echo "导出全部聊天记录..."
  tdl chat export -c "$channel_id" -o "$output_file" --all
else
  # 时间间隔，默认两个月
  interval=$((60 * 24 * 60 * 60))

  # 文件计数器
  counter=1

  # 导出聊天记录
  start=$start_timestamp
  while [ $start -lt $end_timestamp ]
  do
    end=$((start + interval))
    if [ $end -gt $end_timestamp ]; then
      end=$end_timestamp
    fi

    # 导出命令
    output_file="/home/tdl/tdl-export${counter}.json"
    echo "导出时间段: $(date -d @$start) 到 $(date -d @$end)"
    tdl chat export -c "$channel_id" -o "$output_file" -i "$start,$end" --all

    # 更新起始时间戳
    start=$end
    # 递增计数器
    counter=$((counter + 1))
  done
fi

echo "导出完成。"