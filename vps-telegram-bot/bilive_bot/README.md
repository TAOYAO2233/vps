# 🤖 Bililive-go 录制控制机器人 (bilive_bot)

本模块是一个专为 `Bililive-go` 打造的 Telegram 联动控制端。通过异步 HTTP 请求与 Bililive-go 的 API 进行通信，实现移动端直观的直播录制任务管理与开播状态自动推送。

## 📌 核心功能
* **一键添加任务**：直接向机器人发送 Bilibili 或其他支持平台的直播间 URL，后台将自动解析并调用 API 写入录制队列。
* **可视化控制面板**：基于 Telegram 内联键盘（Inline Keyboard）设计，实时展示任务状态（🔴 录制中 / 🟢 监听中 / ⚪ 空闲），并提供启动监听、停止录制、删除任务等快捷操作。
* **双向数据同步**：在 Telegram 端的修改会自动同步至 Bililive-go 本地的 `config.yaml` 配置文件。
* **智能开播提醒**：后台维护一个 30 秒周期的异步轮询任务，一旦检测到目标主播开播并触发录制，会精准向配置的管理员列表推送开播通知。
* **安全白名单**：内置权限校验装饰器，仅限指定的管理员 ID 执行核心控制操作。

## 🛠️ 配置参数
请在 `bl_bot.py` 的文件头部修改以下核心配置：
* `API_BASE_URL`: Bililive-go 的 API 接口服务地址（例如 `http://127.0.0.1:9000/api`）。
* `TELEGRAM_TOKEN`: 从 Telegram 官方 `@BotFather` 处申请到的机器人 Token。
* `ADMIN_IDS`: 允许操控机器人的管理员 Telegram User ID 列表。
* `POLLING_INTERVAL`: 轮询检测开播状态的间隔时间（默认 `30` 秒）。

## 🚀 启动与运行
确保已安装 `httpx` 和 `python-telegram-bot` 依赖，然后在当前目录下执行：
```bash
python bl_bot.py