# VPS 工具与 Telegram 机器人

这个仓库收录 Linux VPS 上使用的 Shell 脚本和 Telegram 机器人。项目彼此独立，部署前请进入相应目录阅读说明，并根据服务器路径、凭据和权限调整配置。

## 项目导航

| 目录 | 用途 | 文档 |
| --- | --- | --- |
| [`vps-telegram-bot/media-control-bot`](vps-telegram-bot/media-control-bot/) | Rust 媒体管理机器人：浏览、无损合并与转封装、RTMP 推流、YouTube 上传、回收站和磁盘告警 | [README](vps-telegram-bot/media-control-bot/README.md) · [使用指南](vps-telegram-bot/media-control-bot/使用指南.md) · [架构](vps-telegram-bot/media-control-bot/ARCHITECTURE.md) |
| [`vps-telegram-bot/vpsupload_bot`](vps-telegram-bot/vpsupload_bot/) | Python 媒体管理机器人，另含独立的早期 Rust 实现 | [README](vps-telegram-bot/vpsupload_bot/README.md) |
| [`vps-telegram-bot/bilive_bot`](vps-telegram-bot/bilive_bot/) | 通过 Telegram 管理 Bililive-go 的直播任务；包含 Python 和 Rust 两种实现 | [README](vps-telegram-bot/bilive_bot/README.md) |
| [`vps-telegram-bot/xboard_audit_bot`](vps-telegram-bot/xboard_audit_bot/) | 解析 xboard-node 日志并向 Telegram 汇总推送 | [README](vps-telegram-bot/xboard_audit_bot/README.md) · [使用向导](vps-telegram-bot/xboard_audit_bot/使用向导.md) · [多机日志](vps-telegram-bot/xboard_audit_bot/多台vps日志推送.md) |
| [`常用VPS .sh文件`](常用VPS%20.sh文件/) | 系统维护、媒体处理及网络相关脚本 | [脚本说明](常用VPS%20.sh文件/说明.md) |

## 选择与运行

- 需要媒体管理与回收站功能，从 `media-control-bot` 开始。它要求 Rust 工具链、FFmpeg、FFprobe，以及 Linux 上可访问的媒体目录；部署示例见其[使用指南](vps-telegram-bot/media-control-bot/使用指南.md)。
- 已使用 Python 版媒体机器人，可查看 `vpsupload_bot`。它的 `install_bot.sh` 会安装系统依赖并在 `/root/video_bot` 创建服务。
- 使用 Bililive-go 的 `/api` 接口管理录制任务，查看 `bilive_bot`。Rust 的 `deploy.sh` 会克隆仓库、编译并创建 systemd 服务；Python 版需单独配置源码中的常量。
- 审计 xboard-node 日志，查看 `xboard_audit_bot`。其安装脚本会更改 rsyslog、systemd 和节点日志配置。

运行任何安装或清理脚本之前，请先阅读源码中的路径与写入操作。特别是 `delete_old_files.sh` 会直接删除匹配文件，`bilive_bot/deploy.sh` 可删除当前目录下既有的 `bilive_bot` 文件夹。先备份重要数据，并在测试环境核对配置。

## 凭据与自动构建

Telegram Token、YouTube OAuth 凭据及 RTMP 推流地址应保存在部署机器上，勿提交到仓库。Rust 媒体机器人提供 [`.env.example`](vps-telegram-bot/media-control-bot/.env.example)；其余项目的配置方式见各自文档。

[`build.yml`](.github/workflows/build.yml) 仅在 `media-control-bot` 目录变更且推送到 `main`/`master` 或向其发起 PR 时，构建 Linux x86_64 musl 二进制并上传为 Actions artifact。其他项目没有由该工作流构建。

## 许可证

[MIT](LICENSE)。
