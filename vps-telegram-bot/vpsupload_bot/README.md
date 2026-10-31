# VPS 媒体机器人（Python 版）

当前部署入口是 [`python/bot_main.py`](python/bot_main.py)，业务逻辑在 `python/video_bot_mod/`。机器人通过 Telegram 管理 `BASE_DIR` 下的视频文件，支持目录浏览、RTMP 推流、YouTube 上传、视频合并、转为 MP4 和批量删除。`RUST/` 是另一套早期实现，不由本目录的安装脚本部署。

## 一键部署

在目标 VPS 下载或克隆仓库后，以 root 运行：

```bash
cd vps-telegram-bot/vpsupload_bot
sudo bash install_bot.sh
```

脚本支持 Debian 系与 Red Hat 系包管理器，安装 Python、FFmpeg/FFprobe 和 Python 包，复制本地源码（缺失时从仓库远端获取），在 `/root/video_bot` 创建虚拟环境与 `.env`，并配置 `videobot` systemd 服务。脚本会覆盖部署目录中的模块文件；升级前请备份该目录里的本地修改和凭据。

```bash
sudo systemctl status videobot
sudo journalctl -u videobot -f
```

## 配置

安装后编辑 `/root/video_bot/.env`，重启服务使配置生效：

| 配置 | 说明 |
| --- | --- |
| `BOT_TOKEN`、`ADMIN_ID` | 必填；Telegram Token 和唯一管理员数字 ID。 |
| `BASE_DIR` | 可操作的视频根目录；默认 `/storage512/bilivego/download`。 |
| `RTMP_URL` | 可选；未设置时无法推流。 |
| `TOKEN_FILE` | YouTube OAuth `authorized_user` 凭据路径，默认位于部署目录。 |
| `ITEMS_PER_PAGE`、`VIDEO_EXTENSIONS` | 列表分页数量与展示的媒体扩展名。 |
| `YOUTUBE_MAX_CONCURRENT_UPLOADS`、`YOUTUBE_UPLOAD_CHUNK_MB` | 并发数与上传分块大小；默认分别为 2 和 10 MB。 |
| `YOUTUBE_UPLOAD_QUEUE_FILE` | 上传队列状态文件路径。 |
| `FFPROBE_TIMEOUT_SECONDS`、`MERGE_MIN_DURATION_RATIO`、`MERGE_MIN_SIZE_RATIO` | 探测超时与合并结果校验参数。 |

YouTube 上传需要具备 `https://www.googleapis.com/auth/youtube.upload` scope 的 OAuth 凭据。将 `token.json` 放到 `TOKEN_FILE` 指向的位置，限制文件权限，然后重启服务。无需 YouTube 功能时可以不配置该文件。

## Telegram 操作

| 命令 | 作用 |
| --- | --- |
| `/start` | 打开主菜单。 |
| `/stop` | 中止当前长任务并取消上传。 |
| `/uploads` | 查看 YouTube 上传状态。 |

菜单提供文件浏览、RTMP 推流、YouTube 上传、合并、转换和删除入口。长任务由任务管理模块协调；操作前仍需核对所选文件，尤其是删除操作。当前程序按 `ADMIN_ID` 检查访问，并用实际路径限制 `BASE_DIR` 边界。

## 手动运行与排错

安装脚本完成后，可在部署目录使用虚拟环境运行 `bot_main.py`；systemd 服务配置见 [`videobot.service`](videobot.service)。如果机器人无响应，先查看服务日志、管理员 ID 与 Token，并确认没有另一实例使用同一 Token。视频信息或转换失败时，检查 `ffmpeg`、`ffprobe` 和媒体目录权限。配置改动后运行 `sudo systemctl restart videobot`。
