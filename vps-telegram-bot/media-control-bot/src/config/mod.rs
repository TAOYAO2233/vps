//! 配置模块
//!
//! 负责从 `.env` 文件和环境变量中加载并验证所有应用配置。
//! 使用 [`dotenvy`] 加载 `.env`，使用 [`serde`] 进行强类型反序列化。

mod settings;

pub use settings::{Config, VideoExtensions};
