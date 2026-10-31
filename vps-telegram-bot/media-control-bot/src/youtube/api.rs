//! YouTube API Hub 辅助工厂（兼容 `google-youtube3 v5`）。

use std::path::Path;

use anyhow::Result;
use google_youtube3::YouTube;
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;

use super::oauth::load_authenticator;

/// 构建 `google_youtube3::YouTube` Hub 实例（供元数据扩展接口备用）。
#[allow(dead_code)]
pub async fn build_youtube_hub(
    token_file: &Path,
) -> Result<YouTube<HttpsConnector<HttpConnector>>> {
    let auth = load_authenticator(token_file).await?;

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();

    let client = hyper::Client::builder().build::<_, hyper::Body>(https);

    Ok(YouTube::new(client, auth))
}
