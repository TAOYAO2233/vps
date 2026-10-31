# VPS Media Control Bot (Rust Edition)

基于 Rust 2021 Edition + Tokio + Teloxide 构建的 VPS 远程媒体控制 Telegram 机器人。
用于在 Linux VPS 上安全浏览与管理录播视频文件，提供**严格零重编码（`-c copy`）**的视频无损合并、MP4 无损重封装、RTMP 直播推流、YouTube 自管可恢复分块上传，以及 `/storage512` 挂载核验与三级磁盘容量告警。

## 🌟 核心特性

- **🎞️ 严格零重编码红线**: 视频合并、MP4 转封装与 RTMP 推流全流程仅允许 `-c copy` 流复制与容器重封装，绝不调用任何音视频编码器，保护低配 VPS CPU 与原始画质。
- **🧩 多阶段无损合并**: 严格保留用户点击选择顺序（`Vec<PathBuf>`），合并前执行编码参数一致性与 `/storage512` 剩余空间预检；依次执行「Stage 1 极速直连 (`-f concat -c copy`) → Stage 2A 标准 TS 无损重封装 → Stage 2B 容错时间戳修复 TS 重封装 → Stage 2C 有界片尾截除探测（保守默认不自动丢内容）」，临时目录全程位于 `/storage512` 内并由 RAII 自动清理，输出采用硬链接原子发布防覆盖。
- **💽 `/storage512` 实时监控与三级告警**: 通过 `/proc/self/mountinfo` 核验真实挂载点（拒绝退化到根分区 `/` 或 `tmpfs`），在 `/start`、每次点击「返回主菜单」、启动自检及后台定时巡检时实时调用 `statvfs` 读取容量；严格按 `>90%`（一级）、`>95%`（二级）、`>98%`（三级）触发主动报警，支持越级单条合并通知、发送成功后置位、滞后回落复位与跨重启去重。
- **🔐 双层敏感信息脱敏**: 源码层移除明文 `rtmp_url` 打印并为 `Config` 手写白名单 `Debug`；输出流层通过 `RedactingMakeWriter` 统一拦截 `pretty` / `json` 日志及跨分块写入，自动将 Telegram Bot Token、RTMP 推流密钥、OAuth 凭据与 YouTube 会话签名参数替换为 `[REDACTED]`。
- **🛡️ 路径、菜单快照与删除身份防护**: `PathGuard` 强制 `canonicalize` 边界校验（失败绝不退化放行）；文件选择器绑定 `MenuSnapshot`（含 `snap_id` 与目录内容摘要 `listing_hash`）防止旧菜单或目录变动引发错选；批量删除绑定一次性确认令牌与文件身份快照（`size`、`modified`、Unix `(dev, ino)`），执行前逐项复核。
- **📡 可观测 RTMP 无损推流**: 显式流映射 `-map 0:v:0` + 可选音频流、`-rw_timeout` 网络超时保护、`-progress pipe:1` 结构化进度解析 + 独立后台 `stderr` 排空防管道死锁 + 本地进度停滞看门狗；支持未知时长媒体展示已推流时长与实时速率。
- **☁️ 自管 YouTube Resumable Upload**: 基于 `yup-oauth2` + `hyper` 自管 HTTP 可恢复分块上传，`YOUTUBE_UPLOAD_CHUNK_MB` 真正控制单次 `PUT` 分块大小（恒为 `256 KiB` 整数倍，`64 KiB` 流式缓冲低内存读取）；区分「本地已发送」与「服务端 `308 Range` 已确认」双进度，仅在收到 `200/201` 且解析出有效 `video_id` 时判定成功；网络中断重试前先查询服务端已接收偏移防止重复成片；排队上限受 `YOUTUBE_MAX_QUEUE_SIZE` 约束且排队任务可被 `/stop` 立即取消。
- **🧰 任务与文件管理**: `/tasks` 查看并单独停止任务，`/history` 查看本次运行的最近结果，`/pending` 核实待确认的 YouTube 会话（需启用 `requeue` 或 `resume` 持久化）；`/find` 按文件名筛选并支持选择本页文件。主菜单可直接打开同盘回收站，恢复文件或二次确认后清空；超过 `TRASH_RETENTION_DAYS` 的文件也会自动清理。
- **⏱️ 录播稳定性检查**: 上传、推流、合并和转封装前要求源文件在 `FILE_STABLE_SECONDS` 内未修改，并在短暂观察后再次核对大小与修改时间。

## 🛠️ 技术栈

- **语言**: Rust 2021 Edition (`rust-version = "1.75"`, `unsafe_code = "forbid"`)
- **异步运行时**: `tokio`（含 `SIGINT` + Unix `SIGTERM` 优雅停机）
- **Telegram 框架**: `teloxide` (`rustls` + `throttle`)
- **错误处理**: `anyhow` + `thiserror`
- **日志与脱敏**: `tracing` + `tracing-subscriber` + 自定义 `RedactingMakeWriter`
- **配置与序列化**: `dotenvy` + `serde` + `serde_json` + `url`
- **系统存储统计**: `rustix` (`statvfs`，Unix 平台安全封装)
- **YouTube 上传与鉴权**: `yup-oauth2` + `hyper` + `hyper-rustls` + `bytes`

## ⚙️ 快速开始

### 1. 环境准备

- Linux VPS（推荐 Debian / Ubuntu），已安装 `ffmpeg` 与 `ffprobe` 且位于 `PATH`
- Rust 工具链（`>= 1.75`）
- Telegram Bot Token（若旧 Token 曾出现在历史日志中，请先通过 [@BotFather](https://t.me/BotFather) 执行 `/revoke` 轮换）
- **单实例部署检查**：启动前确认旧版 Python 服务（如 `videobot.service`）或其他使用同一 Token 的进程已停止并禁用，避免触发 Telegram `409 Conflict: terminated by other getUpdates request`

### 2. 配置文件

```bash
cp .env.example .env
vim .env
chmod 600 .env
# 如需使用 YouTube 上传：
chmod 600 token.json
```

### 3. 编译与检查

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

更多运维细节、systemd 配置、待确认参数说明与故障排查请参阅 [使用指南.md](./使用指南.md) 与 [ARCHITECTURE.md](./ARCHITECTURE.md)。

## 📂 目录结构

```text
media-control-bot/
├── Cargo.toml                  # 项目依赖、MSRV 与 Lint 约束
├── .env.example                # 环境变量配置模板（仅含安全占位符）
├── README.md                   # 项目概览
├── ARCHITECTURE.md             # 架构与核心状态设计文档
├── 使用指南.md                  # 部署、配置、命令与排障指南
└── src/
    ├── main.rs                 # 入口：脱敏日志 → 配置校验 → 状态与磁盘巡检 → 路由 → SIGINT/SIGTERM 停机
    ├── config/                 # 强类型配置解析、合法性校验与白名单 Debug
    ├── core/                   # AppState、MenuSnapshot、删除令牌、独占任务与上传池并发协调
    ├── bot/                    # Telegram 命令、回调分发、InlineKeyboard 构建
    ├── actions/                # 业务逻辑层（browse / stream / youtube / concat / convert / delete）
    ├── media/                  # FFprobe 媒体流探测、FFmpeg 无损执行器、多阶段无损合并校验
    ├── rtmp/                   # RTMP 推流会话封装（-progress pipe:1 + stderr 排空 + 看门狗）
    ├── youtube/                # OAuth2 鉴权、自管 Resumable Upload 状态机、可选队列持久化
    ├── storage/                # PathGuard 边界守卫、目录扫描、同盘回收站、/storage512 挂载与告警监控
    ├── ui/                     # 主控面板（含实时磁盘状态）、分页计算、快照选择器渲染
    ├── utils/                  # 全局流式脱敏器 (redact)、日志初始化、格式化与时间工具
    └── errors/                 # 领域错误定义 (AppError)
```

## 📜 许可证

MIT License
