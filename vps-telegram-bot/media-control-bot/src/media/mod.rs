//! 媒体处理基础设施层
//!
//! 封装 FFmpeg 和 FFprobe 的命令行调用，提供类型安全的 Rust 接口。

pub mod ffmpeg;
pub mod ffprobe;
pub mod merge;
