# 🌐 VPS 自动化管理与视频处理工具箱 (VPS-Toolkit)

欢迎使用 **VPS-Toolkit**！本项目是一套专为 VPS（虚拟专用服务器）运维、视频处理、直播推流以及 Telegram 交互管理打造 institue 级高效工具箱。

项目主要由两部分组成：
1. **🤖 Telegram Bots 家族**：涵盖全功能交互式文件管理器、Xboard 审计监控、Bilibili 直播检测等。
2. **🐚 常用 Shell 脚本集**：提供视频快速转码、拼接、下载、电报数据导出（TDL）及系统状态监控等一键式脚本。

---

## 📂 目录结构与模块概览

```text
📁 VPS-Toolkit
│
├── 📁 vps-telegram-bot               # Telegram 机器人模块
│   ├── 📁 vpsupload_bot              # 核心模块：交互式 VPS 文件管理/上传/处理机器人
│   │   ├── bot_main.py               # 机器人主入口程序
│   │   ├── get josn.py               # JSON 数据解析与获取辅助工具
│   │   └── 更新日志.md                # v2.0.0 核心功能重构日志
│   │
│   ├── 📁 xboard_audit_bot           # 审计监控模块：Xboard 状态监控机器人
│   │   ├── xboard_audit.py           # 审计监控核心逻辑脚本
│   │   ├── xboard-audit.service      # Systemd 守护进程服务配置文件
│   │   └── systemctl守护进程.md       # 服务配置与运维命令指南
│   │
│   └── 📁 bilive_bot                 # 直播模块：Bilibili 直播状态监测机器人
│       └── bl_bot.py                 # 哔哩哔哩直播检测机器人脚本
│
└── 📁 常用VPS .sh文件/sh-files        # 独立 Shell 脚本工具箱
    ├── 📄 使用方法.md                # 脚本功能索引与简易说明
    ├── 📄 viedo_master.sh            # 视频大师：集成视频格式转换与流平滑拼接
    ├── 📄 convert_flv_copy_mp4.sh    # FLV 转 MP4 (封装拷贝流，极速无损)
    ├── 📄 convert_flv_to_mp4.sh      # FLV 转 MP4 (完全重编码，兼容性高)
    ├── 📄 download_video.sh          # 自动化视频下载脚本
    ├── 📄 stream_videos_with_interval.sh # 视频定时/循环直播推流脚本
    ├── 📄 tdl_commands.sh            # TDL工具：高效导出 Telegram 聊天消息记录
    ├── 📄 tdl_forward_commands.sh    # TDL工具：自动转发消息至个人会话或收藏夹
    ├── 📄 monitor_xrayr.sh           # XrayR 节点状态监控与自愈脚本
    ├── 📄 sshkey_manager.sh          # SSH 密钥安全管理与免密配置工具
    ├── 📄 linuxcheck.sh              # VPS 基准性能测试与系统安全基线检查
    ├── 📄 delete_old_files.sh        # 磁盘空间净化：定时自动清理过期历史文件
    └── 📁 v2bx
        └── install.sh                # V2bX 节点一键部署与环境安装脚本