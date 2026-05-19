🎬 VPS 多功能自动化管理与审计工具箱 (VPS Multi-functional Automation Management & Audit Toolkit)
本项目是一套专为 VPS（Virtual Private Server）运维、多媒体流媒体处理、直播自动化录制管理以及网络访问审计打造的全套 Telegram Bot 自动化解决方案。系统基于 Python 异步架构（依托 python-telegram-bot 与 asyncio）及高性能 Shell 脚本构建，旨在通过移动端 Telegram 交互界面，实现远程服务器的高效、零代码、可视化管控。

📂 项目矩阵与目录结构
Plaintext
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
🤖 核心机器人模块详解
1. 🤖 VPS 多媒体主控与媒体上传机器人 (vpsupload_bot)
本模块深度集成了 FFmpeg 工具链与 Google YouTube API v3，将您的远端 VPS 抽象为一个移动端可控的多媒体自动化处理工作站。在 v2.0.0 版本中系统已重构为全动态路径导航架构。

无限深度路径导航：支持在预设的根目录（BASE_DIR）下进行无限层级的目录穿梭。系统会自动对子文件夹（📁）和视频文件（🎥）进行分类及置顶排序，便于可视化筛选。

RTMP 无损直播推流：支持一键选择 VPS 本地的视频媒体流（如 .mp4, .mkv 等），在底层通过 FFmpeg 传入 -c copy 参数进行无损封装推流，将其直接发送至指定的直播平台（如 YouTube Live）。

YouTube 自动化分块上传：

断点续传机制：支持大体积视频的分块机制，默认分块（Chunk Size）大小为 10MB，有效对抗因跨境网络波动导致的传输中断。

隐私与内容安全限制：上传后的视频在默认状态下均会被自动设置为私享 (Private)，并强制开启 18+ 限制 (Made for Kids: False / Restricted)，防止因内容敏感引发版权或账号风控。

智能视频无损拼接 (Concat)：允许用户在 Telegram 交互界面中多选同目录下的视频文件，系统将根据文件名动态提取日期等元数据标识，自动生成 concat_list.txt，并调用 FFmpeg 流拷贝完成无损级合并拼接。

批量 MP4 封装重构：可针对非标准或多元化的视频容器（如 FLV、MKV 等）进行批量高速重封装，并在封装过程中注入 +faststart 标记（Moov Atom 置前），优化 Web 端及网络流式播放的响应首包时间。

异步线程控制与多任务管理：推流、转码以及 API 上传任务均配有高精度的异步文本进度条。当捕获到 /stop 指令时，Bot 会立刻通过异步信号安全终止底层的 FFmpeg 进程或清除 API 网络请求上下文。

🔑 YouTube API 凭据生成指引：

登录 Google Cloud Console，创建项目并启用 YouTube Data API v3，下载 OAuth 2.0 客户端凭据并重命名为 client_secrets.json 置于该模块目录下。

在配备有图形界面的本地计算机上运行认证辅助脚本进行授权：python "get josn.py"。

2. 🤖 Bililive-go 录制控制机器人 (bilive_bot)
本模块作为 Bililive-go 的 Telegram 联动控制端，利用异步 HTTP 请求与 Bililive-go 的 RESTful API 进行交互，实现了移动端可视化直播录制任务调度及开播状态通知推送。

一键动态任务添加：向机器人直接发送 Bilibili 或其他兼容平台的直播间 URL，系统会自动解析并请求后台 API 动态追加至录制队列中。

可视化内联控制面板：基于 Telegram 独特的内联键盘（Inline Keyboard）构建，能够实时反映当前的监听任务状态（🔴 录制中 / 🟢 监听中 / ⚪ 空闲），并提供诸如启动监听、停止录制、彻底删除任务等高频一键快捷键。

双向数据持久化同步：在 Telegram UI 端的任何队列修改都会实时触发 API 回调，自动重写并同步更新至 Bililive-go 本地的 config.yaml 核心配置文件。

智能开播状态轮询机制：后台常驻一个周期为 30秒 的异步守护轮询任务（Polling Task），一旦探测到目标主播开播并触发本地录制，便会精准向管理员列表分发推送包含直播状态的开播通知。

权限白名单控制：内置严格的权限校验装饰器（Decorator），仅允许在 ADMIN_IDS 数组中的合法 Telegram User ID 触发敏感的核心控制逻辑。

⚙️ 核心配置参数速查 (bl_bot.py)：

API_BASE_URL: Bililive-go 的后端服务地址 (例如 http://127.0.0.1:9000/api)。

TELEGRAM_TOKEN: 从官方 🪐 @BotFather 处申请到的机器人唯一 API Token。

ADMIN_IDS: 管理员的数值型 Telegram User ID 列表。

POLLING_INTERVAL: 主播开播检测状态的轮询间隔周期（缺省默认为 30 秒）。

3. 🤖 Xboard 网络访问审计监控机器人 (xboard_audit_bot)
本模块是专为网络节点访问日志及异常审计而设计的监控组件，支持多节点日志流的异步实时监听。它通过 Systemd 系统服务实现常驻运行，确保审计链路的稳定性与自愈性。

🛠️ Systemd 服务生命周期管理指令：

Bash
# 1. 刷新系统服务配置缓存
systemctl daemon-reload

# 2. 启用开机自启并立刻激活服务
systemctl enable --now xboard-audit

# 3. 查看实时运行状态与运行拓扑
systemctl status xboard-audit

# 4. 实时跟踪输出服务异常错误或运行日志
journalctl -u xboard-audit -f

# 5. 服务的重启与终止操作
systemctl restart xboard-audit
systemctl stop xboard-audit
🛠️ 自动化运维与多媒体处理脚本库 (常用VPS .sh文件/sh-files/)
本目录收录了日常 VPS 维护及多媒体处理过程中所需的独立 Shell 高性能脚本，提供了零交互或半交互的便捷调用方案：

🎬 视频转换与封装重构
convert_flv_copy_mp4.sh：利用 FFmpeg 对 FLV 视频文件进行高速无损重封装。由于仅执行流拷贝（-c copy），不经过 CPU 重编码，因而能够在极短时间内将容器转换为标准的 .mp4 格式。

convert_flv_to_mp4.sh：FLV 重编码标准 MP4 脚本。适用于视频编码格式不合规、需要进行强制重新编码规范化处理的特殊场景。

viedo_master.sh（交互式视频处理大师）：集成了视频格式高速转换、多段视频音视频流拼接、转码等复杂功能的综合型交互脚本。

📡 直播下载与循环推流
download_video.sh：多媒体流自动化网络下载脚本，支持对主流流媒体视频进行断点解析下载。

stream_videos_with_interval.sh：循环轮播推流控制脚本。支持指定固定的时间间隔或循环列表，将本地视频文件不间断地推流至指定的 RTMP 服务器上。

✈️ TDL 电报数据管理工具
tdl_commands.sh：基于 TDL 工具链封装的指令集，用于批量导出、备份 Telegram 指定频道的聊天消息与历史文本记录。

tdl_forward_commands.sh：TDL 媒体转发与收藏专用工具，支持跨频道/私聊进行大批量媒体文件的高速转发及个人收藏夹归档。

🛡️ 系统安全基线、监控与网络维护
linuxcheck.sh：Linux 服务器基线与安全检查脚本。一键排查系统内核漏洞、可疑隐藏进程、恶意后门、SSH 暴破日志以及系统基线配置隐患。

monitor_xrayr.sh：针对 XrayR 网络节点的常驻状态监控脚本。支持在节点服务死锁或崩溃时自动触发重启自愈机制（Self-healing Process）。

sshkey_manager.sh：SSH 密钥对自动化管理工具。用于一键分发、更新、加固 VPS 的公钥授权，防止基于密码的字典暴力破解。

delete_old_files.sh：定时磁盘清理与文件维护脚本。可配合 Cron 定期执行，智能扫描过期的大体积多媒体缓存，防止因录制爆满导致 VPS 系统底层崩溃。

v2bx/install.sh：V2bx 节点环境一键化部署脚本，快速完成依赖安装与基础网络节点环境拓扑。