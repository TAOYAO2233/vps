#!/bin/bash

# 1. 定义需要配置的环境配置文件
if [ -n "$BASH_VERSION" ]; then
    CONF_FILE="$HOME/.bashrc"
elif [ -n "$ZSH_VERSION" ]; then
    CONF_FILE="$HOME/.zshrc"
else
    CONF_FILE="$HOME/.profile"
fi

# 2. 核心配置块：开启 ls 颜色、文件分类标识，并自定义核心文件类型颜色
# 颜色解析：di(目录)=明亮蓝色; ex(可执行文件)=明亮绿色; fi(普通文件)=常规白色
read -r -d '' COLOR_CONFIG << 'EOF'

# ===== Custom LS Colors and Aliases =====
export EX_COLOR_DIR="di=01;34"
export EX_COLOR_EXE="ex=01;32"
export EX_COLOR_FILE="fi=00;37"
export LS_COLORS="${EX_COLOR_DIR}:${EX_COLOR_EXE}:${EX_COLOR_FILE}:${LS_COLORS}"

# -F 参数：在条目后加上类型标识（目录加 /, 可执行文件加 *, 快捷方式加 @）
# --color=auto：开启自动颜色高亮
alias ls='ls --color=auto -F'
alias ll='ls -l --color=auto -F'
alias la='ls -A --color=auto -F'
# ========================================
EOF

# 3. 检查是否已存在配置，避免重复写入
if ! grep -q "Custom LS Colors and Aliases" "$CONF_FILE"; then
    echo "$COLOR_CONFIG" >> "$CONF_FILE"
    echo -e "\033[32m[+]\033[0m 配置已成功写入 $CONF_FILE"
else
    echo -e "\033[33m[!]\033[0m 配置已存在，无需重复写入。"
fi

# 4. 立即在当前 Shell 会话中生效
source "$CONF_FILE" 2>/dev/null || true
echo -e "\033[32m[+]\033[0m 终端高亮分类配置完成！请重新打开终端或执行 'source $CONF_FILE' 生效。"