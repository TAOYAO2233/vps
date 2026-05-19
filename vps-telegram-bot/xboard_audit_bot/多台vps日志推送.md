## 🛑 第一阶段：准备公共放行目录

> 由于默认的 /root/ 目录存在严格的系统降权和隔离限制  
> 我们统一使用系统公共的 /home/xboard_log/ 目录来做数据交互
>
> #### 在 VPS-A 上分别输入以下命令：
>
> ```bash
> # 创建新的日志公共存放目录
> mkdir -p /home/xboard_log/
> # 创建 VPS-B 的接收日志文件
> touch /home/xboard_log/xboard_vps_b.log
> 赋予完全读写权限，彻底根除权限阻挡
> chmod 777 /home/xboard_log/
> chmod 666 /home/xboard_log/xboard_vps_b.log
> ```
>
> ### 在 VPS-B（客户端）上执行:
>
> ```bash
> # 同样创建公共目录
> mkdir -p /home/xboard_log/
> chmod 777 /home/xboard_log/
> ```
>
> ### 💡【核心添加】将本地 xboard-node 服务的日志实时、不断流地灌入公共目录：
>
> ```bash
> nohup journalctl -u xboard-node -f --no-pager > /home/xboard_log/xboard.log 2>&1 &
> ```

## 📡 第二阶段：配置客户端（VPS-B）日志外发

> 登录到 VPS-B（客户端），配置系统自带的 rsyslog 将日志实时打包发给主控机
>
> #### 1.写入推送规则
>
> ```bash
> nano /etc/rsyslog.d/xboard-forward.conf
> ```
>
> 粘贴以下内容（注意：请将 主控机VPS-A的公网IP 替换为 VPS-A 的真实公网 IP）：

> ```bash
> module(load="imfile")
>
> input(type="imfile"
> File="/home/xboard_log/xboard.log"
> Tag="xboard-node-b"
> Severity="info"
> Facility="local7")
>
> # 使用 UDP 将日志打给主控机
>
> local7.* @主控机VPS-A的公网IP:514
> ```
>
> #### 2.解除 rsyslog 降权并重启
>
> 为了让 rsyslog 能顺利跨目录读取文件，在 VPS-B 上运行：
>
> ```bash
> # 修改主配置文件，注释掉降权运行行
> sed -i 's/^\$PrivDropToUser/# \$PrivDropToUser/g' /etc/rsyslog.conf
> sed -i 's/^\$PrivDropToGroup/# \$PrivDropToGroup/g' /etc/rsyslog.conf
>
> # 重启服务
> systemctl restart rsyslog
> ```

## 📥 第三阶段：配置主控机（VPS-A）日志接收与分流

> 登录到 VPS-A（主控机 RN2H2G）。
>
> #### 1.开启 rsyslog 的 UDP 接收功能
>
> ```bash
> nano /etc/rsyslog.conf
> ```
>
> 找到以下两行（通常在第 15-20 行左右），取消它们前面的 # 号注释：
>
> ```bash
> module(load="imudp")
> input(type="imudp" port="514")
> ```
>
> #### 2.写入接收并独立保存规则
>
> ```bash
> nano /etc/rsyslog.d/xboard-recv.conf
> ```
>
> 粘贴以下内容：
>
> ```bash
> # 创建空白的目标接收文件并赋予权限
> if $msg contains 'xboard-node-b' or $rawmsg contains 'xboard-node-b' then {
>    action(type="omfile" file="/home/xboard_log/xboard_vps_b.log")
>    stop
> }
> ```
>
> 或者以下内容：
>
> ```bash
> # 如果日志来自 VPS-B，则写入独立日志文件，并停止后续规则处理
> if $fromhost-ip == 'VPS-B的公网IP' then {
>    action(type="omfile" file="/home/xboard_log/xboard_vps_b.log")
>    stop
> }
>
> ```
>
> #### 3.重启主控机服务
>
> ```bash
> systemctl restart rsyslog
> ```
>
> 💡 测试节点链路：  
> 此时让 VPS-B 产生点流量，在主控机执行
>
> ```bash
> tail -f /home/xboard_log/xboard_vps_b.log
> ```
>
> 只要屏幕上刷刷输出数据，说明日志同步彻底打通！
>
> ### 检查主控机（VPS-A）是否真的收到了数据
>
> 1.在 VPS-A 上安装抓包工具：
>
> ```bash
> apt-get install tcpdump -y  # Debian/Ubuntu
> # 或 yum install tcpdump -y  # CentOS
> ```
>
> 2.运行抓包命令，看看有没有来自 VPS-B 的 514 端口数据：
>
> ```bash
> tcpdump -i any udp port 514 -n -vv
> ```
>
> 保持抓包运行，去让你的 Xboard 节点产生一点流量（或者在 VPS-B 上随便重启一下某个服务产生系统日志）。  
> 3.观察 VPS-A 的屏幕：
>
> - 如果屏幕毫无动静：说明数据被 VPS-A 的防火墙（如 ufw/iptables）阻挡了，或者安全组（如阿里云/腾讯云/甲骨文的后台面板） 没有开放 UDP 514 端口。
> - 如果屏幕刷刷显示有数据包进来：说明网络通了，问题出在 VPS-A 的 rsyslog 规则配置上。

## 🤖 第四阶段：部署主控机（VPS-A）Python 集中审计服务

> #### 依旧在 VPS-A（主控机 ） 上操作。
>
> #### 1.部署完美兼容 Python 3.8 的核心脚本:
>
> ```bash
> nano /root/xboard_log/xboard_monitor.py
> ```
>
> 清空里面的旧内容，将以下完整代码粘贴进去：  
> [xboard_audit.py](https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/xboard_audit_bot/xboard_audit.py)  
> 请务必在脚本上方填好你的 TG_BOT_TOKEN 后保存退出。
>
> #### 2.重建 Python 3.8 纯净虚拟环境
>
> ```bash
> cd /root/xboard_log
> rm -rf xboard_env
>
> # 使用系统自带的 python3 构建
> python3 -m venv xboard_env
> ./xboard_env/bin/pip install --upgrade pip
> ./xboard_env/bin/pip install python-telegram-bot
> ```
>
> #### 3.配置 Systemd 守护进程守护
>
> ```bash
> nano /etc/systemd/system/xboard-audit.service
> ```
>
> 整盘覆写为以下完美对齐名称与工作路径的配置：
>
> ```bash
> [Unit]
> Description=Xboard Log全量网络审计Telegram机器人(集中管理版)
> After=network.target
>
> [Service]
> Type=simple
> User=root
> WorkingDirectory=/root/xboard_log
> ExecStart=/root/xboard_log/xboard_env/bin/python3 /root/xboard_log/xboard_monitor.py
> Restart=always
> RestartSec=5
>
>
> [Install]
> WantedBy=multi-user.target
> ```

## 🏁 第五阶段：全面拉起与验证

> 在 主控机（VPS-A） 上执行最后的启动总攻命令：
>
> ```bash
> # 刷新服务引擎
> systemctl daemon-reload
>
> # 启动并允许开机自启
> systemctl enable --now xboard-audit
>
> # 强制重启服务
> systemctl restart xboard-audit
>
> # 检查状态是否成功变绿
> systemctl status xboard-audit
> ```

## 📊 实时监控：

> 大功告成！现在直接输入 `bash
journalctl -u xboard-audit -f`
> 你就能看到两路日志流被主控机完美捕捉，Telegram 机器人也开始安稳、高效地为你全天候播报两个节点的聚合审计动态了。
