# Media Control Bot — Rust 架构设计文档

本项目是基于 Rust 2021 Edition 构建的 VPS 远程媒体控制 Telegram 机器人，以零重编码（`-c copy`）、存储边界与挂载核验、全链路日志脱敏、可恢复上传状态机为核心设计约束。

## 1. 核心架构原则

- **全异步并发与优雅停机**: 基于 `tokio` 异步运行时，统一监听 `SIGINT` 与 Unix `SIGTERM`，支持独占媒体任务与受控并发 YouTube 上传池协同停机回收。
- **严格零重编码边界**: `media/` 与 `rtmp/` 层仅构建 `-c copy` 流复制与容器重封装参数（含 bitstream filter 与受控 `-t` 输入截除），在架构层面杜绝任何音视频编码器调用。
- **双层敏感信息防护**:
  1. **源头层**: `Config` 手动实现白名单 `Debug`，业务日志使用脱敏辅助函数（如 `redact_rtmp_target`、`redact_youtube_session_uri`），面向 Telegram 用户的错误统一经过 `sanitize_user_error`。
  2. **输出流层**: `tracing_subscriber` 挂载自定义 `RedactingMakeWriter`，对 `pretty` 与 `json` 格式及跨 `write()` 分块边界统一做模式与已注册密钥替换（`[REDACTED]`）。
- **状态与文件系统强一致性**:
  - 多选文件集合采用有序 `Vec<PathBuf>`，严格保持用户点击顺序；
  - 选择器交互绑定带目录内容摘要的 `MenuSnapshot`，拦截过期菜单与目录变更引起的序号漂移；
  - 路径访问统一经由 `PathGuard`（强制 `canonicalize` 校验，`BASE_DIR` 不可达或越界时立即拒绝，绝不退化放行）；
  - 中间产物统一收纳在 `/storage512` 下的 `TempWorkDir`（RAII `Drop` 自动清理），最终成片通过 `hard_link` 原子无覆盖发布；
  - 删除操作绑定一次性令牌与文件身份快照（`size`、`modified_nanos`、Unix `(dev, ino)`）。
- **无 `unsafe` 约束**: `Cargo.toml` 启用 `unsafe_code = "forbid"`，POSIX `statvfs` 文件系统统计通过安全封装库 `rustix` 调用。

## 2. 模块划分

```text
media-control-bot/
├── Cargo.toml
├── .env.example
├── README.md
├── ARCHITECTURE.md
├── 使用指南.md
└── src/
    ├── main.rs                 # 程序入口：脱敏日志初始化 → 配置加载校验 → 启动期磁盘自检与后台巡检 → Teloxide 路由 → SIGINT/SIGTERM 停机清理
    ├── config/                 # 配置层
    │   ├── mod.rs
    │   └── settings.rs         # Config、YoutubeRecoveryMode、环境变量解析、合法性校验与白名单 Debug
    ├── errors/                 # 错误处理层
    │   ├── mod.rs
    │   └── error.rs            # AppError 领域错误定义与用户侧脱敏友好文案映射
    ├── core/                   # 核心领域层
    │   ├── mod.rs
    │   ├── state.rs            # AppState、MenuSnapshot、ConfirmedFileTarget、PendingDeleteBatch、UploadTask
    │   ├── task_manager.rs     # 独占任务锁、YouTube 信号量/排队上限协调、SIGINT/SIGTERM 批量取消等待
    │   ├── progress.rs         # Telegram 消息节流进度更新器
    │   └── permissions.rs      # 唯一管理员 ADMIN_ID 鉴权守卫
    ├── bot/                    # Telegram 交互表示层
    │   ├── mod.rs
    │   ├── commands.rs         # /start（含实时磁盘采样）、/stop、/uploads 命令处理
    │   ├── callback.rs         # CallbackQuery 解析、快照核验与动作分发
    │   ├── keyboard.rs         # 主菜单、带 snap_id 的文件选择器与带 token 的删除确认键盘
    │   └── router.rs           # teloxide dptree 消息与回调路由树
    ├── actions/                # 业务编排层
    │   ├── mod.rs
    │   ├── browse.rs           # 浏览媒体流详情（弹窗超长自动降级为消息展示）
    │   ├── stream.rs           # RTMP 推流预检、码率风险提示、会话拉起与真实完成语义通知
    │   ├── youtube.rs          # YouTube 入队限流、排队期即时取消、双进度汇报与可选恢复接入
    │   ├── concat.rs           # 多阶段无损合并编排、空间与流一致性预检、原子成片发布
    │   ├── convert.rs          # 批量无损 MP4 重封装与逐文件校验发布
    │   └── delete.rs           # 一次性令牌核验与逐文件身份复核后移入回收站
    ├── media/                  # 媒体处理基础设施层（零重编码）
    │   ├── mod.rs
    │   ├── ffprobe.rs          # JSON 流信息探测（MediaProbeInfo）、合并兼容性核验
    │   ├── ffmpeg.rs           # FFmpeg 无损执行器（concat / TS remux / MP4 remux / 并发 stderr 排空）
    │   └── merge.rs            # Stage 1 / 2A / 2B / 2C 多阶段无损合并状态机与严格成片校验
    ├── rtmp/                   # RTMP 直播推流基础设施层
    │   ├── mod.rs
    │   └── stream.rs           # -progress pipe:1 解析、独立 stderr 排空、未知时长适配与停滞看门狗
    ├── youtube/                # YouTube 上传基础设施层
    │   ├── mod.rs
    │   ├── oauth.rs            # authorized_user OAuth2 凭据加载与 Bearer Token 获取
    │   ├── api.rs              # HTTPS 客户端构建与上传客户端装配
    │   ├── upload.rs           # 自管 HTTP Resumable Upload（256 KiB 对齐分块、308 Range 核验、退避重试、源文件身份复核）
    │   └── persistence.rs      # 可选上传队列持久化存储（none / requeue / resume，0600 原子写盘）
    ├── storage/                # 存储与文件系统基础设施层
    │   ├── mod.rs
    │   ├── path.rs             # PathGuard 严格边界守卫、无冲突输出路径计算、智能合并命名
    │   ├── filesystem.rs       # TempWorkDir（限定于 /storage512 内）、硬链接原子发布、身份复核删除
    │   ├── scanner.rs          # 单层目录异步扫描与稳定排序（过滤非视频与临时隐藏项）
    │   └── disk.rs             # /proc/self/mountinfo 挂载核验、statvfs 采样、三级容量报警与滞后复位状态机
    ├── ui/                     # UI 渲染层
    │   ├── mod.rs
    │   ├── menu.rs             # 主控面板渲染（嵌入 /storage512 实时容量行）
    │   ├── pagination.rs       # 通用分页切片计算
    │   └── selector.rs         # 选择器渲染与 MenuSnapshot 注册
    └── utils/                  # 通用工具层
        ├── mod.rs
        ├── redact.rs           # 全局敏感凭据注册表、URL/RTMP/JSON/Header 正则脱敏与 RedactingMakeWriter
        ├── logger.rs           # 挂载脱敏 Writer 的 Tracing 初始化
        ├── format.rs           # 字节容量、时长、速率与标题清洗格式化
        └── datetime.rs         # 时间戳格式化工具
```

## 3. 关键依赖清单 (`Cargo.toml`)

- `tokio`: 异步运行时、子进程管理、信号监听与并发原语
- `teloxide`: Telegram Bot API 框架（`rustls` + `throttle`）
- `anyhow` / `thiserror`: 应用层上下文错误链与领域层结构化错误枚举
- `tracing` / `tracing-subscriber`: 结构化日志事件与 JSON/Pretty 格式化层
- `serde` / `serde_json` / `dotenvy` / `url`: 配置反序列化、持久化状态读写与 URL 安全解析脱敏
- `regex` / `once_cell`: FFmpeg 输出解析、智能命名提取与日志敏感模式匹配
- `chrono`: 日期时间格式化
- `yup-oauth2` / `hyper` (`0.14`) / `hyper-rustls` (`0.25`) / `bytes` / `google-youtube3`: OAuth2 令牌刷新与自管 HTTP Resumable Upload 传输层
- `rustix` (`cfg(unix)`，`features = ["std", "fs"]`): 安全调用 POSIX `statvfs` 获取文件系统块统计

## 4. 核心状态结构概览

```rust
pub struct AppState {
    /// 当前浏览目录（始终位于 BASE_DIR 边界内）
    pub current_dir: PathBuf,
    /// 当前运行中的独占任务（推流 / 合并 / 转封装 / 删除）
    pub active_task: Option<TaskInfo>,
    /// 独占任务取消标志
    pub cancel_flag: bool,
    /// YouTube 上传任务池（key: task_id）
    pub youtube_pool: HashMap<String, UploadTask>,
    /// YouTube 最大并发上传信号量
    pub youtube_semaphore: Arc<Semaphore>,
    /// YouTube 允许存在的最大任务总数（运行中 + 排队中）
    pub youtube_max_total_tasks: usize,
    /// 各操作类型的有序文件选择列表（严格保留点击顺序，禁止 HashSet）
    pub selections: HashMap<ActionType, Vec<PathBuf>>,
    /// 当前活跃文件选择器快照（含 snap_id 与目录 listing_hash）
    pub current_snapshot: Option<MenuSnapshot>,
    /// 待确认删除批次（绑定一次性确认 token 与逐文件 ConfirmedFileTarget 身份快照）
    pub pending_delete: Option<PendingDeleteBatch>,
    /// 当前活跃子进程 PID（用于运维诊断对应）
    pub current_process_pid: Option<u32>,
    /// /storage512 磁盘实时采样与三级报警协调器
    pub disk_monitor: Arc<DiskMonitor>,
}
```
