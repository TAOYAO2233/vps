# 刷新服务配置
systemctl daemon-reload

# 开启开机自启并现在启动
systemctl enable --now xboard-audit

# 查看运行状态是否为 active (running)
systemctl status xboard-audit

实时查看报错/运行日志	journalctl -u xboard-audit -f
重启监控服务	systemctl restart xboard-audit
停止监控服务	systemctl stop xboard-audit