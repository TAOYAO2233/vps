# 🎬 VPS 多功能自动化管理与审计工具箱 (VPS Multi-functional Automation Management & Audit Toolkit)

本项目是一套专为 VPS（Virtual Private Server）运维、多媒体流媒体处理、直播自动化录制管理以及网络访问审计打造的**全套 Telegram Bot 自动化解决方案**。基于 Python 异步架构（依托 `python-telegram-bot` 与 `asyncio`）及高性能 Shell 脚本构建，旨在通过移动端 Telegram 交互界面，实现远程服务器的高效、零代码、可视化管控。

---

## 📂 项目矩阵与目录结构

```text
vps-telegram-bot/
├── bilive_bot/                          # 🤖 Bililive-go 录制控制机器人
│   ├── bl_bot.py                        # 核心控制与消息推送代码
│   └── README.md                        # 模块部署与说明文档
│
├── vpsupload_bot/                       # 🤖 VPS 多媒体主控与媒体上传机器人
│   ├── bot_main.py                      # 主控面板异步核心代码
│   ├── get josn.py                      # YouTube API OAuth 凭据生成脚本
│   ├── 更新日志.md                       # v2.0.0 架构重构更新说明
│   └── README.md                        # 模块部署与说明文档
│
├── xboard_audit_bot/                    # 🤖 Xboard 网络访问审计监控机器人
│   ├── xboard_audit.py                  # 多节点日志流异步监听核心
│   ├── xboard-audit.service             # Systemd 守护进程配置文件
│   └── systemctl守护进程.md              # 服务管理常用指令速查
│
└── 常用VPS .sh文件/                     # 🛠️ 自动化运维与多媒体处理脚本库
    └── sh-files/
        ├── convert_flv_copy_mp4.sh       # FLV 高速无损封装转换脚本
        ├── convert_flv_to_mp4.sh         # FLV 重编码标准 MP4 脚本
        ├── delete_old_files.sh           # 定时磁盘清理与文件维护脚本
        ├── download_video.sh             # 多媒体流自动化下载脚本
        ├── linuxcheck.sh                 # Linux 服务器基线与安全检查脚本
        ├── monitor_xrayr.sh              # XrayR 节点状态监控与自愈脚本
        ├── sshkey_manager.sh             # SSH 密钥对自动化管理脚本
        ├── stream_videos_with_interval.sh # 循环轮播推流控制脚本
        ├── tdl_commands.sh               # TDL 电报聊天记录导出指令集
        ├── tdl_forward_commands.sh       # TDL 电报媒体转发与收藏指令集
        ├── v2bx/                         # V2bx 节点一键部署环境
        │   └── install.sh
        ├── viedo_master.sh               # 交互式视频处理大师（合并/转码）
        └── 使用方法.md                    # 脚本库功能索引与使用指南