# Media Control Bot

基于 Rust、Tokio 和 Teloxide 的 Telegram 媒体管理机器人，面向 Linux VPS 上的媒体目录。它通过 FFmpeg/FFprobe 浏览及处理视频，并提供 RTMP 推流、YouTube 上传、回收站和磁盘监控。管理员由 `ADMIN_ID` 指定。

## 功能

| 功能 | 当前实现 |
| --- | --- |
| 文件浏览与筛选 | 在 `BASE_DIR` 内分页浏览 `.mp4`、`.mkv`、`.flv`、`.ts`，查看媒体信息；`/find` 按文件名筛选。 |
| 视频合并 | 保留选择顺序，先尝试 concat 流复制，再按需尝试 TS 重封装和有界片尾修复；发布前检查结果。默认只报告可截除片尾，不自动截除。 |
| MP4 转封装 | 使用 `-c copy` 和 `+faststart`，不重编码。源编码与 MP4 不兼容时会失败。 |
| RTMP 推流 | 用 FFmpeg 实时读取文件并流复制推送到 `RTMP_URL`；目标平台仍须支持源编码。 |
| YouTube 上传 | OAuth 凭据、可恢复分块上传、并发及排队限制；可选 `none`、`requeue`、`resume` 重启恢复策略。 |
| 删除与回收 | 删除前二次确认，先移入同盘回收站；可恢复、手动清空或按保留天数自动清理。移入回收站不会释放空间。 |
| 磁盘监控 | 检查 `STORAGE_MOUNT_POINT` 的真实挂载及容量，按 >90%、>95%、>98% 三档提醒。 |

## 快速开始

需要 Linux、Rust/Cargo（`Cargo.toml` 声明最低 Rust 1.75）、FFmpeg、FFprobe、可访问的媒体目录和 Telegram Bot Token。

```bash
cd vps-telegram-bot/media-control-bot
cp .env.example .env
# 编辑 .env 中的 BOT_TOKEN、ADMIN_ID、BASE_DIR、STORAGE_MOUNT_POINT
cargo build --release
./target/release/media-control-bot
```

`.env.example` 中的 `RTMP_URL` 是示例值；不用推流时请清空。使用 YouTube 上传时，另准备 `TOKEN_FILE` 指向的 OAuth `authorized_user` 凭据。详见[使用指南](使用指南.md)。

## Telegram 命令

| 命令 | 用途 |
| --- | --- |
| `/start` | 打开主菜单和磁盘状态。 |
| `/stop` | 停止当前独占任务并取消上传任务。 |
| `/tasks` | 查看任务并单独停止。 |
| `/uploads` | 查看 YouTube 上传状态。 |
| `/history` | 查看本次运行的近期任务结果。 |
| `/pending` | 查询待核实的上传会话；需要启用持久化恢复策略。 |
| `/trash` | 浏览、恢复或清空回收站。 |
| `/find 关键词`、`/clearfilter` | 过滤或清除文件名筛选。 |

## 构建与文档

仓库的 [GitHub Actions 工作流](../../.github/workflows/build.yml) 在本目录发生变更时构建 `x86_64-unknown-linux-musl` 并上传 artifact。架构与状态边界见[架构说明](ARCHITECTURE.md)。真实 Token、RTMP 密钥、`token.json` 和状态文件应留在部署机器上。
