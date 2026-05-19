# 🎬 VPS 多功能自动化管理与审计工具箱

欢迎使用本仓库。本项目是专门为 VPS 运维、多媒体处理、直播录制管理以及网络访问审计打造的**全套 Telegram Bot 自动化解决方案**。

仓库内包含三个独立且相辅相成的核心 Telegram 机器人，采用 Python 异步框架（`python-telegram-bot` 和 `asyncio`）编写，旨在帮助开发者和运维人员实现高效的远程服务器“零代码”管控。

---

## 📂 目录结构与项目矩阵

```text
vps-telegram-bot/
├── bilive_bot/                          # 🤖 Bililive-go 录制控制机器人
│   └── bl_bot.py                        # 核心控制与消息推送代码
│
├── vpsupload_bot/                       # 🤖 VPS 多媒体主控与媒体上传机器人
│   ├── bot_main.py                      # 主控面板（支持推流、转码、合并、上传）
│   ├── get josn.py                      # YouTube API OAuth 凭据生成本地脚本
│   └── 更新日志.md                       # v2.0.0 重构更新说明
│
└── xboard_audit_bot/                    # 🤖 Xboard 网络访问审计监控机器人
    ├── xboard_audit.py                  # 多节点日志流异步监听与时钟聚合广播核心
    ├── xboard-audit.service             # Systemd 守护进程配置文件
    └── systemctl守护进程.md              # 服务管理常用指令速查