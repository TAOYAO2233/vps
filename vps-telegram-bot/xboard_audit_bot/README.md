# xboard-node 日志审计通知

本项目从 xboard-node 日志中提取连接来源 IP、目标地址、协议及耗时，将短时间内的记录按节点汇总后发送到 Telegram。它依赖现有日志格式中的 `inbound connection from` / `inbound connection to` 行；格式变化时需要调整正则。

## 文件

| 文件 | 用途 |
| --- | --- |
| [`xboard_audit.py`](xboard_audit.py) | Python 3.9+ 审计程序。 |
| [`xboard_audit3.8.py`](xboard_audit3.8.py) | Python 3.8 兼容版。 |
| [`xboard_audit.sh`](xboard_audit.sh) | 交互配置主控机、客户端和新增节点的安装脚本。 |
| [`xboard-audit.service`](xboard-audit.service) | 手动部署时可参考的 systemd 服务文件；路径应与实际部署一致。 |

程序默认监看 `/home/xboard_log/xboard.log` 与 `/home/xboard_log/xboard_vps_b.log`，按 `PUSH_INTERVAL` 汇总并发送给 `TG_CHAT_IDS`。这些值定义在 Python 源码顶部。安装脚本会创建本机日志导流服务 `xboard-tunnel`，配置 rsyslog 的 514/UDP 收发，并将审计程序部署到 `/root/xboard_log`。安装后需手动核对接收者 ID，详见[使用向导](使用向导.md)。

## 使用

推荐按[使用向导](使用向导.md)先配置主控机，再在客户端配置转发。多机 rsyslog 的线路与排错见[多台 VPS 日志推送](多台vps日志推送.md)。安装前阅读脚本：它会改动 `/etc/rsyslog.conf`、`/etc/rsyslog.d/`、`/etc/systemd/system/` 和可能存在的 `/etc/xboard-node/config.yml`，并重启相关服务。

此工具报告匹配到的日志事件，不会自动修复 xboard-node 故障，也不等同于完整的安全审计。日志包含来源 IP、UUID 片段和目标地址；请控制日志文件、Telegram 接收者和传输链路的访问权限。
