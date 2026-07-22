# VPS Media Control Bot (Rust Edition)

基于 Rust 2024 Edition + Tokio + Teloxide 重构的企业级 Telegram 媒体控制机器人。
用于在 VPS 上远程管理视频文件，支持合并、转码、RTMP 推流、YouTube 批量上传等操作。

## 🌟 核心特性

- **🚀 极致性能**: 全异步 Tokio 架构，内存占用极低，无 Python GIL 限制。
- **🛡️ 类型安全**: 强类型检查，杜绝运行时 AttributeError，状态管理基于 `Arc<RwLock<AppState>>`。
- **📝 结构化日志**: 采用 `tracing`，支持控制台高亮输出与 JSON 生产级日志。
- **🗂️ 模块化架构**: 约 40 个源码文件，清晰的四层架构（Presentation / Service / Domain / Infrastructure）。
- **🔒 安全加护**: 内置路径越界防护（Path Traversal 防御），二次确认删除机制。
- **☁️ 并发上传**: YouTube 批量上传支持通过 `Semaphore` 控制并发数，自动排队与限流。
- **🎥 智能容错**: 视频合并提供「极速直连」与「TS 无损封装」两阶段容错机制。

## 🛠️ 技术栈

- **语言**: Rust 2024 Edition
- **异步运行时**: `tokio`
- **Telegram 框架**: `teloxide`
- **错误处理**: `anyhow` + `thiserror`
- **日志系统**: `tracing` + `tracing-subscriber`
- **配置管理**: `dotenvy` + `serde`
- **Google API**: `google-youtube3` + `yup-oauth2`

## ⚙️ 快速开始

### 1. 环境准备

- 安装 Rust 工具链 (`rustup default stable`)
- 安装 FFmpeg (`sudo apt install ffmpeg`)
- 获取 Telegram Bot Token (通过 [@BotFather](https://t.me/BotFather))
- (可选) 准备 YouTube OAuth2 `token.json`

### 2. 配置文件

复制示例配置并修改：

```bash
cp .env.example .env
vim .env
```

### 3. 编译运行

```bash
# 格式化与静态检查
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings

# 编译运行
cargo run --release
```

## 📂 目录结构

```text
media-control-bot/
├── Cargo.toml          # 项目依赖与元数据
├── .env.example        # 环境变量示例
├── src/
│   ├── main.rs         # 入口文件
│   ├── config/         # 配置解析层 (serde)
│   ├── core/           # 核心领域层 (状态管理、任务调度、权限)
│   ├── bot/            # 表示层 (Teloxide 路由、命令、回调)
│   ├── actions/        # 业务逻辑层 (合并、转码、上传等)
│   ├── media/          # 基础设施层 (FFmpeg/FFprobe 封装)
│   ├── youtube/        # 基础设施层 (OAuth2 & YouTube API)
│   ├── rtmp/           # 基础设施层 (RTMP 推流封装)
│   ├── storage/        # 基础设施层 (文件系统、路径安全)
│   ├── ui/             # 表示层组件 (分页、菜单、键盘)
│   ├── utils/          # 通用工具 (格式化、日志、时间)
│   └── errors/         # 错误定义 (thiserror)
```

## 📜 许可证

MIT License
