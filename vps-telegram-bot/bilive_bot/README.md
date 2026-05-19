# 📁 Bilibili 直播状态监测机器人 (bilive_bot)

这是一个基于 Python 异步架构开发的轻量级 Telegram 机器人。它通过定期轮询 Bilibili 开放接口，实时监控指定 Bilibili 主播的直播间状态。当主播开播或下播时，机器人会自动向指定的 Telegram 频道或群组推送美化后的实时通知。

---

## 🚀 核心特性

- **实时开播通知**：主播触发开播状态时，自动抓取直播间标题、分区信息及直播间链接，第一时间向 Telegram 发送通知。  
- **状态自适应轮询**：采用异步高并发轮询机制，支持配置多长时间检查一次直播间状态，对 VPS 性能消耗极低。  
- **日志追踪**：本地记录完整的开播与下播时间线日志，方便后续数据统计或运维排查。  

---

## 📂 文件结构
```text
bilive_bot/
└── bl_bot.py # 机器人主程序（包含 Bilibili API 状态解析、电报通知推送与轮询逻辑）
└── bl_bot.py 
```

---

## ⚙️ 部署与使用指南

### 1. 环境准备
确保 VPS 上已安装 Python 3.8+：

```bash
# Ubuntu/Debian 系统示例
sudo apt update
sudo apt install python3 python3-pip -y
```
### 2. 安装依赖
该机器人基于 python-telegram-bot 异步版本，同时需要 requests 或 aiohttp 来请求 Bilibili 接口：
```bash
pip3 install python-telegram-bot requests
```
### 3. 机器人参数配置
启动前，请编辑 bl_bot.py，根据代码顶部的配置区域修改以下参数：

BOT_TOKEN：从 @BotFather 获取的 Telegram 机器人 Token。

CHANNEL_ID / CHAT_ID：接收直播通知的 Telegram 频道 ID 或群组 ID（公开频道需确保机器人已加入并具备发送消息权限）。

BILI_ROOM_ID：需要监控的 Bilibili 直播间房间号或主播 UID（根据源码变量注释填写）。

CHECK_INTERVAL：轮询检查间隔（秒），建议 30~60 秒，避免请求过于频繁被 B 站风控。
### 4. 后台常驻启动
使用 nohup 命令让机器人在 VPS 后台持久运行，即使断开 SSH 连接也不会中断
```bash
nohup python3 bl_bot.py > bilive.log 2>&1 &
```
## 📊 运维与日常查看
### 查看实时运行日志 / 开播记录：
```bash
tail -f bilive.log
```
### 停止监控进程：
1.查找进程 ID（PID）:
```bash
ps ef | grep bl_bot.py
```
2.杀死对应进程
```bash
kill -9 <PID>
```
