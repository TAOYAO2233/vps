//! YouTube API 基础设施层
//!
//! 封装 YouTube Data API v3 的 OAuth 认证和视频上传逻辑。
//! 使用 `google-youtube3` + `yup-oauth2` + `hyper-rustls` 实现全异步上传。

pub mod api;
pub mod oauth;
pub mod upload;
