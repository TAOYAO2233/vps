#!/bin/bash

# 保存配置文件路径
config_file="$HOME/.yt_dlp_config"

# 如果配置文件存在，读取上次的自定义文件夹路径
if [ -f "$config_file" ]; then
    source "$config_file"
else
    last_custom_dirs=()
fi

# 颜色代码
GREEN='\033[0;32m'
RED='\033[0;31m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # 没有颜色

# 获取当前安装的 yt-dlp 版本
current_version=$(yt-dlp --version 2>/dev/null)

# 获取最新版本号
latest_version=$(curl -s https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest | grep 'tag_name' | cut -d '"' -f 4)

# 版本比较函数
version_greater_or_equal() {
    dpkg --compare-versions "$1" ge "$2"
}

# 检查是否需要更新
if [ -n "$current_version" ] && [ -n "$latest_version" ]; then
    echo -e "${GREEN}当前 yt-dlp 版本: $current_version${NC}"
    echo -e "${GREEN}最新 yt-dlp 版本: $latest_version${NC}"

    if ! version_greater_or_equal "$latest_version" "$current_version"; then
        echo -e "${RED}发现新版本!${NC}"
        read -p "是否要更新到最新版本? [y/N]: " update_choice
        if [[ "$update_choice" =~ ^[Yy]$ ]]; then
            echo -e "${GREEN}正在更新 yt-dlp...${NC}"
            sudo wget https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -O /usr/local/bin/yt-dlp
            sudo chmod a+rx /usr/local/bin/yt-dlp
            echo -e "${GREEN}更新完成。${NC}"
        else
            echo -e "${RED}跳过更新。${NC}"
        fi
    else
        echo -e "${GREEN}你已经拥有最新版本。${NC}"
    fi
else
    echo -e "${RED}无法获取版本信息。${NC}"
fi

# 分隔线
divider="--------------------------------------------"

# 获取用户输入的视频URL
echo -e "${BLUE}$divider"
read -e -p "请输入视频URL: " video_url
echo -e "$divider${NC}"

# 显示视频的所有可用格式
echo -e "${GREEN}获取可用格式列表中...${NC}"
yt-dlp -F "$video_url"
echo -e "${BLUE}$divider${NC}"

# 选择格式
read -p "请输入你要下载的格式代码（如：22），或者直接回车选择默认最佳画质和音质: " format_code
echo -e "$divider"

# 如果用户没有输入格式代码，默认选择最佳视频和音频并合并为MP4格式
if [ -z "$format_code" ]; then
    format_code="bv*+ba"  # 最佳视频和最佳音频组合
    merge_format="--merge-output-format mp4"
else
    merge_format=""
fi

# 提供下载文件夹的选项
while true; do
    echo -e "${YELLOW}请选择下载文件夹:${NC}"
    echo -e "${YELLOW}0. 添加新的自定义文件夹${NC}"
    echo -e "${YELLOW}D. 删除自定义文件夹${NC}"

    # 显示已保存的自定义文件夹选项
    i=1
    for dir in "${last_custom_dirs[@]}"; do
        echo -e "${YELLOW}$i. $dir${NC}"
        ((i++))
    done

    echo -e "${YELLOW}按下回车键默认使用当前文件夹${NC}"
    echo -e "$divider"

    # 选择选项
    read -e -p "请输入选项编号: " option

    # 根据选项设置下载文件夹路径
    if [ -z "$option" ]; then
        download_dir=$(pwd)
        break
    elif [ "$option" -eq 0 ]; then
        read -p "请输入新的自定义下载文件夹的路径: " new_dir
        last_custom_dirs+=("$new_dir")
    elif [[ "$option" == "D" || "$option" == "d" ]]; then
        if [ ${#last_custom_dirs[@]} -eq 0 ]; then
            echo -e "${RED}没有自定义文件夹可以删除。${NC}"
        else
            echo -e "${YELLOW}选择要删除的自定义文件夹:${NC}"
            i=1
            for dir in "${last_custom_dirs[@]}"; do
                echo -e "${YELLOW}$i. $dir${NC}"
                ((i++))
            done
            read -p "请输入要删除的文件夹编号: " del_option
            if [ "$del_option" -le "${#last_custom_dirs[@]}" ] && [ "$del_option" -gt 0 ]; then
                unset 'last_custom_dirs[$((del_option-1))]'
                last_custom_dirs=("${last_custom_dirs[@]}") # 重新索引数组
                echo -e "${GREEN}文件夹已删除。${NC}"
            else
                echo -e "${RED}无效的编号，请重新选择。${NC}"
            fi
        fi
    elif [ "$option" -le "${#last_custom_dirs[@]}" ]; then
        download_dir="${last_custom_dirs[$((option-1))]}"
        break
    else
        echo -e "${RED}无效的选项，请重新选择。${NC}"
    fi
    echo -e "$divider"
done

# 保存自定义文件夹到配置文件
echo "last_custom_dirs=(\"${last_custom_dirs[@]}\")" > "$config_file"

# 如果文件夹不存在，则创建文件夹
if [ ! -d "$download_dir" ]; then
    echo -e "${GREEN}文件夹不存在，正在创建...${NC}"
    mkdir -p "$download_dir"
fi

# 下载视频到指定文件夹
echo -e "${GREEN}开始下载...${NC}"
# 使用 yt-dlp 的进度条显示下载进度，并根据需要合并格式
yt-dlp -f "$format_code" -o "$download_dir/%(title)s.%(ext)s" --progress "$video_url" $merge_format

echo -e "${GREEN}视频下载完成！已保存至 $download_dir${NC}"
