//! YouTube API 客户端构建。
//!
//! 提供构建 `google_youtube3::YouTube` Hub 实例的工厂函数。
//!
//! ## 版本说明
//!
//! 使用 `hyper v0.14` 和 `yup-oauth2 v9`，与 `google-youtube3 v5` 兼容。

use std::path::Path;

use anyhow::Result;
use google_youtube3::YouTube;
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;

use super::oauth::load_authenticator;

/// 构建 YouTube API Hub 实例。
///
/// # Arguments
///
/// * `token_file` - OAuth2 token.json 文件路径
///
/// # Errors
///
/// 若认证失败，返回错误。
pub async fn build_youtube_hub(
    token_file: &Path,
) -> Result<YouTube<HttpsConnector<HttpConnector>>> {
    let auth = load_authenticator(token_file).await?;

    // hyper v0.14 的 HTTPS 连接器
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();

    let client = hyper::Client::builder().build::<_, hyper::Body>(https);

    Ok(YouTube::new(client, auth))
}
