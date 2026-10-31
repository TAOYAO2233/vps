# 多台 VPS 向主控机转发 xboard-node 日志

下述链路与 [`xboard_audit.sh`](xboard_audit.sh) 使用的方式一致：客户端从本地 `journalctl -u xboard-node` 生成日志，rsyslog 把它发往主控机 514/UDP；主控机按节点标识写入 `/home/xboard_log/`，Python 审计程序逐个读取文件。UDP 不保证送达，也不提供传输加密；跨公网使用时至少在防火墙只允许已知客户端 IP，敏感环境宜使用受保护的网络。

## 主控机

先使用[使用向导](使用向导.md)配置主控机。脚本会启用 rsyslog 的 `imudp` 输入并创建 `/etc/rsyslog.d/xboard-recv.conf`，将匹配节点标识的消息写入对应日志文件。确认服务和端口：

```bash
sudo systemctl status rsyslog xboard-audit xboard-tunnel
sudo ss -lun | grep ':514'
sudo tail -f /home/xboard_log/xboard_vps_b.log
```

为每台新增客户端分配不同标识与日志文件，并把 `{"path": "…", "node_name": "…"}` 加入审计程序的 `LOG_TASKS`。避免多个节点共用标识，否则记录会混在一起。调整 rsyslog 规则后先执行 `sudo rsyslogd -N1`，再重启 rsyslog；调整 Python 列表后重启 `xboard-audit`。

## 客户端

在每台客户端运行安装脚本并选择客户端配置，填写主控 IP 和本机标识。脚本负责创建 `xboard-tunnel` 与 rsyslog 转发规则。检查：

```bash
sudo systemctl status xboard-tunnel rsyslog
sudo journalctl -u xboard-tunnel -e
sudo rsyslogd -N1
```

确认本机 `/home/xboard_log/xboard.log` 有新内容，主控机的对应节点文件也在增长。没有数据时依次检查 xboard-node 日志级别、客户端导流服务、514/UDP 防火墙、主控机 rsyslog 规则及审计服务日志。

## 日志权限

安装脚本会为 `/home/xboard_log` 设置宽松权限，便于 rsyslog 写入。部署完成后应按实际运行用户收紧目录和文件权限。Token、来源 IP、UUID 和访问目标均应视为敏感信息，不要将这些日志加入公开仓库。
