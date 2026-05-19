# 📁 Xboard 审计监控机器人 (xboard_audit_bot)

这是一个基于 Python 异步架构开发的 Telegram 运维审计监控机器人。它专门用于对 Xboard 系统或节点环境进行实时健康检查、策略审计与自动故障修复。当发现异常或触发审计规则时，机器人会通过 Telegram 管道向管理员发送即时告警，并配合 Systemd 守护进程实现服务的常驻与自愈。

## 🚀 核心特性

- **自动策略审计**：对目标节点进行连续、自动化的状态审计与合规性检查。
- **故障自愈机制**：当检测到核心依赖或特定服务状态异常时，可触发自动拉起或重启修复。
- **原生 Systemd 守护**：自带标准的 `.service` 配置文件，完美融入 Linux 系统服务管理，支持开机自启和进程死掉后自动拉起。
- **异步非阻塞**：基于 Python 异步事件循环，确保监控轮询与 Telegram 消息推送高效并发，不占用过多 VPS 系统资源。

---

## 📂 文件结构

```txet
xboard_audit_bot
├── xboard_audit.py # 监控与审计核心逻辑脚本
├── xboard-audit.service # Systemd 系统服务配置文件
├── systemctl守护进程.md # 系统服务命令及运维操作指南
├── xboard_audit.sh # 一键运行脚本
└── README.md # 说明
```

---

## ⚙️ 部署与使用指南

> ### 1. 环境准备
>
> 确保您的 VPS 上已安装 Python 3.8+ 环境：
>
> ```bash
> # Ubuntu/Debian 系统示例
> sudo apt update
> sudo apt install python3 python3-pip -y
> ```
>
> ### 2. 安装依赖
>
> 请在 VPS 上安装机器人所需的依赖库：
>
> ```bash
> pip3 install python-telegram-bot
> ```
>
> ### 3. 修改核心配置
>
> 在配置守护进程前，请先打开 xboard_audit.py，根据代码顶部的变量定义，
> 修改您的 Telegram Bot Token、管理员 Chat ID 以及需要审计的节点/系统路径等核心参数。
>
> ### 4. 配置 Systemd 守护进程 (实现开机自启与常驻)
>
> 为了让脚本在后台稳定运行，请按照以下步骤将其注册为 Linux 系统服务：
>
> ```bash
> # 1. 将服务文件移动到系统服务目录下
> sudo cp xboard-audit.service /etc/systemd/system/
> # 2. 重新加载 Systemd 配置以识别新服务
> sudo systemctl daemon-reload
> # 3. 启用开机自启并立即启动该监控服务
> sudosystemctl enable --now xboard-audit
> # 4. 检查服务运行状态，确保显示 active (running)
> sudo systemctl status xboard-audit
> ```

---

## 📊 运维常用命令

> 查看实时运行日志与告警记录：
>
> ```bash
> journalctl -u xboard-audit -f
> ```
>
> 重启审计机器人：
>
> ```bash
> sudo systemctl restart xboard-audit
> ```
>
> 停止审计监控：
>
> ```bash
> sudo systemctl stop xboard-audit
> ```
