#!/bin/bash

# ============================================================
# VPS ls 颜色高亮与常用别名一键配置脚本
# 支持: Bash / Zsh，自动检测 Shell，防重复写入，带备份与卸载支持
# ============================================================

set -u

# 1. 自动检测用户的目标 Shell 配置文件
TARGET_FILES=()

# 检查当前用户的登录 Shell
CURRENT_SHELL="${SHELL:-/bin/bash}"

case "$CURRENT_SHELL" in
    */zsh)
        TARGET_FILES+=("$HOME/.zshrc")
        ;;
    */bash)
        TARGET_FILES+=("$HOME/.bashrc")
        ;;
    *)
        TARGET_FILES+=("$HOME/.profile")
        ;;
esac

# 如果是卸载模式
if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "-u" ]; then
    for conf in "${TARGET_FILES[@]}"; do
        if [ -f "$conf" ] && grep -q "# ===== Custom LS Colors and Aliases =====" "$conf"; then
            sed -i '/# ===== Custom LS Colors and Aliases =====/,/# ========================================/d' "$conf"
            printf "\033[32m[+]\033[0m 已从 %s 中移除配置\n" "$conf"
        fi
    done
    exit 0
fi

# 2. 遍历并配置目标文件
for CONF_FILE in "${TARGET_FILES[@]}"; do
    [ -f "$CONF_FILE" ] || touch "$CONF_FILE"

    # 检查是否已存在配置
    if grep -q "# ===== Custom LS Colors and Aliases =====" "$CONF_FILE"; then
        printf "\033[33m[!]\033[0m 配置已存在于 %s，无需重复写入。\n" "$CONF_FILE"
        continue
    fi

    # 首次写入前先备份
    cp "$CONF_FILE" "${CONF_FILE}.bak.$(date +%Y%m%d%H%M%S)"

    # 3. 写入核心配置
    # 说明：
    # - di=01;34 (目录: 高亮蓝)
    # - ex=01;32 (可执行文件: 高亮绿)
    # - 不硬编码普通文件为白色，避免白底/浅色主题看不清
    # - --group-directories-first: 目录排在文件前面
    # - -h: 文件大小人性化显示 (K/M/G)
    cat << 'EOF' >> "$CONF_FILE"

# ===== Custom LS Colors and Aliases =====
export LS_COLORS="di=01;34:ex=01;32:${LS_COLORS:-}"

# 常用别名定义
alias ls='ls --color=auto -F --group-directories-first'
alias ll='ls -lh --color=auto -F --group-directories-first'
alias la='ls -A --color=auto -F --group-directories-first'
alias lla='ls -lah --color=auto -F --group-directories-first'
# ========================================
EOF

    printf "\033[32m[+]\033[0m 配置已成功写入 %s\n" "$CONF_FILE"
done

# 4. 生效提示
printf "\n\033[32m[✓] 终端高亮配置完成！\033[0m\n"
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
    # 如果是用 source 执行的当前脚本，则当前终端直接生效
    for CONF_FILE in "${TARGET_FILES[@]}"; do
        source "$CONF_FILE" 2>/dev/null || true
    done
    printf "\033[36m[*] 当前会话已自动生效。\033[0m\n"
else
    # 直接执行脚本时，子进程无法影响父进程，给出准确提示
    printf "\033[33m[*] 请执行以下命令使其立即生效，或重新连接终端：\033[0m\n"
    for CONF_FILE in "${TARGET_FILES[@]}"; do
        printf "    source %s\n" "$CONF_FILE"
    done
fi
