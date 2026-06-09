# 📁 VPS 多媒体 Telegram Bot（vpsupload_bot）

`vpsupload_bot` 是一个面向 VPS 的 Telegram 多媒体管理机器人。它可以把 Telegram 聊天窗口变成一个远程文件管理面板，用来浏览视频目录、查看视频信息、RTMP 推流、YouTube 上传、智能合并、批量转码和批量删除文件。

当前版本采用模块化结构，主入口仍然是 `bot_main.py`，核心功能拆分到 `video_bot_mod/` 目录，方便后续维护和扩展。

---

## ✨ 功能特性

- **Telegram 可视化文件管理**：以 `BASE_DIR` 为根目录，支持子目录下钻、返回上级、分页浏览和视频文件筛选。
- **文件详情查看**：点击文件即可查看大小、视频时长和最后修改时间。
- **RTMP 单路推流**：调用 FFmpeg 以 `-re -c copy -f flv` 方式推送到配置好的 `RTMP_URL`。
- **YouTube 上传池**：支持多个视频进入上传队列，并通过并发池控制同时上传数量；可用 `/uploads` 查看任务状态，用 `/stop` 取消上传。
- **智能视频合并**：优先使用 FFmpeg concat 极速无损拼接；失败时自动进入 TS 容错流程，并进行时长与体积双校验。
- **批量转码 MP4**：将 `.mkv`、`.flv`、`.ts` 等视频重封装为 `.mp4`，并添加 `+faststart`。
- **批量删除文件**：删除前二次确认，避免误删。
- **路径安全防护**：所有文件操作都会限制在 `BASE_DIR` 内，避免路径越界。
- **`.env` 配置**：Token、管理员 ID、根目录、RTMP 地址等配置不再写死到 Python 源码中。
- **Systemd 常驻运行**：一键脚本自动注册 `videobot.service`，支持开机自启和日志查看。

---

## 🧩 项目结构

```text
vpsupload_bot/
├── bot_main.py                 # 入口文件：启动机器人并注册 handler
├── install_bot.sh              # 一键部署脚本
├── get_json.py                 # YouTube OAuth token.json 生成辅助脚本
├── README.md                   # 项目说明
└── video_bot_mod/              # 模块化核心代码
    ├── __init__.py             # 版本信息
    ├── actions.py              # 浏览、推流、合并、转码、删除动作
    ├── auth.py                 # 管理员权限校验
    ├── config.py               # .env 配置读取与运行前校验
    ├── handlers.py             # Telegram 命令与 CallbackQuery 路由
    ├── media_utils.py          # 路径安全、ffprobe、进度条、合并校验等工具
    ├── task_manager.py         # 独占任务与 YouTube 上传池管理
    ├── ui.py                   # 主菜单、文件选择器、分页、删除确认
    └── youtube_upload.py       # YouTube 上传命令与上传任务实现
```

> 说明：如果仓库中历史文件名仍为 `get josn.py`，安装脚本会将它下载并保存为 `get_json.py`。

---

## 🚀 一键部署（推荐）

部署脚本需要 root 权限，因为它会安装系统依赖、创建 `/root/video_bot` 工作目录并注册 Systemd 服务。

### 使用 curl

```bash
curl -sSO https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/install_bot.sh
chmod +x install_bot.sh
sudo ./install_bot.sh
```

### 使用 wget

```bash
wget -O install_bot.sh https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/install_bot.sh
chmod +x install_bot.sh
sudo ./install_bot.sh
```

脚本会自动完成：

1. 创建工作目录 `/root/video_bot`。
2. 安装 `curl`、`python3`、`python3-pip`、`python3-venv`、`ffmpeg` / `ffprobe`。
3. 创建 Python 虚拟环境 `/root/video_bot/yt`。
4. 安装 Python 依赖。
5. 从 GitHub 拉取 `bot_main.py` 和 `video_bot_mod/` 模块文件。
6. 交互式生成 `/root/video_bot/.env`。
7. 可选等待检测 `token.json`。
8. 注册并启动 `videobot.service`。

---

## 🛠️ 系统要求

### 支持系统

- Debian / Ubuntu
- CentOS / RHEL / Rocky Linux / AlmaLinux 等 RedHat 系发行版

### 基础依赖

- Python 3.8+
- pip
- venv（Debian / Ubuntu 需要）
- FFmpeg / FFprobe
- curl

### Python 依赖

安装脚本会自动安装以下依赖：

```bash
python-telegram-bot>=21,<23
google-api-python-client>=2,<3
google-auth-oauthlib>=1,<2
google-auth>=2,<3
```

---

## ⚙️ 配置说明

安装脚本会生成 `/root/video_bot/.env`，机器人启动时会优先读取系统环境变量，再读取 `.env`。

示例：

```env
BOT_TOKEN=123456789:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
ADMIN_ID=123456789
BASE_DIR=/storage512/bilivego/download
RTMP_URL=rtmp://example.com/live/your_stream_key
TOKEN_FILE=/root/video_bot/token.json
ITEMS_PER_PAGE=8
YOUTUBE_MAX_CONCURRENT_UPLOADS=2
YOUTUBE_UPLOAD_CHUNK_MB=10
MERGE_MIN_DURATION_RATIO=0.95
MERGE_MIN_SIZE_RATIO=0.30
```

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `BOT_TOKEN` | Telegram Bot Token，从 `@BotFather` 获取 | 必填 |
| `ADMIN_ID` | 允许操作机器人的 Telegram 用户数字 ID | 必填 |
| `BASE_DIR` | 机器人可浏览和操作的视频根目录 | `/storage512/bilivego/download` |
| `RTMP_URL` | RTMP 推流地址，可留空，不使用推流时不影响其他功能 | 空 |
| `TOKEN_FILE` | YouTube OAuth 凭据文件路径 | `/root/video_bot/token.json` |
| `ITEMS_PER_PAGE` | 文件列表每页显示数量 | `8` |
| `YOUTUBE_MAX_CONCURRENT_UPLOADS` | YouTube 同时上传数量上限 | `2` |
| `YOUTUBE_UPLOAD_CHUNK_MB` | YouTube 分片上传块大小，单位 MB | `10` |
| `MERGE_MIN_DURATION_RATIO` | 合并输出时长校验阈值 | `0.95` |
| `MERGE_MIN_SIZE_RATIO` | 合并输出体积辅助校验阈值 | `0.30` |

修改配置后重启服务：

```bash
nano /root/video_bot/.env
systemctl restart videobot
```

---

## 🔑 YouTube API 授权

YouTube 上传使用 OAuth 2.0，不建议也不能直接在 VPS 上保存 Google 账号密码。请先在本地电脑生成 `token.json`，再上传到 VPS。

### 1. 创建 Google Cloud 凭据

1. 登录 Google Cloud Console。
2. 创建项目，并启用 **YouTube Data API v3**。
3. 配置 OAuth 同意屏幕。
4. 创建 OAuth 2.0 客户端 ID，应用类型选择 **Desktop App / 桌面应用**。
5. 下载客户端 JSON 文件，重命名为 `client_secrets.json`。

### 2. 本地生成 token.json

把 `client_secrets.json` 和 `get_json.py` 放在同一个本地目录，然后运行：

```bash
python get_json.py
```

浏览器授权完成后，同目录会生成：

```text
token.json
```

### 3. 上传到 VPS

如果使用一键脚本部署，请把 `token.json` 上传到：

```text
/root/video_bot/token.json
```

常用 SCP 示例：

```bash
scp token.json root@你的服务器IP:/root/video_bot/token.json
```

上传后重启机器人：

```bash
systemctl restart videobot
```

---

## 🎮 Telegram 使用方法

### 基础命令

| 命令 | 作用 |
| --- | --- |
| `/start` | 打开主控制面板 |
| `/stop` | 停止当前独占任务，并标记所有 YouTube 上传任务取消 |
| `/uploads` | 查看 YouTube 上传队列、状态、进度和运行时间 |

> 只有 `.env` 中 `ADMIN_ID` 对应的 Telegram 用户可以操作机器人，其他用户会被静默忽略。

### 主菜单功能

#### 📂 浏览远程文件

进入 `BASE_DIR` 根目录后，可以：

- 点击 📁 文件夹进入下级目录。
- 点击“⬆️ 返回上一级目录”回退。
- 使用“上一页 / 下一页”翻页。
- 点击视频文件查看大小、时长和修改时间。

当前支持的视频扩展名：

```text
.mp4, .mkv, .flv, .ts
```

#### 📡 RTMP 单路推流

选择一个视频文件后，机器人会调用 FFmpeg 推流：

```bash
ffmpeg -re -i <file> -c copy -f flv <RTMP_URL>
```

推流过程中会在 Telegram 中显示进度条。发送 `/stop` 可中断推流。

#### ☁️ YouTube 上传

进入上传模式后可勾选一个或多个视频文件。机器人会：

- 创建 YouTube 上传任务。
- 按 `YOUTUBE_MAX_CONCURRENT_UPLOADS` 控制并发数量。
- 使用可恢复上传机制分片上传。
- 默认上传为 `private` 私享视频。
- 上传成功后返回 YouTube 视频链接和 Studio 编辑链接。

可随时发送：

```text
/uploads
```

查看上传队列。

#### ✂️ 智能视频合并

适合合并录播分段文件。使用方式：

1. 进入同一目录。
2. 勾选至少 2 个视频文件。
3. 点击“▶️ 确认执行”。

合并逻辑：

- 先尝试 FFmpeg concat 无损直连拼接。
- 输出强制为 `.mp4`。
- 自动避让同名文件，例如 `xxx.mp4` 已存在时生成 `xxx_1.mp4`。
- 合并后校验输出时长和体积。
- 如果直连拼接未通过校验，自动触发 `.ts` 容错处理后再次拼接。

#### 🔄 批量转码 MP4

用于把 `.mkv`、`.flv`、`.ts` 等文件重封装为 `.mp4`：

```bash
ffmpeg -y -i <input> -c copy -movflags +faststart <output.mp4>
```

该操作通常不会重新编码，速度快、CPU 占用低。

#### 🗑️ 批量删除文件

勾选文件后会进入二次确认页面，确认后才会永久删除。删除前请确认文件不再需要。

---

## 🔒 任务互斥与安全机制

- RTMP 推流、视频合并、批量转码、批量删除属于**独占任务**，同一时间只允许运行一个。
- YouTube 上传使用独立上传池，允许多个视频排队或并发上传。
- 当 YouTube 上传任务正在运行或排队时，合并、转码、删除、推流会被阻止，避免边上传边改动文件导致冲突。
- `/stop` 会终止当前 FFmpeg 进程，并给 YouTube 上传任务发送取消信号。
- 所有文件路径都会经过 `BASE_DIR` 越界检查，避免误操作系统目录。

---

## 🧪 手动部署方式

如果不想使用一键脚本，可以手动部署。

### 1. 准备目录

```bash
mkdir -p /root/video_bot
cd /root/video_bot
```

### 2. 拉取源码

```bash
BOT_MAIN_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/bot_main.py"
MODULE_URL_BASE="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/video_bot_mod"
GET_JSON_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/get%20josn.py"

curl -fSL -o bot_main.py "$BOT_MAIN_URL"
curl -fSL -o get_json.py "$GET_JSON_URL"
mkdir -p video_bot_mod
for file in __init__.py actions.py auth.py config.py handlers.py media_utils.py task_manager.py ui.py youtube_upload.py; do
    curl -fSL -o "video_bot_mod/$file" "$MODULE_URL_BASE/$file"
done
```

### 3. 创建虚拟环境并安装依赖

```bash
python3 -m venv yt
source yt/bin/activate
pip install --upgrade pip
pip install 'python-telegram-bot>=21,<23' 'google-api-python-client>=2,<3' 'google-auth-oauthlib>=1,<2' 'google-auth>=2,<3'
```

### 4. 写入 .env

```bash
cat > /root/video_bot/.env <<'ENV'
BOT_TOKEN=你的TelegramBotToken
ADMIN_ID=你的Telegram数字ID
BASE_DIR=/storage512/bilivego/download
RTMP_URL=
TOKEN_FILE=/root/video_bot/token.json
ITEMS_PER_PAGE=8
YOUTUBE_MAX_CONCURRENT_UPLOADS=2
YOUTUBE_UPLOAD_CHUNK_MB=10
MERGE_MIN_DURATION_RATIO=0.95
MERGE_MIN_SIZE_RATIO=0.30
ENV
chmod 600 /root/video_bot/.env
```

### 5. 编译检查

```bash
python3 -m py_compile bot_main.py video_bot_mod/*.py
```

### 6. 前台测试运行

```bash
/root/video_bot/yt/bin/python3 /root/video_bot/bot_main.py
```

---

## 🔄 更新源码

如果只是更新 GitHub 上的 `bot_main.py` 和 `video_bot_mod/`，不想覆盖 `.env`，可以执行：

```bash
cd /root/video_bot

BOT_MAIN_URL="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/bot_main.py"
MODULE_URL_BASE="https://raw.githubusercontent.com/TAOYAO2233/vps/refs/heads/main/vps-telegram-bot/vpsupload_bot/video_bot_mod"

curl -fSL -o bot_main.py "$BOT_MAIN_URL"
mkdir -p video_bot_mod
for file in __init__.py actions.py auth.py config.py handlers.py media_utils.py task_manager.py ui.py youtube_upload.py; do
    curl -fSL -o "video_bot_mod/$file" "$MODULE_URL_BASE/$file"
done

source yt/bin/activate
python3 -m py_compile bot_main.py video_bot_mod/*.py
systemctl restart videobot
```

---

## 🧰 Systemd 服务管理

一键脚本会注册服务：

```text
videobot.service
```

常用命令：

```bash
systemctl status videobot
journalctl -u videobot -f
systemctl restart videobot
systemctl stop videobot
systemctl start videobot
```

服务文件路径：

```text
/etc/systemd/system/videobot.service
```

默认工作目录：

```text
/root/video_bot
```

---

## ❓ 常见问题 FAQ

### Q1：发送 `/start` 后机器人没有反应

优先检查：

```bash
systemctl status videobot
journalctl -u videobot -n 100 --no-pager
```

然后确认 `.env` 里的 `ADMIN_ID` 是否是你的 Telegram 数字 ID。非管理员用户会被直接忽略。

### Q2：提示缺少 `token.json`

说明 YouTube OAuth 凭据还没有上传到 `TOKEN_FILE` 指定路径。一键部署默认路径是：

```text
/root/video_bot/token.json
```

上传后执行：

```bash
systemctl restart videobot
```

### Q3：无法获取视频时长，或进度条不更新

请确认 FFmpeg / FFprobe 可用：

```bash
ffmpeg -version
ffprobe -version
```

如果文件本身损坏、时间戳异常或没有可读 duration，机器人可能无法显示准确进度。

### Q4：YouTube 上传期间不能删除、转码或合并？

这是设计行为。上传池运行期间，独占任务会被阻止，避免同一个文件一边上传一边被删除、转码或合并。

### Q5：合并失败怎么办？

机器人会先尝试直连拼接，失败后自动触发 TS 容错。如果仍失败，通常说明片段编码参数、分辨率、时间戳差异过大。建议先对单个文件执行 MP4 重封装或统一转码后再合并。

### Q6：修改 `.env` 后为什么没生效？

`.env` 只在机器人启动时读取。修改后需要重启：

```bash
systemctl restart videobot
```

---

## 📌 版本说明

当前模块化版本：

```text
2.5.1-modular
```

主要变化：

- 从单文件大脚本拆分为 `video_bot_mod/` 模块化结构。
- `bot_main.py` 仅负责启动应用和注册 handler。
- 安装脚本支持从 GitHub 拉取模块化目录文件。
- 配置统一迁移到 `.env`。
- YouTube 上传池支持多个视频并发上传，独占任务仍保持互斥。

---

## ⚠️ 安全提示

- 不要把 `.env`、`token.json`、`client_secrets.json` 提交到公开仓库。
- `ADMIN_ID` 必须填写自己的 Telegram 数字 ID。
- `BASE_DIR` 建议设置为专门的视频目录，不要设置为 `/`、`/root` 或系统关键目录。
- 删除操作是物理删除，确认后不可恢复。
