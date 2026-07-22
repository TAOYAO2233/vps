# Media Control Bot - Rust 架构设计文档

本项目将 Python 编写的 Telegram 媒体控制机器人重构为符合企业级规范的 Rust 项目。

## 1. 核心架构

- **全异步架构**: 基于 `tokio`，保证高并发和高性能。
- **Telegram Bot 框架**: 采用 `teloxide`，利用其强大的 `dptree` 路由和 `Dialogue` 状态管理。
- **状态管理**: 采用 `Arc<RwLock<AppState>>` 实现线程安全、Tokio 安全的全局状态共享，避免 Python 中的 Dict 混乱。
- **错误处理**: 采用 `anyhow` 捕获应用层错误，`thiserror` 定义领域层具体错误类型。
- **配置管理**: `serde` + `dotenvy` 实现强类型的配置加载。
- **日志系统**: `tracing` + `tracing-subscriber` 提供结构化日志。

## 2. 模块划分

```text
media-control-bot/
├── Cargo.toml
├── README.md
├── .env
├── src/
│   ├── main.rs            # 程序入口，初始化日志、配置、状态、路由并启动 Bot
│   ├── config/            # 配置层
│   │   ├── mod.rs
│   │   └── settings.rs    # 定义 Config 结构体及加载逻辑
│   ├── errors/            # 错误处理层
│   │   ├── mod.rs
│   │   └── error.rs       # 定义 AppError，使用 thiserror
│   ├── core/              # 核心领域层
│   │   ├── mod.rs
│   │   ├── state.rs       # AppState，全局状态定义
│   │   ├── task_manager.rs# 任务锁、并发控制（如 YouTube 上传池 Semaphore）
│   │   ├── progress.rs    # 进度条生成与计算
│   │   └── permissions.rs # 权限校验（如 is_admin）
│   ├── bot/               # Telegram 交互层
│   │   ├── mod.rs
│   │   ├── commands.rs    # /start, /stop, /uploads 等指令处理
│   │   ├── callback.rs    # CallbackQuery 统一处理入口
│   │   ├── keyboard.rs    # InlineKeyboardMarkup 生成
│   │   └── router.rs      # teloxide dptree 路由配置
│   ├── actions/           # 业务逻辑层（将各个操作解耦）
│   │   ├── mod.rs
│   │   ├── browse.rs      # 浏览文件详情
│   │   ├── stream.rs      # RTMP 推流
│   │   ├── youtube.rs     # YouTube 批量上传
│   │   ├── concat.rs      # 视频智能合并（直连 + TS 容错）
│   │   ├── convert.rs     # 批量转码 MP4
│   │   └── delete.rs      # 批量删除
│   ├── media/             # 媒体处理基础设施层
│   │   ├── mod.rs
│   │   ├── ffmpeg.rs      # 封装 FFmpeg 命令行调用
│   │   ├── ffprobe.rs     # 封装 FFprobe 获取时长
│   │   ├── merge.rs       # 合并校验逻辑（时长、大小比对）
│   │   └── convert.rs     # 转码进度解析（time_regex）
│   ├── youtube/           # YouTube API 基础设施层
│   │   ├── mod.rs
│   │   ├── api.rs         # 构建 API Client
│   │   ├── upload.rs      # 分块上传逻辑
│   │   └── oauth.rs       # OAuth 凭证加载
│   ├── storage/           # 文件与存储基础设施层
│   │   ├── mod.rs
│   │   ├── filesystem.rs  # 文件大小格式化、安全删除
│   │   ├── path.rs        # 路径越界校验 (assert_path_inside_base)、自动重命名
│   │   └── scanner.rs     # 目录遍历（替代 os.walk，使用 walkdir/fs::read_dir）
│   ├── ui/                # UI 组件层
│   │   ├── mod.rs
│   │   ├── menu.rs        # 主菜单文本与键盘
│   │   ├── pagination.rs  # 分页逻辑计算
│   │   └── selector.rs    # 文件选择器状态与渲染
│   └── utils/             # 通用工具层
│       ├── mod.rs
│       ├── env.rs         # 环境变量辅助读取
│       ├── logger.rs      # Tracing 初始化
│       ├── format.rs      # 时长、进度等字符串格式化
│       └── datetime.rs    # 时间处理
```

## 3. 依赖清单 (Cargo.toml)

- `tokio`: 异步运行时 (`features = ["full"]`)
- `teloxide`: Telegram Bot 框架 (`features = ["macros"]`)
- `anyhow`, `thiserror`: 错误处理
- `tracing`, `tracing-subscriber`: 日志
- `serde`, `serde_json`: 序列化与反序列化
- `dotenvy`: `.env` 文件加载
- `walkdir`: 高效目录遍历
- `regex`: 正则表达式（用于解析 ffmpeg 输出和智能命名）
- `google-youtube3`, `hyper`, `hyper-rustls`, `yup-oauth2`: YouTube API 交互
- `dashmap`: 并发安全的哈希表（可选，视 AppState 设计而定）
- `lazy_static` 或 `once_cell`: 全局静态变量

## 4. 状态管理设计

```rust
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore, Mutex};
use tokio::task::JoinHandle;

pub struct AppState {
    pub current_dir: PathBuf,
    pub active_task: Option<TaskInfo>,
    pub youtube_pool: HashMap<String, UploadTask>,
    pub selections: HashMap<ActionType, HashSet<PathBuf>>,
    pub cancel_flag: bool,
    pub pending_delete_files: Vec<PathBuf>,
    pub youtube_semaphore: Arc<Semaphore>,
}

pub struct TaskInfo {
    pub name: String,
    pub handle: JoinHandle<()>,
}

pub struct UploadTask {
    pub filename: String,
    pub status: String,
    pub progress: f64,
    pub created_at: f64,
    pub handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum ActionType {
    Browse,
    Stream,
    Youtube,
    Concat,
    Convert,
    Delete,
}
```
