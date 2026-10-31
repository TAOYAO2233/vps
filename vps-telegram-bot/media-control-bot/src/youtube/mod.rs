//! YouTube API 基础设施层。
//!
//! 封装 YouTube Data API v3 的 OAuth2 认证、自管 Resumable Upload 协议及可选队列持久化。

pub mod api;
pub mod oauth;
pub mod persistence;
pub mod upload;
