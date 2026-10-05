# 🌐 VPS 自动化管理、多媒体处理与运维工具箱 (VPS-Toolkit)

<div align="center">

![Rust](https://img.shields.io/badge/Rust-2021_Edition-orange?logo=rust)
![Python](https://img.shields.io/badge/Python-3.8%2B-blue?logo=python)
![Shell Script](https://img.shields.io/badge/Shell-Bash%20%2F%20Zsh-4EAA25?logo=gnu-bash)
![FFmpeg](https://img.shields.io/badge/FFmpeg-Supported-green?logo=ffmpeg)
![Telegram Bot](https://img.shields.io/badge/Telegram-Bot%20API-2CA5E0?logo=telegram)
![CI/CD](https://img.shields.io/badge/GitHub_Actions-Musl_Static_Build-blueviolet?logo=githubactions)
![License](https://img.shields.io/badge/License-MIT-green)

**一套专为 Linux VPS 服务器打造的企业级运维、自动化视频流媒体处理、日志审计与 Telegram 远程交互管理工具箱。**

[项目结构](#-项目结构与模块概览) • [Telegram 机器人矩阵](#-telegram-机器人矩阵) • [常用 Shell 脚本集](#-常用-vps-shell-运维与多媒体脚本集) • [CI/CD 自动化](#-cicd-自动化构建) • [安全建议](#-安全规范与最佳实践)

</div>

---

## 📖 项目简介

**VPS-Toolkit** 旨在解决 VPS 运维管理与多媒体处理中的痛点问题。无论是需要一个轻量强悍的 Telegram 远程面板来管理服务器视频与推流，还是需要一键式的系统安全排查、SSH 密钥纳管、Xboard 节点审计或视频批量无损转码，本项目均提供了开箱即用的解决方案。

### ✨ 核心亮点

- **🤖 双模 Telegram 机器人**：提供极速低耗的 **Rust 原生版本**（基于 Tokio + Teloxide 静态编译）以及开箱即用的 **Python 模块化版本**。
- **🎥 工业级视频处理**：支持两阶段容错视频智能拼接、MKV/FLV/TS 批量转码 MP4（`+faststart`）、无缝 RTMP 循环推流与 YouTube 批量受限并发上传。
- **🛡️ 服务器安全加固与审计**：包含防手滑回滚的 SSH 密钥与端口安全管理器、Linux 入侵与高危排查脚本、XrayR 实时日志监控告警以及 Xboard 节点全自动审计与故障自愈。
- **⚡ 运维效率工具集**：TDL 电报数据归档转发、yt-dlp 记忆式交互下载、终端高亮增强、自动化定时磁盘清理等实用工具。

---

## 📂 项目结构与模块概览

```text
vps/
├── .github/
│   └── workflows/
│       └── build.yml                   # GitHub Actions: Rust Bot musl 静态二进制编译工作流
├── vps-telegram-bot/                   # 🤖 Telegram 机器人矩阵
│   ├── media-control-bot/              # 【Rust 版】企业级高性能 VPS 媒体控制机器人
│   │   ├── Cargo.toml                  # Rust 依赖元数据配置
│   │   ├── .env.example                # 环境变量示例
│   │   ├── README.md                   # 模块架构与特性介绍
│   │   ├── 使用指南.md                  # 详尽的部署、配置、6大功能及排错手册
│   │   └── src/
│   │       ├── main.rs                 # 主程序入口
│   │       ├── actions/                # 业务逻辑（浏览、拼接、转码、删除、推流、上传）
│   │       ├── bot/                    # Teloxide 路由、命令与 Callback 处理
│   │       ├── config/                 # 环境变量配置加载与校验
│   │       ├── core/                   # 状态管理 (AppState)、任务管理器与权限控制
│   │       ├── errors/                 # thiserror 自定义错误体系
│   │       ├── media/                  # FFmpeg / FFprobe 异步包装封装
│   │       ├── rtmp/                   # RTMP 推流生命周期管理
│   │       ├── storage/                # 路径安全检查 (Path Traversal 防御) 与文件系统
│   │       ├── ui/                     # Telegram 内联键盘、分页器与菜单组件
│   │       ├── utils/                  # 格式化、日志 tracing、时间工具
│   │       └── youtube/                # YouTube Data API v3 OAuth2 批量上传
│   ├── vpsupload_bot/                  # 【Python / Rust 版】多媒体文件管理机器人
│   │   ├── bot_main.py                 # Python 版机器人主入口
│   │   ├── install_bot.sh              # 一键交互式部署脚本 (Debian/Ubuntu/CentOS)
│   │   ├── videobot.service            # Systemd 后台常驻守护进程配置
│   │   ├── get josn.py                 # YouTube OAuth2 授权 token.json 提取辅助脚本
│   │   ├── README.md                   # 详细使用与配置说明文档
│   │   ├── video_bot_mod/              # Python 模块化拆分源码
│   │   │   ├── actions.py              # 文件操作与 FFmpeg 核心执行
│   │   │   ├── auth.py                 # 管理员鉴权
│   │   │   ├── config.py               # .env 读取与校验
│   │   │   ├── handlers.py             # 命令与消息处理器
│   │   │   ├── media_utils.py          # 媒体探针、进度条与合并校验
│   │   │   ├── task_manager.py         # 独占任务与 YouTube 任务池管理
│   │   │   ├── ui.py                   # 键盘与界面渲染
│   │   │   └── youtube_upload.py       # YouTube 上传管道
│   │   └── RUST/                       # 早期 Rust 实现版本
│   │       ├── Cargo.toml
│   │       └── src/
│   └── xboard_audit_bot/               # 📊 Xboard 节点安全审计与运维机器人
│       ├── xboard_audit.py             # 审计监控主程序 (Python 异步)
│       ├── xboard_audit3.8.py          # 针对 Python 3.8 兼容优化版本
│       ├── xboard_audit.sh             # 一键安装部署脚本
│       ├── xboard-audit.service        # Systemd 系统服务
│       ├── 使用向导.md                 # 快速安装与维护指南
│       ├── 多台vps日志推送.md          # 基于 rsyslog UDP 的多节点集中日志上报方案
│       └── README.md                   # 功能特性说明
└── 常用VPS .sh文件/                    # 🐚 常用 Shell 运维与视频脚本集
    ├── sh-files/
    │   ├── viedo_master.sh             # Video Master v8.0: 交互式视频合并/转码/智能命名大师
    │   ├── convert_flv_copy_mp4.sh     # 高并发极速流复制无损 FLV 封装转 MP4
    │   ├── convert_flv_to_mp4.sh       # 多任务并发重编码 FLV -> MP4
    │   ├── stream_videos_with_interval.sh # 交互式/循环 RTMP 直播推流工具（支持元数据缓存与翻页）
    │   ├── download_video.sh           # yt-dlp 交互式下载器（带历史目录记忆与画质选择）
    │   ├── delete_old_files.sh         # 多目录定时批量清理过期文件
    │   ├── sshkey_manager.sh           # 企业级 SSH 密钥与端口安全配置（带配置校验与防火墙联动）
    │   ├── linuxcheck.sh               # Linux 服务器安全基线检查与应急响应脚本
    │   ├── monitor_xrayr.sh            # XrayR 访问日志实时监控与 Telegram 批量告警
    │   ├── tdl_commands.sh             # Telegram TDL 消息历史/媒体按时段批量导出
    │   ├── tdl_forward_commands.sh     # Telegram TDL 批量自动化转发
    │   ├── v2bx/
    │   │   └── install.sh              # V2bX 多协议节点服务一键安装
    │   └── 使用方法.md                 # 脚本使用速查表
    └── vps-sh/
        └── lscolorsetup.sh             # 终端 ls 配色高亮与别名一键配置 (支持 Bash/Zsh)
```

---

## 🤖 Telegram 机器人矩阵

### 1. `media-control-bot` (Rust 旗舰版多媒体控制机器人)

> 目录：[`vps-telegram-bot/media-control-bot`](file:///E:/Github%20project/vps/vps-telegram-bot/media-control-bot)

采用 **Rust 2021 + Tokio + Teloxide** 打造，专为生产级 VPS 环境设计，拥有极低资源消耗与超高并发稳定性。

- **核心功能**：
  - 🗂️ **交互式文件浏览**：在 Telegram 内以键盘分页浏览 `BASE_DIR` 目录下的多媒体，实时探针展示视频时长、大小与修改时间。
  - 🧩 **两阶段容错视频拼接**：首选 `ffmpeg concat` 极速无损合并；遇到时间戳错乱等异常时，自动回退到 `TS` 容器转码容错模式，并进行时长/体积双重校验。
  - 🔄 **批量重封装与转码**：支持将 `.mkv`、`.flv`、`.ts` 一键封装为优化过的 Web 友好 `.mp4`（启用 `+faststart`）。
  - 📡 **RTMP 单路直播推流**：将 VPS 上的视频直推至 YouTube、Bilibili 或自建 RTMP 服务。
  - ☁️ **YouTube 批量上传池**：基于 `tokio::sync::Semaphore` 控制最大并发数，支持排队、`/uploads` 进度查看和 `/stop` 中止。
  - 🛡️ **严格安全防护**：内置 `Path Traversal` 越界防护，任何超出根目录的操作均被拦截；删除操作强制二次确认。
- **快速构建与运行**：
  ```bash
  cd vps-telegram-bot/media-control-bot
  cp .env.example .env && vim .env
  cargo build --release
  ./target/release/media-control-bot
  ```
  详细说明请参阅 [media-control-bot 使用指南](file:///E:/Github%20project/vps/vps-telegram-bot/media-control-bot/%E4%BD%BF%E7%94%A8%E6%8C%87%E5%8D%97.md)。

---

### 2. `vpsupload_bot` (Python 模块化多媒体管理机器人)

> 目录：[`vps-telegram-bot/vpsupload_bot`](file:///E:/Github%20project/vps/vps-telegram-bot/vpsupload_bot)

采用经典 Python 异步架构，结构解耦清晰，自带完善的一键安装脚本，适合快速部署。

- **一键部署**：
  ```bash
  curl -sSO https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/install_bot.sh
  chmod +x install_bot.sh && sudo ./install_bot.sh
  ```
- **核心组件**：
  - `bot_main.py`：程序入口与 Handler 挂载。
  - `video_bot_mod/`：分层实现鉴权、UI 分页、FFmpeg 操作、任务队列与 YouTube 上传。
  - `videobot.service`：注册为 Systemd 服务守护运行。
  - `get josn.py`：配合 Google Cloud Console 快速生成 `token.json`。
  - `RUST/`：内含 Rust 移植版本源码。

---

### 3. `xboard_audit_bot` (Xboard 节点运维与审计告警机器人)

> 目录：[`vps-telegram-bot/xboard_audit_bot`](file:///E:/Github%20project/vps/vps-telegram-bot/xboard_audit_bot)

专为 Xboard 节点与代理服务设计的 Telegram 监控告警套件。

- **主要能力**：
  - 实时审计节点通信、违规流量与运行状况，触发策略即时推送到 Telegram。
  - 进程异常退出自动重启与自愈保护。
  - 配合 **rsyslog UDP** 实现跨多台 VPS 节点的日志汇总推送（详见 [多台vps日志推送.md](file:///E:/Github%20project/vps/vps-telegram-bot/xboard_audit_bot/%E5%A4%9A%E5%8F%B0vps%E6%97%A5%E5%BF%97%E6%8E%A8%E9%80%81.md)）。
- **一键部署**：
  ```bash
  curl -sS -O https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/xboard_audit_bot/xboard_audit.sh
  chmod +x xboard_audit.sh && ./xboard_audit.sh
  ```

---

## 🐚 常用 VPS Shell 运维与多媒体脚本集

位于 [`常用VPS .sh文件`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6) 目录，涵盖日常 VPS 管理中各类高频需求：

### 🎬 视频与流媒体处理

| 脚本文件 | 说明 | 核心亮点 |
| :--- | :--- | :--- |
| [`viedo_master.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/viedo_master.sh) | **Video Master Tool v8.0** | 交互式终端菜单，支持多段视频拼接、日期+标题智能命名提取、格式转换与时间戳修复。 |
| [`stream_videos_with_interval.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/stream_videos_with_interval.sh) | **RTMP 轮播推流工具** | 分页选择视频、元数据本地缓存加速、支持自定义间隔等待与无人值守循环直播推流。 |
| [`convert_flv_copy_mp4.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/convert_flv_copy_mp4.sh) | **FLV 极速流复制转 MP4** | 采用 `-c copy` 极速封装，支持设置最大并发任务数 `MAX_JOBS`，不消耗 CPU 算力。 |
| [`convert_flv_to_mp4.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/convert_flv_to_mp4.sh) | **FLV 重编码转 MP4** | 全量重编码转码方案，带任务日志记录与并发队列控制。 |
| [`download_video.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/download_video.sh) | **yt-dlp 增强下载器** | 交互式命令行，自动持久化记录上次下载路径，支持画质选择与播放列表解析。 |
| [`delete_old_files.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/delete_old_files.sh) | **过期录播与文件清理** | 遍历多个目标路径，自动检索并批量清除超过指定天数的文件，释放磁盘空间。 |

---

### 🛡️ 系统安全与运维审计

| 脚本文件 | 说明 | 核心亮点 |
| :--- | :--- | :--- |
| [`sshkey_manager.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/sshkey_manager.sh) | **企业级 SSH 密钥与端口管理器** | 支持 Ed25519/RSA 密钥生成与防重复导入；修改端口前执行 `sshd -t` 语法校验，失败自动回滚；自动联动 UFW/Firewalld/iptables 防火墙并更新 SELinux 标签。 |
| [`linuxcheck.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/linuxcheck.sh) | **Linux 安全基线与应急排查** | 检测常用系统命令是否被篡改、查看异常网络连接、对外开放端口、高危端口监听、系统启动项与计划任务排查。 |
| [`monitor_xrayr.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/monitor_xrayr.sh) | **XrayR 实时日志审计告警** | 实时监听访问日志，通过时间窗口（如 10 秒）聚合告警并推送到 Telegram，防止频繁告警风暴。 |
| [`lscolorsetup.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/vps-sh/lscolorsetup.sh) | **终端 ls 配色高亮与常用别名** | 自动识别 Bash / Zsh 环境，安全配置高对比彩色目录及 `ll`/`la` 别名，带配置备份与一键卸载功能。 |

---

### 📲 Telegram 数据与节点工具

| 脚本文件 | 说明 | 核心亮点 |
| :--- | :--- | :--- |
| [`tdl_commands.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/tdl_commands.sh) | **TDL 电报数据导出工具** | 基于 TDL 命令行，支持指定时间范围过滤导出群组/频道中的聊天记录与多媒体文件。 |
| [`tdl_forward_commands.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/tdl_forward_commands.sh) | **TDL 批量消息转发工具** | 将导出的本地消息记录自动化批量转发至自己的会话或指定频道，带成功与失败日志。 |
| [`v2bx/install.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/v2bx/install.sh) | **V2bX 节点安装维护脚本** | 快速拉取并部署 V2bX 多协议代理核心。 |

---

## ⚙️ CI/CD 自动化构建

仓库通过 [`.github/workflows/build.yml`](file:///E:/Github%20project/vps/.github/workflows/build.yml) 配置了自动化持续集成流水线：

- **触发条件**：当推送到 `main`/`master` 分支或对 `vps-telegram-bot/media-control-bot/**` 路径提交 PR 时自动触发。
- **构建环境**：Ubuntu Latest + `musl-tools` + `x86_64-unknown-linux-musl`。
- **构建产物**：完全静态链接的 Linux 二进制文件（无动态 glibc 依赖），可直接在任何主流 Linux VPS 上分发运行。
- **产物归档**：自动打包并上传构建产物 `media-control-bot-linux-x86_64`。

---

## 🔒 安全规范与最佳实践

1. **凭证隔离**：
   - 所有敏感配置（Telegram `BOT_TOKEN`、管理员 `ADMIN_ID`、YouTube `token.json` 等）均使用 `.env` 或独立文件管理，严禁提交到公共 Git 仓库中。
   - 生产环境中请务必为敏感文件配置严格的文件访问权限：
     ```bash
     chmod 600 .env token.json
     ```
2. **路径安全**：
   - 机器人所有文件操作均受到 `BASE_DIR` 目录越界检查（Canonicalize Path 约束），请合理设置根目录，避免设置为系统根目录 `/`。
3. **SSH 运维安全**：
   - 使用 [`sshkey_manager.sh`](file:///E:/Github%20project/vps/%E5%B8%B8%E7%94%A8VPS%20.sh%E6%96%87%E4%BB%B6/sh-files/sshkey_manager.sh) 更换端口或禁用密码登录时，脚本会自动执行严格的预检；请务必保留至少一个活跃终端连接，测试新连接成功后再退出。

---

## 📄 开源许可证

本项目基于 [MIT License](LICENSE) 开源协议发布。