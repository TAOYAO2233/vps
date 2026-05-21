# 🌐 VPS 自动化管理与视频处理工具箱 (VPS-Toolkit)

欢迎使用 **VPS-Toolkit**！本项目是一套专为 VPS（虚拟专用服务器）运维、视频处理、直播推流以及 Telegram 交互管理打造 institue 级高效工具箱。

项目主要由两部分组成：

1. **🤖 Telegram Bots 家族**：涵盖全功能交互式文件管理器、Xboard 审计监控、Bilibili 直播检测等。
2. **🐚 常用 Shell 脚本集**：提供视频快速转码、拼接、下载、电报数据导出（TDL）及系统状态监控等一键式脚本。

---

## 📂 目录结构与模块概览

```
📁 vps
├─ README.md
├─ vps-telegram-bot
│  ├─ bilive_bot
│  │  ├─ blbot.service
│  │  ├─ bl_bot.py
│  │  └─ README.md
│  ├─ vpsupload_bot
│  │  ├─ bot_main.py
│  │  ├─ get josn.py
│  │  ├─ install_bot.sh
│  │  ├─ README.md
│  │  ├─ videobot.service
│  │  ├─ 使用向导.md
│  │  └─ 更新日志.md
│  └─ xboard_audit_bot
│     ├─ README.md
│     ├─ xboard-audit.service
│     ├─ xboard_audit.py
│     ├─ xboard_audit.sh
│     ├─ xboard_audit3.8.py
│     ├─ 使用向导.md
│     └─ 多台vps日志推送.md
└─ 常用VPS .sh文件
   ├─ sh-files
   │  ├─ convert_flv_copy_mp4.sh
   │  ├─ convert_flv_to_mp4.sh
   │  ├─ delete_old_files.sh
   │  ├─ download_video.sh
   │  ├─ linuxcheck.sh
   │  ├─ monitor_xrayr.sh
   │  ├─ sshkey_manager.sh
   │  ├─ stream_videos_with_interval.sh
   │  ├─ tdl_commands.sh
   │  ├─ tdl_forward_commands.sh
   │  ├─ v2bx
   │  │  └─ install.sh
   │  ├─ viedo_master.sh
   │  └─ 使用方法.md
   └─ vps-sh
      └─ lscolorsetup.sh

```