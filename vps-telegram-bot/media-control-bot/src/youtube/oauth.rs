//! YouTube OAuth2 认证。
//!
//! 从本地 `token.json` 文件加载 OAuth2 凭证，用于 YouTube Data API v3 调用。
//! 对应 Python 版本的 `Credentials.from_authorized_user_file` 调用。
//!
//! ## 版本说明
//!
//! 本模块使用 `yup-oauth2 v9` 配合 `hyper v0.14`，与 `google-youtube3 v5` 的内部依赖一致。

use std::path::Path;

use anyhow::{Context, Result};
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;
use tracing::debug;
use yup_oauth2::authenticator::Authenticator;

/// YouTube API 所需的 OAuth2 Scope。
#[allow(dead_code)]
pub const YOUTUBE_SCOPES: &[&str] = &["https://www.googleapis.com/auth/youtube.upload"];

/// 从 token.json 文件加载 OAuth2 认证器。
///
/// # Arguments
///
/// * `token_file` - token.json 文件路径
///
/// # Errors
///
/// 若文件不存在或格式不正确，返回错误。
pub async fn load_authenticator(
    token_file: &Path,
) -> Result<Authenticator<HttpsConnector<HttpConnector>>> {
    debug!(token_file = ?token_file, "Loading YouTube OAuth token");

    let secret = yup_oauth2::read_authorized_user_secret(token_file)
        .await
        .with_context(|| {
            format!(
                "Failed to read YouTube token file: {}",
                token_file.display()
            )
        })?;

    // hyper v0.14 的 HTTPS 连接器
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();

    let client = hyper::Client::builder().build::<_, hyper::Body>(https);

    let auth = yup_oauth2::AuthorizedUserAuthenticator::with_client(secret, client)
        .build()
        .await
        .context("Failed to build YouTube authenticator")?;

    Ok(auth)
}
