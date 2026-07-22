//! 错误处理模块
//!
//! 定义应用级别的错误类型体系。
//! - [`AppError`] 是所有领域错误的统一枚举，使用 [`thiserror`] 派生实现。
//! - [`AppResult<T>`] 是 `Result<T, anyhow::Error>` 的类型别名，用于应用层函数签名。

mod error;

pub use error::AppError;

/// 应用层通用 Result 类型别名。
///
/// 使用 [`anyhow::Error`] 作为错误类型，便于跨层错误传播与上下文附加。
#[allow(dead_code)]
pub type AppResult<T> = anyhow::Result<T>;
