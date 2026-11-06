#!/usr/bin/env bash

# 为当前用户的 Bash 或 Zsh 配置 GNU ls 颜色与常用别名。
# 可用 bash lscolorsetup.sh 安装，也可通过管道交给 bash 执行。

if [[ -z ${BASH_VERSION:-} ]]; then
    printf '请用 Bash 执行此脚本。\n' >&2
    return 1 2>/dev/null || exit 1
fi
if [[ -n ${BASH_SOURCE[0]-} && ${BASH_SOURCE[0]} != "$0" ]]; then
    printf '请用 Bash 执行此脚本，不要用 source 加载它。\n' >&2
    return 1 2>/dev/null || exit 1
fi

set -euo pipefail

START_MARKER='# ===== Custom LS Colors and Aliases ====='
END_MARKER='# ========================================'

case "${1:-}" in
    '') ACTION=install ;;
    -u|--uninstall) ACTION=uninstall ;;
    *)
        printf '用法：bash %s [--uninstall]\n' "$0" >&2
        exit 2
        ;;
esac

LOGIN_SHELL="${SHELL:-}"
case "${LOGIN_SHELL##*/}" in
    bash) CONF_FILE="$HOME/.bashrc" ;;
    zsh) CONF_FILE="$HOME/.zshrc" ;;
    *)
        printf '仅支持 Bash 和 Zsh；当前登录 Shell：%s\n' "${LOGIN_SHELL:-未设置}" >&2
        exit 1
        ;;
esac

# 返回 0：完整区块；1：没有区块；2：标记缺失、重复或顺序错误。
block_state() {
    local line line_no=0 starts=0 ends=0 start_line=0 end_line=0
    while IFS= read -r line || [[ -n $line ]]; do
        ((line_no += 1))
        if [[ $line == "$START_MARKER" ]]; then
            ((starts += 1))
            start_line=$line_no
        elif [[ $line == "$END_MARKER" ]]; then
            ((ends += 1))
            end_line=$line_no
        fi
    done < "$CONF_FILE"
    if (( starts == 0 && ends == 0 )); then return 1; fi
    if (( starts == 1 && ends == 1 && start_line < end_line )); then return 0; fi
    return 2
}

backup_config() {
    BACKUP_FILE="${CONF_FILE}.bak.$(date +%Y%m%d%H%M%S).$$"
    cp -p -- "$CONF_FILE" "$BACKUP_FILE"
    printf '已备份：%s\n' "$BACKUP_FILE"
}

if [[ $ACTION == uninstall ]]; then
    if [[ ! -f $CONF_FILE ]]; then
        printf '配置文件不存在，无需卸载：%s\n' "$CONF_FILE"
        exit 0
    fi
    if block_state; then
        :
    else
        state=$?
        if (( state == 1 )); then
            printf '未找到本脚本的配置：%s\n' "$CONF_FILE"
            exit 0
        fi
        printf '配置区块标记不完整，已停止卸载：%s\n' "$CONF_FILE" >&2
        exit 1
    fi

    backup_config
    temp_file=$(mktemp)
    if {
        skipping=0
        while IFS= read -r line || [[ -n $line ]]; do
            if [[ $line == "$START_MARKER" ]]; then
                skipping=1
                continue
            fi
            if [[ $line == "$END_MARKER" ]]; then
                skipping=0
                continue
            fi
            if (( ! skipping )); then printf '%s\n' "$line"; fi
        done < "$CONF_FILE"
    } > "$temp_file" && cat -- "$temp_file" > "$CONF_FILE"; then
        rm -f -- "$temp_file"
        printf '已从 %s 移除配置。重新打开终端后生效。\n' "$CONF_FILE"
    else
        rm -f -- "$temp_file"
        printf '卸载失败；可用备份恢复：%s\n' "$BACKUP_FILE" >&2
        exit 1
    fi
    exit 0
fi

if [[ ! -e $CONF_FILE ]]; then
    touch -- "$CONF_FILE"
fi
if block_state; then
    printf '配置已存在于 %s，无需重复写入。\n' "$CONF_FILE"
    exit 0
else
    state=$?
    if (( state != 1 )); then
        printf '配置区块标记不完整，已停止安装：%s\n' "$CONF_FILE" >&2
        exit 1
    fi
fi

backup_config
if cat <<'EOF' >> "$CONF_FILE"; then

# ===== Custom LS Colors and Aliases =====
# 放在现有 LS_COLORS 后面，使目录和可执行文件颜色优先生效；重复加载不追加。
case ":${LS_COLORS-}" in
    *":di=01;34:ex=01;32") ;;
    *) export LS_COLORS="${LS_COLORS:+${LS_COLORS%:}:}di=01;34:ex=01;32" ;;
esac

alias ls='ls --color=auto -F --group-directories-first'
alias ll='ls -lh --color=auto -F --group-directories-first'
alias la='ls -A --color=auto -F --group-directories-first'
alias lla='ls -lah --color=auto -F --group-directories-first'
# ========================================
EOF
    printf '已写入 %s。重新打开终端后生效。\n' "$CONF_FILE"
else
    printf '写入失败；可用备份恢复：%s\n' "$BACKUP_FILE" >&2
    exit 1
fi
