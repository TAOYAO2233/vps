#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

# 从 GitHub Actions 下载产物并上传到网盘后，填写固定版本的二进制直链及其 SHA-256。
# 未配置时安装/升级直接报错，绝不在 VPS 编译源码。
BINARY_URL_X86_64=""
BINARY_SHA256_X86_64=""
BINARY_URL_AARCH64=""
BINARY_SHA256_AARCH64=""

ETC=/etc/xboard-audit
ENV_FILE=$ETC/.env
CERT_DIR=$ETC/certs
LOG_DIR=/var/log/xboard-audit/nodes
STATE_DIR=/var/lib/xboard-audit
LOCK_FILE=$STATE_DIR/maintenance.lock
CLIENT_QUEUE_DIR=/var/lib/xboard-audit-queue
IMPORT_DIR=$ETC/import
BIN=/usr/local/bin/xboard-audit
MANAGER=/usr/local/sbin/xboard-audit-manage
RSYSLOG_DIR=/etc/rsyslog.d
MASTER_LOCAL=$RSYSLOG_DIR/40-xboard-local.conf
NODE_RULES=$RSYSLOG_DIR/41-xboard-nodes.conf
MASTER_REMOTE=$RSYSLOG_DIR/42-xboard-remote.conf
CLIENT_RULE=$RSYSLOG_DIR/43-xboard-forward.conf
PORT=6514

die() { printf '错误：%s\n' "$*" >&2; exit 1; }
info() { printf '%s\n' "$*"; }
need_root() { [[ $EUID -eq 0 ]] || die "请使用 root 运行"; }
valid_node() { [[ $1 =~ ^[A-Za-z][A-Za-z0-9_-]{0,31}$ ]]; }
valid_integer() { [[ $1 =~ ^[0-9]+$ ]]; }
valid_ip() { [[ $1 =~ ^[0-9a-fA-F:.]+$ ]]; }

ensure_lock_file() {
    [[ -d $STATE_DIR && ! -L $STATE_DIR && ! -L $LOCK_FILE ]] || die "维护锁目录或文件不安全"
    if [[ ! -e $LOCK_FILE ]]; then
        install -m 0600 -o xboard-audit -g xboard-audit /dev/null "$LOCK_FILE"
    fi
    [[ -f $LOCK_FILE ]] || die "维护锁文件不是普通文件"
    chown xboard-audit:xboard-audit "$LOCK_FILE"
    chmod 0600 "$LOCK_FILE"
}

require_master() { [[ -f $ENV_FILE && -f $CERT_DIR/ca.crt ]] || die "此操作只能在已配置的主机执行"; }
require_client() { [[ -f $CLIENT_RULE ]] || die "此操作只能在已配置的客户端执行"; }

# 不 source .env：配置文件是数据，不作为 Shell 代码执行。
env_get() {
    local key=$1 line
    [[ -f $ENV_FILE ]] || return 1
    line=$(grep -m1 "^${key}=" "$ENV_FILE" || true)
    [[ -n $line ]] || return 1
    printf '%s' "${line#*=}"
}

env_set() {
    local key=$1 value=$2 tmp
    [[ $value != *$'\n'* && $value != *$'\r'* ]] || die "配置值不能包含换行"
    tmp=$(mktemp "$ETC/.env.XXXXXX")
    if [[ -f $ENV_FILE ]]; then
        awk -F= -v key="$key" '$1 != key {print}' "$ENV_FILE" > "$tmp"
    fi
    printf '%s=%s\n' "$key" "$value" >> "$tmp"
    chown root:xboard-audit "$tmp"
    chmod 0640 "$tmp"
    mv -f "$tmp" "$ENV_FILE"
}

ensure_dirs() {
    local created_group=0 created_user=0
    if ! getent group xboard-audit >/dev/null; then groupadd --system xboard-audit; created_group=1; fi
    if ! id xboard-audit >/dev/null 2>&1; then
        useradd --system --no-create-home --shell /usr/sbin/nologin --gid xboard-audit xboard-audit
        created_user=1
    fi
    install -d -m 0750 -o root -g xboard-audit "$ETC"
    : > "$ETC/.managed-by-xboard-audit"
    printf 'master\n' > "$ETC/install-role"
    (( created_user == 0 )) || : > "$ETC/.created-user"
    (( created_group == 0 )) || : > "$ETC/.created-group"
    install -d -m 0700 -o root -g root "$CERT_DIR"
    install -d -m 0750 -o root -g xboard-audit "${LOG_DIR%/nodes}"
    install -d -m 2750 -o syslog -g xboard-audit "$LOG_DIR"
    install -d -m 0750 -o xboard-audit -g xboard-audit "$STATE_DIR"
    ensure_lock_file
}

download_binary() {
    local arch url sha tmp backup
    ensure_lock_file
    arch=$(uname -m)
    case "$arch" in
        x86_64) url=$BINARY_URL_X86_64; sha=$BINARY_SHA256_X86_64 ;;
        aarch64) url=$BINARY_URL_AARCH64; sha=$BINARY_SHA256_AARCH64 ;;
        *) die "不支持的 CPU 架构：$arch" ;;
    esac
    [[ -n $url && -n $sha ]] || die "尚未配置二进制下载地址或 SHA-256；请先将 Actions 产物上传网盘并填写 manage.sh 顶部变量"
    [[ $url == https://* ]] || die "二进制下载地址必须使用 HTTPS"
    [[ $sha =~ ^[0-9a-fA-F]{64}$ ]] || die "SHA-256 格式错误"
    tmp=$(mktemp /tmp/xboard-audit.XXXXXX)
    backup=${BIN}.previous
    trap 'rm -f "$tmp"' RETURN
    curl --fail --location --silent --show-error --retry 3 --output "$tmp" "$url" || die "二进制下载失败"
    printf '%s  %s\n' "$sha" "$tmp" | sha256sum --check --status || die "二进制 SHA-256 校验失败"
    if [[ -f $BIN ]]; then cp -p "$BIN" "$backup"; fi
    install -m 0755 "$tmp" "$BIN"
    if systemctl list-unit-files xboard-audit.service --no-legend 2>/dev/null | grep -q '^xboard-audit.service'; then
        systemctl restart xboard-audit || rollback_binary "$backup"
        sleep 2
        systemctl is-active --quiet xboard-audit || rollback_binary "$backup"
    fi
    info "二进制安装完成：$arch"
}

rollback_binary() {
    local backup=$1
    if [[ -f $backup ]]; then
        install -m 0755 "$backup" "$BIN"
        systemctl restart xboard-audit || true
    fi
    die "新版本启动失败；已尝试恢复上一版本"
}

make_ca_and_server() {
    if [[ -f $CERT_DIR/ca.key ]]; then
        [[ -f $CERT_DIR/ca.crt && -f $CERT_DIR/server.key && -f $CERT_DIR/server.ext ]] || die "主机证书文件不完整"
        if [[ ! -f $CERT_DIR/server.crt ]] || ! openssl x509 -in "$CERT_DIR/server.crt" -checkend 2592000 -noout >/dev/null 2>&1; then
            renew_server_cert
        fi
        return
    fi
    openssl req -x509 -newkey rsa:3072 -nodes -days 3650 \
        -keyout "$CERT_DIR/ca.key" -out "$CERT_DIR/ca.crt" \
        -subj '/CN=xboard-audit-ca' -addext 'basicConstraints=critical,CA:TRUE' >/dev/null 2>&1 || die "生成 CA 失败"
    openssl req -newkey rsa:3072 -nodes -keyout "$CERT_DIR/server.key" \
        -out "$CERT_DIR/server.csr" -subj '/CN=xboard-audit-master' >/dev/null 2>&1 || die "生成主机证书请求失败"
    cat > "$CERT_DIR/server.ext" <<'EOF'
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:xboard-audit-master
EOF
    openssl x509 -req -in "$CERT_DIR/server.csr" -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" \
        -CAcreateserial -out "$CERT_DIR/server.crt" -days 825 -sha256 -extfile "$CERT_DIR/server.ext" >/dev/null 2>&1 || die "签发主机证书失败"
    chmod 0600 "$CERT_DIR"/*.key
    chmod 0644 "$CERT_DIR"/*.crt
    info "已生成主机 CA 和服务器证书"
}

renew_server_cert() {
    local csr cert backup
    csr=$(mktemp "$CERT_DIR/server-renew.XXXXXX.csr")
    cert=$(mktemp "$CERT_DIR/server-renew.XXXXXX.crt")
    backup=$(mktemp "$CERT_DIR/server-backup.XXXXXX.crt")
    openssl req -new -key "$CERT_DIR/server.key" -out "$csr" -subj '/CN=xboard-audit-master' >/dev/null 2>&1 || die "生成续期请求失败"
    openssl x509 -req -in "$csr" -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" \
        -CAcreateserial -out "$cert" -days 825 -sha256 -extfile "$CERT_DIR/server.ext" >/dev/null 2>&1 || die "续签主机证书失败"
    if [[ -f $CERT_DIR/server.crt ]]; then cp -p "$CERT_DIR/server.crt" "$backup"; else : > "$backup"; fi
    install -m 0644 "$cert" "$CERT_DIR/server.crt"
    if ! rsyslogd -N1 >/dev/null 2>&1 || ! systemctl restart rsyslog; then
        if [[ -s $backup ]]; then install -m 0644 "$backup" "$CERT_DIR/server.crt"; else rm -f "$CERT_DIR/server.crt"; fi
        systemctl restart rsyslog || true
        rm -f -- "$csr" "$cert" "$backup"
        die "主机证书续期失败；已恢复原证书"
    fi
    rm -f -- "$csr" "$cert" "$backup"
    info "主机 TLS 证书已续期"
}

renew_node_cert() {
    require_master
    local node=${1:-} bundle
    [[ -n $node ]] || read -r -p '要续期的节点 ID: ' node
    valid_node "$node" || die "节点 ID 格式错误"
    grep -Fxq "$node" "$ETC/nodes.list" || die "节点未登记"
    bundle=$(mktemp -d "$ETC/export-${node}-renew-XXXXXX")
    openssl req -newkey rsa:3072 -nodes -keyout "$bundle/client.key" \
        -out "$bundle/client.csr" -subj "/CN=xboard-$node" >/dev/null 2>&1 || die "生成客户端续期请求失败"
    cat > "$bundle/client.ext" <<'EOF'
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
EOF
    openssl x509 -req -in "$bundle/client.csr" -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" \
        -CAcreateserial -out "$bundle/client.crt" -days 825 -sha256 -extfile "$bundle/client.ext" >/dev/null 2>&1 || die "签发客户端续期证书失败"
    cp "$CERT_DIR/ca.crt" "$bundle/ca.crt"
    printf '%s\n' "$node" > "$bundle/node-id"
    chmod 0600 "$bundle/client.key"
    info "续期包已生成：$bundle。安全复制到客户端后运行 install-client 更新证书。"
}

install_service_files() {
    cat > /etc/systemd/system/xboard-audit.service <<'EOF'
[Unit]
Description=Xboard Rust audit and Telegram summary
After=network-online.target rsyslog.service
Wants=network-online.target

[Service]
Type=simple
User=xboard-audit
Group=xboard-audit
ExecStartPre=/usr/local/bin/xboard-audit --check-config
ExecStart=/usr/local/bin/xboard-audit
Restart=always
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/xboard-audit
ReadOnlyPaths=/etc/xboard-audit /var/log/xboard-audit/nodes

[Install]
WantedBy=multi-user.target
EOF
    cat > /etc/systemd/system/xboard-audit-maintenance.service <<'EOF'
[Unit]
Description=Rotate and clean Xboard audit logs

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/xboard-audit-manage maintenance
EOF
    cat > /etc/systemd/system/xboard-audit-maintenance.timer <<'EOF'
[Unit]
Description=Daily Xboard audit log maintenance (UTC+8)

[Timer]
OnCalendar=*-*-* 16:00:00 UTC
Persistent=true

[Install]
WantedBy=timers.target
EOF
    cat > "$ETC/logrotate.conf" <<'EOF'
/var/log/xboard-audit/nodes/*.log {
    daily
    rotate 10000
    maxsize 100M
    missingok
    notifempty
    dateext
    dateformat -%Y%m%d%H%M%S
    create 0640 syslog xboard-audit
    sharedscripts
    postrotate
        /bin/systemctl kill -s HUP rsyslog.service >/dev/null 2>&1 || true
    endscript
}
EOF
    systemctl daemon-reload
}

write_local_rule() {
    cat > "$MASTER_LOCAL" <<'EOF'
if $programname == 'xboard-node' then {
    action(type="omfile" file="/var/log/xboard-audit/nodes/vps-main-A.log"
           fileOwner="syslog" fileGroup="xboard-audit" fileCreateMode="0640")
}
EOF
}

write_remote_rule() {
    local node first=1
    cat > "$MASTER_REMOTE" <<'EOF'
global(DefaultNetstreamDriverCAFile="/etc/xboard-audit/certs/ca.crt"
       DefaultNetstreamDriverCertFile="/etc/xboard-audit/certs/server.crt"
       DefaultNetstreamDriverKeyFile="/etc/xboard-audit/certs/server.key")
module(load="imtcp")
EOF
    [[ -s $ETC/nodes.list ]] || return 0
    printf 'input(type="imtcp" port="6514" ruleset="xboard_remote"\n' >> "$MASTER_REMOTE"
    printf '      StreamDriver.Name="gtls" StreamDriver.Mode="1"\n' >> "$MASTER_REMOTE"
    printf '      StreamDriver.AuthMode="x509/name" PermittedPeer=[' >> "$MASTER_REMOTE"
    while IFS= read -r node; do
        [[ -n $node ]] || continue
        valid_node "$node" || die "节点登记文件中有非法节点名"
        if (( first )); then first=0; else printf ',' >> "$MASTER_REMOTE"; fi
        printf '"xboard-%s"' "$node" >> "$MASTER_REMOTE"
    done < "$ETC/nodes.list"
    printf '])\n' >> "$MASTER_REMOTE"
}

write_node_rules() {
    local node
    {
        printf 'ruleset(name="xboard_remote") {\n'
        while IFS= read -r node; do
            [[ -n $node ]] || continue
            valid_node "$node" || die "节点登记文件中有非法节点名"
            printf '    if $programname == "xboard-%s" then {\n' "$node"
            printf '        action(type="omfile" file="%s/%s.log" fileOwner="syslog" fileGroup="xboard-audit" fileCreateMode="0640")\n' "$LOG_DIR" "$node"
            printf '        stop\n    }\n'
        done < "$ETC/nodes.list"
        printf '}\n'
    } > "$NODE_RULES"
}

validate_rsyslog() { rsyslogd -N1 >/dev/null 2>&1 || die "rsyslog 配置验证失败，请检查 journalctl -u rsyslog"; }

verify_local_capture() {
    local probe="xboard-audit-probe-$(date +%s)-$$" attempt
    command -v logger >/dev/null || die "缺少 logger，无法验证本机日志链路"
    logger -t xboard-node -- "$probe"
    for attempt in 1 2 3 4 5; do
        if grep -Fq "$probe" "$LOG_DIR/vps-main-A.log" 2>/dev/null; then return 0; fi
        sleep 1
    done
    die "本机 rsyslog 未把 xboard-node 标签写入节点日志；请检查 journald/rsyslog 输入"
}

verify_real_source() {
    local since attempt line
    systemctl is-active --quiet xboard-node.service || die "xboard-node.service 未运行，无法验证真实日志链路"
    since=$(date '+%Y-%m-%d %H:%M:%S')
    info "请在 60 秒内通过本机节点产生一次真实连接，正在核对 journald 与节点日志。"
    for ((attempt=0; attempt<60; attempt++)); do
        while IFS= read -r line; do
            [[ $line == *'inbound connection to'* ]] || continue
            if grep -Fq -- "$line" "$LOG_DIR/vps-main-A.log" 2>/dev/null; then
                info "真实 xboard-node 日志已到达主机节点文件"
                return 0
            fi
        done < <(journalctl -u xboard-node.service --since "$since" -o cat --no-pager 2>/dev/null)
        sleep 1
    done
    die "60 秒内未确认真实 xboard-node 日志到达；请检查服务和 rsyslog 输入后运行 verify-source"
}

restart_audit_or_restore() {
    local backup=$1
    if ! systemctl restart xboard-audit || ! systemctl is-active --quiet xboard-audit; then
        mv -f "$backup" "$ENV_FILE"
        systemctl restart xboard-audit || true
        die "审计服务启动失败；已恢复原配置"
    fi
    rm -f "$backup"
}

install_master() {
    need_root
    systemctl is-active --quiet xboard-node.service || die "xboard-node.service 未运行"
    # 下载地址为空时先停止，避免部分安装。
    case "$(uname -m)" in
        x86_64) [[ -n $BINARY_URL_X86_64 && -n $BINARY_SHA256_X86_64 ]] || die "尚未配置二进制下载地址" ;;
        aarch64) [[ -n $BINARY_URL_AARCH64 && -n $BINARY_SHA256_AARCH64 ]] || die "尚未配置二进制下载地址" ;;
        *) die "不支持的 CPU 架构" ;;
    esac
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y rsyslog rsyslog-gnutls openssl curl ca-certificates logrotate util-linux
    ensure_dirs
    install -m 0750 "$0" "$MANAGER"
    make_ca_and_server
    if [[ ! -f $ENV_FILE ]]; then
        local token ids
        read -r -s -p 'Telegram Bot Token: ' token; printf '\n'
        read -r -p 'Chat ID（多个用逗号分隔）: ' ids
        [[ -n $token && -n $ids ]] || die "Token 和 Chat ID 不能为空"
        env_set TG_BOT_TOKEN "$token"
        env_set TG_CHAT_IDS "$ids"
        env_set PUSH_INTERVAL_SECONDS 60
        env_set LOG_RETENTION_HOURS 48
    fi
    [[ -f $ETC/nodes.list ]] || : > "$ETC/nodes.list"
    touch "$LOG_DIR/vps-main-A.log"
    chown syslog:xboard-audit "$LOG_DIR/vps-main-A.log"
    chmod 0640 "$LOG_DIR/vps-main-A.log"
    write_local_rule
    write_remote_rule
    write_node_rules
    validate_rsyslog
    systemctl restart rsyslog
    verify_local_capture
    install_service_files
    download_binary
    "$BIN" --check-config || die "主机 .env 验证失败"
    systemctl enable --now xboard-audit
    systemctl enable xboard-audit-maintenance.timer
    systemctl restart xboard-audit-maintenance.timer
    verify_real_source
    info "主机安装完成。请在防火墙中仅允许已授权 VPS 连接 TCP/6514。"
}

upgrade_master() {
    require_master
    download_binary
    if [[ $(readlink -f -- "$0") != $(readlink -f -- "$MANAGER") ]]; then
        install -m 0750 "$0" "$MANAGER"
    fi
    info "主机二进制和管理脚本已更新"
}

update_interval() {
    require_master
    local value=${1:-} backup
    [[ -n $value ]] || read -r -p '发送间隔（秒，10–3600）: ' value
    valid_integer "$value" && (( value >= 10 && value <= 3600 )) || die "发送间隔必须在 10–3600 秒之间"
    backup=$(mktemp "$ETC/.env.backup.XXXXXX")
    cp -p "$ENV_FILE" "$backup"
    env_set PUSH_INTERVAL_SECONDS "$value"
    if ! "$BIN" --check-config; then mv -f "$backup" "$ENV_FILE"; die "配置验证失败；已恢复原配置"; fi
    restart_audit_or_restore "$backup"
    info "发送间隔已设为 ${value} 秒"
}

update_retention() {
    require_master
    local value=${1:-} backup
    [[ -n $value ]] || read -r -p '日志保留小时数（1–8760）: ' value
    valid_integer "$value" && (( value >= 1 && value <= 8760 )) || die "日志保留时间必须在 1–8760 小时之间"
    backup=$(mktemp "$ETC/.env.backup.XXXXXX")
    cp -p "$ENV_FILE" "$backup"
    env_set LOG_RETENTION_HOURS "$value"
    if ! "$BIN" --check-config; then mv -f "$backup" "$ENV_FILE"; die "配置验证失败；已恢复原配置"; fi
    restart_audit_or_restore "$backup"
    info "日志保留时间已设为 ${value} 小时"
}

update_telegram() {
    require_master
    local token ids backup
    read -r -s -p '新 Bot Token（留空保持原值）: ' token; printf '\n'
    read -r -p '新 Chat ID（留空保持原值）: ' ids
    backup=$(mktemp "$ETC/.env.backup.XXXXXX")
    cp -p "$ENV_FILE" "$backup"
    [[ -z $token ]] || env_set TG_BOT_TOKEN "$token"
    [[ -z $ids ]] || env_set TG_CHAT_IDS "$ids"
    if ! "$BIN" --check-config; then mv -f "$backup" "$ENV_FILE"; die "Telegram 配置验证失败；已恢复原配置"; fi
    restart_audit_or_restore "$backup"
    info "Telegram 配置已更新（Token 不回显）"
}

add_node() {
    require_master
    local node=${1:-} bundle nodes_backup
    [[ -n $node ]] || read -r -p '新节点 ID（字母开头，仅字母数字_-）: ' node
    valid_node "$node" || die "节点 ID 格式错误"
    [[ $node != vps-main-A ]] || die "该名称保留给主机"
    grep -Fxq "$node" "$ETC/nodes.list" 2>/dev/null && die "节点已存在"
    bundle="$ETC/export-$node"
    [[ ! -e $bundle ]] || die "证书包已存在，请先核对或移走"
    install -d -m 0700 "$bundle"
    openssl req -newkey rsa:3072 -nodes -keyout "$bundle/client.key" \
        -out "$bundle/client.csr" -subj "/CN=xboard-$node" >/dev/null 2>&1 || die "生成客户端证书请求失败"
    cat > "$bundle/client.ext" <<'EOF'
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
EOF
    openssl x509 -req -in "$bundle/client.csr" -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" \
        -CAcreateserial -out "$bundle/client.crt" -days 825 -sha256 -extfile "$bundle/client.ext" >/dev/null 2>&1 || die "签发客户端证书失败"
    cp "$CERT_DIR/ca.crt" "$bundle/ca.crt"
    printf '%s\n' "$node" > "$bundle/node-id"
    chmod 0600 "$bundle/client.key"
    nodes_backup=$(mktemp "$ETC/nodes.backup.XXXXXX")
    cp -p "$ETC/nodes.list" "$nodes_backup"
    printf '%s\n' "$node" >> "$ETC/nodes.list"
    write_node_rules
    write_remote_rule
    if ! rsyslogd -N1 >/dev/null 2>&1; then
        mv -f "$nodes_backup" "$ETC/nodes.list"
        write_node_rules
        write_remote_rule
        die "rsyslog 配置验证失败；已恢复节点登记"
    fi
    if ! systemctl restart rsyslog; then
        mv -f "$nodes_backup" "$ETC/nodes.list"
        write_node_rules
        write_remote_rule
        systemctl restart rsyslog || true
        die "rsyslog 重启失败；节点登记已恢复"
    fi
    rm -f "$nodes_backup"
    info "节点已登记。将 $bundle 安全复制到客户端，再运行：sudo bash manage.sh install-client <证书包目录> <主机公网 IP>"
    info "证书包包含客户端私钥；复制完成后请从主机删除导出包。"
}

rollback_client_install() {
    local backup_dir=$1 had_marker=$2 had_queue=$3
    [[ $backup_dir == /tmp/xboard-audit-client.* ]] || die "客户端备份路径校验失败"
    rm -f -- "$CLIENT_RULE"
    rm -rf -- "$CERT_DIR"
    if [[ -f $backup_dir/client-rule.conf ]]; then cp -p "$backup_dir/client-rule.conf" "$CLIENT_RULE"; fi
    if [[ -d $backup_dir/certs ]]; then cp -a "$backup_dir/certs" "$CERT_DIR"; fi
    if [[ -f $backup_dir/install-role ]]; then cp -p "$backup_dir/install-role" "$ETC/install-role"; else rm -f -- "$ETC/install-role"; fi
    if [[ -f $backup_dir/client-node-id ]]; then cp -p "$backup_dir/client-node-id" "$ETC/client-node-id"; else rm -f -- "$ETC/client-node-id"; fi
    if (( had_marker == 0 )); then rm -f -- "$ETC/.managed-by-xboard-audit"; fi
    if (( had_queue == 0 )); then rm -rf -- "$CLIENT_QUEUE_DIR"; fi
    if (( had_marker == 0 )); then rmdir "$ETC" 2>/dev/null || true; fi
    rsyslogd -N1 >/dev/null 2>&1 && systemctl restart rsyslog || true
    rm -rf -- "$backup_dir"
    die "客户端配置验证或启动失败；已恢复此前的 rsyslog 规则和证书"
}

install_client() {
    need_root
    [[ ! -f $ENV_FILE ]] || die "当前机器已配置为主机，不能安装客户端"
    systemctl is-active --quiet xboard-node.service || die "xboard-node.service 未运行"
    local bundle=${1:-} master_ip=${2:-} node backup_dir had_marker=0 had_queue=0
    [[ -n $bundle && -d $bundle ]] || die "用法：install-client <证书包目录> <主机公网 IP>"
    valid_ip "$master_ip" || die "主机公网 IP 格式错误"
    [[ -f $bundle/node-id && -f $bundle/client.key && -f $bundle/client.crt && -f $bundle/ca.crt ]] || die "证书包不完整"
    node=$(cat "$bundle/node-id")
    valid_node "$node" || die "节点 ID 格式错误"
    if [[ -f $ETC/client-node-id ]] && [[ $(cat "$ETC/client-node-id") != "$node" ]]; then
        die "本机已配置为另一节点；请先卸载以清空旧发送队列，再安装新节点"
    fi
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y rsyslog rsyslog-gnutls ca-certificates
    backup_dir=$(mktemp -d /tmp/xboard-audit-client.XXXXXX)
    [[ -f $ETC/.managed-by-xboard-audit ]] && had_marker=1
    [[ -d $CLIENT_QUEUE_DIR ]] && had_queue=1
    if [[ -f $CLIENT_RULE ]]; then cp -p "$CLIENT_RULE" "$backup_dir/client-rule.conf"; fi
    if [[ -d $CERT_DIR ]]; then cp -a "$CERT_DIR" "$backup_dir/certs"; fi
    if [[ -f $ETC/install-role ]]; then cp -p "$ETC/install-role" "$backup_dir/install-role"; fi
    if [[ -f $ETC/client-node-id ]]; then cp -p "$ETC/client-node-id" "$backup_dir/client-node-id"; fi
    install -d -m 0700 -o root -g root "$CERT_DIR"
    : > "$ETC/.managed-by-xboard-audit"
    printf 'client\n' > "$ETC/install-role"
    printf '%s\n' "$node" > "$ETC/client-node-id"
    install -d -m 0700 -o syslog -g syslog "$CLIENT_QUEUE_DIR"
    install -m 0600 "$bundle/client.key" "$CERT_DIR/client.key"
    install -m 0644 "$bundle/client.crt" "$CERT_DIR/client.crt"
    install -m 0644 "$bundle/ca.crt" "$CERT_DIR/ca.crt"
    cat > "$CLIENT_RULE" <<EOF
global(DefaultNetstreamDriverCAFile="$CERT_DIR/ca.crt"
       DefaultNetstreamDriverCertFile="$CERT_DIR/client.crt"
       DefaultNetstreamDriverKeyFile="$CERT_DIR/client.key")
template(name="xboardForward" type="string"
         string="<%pri%>%timestamp:::date-rfc3339% %hostname% xboard-$node: %msg%\\n")
if \$programname == 'xboard-node' then {
    action(type="omfwd" target="$master_ip" port="$PORT" protocol="tcp"
           template="xboardForward" StreamDriver="gtls" StreamDriverMode="1"
           StreamDriverAuthMode="x509/name" StreamDriverPermittedPeers="xboard-audit-master"
           queue.type="LinkedList" queue.filename="forward"
           queue.spoolDirectory="$CLIENT_QUEUE_DIR"
           queue.maxDiskSpace="512m" queue.saveOnShutdown="on"
           action.resumeRetryCount="-1")
}
EOF
    # 队列存放在本工具专用目录，卸载时可完整清除。
    if ! rsyslogd -N1 >/dev/null 2>&1 || ! systemctl restart rsyslog; then
        rollback_client_install "$backup_dir" "$had_marker" "$had_queue"
    fi
    install -m 0750 "$0" "$MANAGER"
    rm -rf -- "$backup_dir"
    logger -t xboard-node -- "xboard-audit-probe-$(date +%s)-$$" || true
    info "客户端转发已配置：$node -> $master_ip:$PORT"
    info "请在主机检查 ${LOG_DIR}/${node}.log 是否收到新日志。"
}

interactive_install_client() {
    [[ ! -f $ENV_FILE ]] || die "当前机器已配置为主机，不能安装客户端"
    [[ ! -L $IMPORT_DIR ]] || die "证书接收目录不能是符号链接"
    install -d -m 0700 "$IMPORT_DIR"
    info "证书接收目录已创建：$IMPORT_DIR"
    info "请将主机生成的完整 export-<节点ID> 文件夹上传到此目录。"
    local answer entry node selected master_ip
    local bundles=()
    while true; do
        read -r -p '上传完成后按 Enter 扫描；输入 0 返回菜单：' answer || return 0
        [[ $answer == 0 ]] && return 0
        bundles=()
        for entry in "$IMPORT_DIR"/export-*; do
            [[ -d $entry && ! -L $entry ]] || continue
            node=${entry##*/export-}
            valid_node "$node" || continue
            [[ -f $entry/node-id && ! -L $entry/node-id && -f $entry/client.key && ! -L $entry/client.key && -f $entry/client.crt && ! -L $entry/client.crt && -f $entry/ca.crt && ! -L $entry/ca.crt ]] || continue
            [[ $(< "$entry/node-id") == "$node" ]] || continue
            bundles+=("$entry")
        done
        if (( ${#bundles[@]} == 0 )); then
            info "未发现完整证书包；请上传 export-<节点ID> 文件夹后重试。"
            continue
        fi
        if (( ${#bundles[@]} == 1 )); then
            selected=${bundles[0]}
        else
            info "发现多个证书包："
            for entry in "${bundles[@]}"; do info "  ${entry##*/export-}"; done
            read -r -p '输入本机节点 ID：' node || return 0
            selected=""
            for entry in "${bundles[@]}"; do
                if [[ ${entry##*/export-} == "$node" ]]; then selected=$entry; break; fi
            done
            if [[ -z $selected ]]; then info "未找到该节点证书包"; continue; fi
        fi
        info "已识别证书包：${selected##*/export-}"
        read -r -p '主机公网 IP：' master_ip || return 0
        install_client "$selected" "$master_ip"
        rm -r -- "$selected"
        rmdir "$IMPORT_DIR" 2>/dev/null || true
        info "证书包已安装，上传副本已清理。"
        return 0
    done
}

remove_node() {
    require_master
    local node=${1:-} tmp nodes_backup
    [[ -n $node ]] || read -r -p '要移除的节点 ID: ' node
    valid_node "$node" || die "节点 ID 格式错误"
    grep -Fxq "$node" "$ETC/nodes.list" || die "节点不存在"
    nodes_backup=$(mktemp "$ETC/nodes.backup.XXXXXX")
    cp -p "$ETC/nodes.list" "$nodes_backup"
    tmp=$(mktemp "$ETC/nodes.XXXXXX")
    grep -Fxv "$node" "$ETC/nodes.list" > "$tmp" || true
    mv "$tmp" "$ETC/nodes.list"
    write_node_rules
    write_remote_rule
    if ! rsyslogd -N1 >/dev/null 2>&1; then
        mv -f "$nodes_backup" "$ETC/nodes.list"
        write_node_rules
        write_remote_rule
        die "rsyslog 配置验证失败；已恢复节点登记"
    fi
    if ! systemctl restart rsyslog; then
        mv -f "$nodes_backup" "$ETC/nodes.list"
        write_node_rules
        write_remote_rule
        systemctl restart rsyslog || true
        die "rsyslog 重启失败；节点登记已恢复"
    fi
    rm -f "$nodes_backup"
    info "已停止接收节点 $node；历史日志和客户端配置未删除。"
}

maintenance() {
    require_master
    local hours lock_fd
    hours=$(env_get LOG_RETENTION_HOURS)
    valid_integer "$hours" && (( hours >= 1 && hours <= 8760 )) || die "LOG_RETENTION_HOURS 无效"
    ensure_lock_file
    exec {lock_fd}<>"$LOCK_FILE"
    flock -x "$lock_fd" || die "无法暂停 Telegram 发送"
    /usr/sbin/logrotate -s "$STATE_DIR/logrotate.state" "$ETC/logrotate.conf"
    [[ -x $BIN ]] || die "Rust 二进制不存在，无法按日志内容清理"
    "$BIN" --compact-logs
    exec {lock_fd}>&-
}

uninstall_all() {
    need_root
    local answer role="" created_user=0 created_group=0 was_master=0 was_client=0 audit_running=0 timer_running=0 had_rules=0 file backup_dir
    [[ $ETC == /etc/xboard-audit && $STATE_DIR == /var/lib/xboard-audit && $CLIENT_QUEUE_DIR == /var/lib/xboard-audit-queue ]] || die "卸载路径校验失败"
    [[ -f $ETC/.managed-by-xboard-audit ]] || die "未找到本脚本的安装标记，拒绝清理未知目录"
    [[ ! -L $ETC && ! -L $STATE_DIR && ! -L ${LOG_DIR%/nodes} && ! -L $CLIENT_QUEUE_DIR ]] || die "发现符号链接，拒绝递归清理"
    if [[ -f $MASTER_LOCAL || -f $MASTER_REMOTE || -f $ENV_FILE ]]; then role="主机"; was_master=1; fi
    if [[ -f $CLIENT_RULE ]]; then role="${role:+${role}及}客户端"; was_client=1; fi
    if [[ -z $role && -f $ETC/install-role ]]; then
        case "$(cat "$ETC/install-role")" in
            master) role="主机（部分安装）"; was_master=1 ;;
            client) role="客户端（部分安装）"; was_client=1 ;;
        esac
    fi
    [[ -n $role ]] || die "无法识别主机或客户端安装，拒绝清理"
    [[ -f $ETC/.created-user ]] && created_user=1
    [[ -f $ETC/.created-group ]] && created_group=1
    info "将卸载本机的 $role 组件，永久删除本工具的 .env、证书、节点日志、轮转日志、状态与发送队列。"
    info "不会删除系统 rsyslog 软件包、共享系统日志、Telegram 已发送消息或其它 VPS 上的配置。"
    read -r -p '输入 UNINSTALL 确认：' answer
    [[ $answer == UNINSTALL ]] || die "已取消卸载"

    if (( was_master )); then
        systemctl is-active --quiet xboard-audit && audit_running=1 || true
        systemctl is-active --quiet xboard-audit-maintenance.timer && timer_running=1 || true
        systemctl stop xboard-audit-maintenance.timer >/dev/null 2>&1 || true
        systemctl stop xboard-audit-maintenance.service >/dev/null 2>&1 || true
        systemctl stop xboard-audit >/dev/null 2>&1 || true
        systemctl is-active --quiet xboard-audit-maintenance.service && die "日志维护任务仍在运行，拒绝卸载"
        systemctl is-active --quiet xboard-audit && die "审计服务仍在运行，拒绝删除状态和日志"
        systemctl is-active --quiet xboard-audit-maintenance.timer && die "清理定时器仍在运行，拒绝卸载"
    fi

    backup_dir=$(mktemp -d "$ETC/uninstall-backup.XXXXXX")
    for file in "$MASTER_LOCAL" "$MASTER_REMOTE" "$NODE_RULES" "$CLIENT_RULE"; do
        if [[ -f $file ]]; then cp -p "$file" "$backup_dir/" || die "无法备份 rsyslog 规则"; had_rules=1; fi
    done
    if (( had_rules )); then
        rm -f -- "$MASTER_LOCAL" "$MASTER_REMOTE" "$NODE_RULES" "$CLIENT_RULE"
        if ! rsyslogd -N1 >/dev/null 2>&1; then
            for file in "$backup_dir/"*; do [[ -f $file ]] && cp -p "$file" "$RSYSLOG_DIR/"; done
            if (( audit_running )); then systemctl start xboard-audit || true; fi
            if (( timer_running )); then systemctl start xboard-audit-maintenance.timer || true; fi
            die "移除本工具规则后 rsyslog 验证失败；规则已恢复，卸载未执行"
        fi
        if ! systemctl restart rsyslog; then
            for file in "$backup_dir/"*; do [[ -f $file ]] && cp -p "$file" "$RSYSLOG_DIR/"; done
            systemctl restart rsyslog || true
            if (( audit_running )); then systemctl start xboard-audit || true; fi
            if (( timer_running )); then systemctl start xboard-audit-maintenance.timer || true; fi
            die "rsyslog 重启失败；规则已恢复，卸载未执行"
        fi
    fi

    if (( was_master )); then
        systemctl disable xboard-audit-maintenance.timer >/dev/null 2>&1 || true
        systemctl disable xboard-audit >/dev/null 2>&1 || true
        systemctl clean --what=state xboard-audit-maintenance.timer >/dev/null 2>&1 || \
            rm -f -- /var/lib/systemd/timers/stamp-xboard-audit-maintenance.timer
        rm -f -- /etc/systemd/system/xboard-audit.service \
            /etc/systemd/system/xboard-audit-maintenance.service \
            /etc/systemd/system/xboard-audit-maintenance.timer
        systemctl daemon-reload
        rm -rf -- "$STATE_DIR" "${LOG_DIR%/nodes}"
        rm -f -- "$BIN" "${BIN}.previous"
    fi
    if (( was_client )); then rm -rf -- "$CLIENT_QUEUE_DIR"; fi
    # /etc/xboard-audit 是本脚本专用目录，路径在上方已固定校验。
    rm -rf -- "$ETC"
    rm -f -- "$MANAGER"
    if (( created_user )); then userdel xboard-audit >/dev/null 2>&1 || info "提示：专用服务账户未能删除，请手动检查"; fi
    if (( created_group )); then groupdel xboard-audit >/dev/null 2>&1 || info "提示：专用服务组未能删除，请手动检查"; fi
    info "本机 $role 组件已卸载。其它 VPS 请分别运行卸载命令。"
}

status() {
    systemctl --no-pager --full status rsyslog xboard-audit xboard-audit-maintenance.timer || true
    if [[ -f $ENV_FILE ]]; then
        info "发送间隔：$(env_get PUSH_INTERVAL_SECONDS) 秒；日志保留：$(env_get LOG_RETENTION_HOURS) 小时"
        if [[ -x $BIN ]]; then "$BIN" --status || true; fi
    fi
    local certificate
    for certificate in "$CERT_DIR/server.crt" "$CERT_DIR/client.crt"; do
        if [[ -f $certificate ]] && ! openssl x509 -in "$certificate" -checkend 2592000 -noout >/dev/null 2>&1; then
            info "提示：$(basename "$certificate") 已过期或将在 30 天内过期，请续期证书。"
        fi
    done
    if [[ -f $CERT_DIR/ca.crt ]] && ! openssl x509 -in "$CERT_DIR/ca.crt" -checkend 15552000 -noout >/dev/null 2>&1; then
        info "提示：CA 证书已过期或将在 180 天内过期；需要规划 CA 与所有客户端证书的轮换。"
    fi
}

menu() {
    local choice
    while true; do
        cat <<'EOF'
1) 安装主机           2) 安装客户端
3) 修改发送间隔       4) 修改日志保留时长
5) 修改 Telegram 配置 6) 添加 VPS 节点
7) 移除 VPS 节点      8) 发送测试消息
9) 查看状态          10) 卸载本机全部组件
11) 续期主机证书      12) 生成客户端续期包
13) 验证主机真实日志链路
14) 更新主机二进制与管理脚本
0) 退出
EOF
        read -r -p '选择：' choice || return 0
        case "$choice" in
            1) install_master ;;
            2) interactive_install_client ;;
            3) update_interval ;;
            4) update_retention ;;
            5) update_telegram ;;
            6) add_node ;;
            7) remove_node ;;
            8) require_master; "$BIN" --test-telegram ;;
            9) status ;;
            10) uninstall_all; return 0 ;;
            11) require_master; renew_server_cert ;;
            12) renew_node_cert ;;
            13) require_master; verify_real_source ;;
            14) upgrade_master ;;
            0) return 0 ;;
            *) info "未知选项，请重新选择" ;;
        esac
        printf '\n'
    done
}

need_root
case "${1:-menu}" in
    menu) menu ;;
    install-master) install_master ;;
    install-client) shift; install_client "$@" ;;
    upgrade) upgrade_master ;;
    interval) shift; update_interval "${1:-}" ;;
    retention) shift; update_retention "${1:-}" ;;
    telegram) update_telegram ;;
    add-node) shift; add_node "${1:-}" ;;
    remove-node) shift; remove_node "${1:-}" ;;
    maintenance) maintenance ;;
    test-telegram) require_master; "$BIN" --test-telegram ;;
    status) status ;;
    uninstall) uninstall_all ;;
    renew-server) require_master; renew_server_cert ;;
    renew-node) shift; renew_node_cert "${1:-}" ;;
    verify-source) require_master; verify_real_source ;;
    *) die "未知命令" ;;
esac
