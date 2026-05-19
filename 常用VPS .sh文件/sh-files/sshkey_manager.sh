#!/bin/bash

# ==============================================================================
# 脚本名称: sshkey_manager.sh
# 描述: 具备保姆级引导、防重复添加、智能端口修改与防火墙联动的 SSH 密钥管理脚本
# ==============================================================================

# 定义全局色彩变量
gl_lv='\033[32m'
gl_huang='\033[33m'
gl_bai='\033[0m'
gl_kjlan='\033[96m'
gl_hong='\033[31m'

# 初始化环境函数
init_env() {
    mkdir -p "${HOME}/.ssh"
    chmod 700 "${HOME}/.ssh"
    touch "${HOME}/.ssh/authorized_keys"
    chmod 600 "${HOME}/.ssh/authorized_keys"
}

# 获取系统 IP 地址函数
ip_address() {
    ipv4_address=$(curl -s https://ipinfo.io/ip && echo)
    if [ -z "$ipv4_address" ]; then
        ipv4_address=$(ip route get 8.8.8.8 2>/dev/null | grep -oP 'src \K[^ ]+' || hostname -I | awk '{print $1}')
    fi
}

# 辅助函数：操作完成暂停提示
break_end() {
    echo ""
    echo -e "${gl_lv}操作完成${gl_bai}"
    echo "按任意键继续..."
    read -n 1 -s -r -p ""
    echo ""
    clear
}

# 智能去重添加公钥的核心函数
safe_add_keys() {
    local key_source_file="$1"
    local added_count=0
    
    # 自动备份
    cp "${HOME}/.ssh/authorized_keys" "${HOME}/.ssh/authorized_keys.bak"
    echo -e "\n${gl_lv}[提示] 已备份原有 authorized_keys 文件至 authorized_keys.bak${gl_bai}"

    # 逐行读取下载下来的临时公钥，防止重复追加
    while IFS= read -r line || [ -n "$line" ]; do
        # 跳过空行或注释行
        [[ -z "${line// }" || "$line" =~ ^# ]] && continue
        
        # 提取公钥的关键特征部分进行比对
        local key_fingerprint=$(echo "$line" | awk '{print $2}')
        if [ -z "$key_fingerprint" ]; then
            key_fingerprint="$line"
        fi

        if grep -q "$key_fingerprint" "${HOME}/.ssh/authorized_keys"; then
            continue
        else
            echo "$line" >> "${HOME}/.ssh/authorized_keys"
            added_count=$((added_count + 1))
        fi
    done < "$key_source_file"

    if [ "$added_count" -gt 0 ]; then
        chmod 600 "${HOME}/.ssh/authorized_keys"
        echo -e "${gl_lv}成功添加了 $added_count 个新的公钥！${gl_bai}"
    else
        echo -e "${gl_huang}没有新的公钥需要添加（可能已全部存在）${gl_bai}"
    fi
}

# 密钥管理核心功能菜单
sshkey_panel() {
    while true; do
        clear
        echo -e "=== ${gl_kjlan}SSH Key 密钥管理面板${gl_bai} ==="
        echo -e "${gl_huang}将会生成密钥对，更安全的方式SSH登录${gl_bai}"
        echo "--------------------------------------------------------"
        echo -e "1. 生成新密钥对                     2. 手动输入已有公钥"
        echo -e "3. 从 GitHub 导入已有公钥           4. 从 URL 导入已有公钥"
        echo -e "5. 编辑公钥文件 (authorized_keys)   6. 查看本机密钥"
        echo "--------------------------------------------------------"
        echo -e "7. ${gl_hong}关闭 SSH 密码登录 (仅限密钥)${gl_bai}     8. 开启 SSH 密码登录"
        echo -e "9. ${gl_lv}修改 SSH 登录端口${gl_bai}"
        echo "--------------------------------------------------------"
        echo "0. 退出脚本"
        echo "--------------------------------------------------------"
        read -e -p "请输入你的选择: " choice
        case $choice in
            1)
                clear
                init_env
                echo "正在生成高安全性 Ed25519 密钥对..."
                ssh-keygen -t ed25519 -C "sshkey_manager@local" -f "${HOME}/.ssh/sshkey" -N ""
                cat "${HOME}/.ssh/sshkey.pub" >> "${HOME}/.ssh/authorized_keys"
                chmod 600 "${HOME}/.ssh/authorized_keys"
                ip_address
                echo ""
                echo -e "🎉 ${gl_lv}私钥信息已成功生成！${gl_bai}"
                echo -e "务必在下方复制出全部内容并保存，可本地新建文件命名为: ${gl_huang}${ipv4_address}_ssh.key${gl_bai}"
                echo "此文件将作为今后 SSH 登录此服务器的唯一凭证。"
                echo "------------------------------------------------------------------------"
                cat "${HOME}/.ssh/sshkey"
                echo "------------------------------------------------------------------------"
                break_end
                ;;
                
            2)
                clear
                init_env
                echo "请输入您已有的 SSH 公钥 (以 ssh-rsa 或 ssh-ed25519 等开头):"
                read -e -p "> " custom_pubkey
                if [ -n "$custom_pubkey" ]; then
                    # 写入临时文件，走安全去重逻辑
                    echo "$custom_pubkey" > /tmp/custom_key.tmp
                    safe_add_keys /tmp/custom_key.tmp
                    rm -f /tmp/custom_key.tmp
                else
                    echo -e "${gl_hong}输入为空，取消操作。${gl_bai}"
                fi
                break_end
                ;;
                
            3)
                clear
                init_env
                echo -e "${gl_huang}操作前，请确保您已在 GitHub 账户中添加了 SSH 公钥：${gl_bai}"
                echo "  1. 登录 https://github.com/settings/keys"
                echo "  2. 点击 New SSH key 或 Add SSH key"
                echo "  3. Title 可随意填写（例如：VPS-Server）"
                echo "  4. 将本地公钥内容（通常是 ~/.ssh/id_ed25519.pub 的全部内容）粘贴到 Key 字段"
                echo "  5. 点击 Add SSH key 完成添加"
                echo ""
                echo "添加完成后，GitHub 会公开提供您的所有公钥，地址为："
                echo "  https://github.com/您的用户名.keys"
                echo "------------------------------------------------------------------------"
                read -e -p "请输入您的 GitHub 用户名（username，不含 @）: " github_user
                
                if [ -n "$github_user" ]; then
                    echo -e "\n此脚本将从远程 URL 拉取 SSH 公钥，并添加到 ${HOME}/.ssh/authorized_keys"
                    echo -e "远程公钥地址：\n  ${gl_kjlan}https://github.com/${github_user}.keys${gl_bai}"
                    
                    # 获取远程密钥到临时文件
                    curl -sSf "https://github.com/${github_user}.keys" > /tmp/gh_keys.tmp 2>/dev/null
                    if [ $? -eq 0 ] && [ -s /tmp/gh_keys.tmp ]; then
                        safe_add_keys /tmp/gh_keys.tmp
                    else
                        echo -e "${gl_hong}获取公钥失败！请检查用户名是否正确，或该 GitHub 账户内是否未添加任何公钥。${gl_bai}"
                    fi
                    rm -f /tmp/gh_keys.tmp
                else
                    echo -e "${gl_hong}用户名不能为空。${gl_bai}"
                fi
                break_end
                ;;

            4)
                clear
                init_env
                read -e -p "请输入公钥文件的完整 URL 路径: " pubkey_url
                if [ -n "$pubkey_url" ]; then
                    echo -e "\n此脚本将从远程 URL 拉取 SSH 公钥，并添加到 ${HOME}/.ssh/authorized_keys"
                    echo -e "远程公钥地址：\n  ${gl_kjlan}${pubkey_url}${gl_bai}"
                    
                    curl -sSf "$pubkey_url" > /tmp/url_keys.tmp 2>/dev/null
                    if [ $? -eq 0 ] && [ -s /tmp/url_keys.tmp ]; then
                        safe_add_keys /tmp/url_keys.tmp
                    else
                        echo -e "${gl_hong}下载失败，请检查 URL 是否有效。${gl_bai}"
                    fi
                    rm -f /tmp/url_keys.tmp
                else
                    echo -e "${gl_hong}URL 不能为空。${gl_bai}"
                fi
                break_end
                ;;

            5)
                clear
                init_env
                echo "即将打开 nano 编辑器编辑 authorized_keys 文件..."
                echo "提示：编辑完成后按 Ctrl+O 保存，按 Ctrl+X 退出。"
                sleep 2
                if command -v nano &>/dev/null; then
                    nano "${HOME}/.ssh/authorized_keys"
                else
                    vi "${HOME}/.ssh/authorized_keys"
                fi
                break_end
                ;;

            6)
                clear
                echo -e "=== ${gl_kjlan}当前服务器已授信的公钥列表 (.ssh/authorized_keys)${gl_bai} ==="
                echo "------------------------------------------------------------------------"
                if [ -f "${HOME}/.ssh/authorized_keys" ] && [ -s "${HOME}/.ssh/authorized_keys" ]; then
                    cat "${HOME}/.ssh/authorized_keys"
                else
                    echo "当前没有配置任何授权公钥。"
                fi
                echo "------------------------------------------------------------------------"
                break_end
                ;;
                
            7)
                clear
                echo -e "${gl_huang}警告：在关闭密码登录之前，请确保你已经成功配置并测试过密钥登录！${gl_bai}"
                read -e -p "你确定要禁用密码登录，切换为纯密钥登录吗？(y/n): " confirm
                if [ "$confirm" = "y" ] || [ "$confirm" = "Y" ]; then
                    if [ -f /etc/ssh/sshd_config ]; then
                        sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
                        sudo sed -i 's/^#\?PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config
                        if [ -d /etc/ssh/sshd_config.d ]; then
                            sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config.d/* 2>/dev/null
                        fi
                        if command -v systemctl &>/dev/null; then
                            sudo systemctl restart ssh || sudo systemctl restart sshd
                        else
                            sudo service ssh restart || sudo service sshd restart
                        fi
                        echo -e "${gl_lv}SSH 密码登录已禁用，现已仅允许密钥验证。${gl_bai}"
                    else
                        echo "未找到 /etc/ssh/sshd_config 配置文件。"
                    fi
                fi
                break_end
                ;;
                
            8)
                clear
                if [ -f /etc/ssh/sshd_config ]; then
                    sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
                    if [ -d /etc/ssh/sshd_config.d ]; then
                        sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config.d/* 2>/dev/null
                    fi
                    if command -v systemctl &>/dev/null; then
                        sudo systemctl restart ssh || sudo systemctl restart sshd
                    else
                        sudo service ssh restart || sudo service sshd restart
                    fi
                    echo -e "${gl_lv}SSH 密码登录已重新开启。${gl_bai}"
                else
                    echo "未找到 /etc/ssh/sshd_config 配置文件。"
                fi
                break_end
                ;;

            9)
                clear
                current_port=$(ss -tlnp | grep -E 'sshd|ssh' | awk '{print $4}' | awk -F':' '{print $nf}' | sed 's/ //g' | tr '\n' ' ' | awk '{print $1}')
                [ -z "$current_port" ] && current_port=$(grep -E "^Port " /etc/ssh/sshd_config | awk '{print $2}')
                [ -z "$current_port" ] && current_port="22"

                echo -e "当前 SSH 服务的运行端口为: ${gl_huang}${current_port}${gl_bai}"
                read -e -p "请输入你想要设置的新 SSH 端口 (建议范围 1024-65535): " new_port
                
                if [[ "$new_port" =~ ^[0-9]+$ ]] && [ "$new_port" -ge 1 ] && [ "$new_port" -le 65535 ]; then
                    echo "正在尝试将端口修改为 $new_port ..."
                    
                    if command -v ufw &>/dev/null && sudo ufw status | grep -q "Status: active"; then
                        echo "检测到 UFW 防火墙处于激活状态，正在放行端口 $new_port/tcp ..."
                        sudo ufw allow "$new_port"/tcp
                    elif command -v firewall-cmd &>/dev/null && sudo firewall-cmd --state &>/dev/null; then
                        echo "检测到 Firewalld 防火墙处于激活状态，正在放行端口 $new_port/tcp ..."
                        sudo firewall-cmd --permanent --add-port="$new_port"/tcp
                        sudo firewall-cmd --reload
                    fi

                    if [ -f /etc/ssh/sshd_config ]; then
                        sudo sed -i 's/^#\?Port.*/Port '"$new_port"'/' /etc/ssh/sshd_config
                        if command -v systemctl &>/dev/null; then
                            sudo systemctl restart ssh || sudo systemctl restart sshd
                        else
                            sudo service ssh restart || sudo service sshd restart
                        fi
                        echo -e "${gl_lv}SSH 端口已成功修改为 $new_port ！${gl_bai}"
                        echo -e "${gl_huang}请注意：为了防止连接中断，请开一个新终端测试新端口连接，确定成功前千万别关闭当前窗口！${gl_bai}"
                    else
                        echo "未找到 /etc/ssh/sshd_config 配置文件。"
                    fi
                else
                    echo -e "${gl_hong}输入无效！请输入 1 至 65535 之间的纯数字。${gl_bai}"
                fi
                break_end
                ;;
                
            0)
                clear
                echo "感谢使用！"
                exit 0
                ;;
                
            *)
                echo "无效的选择，请重新输入。"
                sleep 1
                ;;
        esac
    done
}

# 运行脚本
sshkey_panel