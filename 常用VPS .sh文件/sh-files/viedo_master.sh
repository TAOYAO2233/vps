#!/bin/bash

# ==============================================================================
# 脚本名称: Video Master Tool v8.0 (Smart Naming Edition)
# 更新日志:
#   v8.0: 优化拼接命名规则 -> "提取的日期 + 原始标题(去除时间戳)"
# ==============================================================================

# --- 全局配置 ---
LIST_FILE="concat_list.txt"
CONFIG_FILE="$HOME/.video_master_last_path"
CURRENT_WORK_DIR=""
ALL_VIDEO_EXTS="@(flv|mp4|mkv|ts|mov|avi|wmv|m4v)"

# --- 基础工具 ---

check_dependency() {
    if ! command -v ffmpeg &> /dev/null; then
        echo -e "\033[31m[Critical Error] 未检测到 FFmpeg。请先安装。\033[0m"
        exit 1
    fi
}

save_config() { echo "$1" > "$CONFIG_FILE"; }

init_workspace() {
    local mode="$1"
    if [ "$mode" != "force" ] && [ -f "$CONFIG_FILE" ]; then
        read -r saved_path < "$CONFIG_FILE"
        if [ -d "$saved_path" ]; then
            cd "$saved_path" || return
            CURRENT_WORK_DIR=$(pwd)
            return
        fi
    fi

    clear
    echo "=========================================="
    echo "      Video Master - 路径设置             "
    echo "=========================================="
    local default_path=$(pwd)
    [ -f "$CONFIG_FILE" ] && read -r saved_path < "$CONFIG_FILE" && [ -d "$saved_path" ] && default_path="$saved_path"
    
    read -e -i "$default_path" -p "路径 > " user_path
    [ -z "$user_path" ] && user_path="$default_path"

    if [ ! -d "$user_path" ]; then
        echo -e "\033[31m[Error] 路径不存在\033[0m"; read -p "按回车重试..."; init_workspace "force"; return
    fi

    cd "$user_path" || { echo "无法进入目录"; exit 1; }
    save_config "$(pwd)"
    CURRENT_WORK_DIR=$(pwd)
}

# --- 核心: 动态文件扫描 ---

get_file_list() {
    target_ext="$1"
    files=()
    shopt -s extglob nullglob nocaseglob
    if [ "$target_ext" == "all" ]; then
        files=( *.$ALL_VIDEO_EXTS )
    else
        files=( *."$target_ext" )
    fi
    shopt -u extglob nullglob nocaseglob
}

print_file_list_ui() {
    get_file_list "$1"
    if [ ${#files[@]} -eq 0 ]; then
        echo -e "\033[33m[Warning] 当前目录下没有找到类型为 [$1] 的文件。\033[0m" >&2
        return 1
    fi

    echo "------------------------------------------------" >&2
    echo "   筛选类型: [${1^^}]" >&2
    echo "   文件列表 (按名称排序)" >&2
    echo "------------------------------------------------" >&2
    local idx=1
    for f in "${files[@]}"; do
        size=$(du -h "$f" | cut -f1)
        printf "[%2d] %s \t(%s)\n" "$idx" "$f" "$size" >&2
        ((idx++))
    done
    echo "------------------------------------------------" >&2
    return 0
}

parse_selection() {
    selected_indices=()
    for part in $1; do
        if [[ "$part" =~ ^[0-9]+-[0-9]+$ ]]; then
            start=${part%-*}
            end=${part#*-}
            for ((i=start; i<=end; i++)); do selected_indices+=("$i"); done
        elif [[ "$part" =~ ^[0-9]+$ ]]; then
            if [ "$part" -ne 0 ]; then selected_indices+=("$part"); fi
        fi
    done
}

select_extension_menu() {
    echo "----------------------------" >&2
    echo "请选择目标文件的格式:" >&2
    echo " 1. flv" >&2
    echo " 2. mp4" >&2
    echo " 3. mkv" >&2
    echo " 4. 所有视频 (All)" >&2
    echo "----------------------------" >&2
    read -p "选择 [1-4] (默认 All): " ext_choice
    case $ext_choice in
        1) echo "flv";;
        2) echo "mp4";;
        3) echo "mkv";;
        *) echo "all";;
    esac
}

# --- 业务功能 ---

task_concat() {
    echo -e "\033[36m[模式 1: 拼接视频]\033[0m"
    local target_type=$(select_extension_menu)
    
    print_file_list_ui "$target_type" || { read -p "按回车返回..."; return; }
    
    read -p "请输入文件编号 (输入 0 或回车返回): " selection
    if [ -z "$selection" ] || [ "$selection" == "0" ]; then return; fi

    parse_selection "$selection"
    if [ ${#selected_indices[@]} -lt 2 ]; then echo "[Info] 需至少2个文件"; read -p "回车继续..."; return; fi

    > "$LIST_FILE"
    count=0
    first_file=""

    for idx in "${selected_indices[@]}"; do
        real_idx=$((idx-1))
        file="${files[$real_idx]}"
        if [ -n "$file" ]; then
            echo "file '$PWD/$file'" >> "$LIST_FILE"
            if [ $count -eq 0 ]; then first_file="$file"; fi
            ((count++))
        fi
    done

    # ==========================
    # v8.0 新增: 智能命名逻辑
    # ==========================
    
    # 1. 获取纯文件名 (去除后缀)
    base_name="${first_file%.*}"
    
    # 2. 提取日期 (格式如 2025-12-25 或 20251225)
    date_str=$(echo "$base_name" | grep -oE '[0-9]{4}[-_.]?[0-9]{2}[-_.]?[0-9]{2}' | head -n 1)
    [ -z "$date_str" ] && date_str="Merged_$(date +%Y%m%d)"
    
    # 3. 提取标题 (去除时间戳部分)
    # 逻辑: 使用 sed 去除开头的 [xxxx-xx-xx ...] 块，或者去除 date_str 本身
    # -E 's/^\[[0-9].*?\]//' 匹配以 [数字 开头的第一个中括号块并移除
    title_part=$(echo "$base_name" | sed -E 's/^\[[0-9][^]]*\]//')
    
    # 如果处理后没有变化（说明不是 [时间]标题 格式），则尝试手动去除日期字符串
    if [ "$title_part" == "$base_name" ]; then
        title_part=$(echo "$base_name" | sed "s/$date_str//g" | sed -E 's/^[-_.]+//')
    fi
    
    # 获取输出后缀
    output_ext="${first_file##*.}"
    
    # 4. 组合最终文件名: 日期 + _ + 剩余标题
    # 如果 title_part 不为空，加个下划线连接，否则只用日期
    if [ -n "$title_part" ]; then
        # 移除 title_part 开头可能残留的空格或特殊字符
        output_name="${date_str}_${title_part}.${output_ext}"
    else
        output_name="${date_str}_merged.${output_ext}"
    fi

    # 清理文件名中的双下划线 (美观)
    output_name=$(echo "$output_name" | sed 's/__/_/g')

    echo "------------------------------------------------"
    echo "[Info] 正在拼接 (Stream Copy)..."
    echo "[Info] 源文件: $first_file"
    echo -e "\033[32m[Info] 输出名: $output_name\033[0m"
    echo "------------------------------------------------"
    
    ffmpeg -y -f concat -safe 0 -i "$LIST_FILE" -c copy "$output_name"
    
    [ $? -eq 0 ] && rm "$LIST_FILE" && echo -e "\033[32m[Success] 完成\033[0m"
    read -p "按回车返回..."
}

task_convert() {
    echo -e "\033[36m[模式 2: 转 MP4]\033[0m"
    local target_type=$(select_extension_menu)
    
    print_file_list_ui "$target_type" || { read -p "按回车返回..."; return; }
    
    read -p "请输入文件编号 (输入 0 或回车返回): " selection
    if [ -z "$selection" ] || [ "$selection" == "0" ]; then return; fi

    parse_selection "$selection"
    if [ ${#selected_indices[@]} -eq 0 ]; then return; fi

    for idx in "${selected_indices[@]}"; do
        real_idx=$((idx-1))
        file="${files[$real_idx]}"
        if [ -n "$file" ]; then
            base_name="${file%.*}"
            output_name="${base_name}.mp4"
            echo "转换中: $file -> $output_name"
            ffmpeg -y -i "$file" -c copy -movflags +faststart "$output_name" < /dev/null
        fi
    done
    echo -e "\033[32m[Success] 批量转换结束\033[0m"
    read -p "按回车返回..."
}

task_delete() {
    echo -e "\033[31m[模式 3: 删除文件]\033[0m"
    print_file_list_ui "all" || { read -p "按回车返回..."; return; }
    
    read -p "请输入编号 (输入 0 或回车返回): " selection
    if [ -z "$selection" ] || [ "$selection" == "0" ]; then return; fi

    parse_selection "$selection"
    if [ ${#selected_indices[@]} -eq 0 ]; then return; fi

    del_list=()
    for idx in "${selected_indices[@]}"; do
        real_idx=$((idx-1))
        f="${files[$real_idx]}"
        [ -n "$f" ] && del_list+=("$f")
    done

    echo "----------------------------------------"
    echo "即将删除 ${#del_list[@]} 个文件："
    for f in "${del_list[@]}"; do echo " - $f"; done
    echo "----------------------------------------"

    read -p "输入 'yes' 确认删除: " confirm
    if [ "$confirm" == "yes" ]; then
        for f in "${del_list[@]}"; do rm "$f"; echo "[Deleted] $f"; done
    fi
    read -p "按回车返回..."
}

# --- 主循环 ---
check_dependency
init_workspace

while true; do
    clear
    echo "=========================================="
    echo "      Video Master v8.0 (Smart Rename)    "
    echo "=========================================="
    echo " 当前目录: $CURRENT_WORK_DIR"
    echo "------------------------------------------"
    echo " 1. 拼接视频 (Concat) -> 选择格式"
    echo " 2. 转换 MP4 (Convert)-> 选择格式"
    echo " 3. 删除文件 (Delete) -> 所有视频"
    echo " 4. 切换目录 (Change Dir)"
    echo " 5. 退出 (Exit)"
    echo "=========================================="
    read -p "选择: " opt
    case $opt in
        1) task_concat ;;
        2) task_convert ;;
        3) task_delete ;;
        4) init_workspace "force" ;; 
        5) exit 0 ;;
        *) ;;
    esac
done