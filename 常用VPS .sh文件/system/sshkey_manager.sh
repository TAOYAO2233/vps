#!/usr/bin/env bash
# SSH Key Manager. Keep the current SSH session open during configuration changes.
umask 077
set -o pipefail

SSH_CONFIG=/etc/ssh/sshd_config
AUTH_FILE="$HOME/.ssh/authorized_keys"
LOG_FILE=${SSHKEY_MANAGER_LOG:-/var/log/sshkey-manager.log}
BACKUP_DIR=/var/backups/sshkey-manager
TMP_FILES=()
KEY_TMP_DIR=''
KEY_BACKUP_DIR=''
KEY_DEST=''
KEY_TX_ACTIVE=0
TX_DIR=''
TX_ACTIVE=0
FW_ADDED=''
SE_ADDED=''
SERVICE=''

say() { printf '%s\n' "$*"; }
fail() { say "[错误] $*" >&2; return 1; }
pause() { read -r -p '按 Enter 返回菜单...' _ || :; }
confirm() { local answer; read -r -p "$1 (输入 YES 确认): " answer && [[ $answer == YES ]]; }
log_event() {
    local event=$1 result=$2 detail=${3:-}
    if [[ $EUID -eq 0 ]]; then
        install -d -m 700 "${LOG_FILE%/*}" 2>/dev/null || return 0
        printf '%s\tevent=%s\tuser=%s\tresult=%s\ttx=%s\t%s\n' \
            "$(date -u +%FT%TZ)" "$event" "$(id -un)" "$result" "${TX_DIR##*/}" "$detail" >> "$LOG_FILE" 2>/dev/null || :
        chmod 600 "$LOG_FILE" 2>/dev/null || :
    fi
}
cleanup() {
    local t
    if (( KEY_TX_ACTIVE )); then
        rollback_generated_key || say '[严重] 密钥恢复失败；请检查密钥备份目录。' >&2
    fi
    cleanup_key_temp
    for t in "${TMP_FILES[@]}"; do [[ -n $t ]] && rm -f -- "$t"; done
    if (( TX_ACTIVE )); then
        say '[警告] 操作中断，正在恢复 SSH 配置。' >&2
        rollback || say '[严重] 自动恢复失败；请使用控制台和备份目录手工恢复。' >&2
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

require_root() { (( EUID == 0 )) || fail '此操作必须以 root 身份运行（sudo bash 脚本路径）。'; }
init_env() {
    [[ ! -L $HOME/.ssh && ! -L $AUTH_FILE ]] || { fail '拒绝操作符号链接形式的 SSH 目录或授权文件。'; return 1; }
    install -d -m 700 "$HOME/.ssh" || return 1
    [[ -e $AUTH_FILE ]] || ( umask 077; : > "$AUTH_FILE" ) || return 1
    chmod 700 "$HOME/.ssh" && chmod 600 "$AUTH_FILE"
}
backup_auth() {
    local b="${AUTH_FILE}.bak.$(date +%Y%m%d%H%M%S).$$"
    cp -p -- "$AUTH_FILE" "$b" && chmod 600 "$b" || return 1
    say "授权文件备份：$b"
}
cleanup_key_temp() {
    [[ -n $KEY_TMP_DIR ]] || return 0
    rm -f -- "$KEY_TMP_DIR/sshkey" "$KEY_TMP_DIR/sshkey.pub"
    rmdir -- "$KEY_TMP_DIR" 2>/dev/null || :
    KEY_TMP_DIR=''
}
rollback_generated_key() {
    local ok=0
    (( KEY_TX_ACTIVE )) || return 0
    if [[ -f $KEY_BACKUP_DIR/sshkey ]]; then
        mv -f -- "$KEY_BACKUP_DIR/sshkey" "$KEY_DEST" || ok=1
    elif [[ ! -e $KEY_DEST ]]; then :
    elif (( ! KEY_HAD_PRIV )); then rm -f -- "$KEY_DEST" || ok=1
    fi
    if [[ -f $KEY_BACKUP_DIR/sshkey.pub ]]; then
        mv -f -- "$KEY_BACKUP_DIR/sshkey.pub" "$KEY_DEST.pub" || ok=1
    elif [[ ! -e $KEY_DEST.pub ]]; then :
    elif (( ! KEY_HAD_PUB )); then rm -f -- "$KEY_DEST.pub" || ok=1
    fi
    KEY_TX_ACTIVE=0
    (( ok == 0 )) || { fail "密钥恢复未完成，备份位于 $KEY_BACKUP_DIR"; return 1; }
    rmdir -- "$KEY_BACKUP_DIR" 2>/dev/null || :
}

# Extract key material after optional authorized_keys options. ssh-keygen validates it.
key_data() {
    local line=$1 part type blob
    [[ $line != *$'\r'* && $line != *$'\n'* && ${#line} -le 16384 ]] || return 1
    if [[ $line =~ (^|[[:space:]])(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521))[[:space:]]+([A-Za-z0-9+/=]+)($|[[:space:]]) ]]; then
        type=${BASH_REMATCH[2]}; blob=${BASH_REMATCH[4]}
        printf '%s %s\n' "$type" "$blob"
    else
        return 1
    fi
}
key_fingerprint() {
    local material output tmp
    material=$(key_data "$1") || return 1
    tmp=$(mktemp) || return 1
    printf '%s\n' "$1" > "$tmp"
    ssh-keygen -lf "$tmp" -E sha256 >/dev/null 2>&1 || { rm -f -- "$tmp"; return 1; }
    printf '%s\n' "$material" > "$tmp"
    output=$(ssh-keygen -lf "$tmp" -E sha256 2>/dev/null)
    rm -f -- "$tmp"
    [[ $output == *SHA256:* ]] || return 1
    printf '%s\n' "$output" | awk '{print $2}'
}
key_count() {
    local line count=0
    [[ -f $AUTH_FILE ]] || { say 0; return; }
    while IFS= read -r line || [[ -n $line ]]; do
        key_fingerprint "$line" >/dev/null 2>&1 && ((count+=1))
    done < "$AUTH_FILE"
    say "$count"
}
list_keys() {
    local line fp n=0
    [[ -f $AUTH_FILE ]] || { say '没有授权文件。'; return; }
    while IFS= read -r line || [[ -n $line ]]; do
        fp=$(key_fingerprint "$line" 2>/dev/null) || continue
        ((n+=1))
        printf '[%d] %s %s\n' "$n" "$fp" "${line:0:110}"
    done < "$AUTH_FILE"
    ((n)) || say '没有有效的授权公钥。'
}
add_keys() {
    local source=$1 line fp oldfp existing=0 duplicate=0 added=0 invalid=0 tmp
    init_env || return 1
    tmp=$(mktemp "$HOME/.ssh/.authorized_keys.XXXXXXXX") || return 1
    TMP_FILES+=("$tmp")
    cp -p -- "$AUTH_FILE" "$tmp" || return 1
    while IFS= read -r line || [[ -n $line ]]; do
        [[ -z ${line//[[:space:]]/} || $line == \#* ]] && continue
        fp=$(key_fingerprint "$line" 2>/dev/null) || { ((invalid+=1)); continue; }
        existing=0; duplicate=0
        while IFS= read -r old || [[ -n $old ]]; do
            oldfp=$(key_fingerprint "$old" 2>/dev/null) || continue
            if [[ $oldfp == "$fp" ]]; then
                existing=1
                [[ $old == "$line" ]] && duplicate=1
            fi
        done < "$tmp"
        if (( existing )); then
            if (( duplicate )); then say "跳过重复公钥 $fp";
            else say "[冲突] $fp 已存在，但授权选项或备注不同；请人工检查。"; fi
            continue
        fi
        say "来源公钥：$fp ${line:0:100}"
        confirm '确认授权此公钥' || continue
        printf '%s\n' "$line" >> "$tmp" || return 1
        ((added+=1))
    done < "$source"
    if (( added )); then
        backup_auth || return 1
        chmod 600 "$tmp" && mv -f -- "$tmp" "$AUTH_FILE" || return 1
        say "已添加 $added 条；当前有效公钥 $(key_count) 条。"
        log_event add_key success "count=$added"
    fi
    (( invalid )) && say "已拒绝 $invalid 条无效记录。"
    (( added || invalid == 0 ))
}
delete_key() {
    local line fp n=0 selected=0 answer tmp
    init_env || return 1
    list_keys
    read -r -p '删除编号（0 取消）：' answer || return 1
    [[ $answer =~ ^[0-9]+$ ]] || return 1
    (( answer > 0 )) || return 0
    tmp=$(mktemp "$HOME/.ssh/.authorized_keys.XXXXXXXX") || return 1
    TMP_FILES+=("$tmp")
    while IFS= read -r line || [[ -n $line ]]; do
        if fp=$(key_fingerprint "$line" 2>/dev/null); then
            ((n+=1))
            if (( n == answer )); then selected=1; continue; fi
        fi
        printf '%s\n' "$line" >> "$tmp" || return 1
    done < "$AUTH_FILE"
    (( selected )) || { fail '编号不存在。'; return 1; }
    if (( n == 1 )); then
        say '[危险] 这是最后一条有效公钥。'
        confirm '确认删除最后一条；可能失去远程访问' || return 0
    fi
    if [[ -n ${SSH_CONNECTION:-} ]]; then
        confirm '当前为 SSH 会话；确认当前连接不依赖该公钥' || return 0
    fi
    backup_auth || return 1
    chmod 600 "$tmp" && mv -f -- "$tmp" "$AUTH_FILE" || return 1
    [[ $(stat -c %a "$AUTH_FILE") == 600 ]] || { fail '删除后权限验证失败。'; return 1; }
    say "已删除第 $answer 条，剩余 $(key_count) 条有效公钥。"
    log_event delete_key success "index=$answer"
}
fetch_keys() {
    local url=$1 tmp
    command -v curl >/dev/null || { fail '需要 curl。'; return 1; }
    [[ $url == https://* && $url != *'@'* && $url != *' '* ]] || { fail '仅允许不含凭据的 HTTPS URL。'; return 1; }
    tmp=$(mktemp) || return 1; TMP_FILES+=("$tmp")
    curl --fail --silent --show-error --proto '=https' --proto-redir '=https' \
        --location --max-redirs 2 --connect-timeout 5 --max-time 15 --max-filesize 65536 \
        --output "$tmp" "$url" || return 1
    [[ -s $tmp && $(wc -c < "$tmp") -le 65536 ]] || { fail '下载内容为空或超过 64 KiB。'; return 1; }
    say "来源：$url（请核对密钥所有权）"
    add_keys "$tmp"
}
generate_key() {
    local dest="$HOME/.ssh/sshkey" tmpdir pass backup_dir
    init_env || return 1
    say '推荐在客户端生成密钥，只把 .pub 公钥导入服务器。'
    confirm '仍要在服务器生成私钥' || return 0
    [[ ! -L $dest && ! -L $dest.pub ]] || { fail '拒绝覆盖符号链接形式的密钥文件。'; return 1; }
    if [[ -e $dest || -e $dest.pub ]]; then
        say "已有密钥：$dest"
        confirm '确认覆盖现有密钥' && confirm '再次确认：旧私钥会被替换' || return 0
    fi
    read -r -s -p '输入新私钥口令（可留空，不建议）：' pass || return 1; say
    tmpdir=$(mktemp -d "$HOME/.ssh/.keygen.XXXXXXXX") || return 1
    KEY_TMP_DIR=$tmpdir
    if ! ssh-keygen -q -t ed25519 -C 'sshkey_manager@local' -f "$tmpdir/sshkey" -N "$pass"; then
        unset pass; cleanup_key_temp; return 1
    fi
    unset pass
    chmod 600 "$tmpdir/sshkey" && chmod 644 "$tmpdir/sshkey.pub" || { cleanup_key_temp; return 1; }
    backup_dir=$(mktemp -d "$HOME/.ssh/.key-backup.XXXXXXXX") || { cleanup_key_temp; return 1; }
    KEY_BACKUP_DIR=$backup_dir
    KEY_DEST=$dest
    KEY_HAD_PRIV=0; KEY_HAD_PUB=0
    [[ -e $dest ]] && KEY_HAD_PRIV=1
    [[ -e $dest.pub ]] && KEY_HAD_PUB=1
    KEY_TX_ACTIVE=1
    if (( KEY_HAD_PRIV )); then mv -- "$dest" "$backup_dir/sshkey" || { rollback_generated_key; cleanup_key_temp; return 1; }; fi
    if (( KEY_HAD_PUB )); then mv -- "$dest.pub" "$backup_dir/sshkey.pub" || { rollback_generated_key; cleanup_key_temp; return 1; }; fi
    mv -- "$tmpdir/sshkey" "$dest" || { rollback_generated_key; cleanup_key_temp; return 1; }
    mv -- "$tmpdir/sshkey.pub" "$dest.pub" || { rollback_generated_key; cleanup_key_temp; return 1; }
    add_keys "$dest.pub" || { rollback_generated_key; cleanup_key_temp; return 1; }
    KEY_TX_ACTIVE=0
    cleanup_key_temp
    if (( KEY_HAD_PRIV || KEY_HAD_PUB )); then say "旧密钥备份：$backup_dir"; else rmdir -- "$backup_dir" 2>/dev/null || :; fi
    say "私钥：$dest（权限 600）。请通过安全方式备份至客户端。"
    if confirm '确认在当前终端显示完整私钥'; then
        say '[警告] 请确保终端记录与旁观者不会泄露此私钥。'
        cat -- "$dest"
    fi
    log_event generate_key success
}

sshd_bin() { if command -v sshd >/dev/null; then command -v sshd; elif [[ -x /usr/sbin/sshd ]]; then say /usr/sbin/sshd; else return 1; fi; }
effective() { local bin; bin=$(sshd_bin) || return 1; "$bin" -T -C "user=${1:-$(id -un)},host=localhost,addr=127.0.0.1" 2>/dev/null; }
active_ports() {
    local p
    command -v ss >/dev/null || return 1
    ss -H -ltn 2>/dev/null | awk '{print $4}' | sed -nE 's/.*:([0-9]+)$/\1/p' | sort -un
}
port_listening() { active_ports | grep -qx "$1"; }
service_detect() {
    if command -v systemctl >/dev/null && [[ -d /run/systemd/system ]]; then
        if systemctl is-active --quiet ssh.socket || systemctl is-active --quiet sshd.socket; then
            fail '检测到 systemd SSH socket 激活；该模式的端口修改需手工操作。'
            return 1
        fi
        if systemctl is-active --quiet ssh.service; then SERVICE=ssh.service
        elif systemctl is-active --quiet sshd.service; then SERVICE=sshd.service
        else fail '找不到运行中的 ssh.service 或 sshd.service。'; return 1; fi
    else
        fail '需要运行中的 systemd SSH 服务；其他服务模式请手工管理。'; return 1
    fi
}
reload_service() {
    "$(sshd_bin | tail -n 1)" -t || return 1
    systemctl reload "$SERVICE" || return 1
    systemctl is-active --quiet "$SERVICE"
}
begin_tx() {
    require_root && [[ -f $SSH_CONFIG && ! -L $SSH_CONFIG ]] || return 1
    command -v flock >/dev/null || { fail '需要 flock 以防止并发修改。'; return 1; }
    exec 9>/run/lock/sshkey-manager.lock || return 1
    flock -n 9 || { fail '已有 SSH 配置修改正在进行。'; return 1; }
    tx_setup || { flock -u 9; return 1; }
}
tx_setup() {
    service_detect || return 1
    "$(sshd_bin)" -t || { fail '原始 SSH 配置已有语法错误。'; return 1; }
    install -d -m 700 "$BACKUP_DIR" || return 1
    TX_DIR=$(mktemp -d "$BACKUP_DIR/tx.XXXXXXXX") || return 1
    cp -p -- "$SSH_CONFIG" "$TX_DIR/sshd_config" || return 1
    [[ -s $TX_DIR/sshd_config ]] || { fail '配置备份校验失败。'; return 1; }
    effective | awk '$1=="port"{print $2}' > "$TX_DIR/ports" || return 1
    [[ -s $TX_DIR/ports ]] || { fail '无法记录原始 SSH 端口。'; return 1; }
    systemctl is-active "$SERVICE" > "$TX_DIR/service" || return 1
    if command -v ufw >/dev/null; then ufw status numbered > "$TX_DIR/ufw.before" 2>&1 || return 1; fi
    if command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
        firewall-cmd --list-all > "$TX_DIR/firewalld.before" || return 1
        firewall-cmd --permanent --list-all > "$TX_DIR/firewalld.permanent.before" || return 1
    fi
    TX_ACTIVE=1
    say "事务备份：$TX_DIR"
}
rollback() {
    local ok=0 p
    (( TX_ACTIVE )) || return 0
    cp -p -- "$TX_DIR/sshd_config" "$SSH_CONFIG" || ok=1
    if (( ok == 0 )); then reload_service || ok=1; fi
    if [[ -n $FW_ADDED ]]; then
        case $FW_ADDED in
            ufw:*) ufw delete allow "${FW_ADDED#*:}"/tcp || ok=1 ;;
            firewalld:*) firewall-cmd --permanent --remove-port="${FW_ADDED#*:}"/tcp && firewall-cmd --reload || ok=1 ;;
        esac
    fi
    if [[ -n $SE_ADDED ]]; then semanage port -d -t ssh_port_t -p tcp "$SE_ADDED" || ok=1; fi
    while read -r p; do [[ -z $p ]] || port_listening "$p" || ok=1; done < "$TX_DIR/ports"
    TX_ACTIVE=0
    FW_ADDED=''
    SE_ADDED=''
    flock -u 9 2>/dev/null || :
    if (( ok )); then log_event rollback failure; fail "恢复未完全成功！请检查 $TX_DIR 并使用控制台修复。"; fi
    log_event rollback success
    say "已恢复原始配置，备份保留在 $TX_DIR"
}
commit_tx() { TX_ACTIVE=0; log_event "$1" success; say "操作完成；备份：$TX_DIR"; FW_ADDED=''; SE_ADDED=''; flock -u 9 2>/dev/null || :; }

# Only the main global section is edited. Conflicting Match or drop-in settings fail verification.
write_config() {
    local directives=$1 kind=$2 tmp
    tmp=$(mktemp "${SSH_CONFIG%/*}/.sshkey-manager.XXXXXXXX") || return 1; TMP_FILES+=("$tmp")
    {
        say '# BEGIN sshkey-manager managed settings'
        printf '%s\n' "$directives"
        awk -v kind="$kind" '
            /^# BEGIN sshkey-manager managed settings$/ {managed=1; next}
            /^# END sshkey-manager managed settings$/ {managed=0; next}
            managed && kind=="port" && $1!="Port" {print}
            managed && kind=="auth" && $1=="Port" {print}
        ' "$SSH_CONFIG"
        say '# END sshkey-manager managed settings'
        awk -v kind="$kind" '
            /^# BEGIN sshkey-manager managed settings$/ {managed=1; next}
            /^# END sshkey-manager managed settings$/ {managed=0; next}
            managed {next}
            !matchblock && /^[[:space:]]*Match[[:space:]]/ {matchblock=1}
            !matchblock && ((kind=="port" && /^[[:space:]]*Port[[:space:]]/) || (kind=="auth" && /^[[:space:]]*(PasswordAuthentication|KbdInteractiveAuthentication|PubkeyAuthentication)[[:space:]]/)) {
                print "# sshkey-manager previous: " $0; next
            }
            {print}
        ' "$SSH_CONFIG"
    } > "$tmp" || return 1
    chmod --reference="$SSH_CONFIG" "$tmp" && chown --reference="$SSH_CONFIG" "$tmp" && mv -f -- "$tmp" "$SSH_CONFIG"
}
config_conflicts() {
    local directive=$1
    if [[ -d /etc/ssh/sshd_config.d ]] && grep -Ein "^[[:space:]]*(${directive})[[:space:]]" /etc/ssh/sshd_config.d/*.conf 2>/dev/null; then
        fail '配置片段中存在相关指令；请手工检查后再操作。'
        return 1
    fi
}
check_key_login() {
    local user=$1 data ak path
    id "$user" >/dev/null 2>&1 || { fail '用户不存在。'; return 1; }
    data=$(effective "$user") || { fail '无法取得该用户的有效 SSH 配置。'; return 1; }
    [[ $(awk '$1=="pubkeyauthentication"{print $2}' <<< "$data") == yes ]] || { fail 'PubkeyAuthentication 未启用。'; return 1; }
    [[ $(awk '$1=="authenticationmethods"{print $2}' <<< "$data") == any ]] || { fail 'AuthenticationMethods 存在额外限制；请手工验证。'; return 1; }
    if [[ $user == root ]]; then
        [[ $(awk '$1=="permitrootlogin"{print $2}' <<< "$data") == yes || $(awk '$1=="permitrootlogin"{print $2}' <<< "$data") == prohibit-password ]] || { fail 'PermitRootLogin 不允许 root 密钥登录。'; return 1; }
    fi
    ak=$(awk '$1=="authorizedkeysfile"{print $2}' <<< "$data")
    [[ $ak == .ssh/authorized_keys ]] || { fail "AuthorizedKeysFile=$ak；当前版本只自动验证默认路径。"; return 1; }
    path=$(getent passwd "$user" | cut -d: -f6)/.ssh/authorized_keys
    [[ -f $path && ! -L $path ]] || { fail '目标用户授权文件不存在。'; return 1; }
    [[ $(stat -c %a "$path") == 600 && $(stat -c %a "${path%/*}") == 700 ]] || { fail '目标用户 SSH 文件权限异常。'; return 1; }
    [[ ! $(stat -c %a "${path%/.ssh/authorized_keys}") =~ [2367][0-7]$ ]] || { fail '用户主目录可能允许其他用户写入。'; return 1; }
    local previous_auth=$AUTH_FILE count
    AUTH_FILE=$path
    count=$(key_count)
    AUTH_FILE=$previous_auth
    (( count > 0 )) || fail '没有格式有效的授权公钥。'
}
set_password() {
    local mode=$1 user data
    require_root || return 1
    read -r -p '目标 SSH 用户：' user || return 1
    if [[ $mode == no ]]; then
        check_key_login "$user" || return 1
        say "在另一终端使用 ssh -o PreferredAuthentications=publickey -o PasswordAuthentication=no $user@服务器地址 登录并执行命令。"
        confirm '已通过独立连接成功使用密钥登录' || return 0
    fi
    config_conflicts 'PasswordAuthentication|KbdInteractiveAuthentication|PubkeyAuthentication' || return 1
    if grep -Eq '^[[:space:]]*Match[[:space:]]' "$SSH_CONFIG" /etc/ssh/sshd_config.d/*.conf 2>/dev/null; then
        fail '存在 Match 配置，无法确认所有连接的认证结果；请手工修改。'
        return 1
    fi
    begin_tx || return 1
    if [[ $mode == no ]]; then
        write_config $'PasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes' auth || { rollback; return 1; }
    else
        write_config $'PasswordAuthentication yes\nKbdInteractiveAuthentication yes\nPubkeyAuthentication yes' auth || { rollback; return 1; }
    fi
    data=$(effective "$user") || { rollback; return 1; }
    [[ $(awk '$1=="passwordauthentication"{print $2}' <<< "$data") == "$mode" ]] || { rollback; return 1; }
    [[ $(awk '$1=="kbdinteractiveauthentication"{print $2}' <<< "$data") == "$mode" ]] || { rollback; return 1; }
    [[ $(awk '$1=="pubkeyauthentication"{print $2}' <<< "$data") == yes ]] || { rollback; return 1; }
    reload_service || { rollback; return 1; }
    if [[ $mode == no ]]; then
        say '请再次从独立终端验证密钥登录仍然可用。'
        confirm '修改后已成功用密钥建立新连接' || { rollback; return 1; }
    fi
    say '配置已加载；密码认证状态还需在客户端验证。'
    commit_tx "password_$mode"
}
open_port() {
    local port=$1
    if command -v ufw >/dev/null && ufw status | grep -q '^Status: active'; then
        if ! ufw status | grep -Eq "^${port}/tcp[[:space:]]+ALLOW"; then
            ufw allow "$port"/tcp || return 1
            FW_ADDED="ufw:$port"
        fi
    elif command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
        if ! firewall-cmd --permanent --query-port="$port"/tcp >/dev/null 2>&1; then
            firewall-cmd --permanent --add-port="$port"/tcp || return 1
            FW_ADDED="firewalld:$port"
            firewall-cmd --reload || return 1
        fi
    fi
    if command -v getenforce >/dev/null && [[ $(getenforce) == Enforcing ]]; then
        command -v semanage >/dev/null || { fail 'SELinux Enforcing 且缺少 semanage。'; return 1; }
        if ! semanage port -l | awk '$1=="ssh_port_t"{print}' | grep -Eq "(^|[,[:space:]])${port}([,[:space:]]|$)"; then
            semanage port -a -t ssh_port_t -p tcp "$port" || return 1
            SE_ADDED=$port
        fi
    fi
}
change_port() {
    local new old ports directives p
    require_root || return 1
    read -r -p '新端口 (1-65535)：' new || return 1
    [[ $new =~ ^[0-9]{1,5}$ ]] || { fail '端口无效。'; return 1; }
    new=$((10#$new))
    (( new >= 1 && new <= 65535 )) || { fail '端口必须在 1 到 65535 之间。'; return 1; }
    config_conflicts Port || return 1
    service_detect || return 1
    ports=$(effective | awk '$1=="port"{print $2}') || return 1
    [[ -n $ports ]] || { fail '无法确定当前有效端口。'; return 1; }
    while read -r p; do port_listening "$p" || { fail "旧端口 $p 未监听，停止迁移。"; return 1; }; done <<< "$ports"
    if port_listening "$new"; then fail '新端口已被占用或已经监听。'; return 1; fi
    say '请先在云服务商安全组放行新端口；脚本无法验证云安全组。'
    confirm '已放行安全组并保持当前 SSH 会话' || return 0
    begin_tx || return 1
    open_port "$new" || { rollback; return 1; }
    directives=$(printf 'Port %s\n' $ports; printf 'Port %s' "$new")
    write_config "$directives" port || { rollback; return 1; }
    "$(sshd_bin)" -t && effective | awk '$1=="port"{print $2}' | grep -qx "$new" || { rollback; return 1; }
    while read -r p; do effective | awk '$1=="port"{print $2}' | grep -qx "$p" || { rollback; return 1; }; done <<< "$ports"
    reload_service || { rollback; return 1; }
    port_listening "$new" || { rollback; return 1; }
    while read -r p; do port_listening "$p" || { rollback; return 1; }; done <<< "$ports"
    say "新旧端口都在监听。请在另一终端用 ssh -p $new 用户@服务器地址 完成密钥登录并执行命令。"
    if ! confirm '已从外部成功使用新端口登录'; then
        say '未确认新连接，保留新旧双端口配置。'
        commit_tx port_transition
        return 0
    fi
    if confirm '确认关闭旧 SSH 端口监听'; then
        write_config "Port $new" port || { rollback; return 1; }
        [[ $(effective | awk '$1=="port"{print $2}') == "$new" ]] || { rollback; return 1; }
        reload_service && port_listening "$new" || { rollback; return 1; }
        say '旧防火墙规则未自动删除；请确认其他服务用途后手工清理。'
    else
        say '保留双端口监听。'
    fi
    commit_tx port_change
}
environment() {
    local service='未知' ports='未知' data
    [[ -r /etc/os-release ]] && . /etc/os-release && say "系统：${PRETTY_NAME:-未知}"
    say "用户：$(id -un)"
    if command -v systemctl >/dev/null; then
        for service in ssh.service sshd.service ssh.socket sshd.socket; do
            systemctl is-active --quiet "$service" && say "SSH 管理：$service (active)"
        done
    fi
    ports=$(effective 2>/dev/null | awk '$1=="port"{print $2}' | paste -sd, -)
    say "配置端口：${ports:-未知}"
    data=$(effective 2>/dev/null)
    say "密码认证：$(awk '$1=="passwordauthentication"{print $2}' <<< "$data" | head -1)"
    say "公钥认证：$(awk '$1=="pubkeyauthentication"{print $2}' <<< "$data" | head -1)"
    say "当前授权公钥：$(key_count)"
    if command -v ufw >/dev/null; then ufw status | head -1; fi
    if command -v firewall-cmd >/dev/null; then firewall-cmd --state 2>/dev/null || :; fi
    if command -v getenforce >/dev/null; then say "SELinux：$(getenforce)"; fi
}
main() {
    local choice tmp user url
    command -v ssh-keygen >/dev/null || { fail '需要 ssh-keygen。'; exit 1; }
    while :; do
        say '=== SSH Key 密钥管理 ==='
        environment
        say '1 生成密钥  2 手动添加公钥  3 GitHub 导入  4 URL 导入'
        say '5 编辑授权文件  6 查看公钥  d 删除公钥'
        say '7 关闭密码登录  8 开启密码登录  9 修改端口  0 退出'
        read -r -p '选择：' choice || break
        case $choice in
            1) generate_key ;;
            2) tmp=$(mktemp) || continue; TMP_FILES+=("$tmp"); read -r -p '粘贴公钥：' user; printf '%s\n' "$user" > "$tmp"; add_keys "$tmp" ;;
            3) read -r -p 'GitHub 用户名：' user; if [[ $user =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$ && $user != *--* ]]; then fetch_keys "https://github.com/$user.keys"; else fail '用户名格式无效。'; fi ;;
            4) read -r -p 'HTTPS 公钥 URL：' url; fetch_keys "$url" ;;
            5) init_env && backup_auth && "${EDITOR:-vi}" "$AUTH_FILE" && chmod 600 "$AUTH_FILE" ;;
            6) list_keys ;;
            d|D) delete_key ;;
            7) set_password no ;;
            8) set_password yes ;;
            9) change_port ;;
            0) break ;;
            *) fail '无效选择。' ;;
        esac
        pause
    done
}
[[ ${BASH_SOURCE[0]} == "$0" ]] && main "$@"
