//! 核心领域层
//!
//! 包含应用的核心状态、任务管理、进度计算和权限校验逻辑。
//! 这些模块不依赖任何 Telegram 或外部 API，保证领域逻辑的纯粹性。

pub mod permissions;
pub mod progress;
pub mod state;
pub mod task_manager;

pub use permissions::PermissionGuard;
pub use progress::ProgressBar;
