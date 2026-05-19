# 📁 VPS 交互式文件管理器与媒体处理机器人 (vpsupload_bot)

这是一个基于 Python 异步架构开发的全功能 Telegram 机器人。它能够将您的 Telegram 客户端转变为一个可视化的 VPS 远程文件管理器，并无缝集成基于 FFmpeg 的视频转码与流拼接功能。

---

## 🚀 v2.0.0 核心重构特性

在最新的 `v2.0.0` 版本中，项目迎来了底层架构的彻底重构，核心优化如下：

1. **动态路径导航系统 (Dynamic Path Navigation)**
   - **打破限制**：彻底取消了旧版本硬编码的单级目录限制。
   - **无限穿梭**：引入了 `BASE_DIR`（根目录）概念，通过 `context.user_data` 实时维护用户的路径状态，支持在 VPS 目录树中进行无限深度的文件夹下钻与穿梭。

2. **交互式文件系统浏览器**
   - **智能探测**：自动识别当前路径下的子目录，并在菜单顶部冠以 📁 标识优先排列，完全对齐主流文件管理器的操作逻辑。
   - **平滑回溯**：在非根目录下动态生成“⬆️ 返回上一级目录”按钮，方便随时一键返回。

3. **上下文感知的工作流 (Context-Aware Workflow)**
   - **合并任务 (Concat)**：视频拼接所需的临时 `.txt` 队列及最终生成的合并视频，将自动在“当前操作目录”下执行 I/O 操作，避免跨目录合并导致的路径溢出或找不到文件的错误。
   - **转码任务 (Convert)**：转码后的 `.mp4` 文件将直接存储在源文件所在目录，实现科学的就近管理原则。

4. **UI/UX 与异步性能升级**
   - **智能路径缩写**：在菜单标题中引入路径映射机制，将冗长的系统绝对路径映射为直观的 `🏠 根目录/..` 标识，大幅节省移动端屏幕空间。
   - **自动状态清理**：切换目录或返回上一级时，系统会自动重置当前的选择状态（Select Set），防范跨目录误操作。
   - **非阻塞异步分发**：所有核心 Action 逻辑均通过 `asyncio.create_task` 挂载，确保耗时较长的 FFmpeg 视频转码/合并任务在后台静默运行，绝不阻塞目录切换和前端 UI 的实时交互响应。

---

## 🛠️ 文件结构

```text
📁 vpsupload_bot
├── 📄 bot_main.py       # 机器人核心主程序（包含路由、异步任务分发及 UI 交互逻辑）
├── 📄 get josn.py       # JSON 数据解析与配置获取辅助工具
└── 📄 更新日志.md        # 版本演进与历史重大更新记录
└── 📄 README.md 
```
---
## ⚙️ 部署与使用指南
1. 环境准备
确保您的 VPS 上已安装 Python 3.8+ 以及 FFmpeg / FFprobe（用于读取视频时长和处理媒体）：

Bash
# Ubuntu/Debian 系统示例
sudo apt update
sudo apt install python3 python3-pip ffmpeg -y
2. 安装依赖
机器人核心及 YouTube API 所需的完整依赖库如下，请在 VPS 上执行安装：

Bash
pip3 install python-telegram-bot google-api-python-client google-auth-httplib2 google-auth-oauthlib
3. YouTube API 授权初始化（关键步骤）
由于 YouTube 上传功能使用的是 OAuth2 认证机制，需要提前生成包含 Refresh Token 的 token.json：

前往 Google Cloud Console 创建项目，启用 YouTube Data API v3。

配置 OAuth 同意屏幕（测试版），并创建 OAuth 2.0 客户端 ID（应用类型选择“桌面应用”）。

下载客户端凭据 JSON 文件，重命名为 client_secrets.json，并将其与 get josn.py 放在您本地电脑的同一个文件夹下。

在本地电脑上运行该脚本：

Bash
python "get josn.py"
此时本地电脑会弹窗浏览器要求人工授权，授权完成后，脚本会在同级目录下自动生成 token.json。

将生成的 token.json 复制或上传到 VPS 上机器人所在的 vpsupload_bot 文件夹中。

4. 机器人参数配置
在启动前，请用编辑器打开 bot_main.py，修改 == 配置区域 == 下的参数：

BOT_TOKEN：填写从 @BotFather 获取的真实 Telegram 机器人 Token。

ADMIN_ID：填写您个人的 Telegram 用户数字 ID（确保安全，非管理员无法操作）。

BASE_DIR：修改为您VPS上用于存放视频和浏览的基础根目录路径。

RTMP_URL：配置您需要推流的目标平台（如 YouTube/Bilibili）的 RTMP 推流地址。

5. 后台常驻启动
使用 nohup 命令让机器人在后台持久化运行：

Bash
nohup python3 bot_main.py > bot.log 2>&1 &
🎮 常用交互指令与特性
基础指令
/start - 唤醒机器人，展示主控制面板并进入文件浏览器根目录。

/stop - 随时强制终止正在运行的后台进程（如强制中断推流、批量转码或无损合并）。

文件选择模式说明
单选模式（查看详情、RTMP单路推流、YouTube上传）：直接点击对应文件即可触发相应动作。

查看详情：弹窗显示文件名、智能文件大小（MB/GB）、视频精确时长及修改时间。

YouTube 上传：以 10MB 分片断点续传方式异步上传，默认发布为 Private（私享） 且开启 18+（Age Restricted） 限制。

多选模式（智能视频合并、批量转码 MP4、批量删除）：

点击文件可进行勾选（⬜️ 变 ✅），支持跨页勾选。

勾选完成后，点击底部的 ▶️ 确认执行 (X 个文件) 批量提交任务。

智能合并：基于第一个文件的名称，自动通过正则提取日期并执行 smart_rename，实现无损、极速的视频顺序拼接。