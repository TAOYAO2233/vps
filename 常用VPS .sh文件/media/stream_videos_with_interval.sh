#!/bin/bash

# ================= 配置区域 =================
CONFIG_FILE="$HOME/.stream_config"
CACHE_FILE="$HOME/.stream_video_cache" # 新增：元数据缓存文件
FILES_PER_PAGE=10
LOG_DIR="/var/log/stream_logs/"
LOG_FILE="${LOG_DIR}stream_log_$(date +'%Y%m%d_%H%M%S').log"
INTERVAL=10
LOOP=false
# ===========================================

# 支持的视频文件扩展名说明
declare -A FILE_TYPES
FILE_TYPES=(
  [mp4]="MP4 - 通用格式"
  [avi]="AVI - 老旧格式"
  [mkv]="MKV - 多轨道封装"
  [mov]="MOV - Apple格式"
  [flv]="FLV - 流媒体格式"
  [wmv]="WMV - 微软格式"
  [webm]="WebM - Web格式"
)

# 声明关联数组用于存储元数据
declare -A META_DURATION
declare -A META_SIZE

RTMP_URL=""
VIDEO_DIR=""

mkdir -p "$LOG_DIR"

# --- 基础配置函数 (保持不变) ---
get_config_value() {
  local key="$1"
  if [ -f "$CONFIG_FILE" ]; then
    grep "^$key=" "$CONFIG_FILE" | cut -d'=' -f2
  fi
}

set_config_value() {
  local key="$1"
  local value="$2"
  if grep -q "^$key=" "$CONFIG_FILE"; then
    sed -i "s|^$key=.*|$key=$value|" "$CONFIG_FILE"
  else
    echo "$key=$value" >> "$CONFIG_FILE"
  fi
}

initialize_config() {
  if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "配置文件不存在，正在初始化..."
    read -rp "请输入 RTMP 服务器地址 (格式: rtmp://server/live/stream-key): " RTMP_URL
    echo "RTMP_URL=$RTMP_URL" > "$CONFIG_FILE"
    read -rp "请输入视频文件目录: " VIDEO_DIR
    echo "VIDEO_DIR=$VIDEO_DIR" >> "$CONFIG_FILE"
  fi
}

load_config() {
  RTMP_URL=$(get_config_value "RTMP_URL")
  VIDEO_DIR=$(get_config_value "VIDEO_DIR")
}

display_and_ask_update() {
  echo "========================================"
  echo "当前推流地址: $RTMP_URL"
  echo "视频源目录:   $VIDEO_DIR"
  echo "========================================"
  read -rp "是否修改配置？(y/n) [默认n]: " update_choice
  if [[ $update_choice =~ ^[yY]$ ]]; then
    read -rp "请输入新的完整 RTMP 地址: " new_url
    [ -n "$new_url" ] && set_config_value "RTMP_URL" "$new_url"
    read -rp "请输入新的视频目录: " new_dir
    [ -n "$new_dir" ] && set_config_value "VIDEO_DIR" "$new_dir"
    load_config
  fi
}

# --- 扫描文件函数 ---
get_video_files() {
  local selected_ext="$1"
  if [ ! -d "$VIDEO_DIR" ]; then
      echo "错误：目录 $VIDEO_DIR 不存在！"
      return 1
  fi

  echo "正在扫描文件系统..."
  if [[ "$selected_ext" == "all" ]]; then
    mapfile -t VIDEO_FILES < <(find "$VIDEO_DIR" -type f \( -name "*.mp4" -o -name "*.flv" -o -name "*.mkv" -o -name "*.avi" -o -name "*.mov" -o -name "*.wmv" -o -name "*.webm" \) | sort)
  else
    mapfile -t VIDEO_FILES < <(find "$VIDEO_DIR" -type f -name "*.$selected_ext" | sort)
  fi
  
  if [ ${#VIDEO_FILES[@]} -eq 0 ]; then
      return 1
  fi
}

# --- 【核心新增】元数据缓存与获取 ---
load_cache() {
    # 如果缓存文件存在，读取到内存数组中
    if [ -f "$CACHE_FILE" ]; then
        while IFS='|' read -r path size duration; do
            META_SIZE["$path"]="$size"
            META_DURATION["$path"]="$duration"
        done < "$CACHE_FILE"
    fi
}

save_cache() {
    # 将内存数组写入缓存文件
    : > "$CACHE_FILE" # 清空文件
    for path in "${!META_SIZE[@]}"; do
        echo "$path|${META_SIZE[$path]}|${META_DURATION[$path]}" >> "$CACHE_FILE"
    done
}

process_metadata() {
    echo "正在分析视频信息 (时长/大小)..."
    load_cache
    
    local dirty=false
    local total=${#VIDEO_FILES[@]}
    local current=0
    
    for file in "${VIDEO_FILES[@]}"; do
        ((current++))
        # 检查是否已有缓存且文件未修改（这里简单检查路径key是否存在）
        # 如果需要更严谨，可以对比文件修改时间，但会降低速度
        if [[ -z "${META_DURATION[$file]}" || -z "${META_SIZE[$file]}" ]]; then
            # 显示进度条
            echo -ne "\r>> 分析新文件 [$current/$total]: $(basename "$file") ... "
            
            # 1. 获取文件大小 (人类可读, e.g. 1.2G)
            local fsize=$(du -h "$file" | cut -f1)
            
            # 2. 获取时长 (秒)
            local dur_sec=$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$file" 2>/dev/null | cut -d. -f1)
            
            # 格式化时长为 HH:MM:SS
            if [[ "$dur_sec" =~ ^[0-9]+$ ]]; then
                local dur_fmt=$(printf '%02d:%02d:%02d' $((dur_sec/3600)) $(((dur_sec%3600)/60)) $((dur_sec%60)))
            else
                local dur_fmt="未知"
            fi
            
            META_SIZE["$file"]="$fsize"
            META_DURATION["$file"]="$dur_fmt"
            dirty=true
        fi
    done
    
    # 如果有新数据，保存缓存
    if [ "$dirty" = true ]; then
        echo -e "\n保存元数据缓存..."
        save_cache
    else
        echo -e "\r元数据加载完成 (来自缓存)。                "
    fi
}

# --- 菜单逻辑 ---
select_file_type() {
  echo "----------------------------------------"
  echo "选择要扫描的文件类型："
  local index=1
  local ext_keys=("${!FILE_TYPES[@]}")
  
  for ext in "${ext_keys[@]}"; do
    echo "$index) $ext (${FILE_TYPES[$ext]})"
    ((index++))
  done
  echo "$index) 全部类型"
  
  read -rp "请输入选项数字 [默认全部]: " choice
  if [[ -z $choice || $choice -eq $index ]]; then
      get_video_files "all"
  elif [[ $choice =~ ^[0-9]+$ ]] && (( choice > 0 && choice < index )); then
      local selected_ext="${ext_keys[$((choice-1))]}"
      get_video_files "$selected_ext"
  else
      get_video_files "all"
  fi
}

scan_files_loop() {
  while true; do
    select_file_type
    if [ ${#VIDEO_FILES[@]} -gt 0 ]; then
      # 找到文件后，立即处理元数据
      process_metadata
      break
    else
      echo "----------------------------------------"
      echo "警告：在目录 [$VIDEO_DIR] 中未找到任何视频文件！"
      echo "r) 重新选择文件类型"
      echo "c) 修改视频目录路径"
      echo "q) 退出脚本"
      read -rp "请输入选项 [默认r]: " retry_opt
      case $retry_opt in
        c|C)
           read -rp "请输入新的视频目录: " new_dir
           if [ -n "$new_dir" ]; then
               set_config_value "VIDEO_DIR" "$new_dir"
               load_config
               echo "目录已更新..."
           fi
           ;;
        q|Q) exit 0 ;;
        *) echo "重试..." ;;
      esac
    fi
  done
}

# --- 【修改】列表显示 ---
list_and_select_videos() {
  local page=1
  local total_pages
  local num_files=${#VIDEO_FILES[@]}
  
  while true; do
    total_pages=$(( (num_files + FILES_PER_PAGE - 1) / FILES_PER_PAGE ))
    clear
    # 打印表头
    printf "=== 视频列表 (%d/%d 页 | 共 %d 个) ===\n" "$page" "$total_pages" "$num_files"
    printf "%-4s %-10s %-10s %-s\n" "ID" "大小" "时长" "文件名"
    echo "---------------------------------------------------------"
    
    local start=$(( (page - 1) * FILES_PER_PAGE ))
    local end=$(( start + FILES_PER_PAGE - 1 ))
    [ $end -ge $num_files ] && end=$(( num_files - 1 ))

    for i in $(seq "$start" "$end"); do
      local fpath="${VIDEO_FILES[$i]}"
      local fname=$(basename "$fpath")
      local fsize="${META_SIZE[$fpath]}"
      local fdur="${META_DURATION[$fpath]}"
      
      # 如果缓存丢失，显示占位符
      [ -z "$fsize" ] && fsize="-"
      [ -z "$fdur" ] && fdur="-"
      
      # 格式化输出
      printf "%-4s %-10s %-10s %-s\n" "$((i + 1)))" "[$fsize]" "[$fdur]" "$fname"
    done
    
    echo "---------------------------------------------------------"
    echo "[n]下一页 [p]上一页 [a]全选 [q]退出 [1,3-5]选择ID"
    
    read -rp "指令: " input
    case $input in
      q|Q) exit 0 ;;
      a|A) SELECTED_FILES=("${VIDEO_FILES[@]}"); break ;;
      n|N) if (( page < total_pages )); then ((page++)); fi ;;
      p|P) if (( page > 1 )); then ((page--)); fi ;;
      *)
        if [[ "$input" =~ ^[0-9,\-]+$ ]]; then
            IFS=',' read -r -a inputs <<< "$input"
            SELECTED_FILES=()
            for item in "${inputs[@]}"; do
                if [[ $item =~ ([0-9]+)-([0-9]+) ]]; then
                    for ((j=${BASH_REMATCH[1]}; j<=${BASH_REMATCH[2]}; j++)); do
                        idx=$((j-1))
                        if (( idx >= 0 && idx < num_files )); then
                            SELECTED_FILES+=("${VIDEO_FILES[$idx]}")
                        fi
                    done
                else
                    idx=$((item-1))
                    if (( idx >= 0 && idx < num_files )); then
                        SELECTED_FILES+=("${VIDEO_FILES[$idx]}")
                    fi
                fi
            done
            [ ${#SELECTED_FILES[@]} -gt 0 ] && break
        fi
        ;;
    esac
  done
}

ask_loop() {
  read -rp "是否循环推流？(y/n) [默认n]: " loop_choice
  [[ $loop_choice =~ ^[yY]$ ]] && LOOP=true
}

stream_videos() {
  local total_files=${#SELECTED_FILES[@]}
  if [[ $total_files -eq 0 ]]; then echo "未选择文件"; exit 1; fi
  
  echo "----------------------------------------"
  echo "推流模式选择："
  echo "1) 快速模式 (-c copy) : 适合 h264/aac"
  echo "2) 兼容模式 (转码)    : 适合 mkv/hevc 等"
  read -rp "请选择 [默认1]: " stream_mode
  
  local ffmpeg_opts="-c copy"
  if [[ "$stream_mode" == "2" ]]; then
      ffmpeg_opts="-c:v libx264 -preset veryfast -b:v 3000k -maxrate 3000k -bufsize 6000k -c:a aac -b:a 128k -ar 44100"
  fi

  while true; do
    local current_index=1
    for video in "${SELECTED_FILES[@]}"; do
      # 这里不再重复获取时长，直接用缓存
      # 但为了精确推流进度，保留ffprobe获取秒数用于计算百分比
      # 也可以直接解析 META_DURATION 转秒数，但这里重新查一下更稳妥，不影响性能(因为是单文件)
      
      echo -e "\n=== 开始推流 ($current_index/$total_files) ==="
      echo "文件: $(basename "$video")"
      echo "信息: 大小[${META_SIZE[$video]}] 时长[${META_DURATION[$video]}]"
      
      # 重新获取精确秒数用于进度条
      local duration=$(ffprobe -v error -select_streams v:0 -show_entries stream=duration -of default=nk=1:nw=1 "$video" | awk '{printf "%d", $1}')
      [ -z "$duration" ] && duration=0
      local duration_fmt="${META_DURATION[$video]}"
      
      local progress_file=$(mktemp)
      ffmpeg -re -i "$video" $ffmpeg_opts -f flv "$RTMP_URL" -progress "$progress_file" >> "$LOG_FILE" 2>&1 &
      local pid=$!
      
      while kill -0 $pid 2>/dev/null; do
        if [[ -f $progress_file ]]; then
          local ms=$(grep -a "out_time_ms=" "$progress_file" | tail -n 1 | cut -d= -f2)
          if [[ "$ms" =~ ^[0-9]+$ ]]; then
             local sec=$((ms / 1000000))
             local pct=0
             [ $duration -gt 0 ] && pct=$((sec * 100 / duration))
             local elap_fmt=$(printf '%02d:%02d:%02d' $((sec/3600)) $(((sec%3600)/60)) $((sec%60)))
             echo -ne "\r>> 进度: $elap_fmt / $duration_fmt ($pct%)  "
          fi
        fi
        sleep 1
      done
      
      rm -f "$progress_file"
      echo -e "\n完成."
      sleep "$INTERVAL"
      ((current_index++))
    done
    
    if [[ $LOOP == false ]]; then break; fi
    echo "=== 循环播放 ==="
    sleep 2
  done
}

# 执行流程
initialize_config
load_config
display_and_ask_update
scan_files_loop # 包含 process_metadata
list_and_select_videos
ask_loop
stream_videos