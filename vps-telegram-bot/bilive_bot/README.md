# Bililive-go Telegram 管理机器人

本目录有两个独立实现，都通过 Bililive-go 的 HTTP API 管理直播录制任务：`Python/bl_bot.py` 和 `RUST/src/main.rs`。不要用同一个 Telegram Bot Token 同时运行两版，否则轮询更新会互相冲突。

## 功能与文件

- 查看 Bililive-go 的直播任务和状态，通过 Telegram 按钮添加、删除、启动或停止任务。
- `Python/bl_bot.py`：Python 版，直接在源码顶部填写 API 地址、Token、管理员 ID 列表与轮询间隔。
- `RUST/`：Rust 版，从 `.env` 读取设置；`deploy.sh` 为它克隆源码、编译并创建 `bilive_bot.service`。

## Rust 版部署

先确认 Bililive-go 的 API 可以从部署机器访问，安装 `git`、`curl`、Rust/Cargo 和 systemd。建议在**空目录**中运行 `deploy.sh`：脚本会创建 `bilive_bot` 子目录，若该目录已存在且你选择覆盖，它会删除整个目录。

```bash
cd vps-telegram-bot/bilive_bot
sudo bash deploy.sh
```

脚本会交互收集 `API_BASE_URL`（默认 `http://127.0.0.1:9000/api`）、`TELOXIDE_TOKEN`、逗号分隔的 `ADMIN_IDS`、`POLLING_INTERVAL`（默认 30 秒）和 `RUST_LOG`，在新目录写入 `.env`，并构建 `RUST` 项目。脚本创建服务文件的重定向需要 root 权限；运行后检查：

```bash
sudo systemctl status bilive_bot
sudo journalctl -u bilive_bot -f
```

手动运行 Rust 版时，在 `RUST/` 下创建相同配置的 `.env`，执行 `cargo run --release`。程序入口使用 `dotenvy` 加载它。

## Python 版

安装 `python-telegram-bot` 与 `httpx`，编辑 [`Python/bl_bot.py`](Python/bl_bot.py) 顶部的 `API_BASE_URL`、`TELEGRAM_TOKEN`、`ADMIN_IDS`、`POLLING_INTERVAL`，再执行 `python3 Python/bl_bot.py`。Python 版没有从 Rust 部署脚本的 `.env` 读取配置。

## 运行注意

管理员操作会修改 Bililive-go 任务配置。先用测试任务验证 API 地址及权限。Token 应只保存在部署机器上；Python 版的源码配置方式容易误提交真实 Token，修改后检查 Git 状态。`deploy.sh` 会用 systemd 启动服务，重部署前先检查旧服务和目录。
