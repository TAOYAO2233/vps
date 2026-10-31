//! YouTube OAuth2 认证与访问令牌管理。
//!
//! 从本地 `token.json` 文件加载 OAuth2 授权用户凭据，自动将 `client_secret`、`refresh_token`
//! 与每次签发的 `access_token` 注册进全局脱敏器，防止任何异常日志泄露凭据。
//! 同时为凭据加载与令牌刷新提供有界超时和 `/stop` 取消监听保护。

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;
use tokio::sync::watch;
use tracing::debug;
use yup_oauth2::authenticator::Authenticator;

use crate::errors::AppError;
use crate::utils::redact::{redact_secrets, register_secret};

/// YouTube 上传所需的 OAuth2 Scope。
pub const YOUTUBE_SCOPES: &[&str] = &["https://www.googleapis.com/auth/youtube.upload"];

/// OAuth2 凭据加载与令牌刷新单次网络超时上限。
pub const OAUTH_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// OAuth2 认证器类型别名。
pub type YoutubeAuthenticator = Authenticator<HttpsConnector<HttpConnector>>;

/// 从 `token.json` 文件加载 OAuth2 认证器（兼容无取消上下文调用方）。
pub async fn load_authenticator(token_file: &Path) -> Result<YoutubeAuthenticator> {
    let (_tx, rx) = watch::channel(false);
    load_authenticator_cancellable(token_file, &rx).await
}

/// 从 `token.json` 文件加载 OAuth2 认证器（带取消监听与有界超时），并立即将敏感字段注册到全局脱敏表。
pub async fn load_authenticator_cancellable(
    token_file: &Path,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<YoutubeAuthenticator> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }

    if !token_file.exists() {
        return Err(AppError::YoutubeTokenMissing {
            path: token_file.to_path_buf(),
        }
        .into());
    }

    debug!(token_file = ?token_file, "Loading YouTube OAuth token file");

    let mut cancel_watch = cancel_rx.clone();
    let secret = tokio::select! {
        res = tokio::time::timeout(
            OAUTH_OPERATION_TIMEOUT,
            yup_oauth2::read_authorized_user_secret(token_file),
        ) => {
            match res {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return Err(anyhow::anyhow!(
                        "Failed to read YouTube token file ({}): {}",
                        token_file.display(),
                        redact_secrets(&e.to_string())
                    ));
                }
                Err(_) => {
                    return Err(AppError::YoutubeApiError {
                        message: "读取 OAuth token.json 超时".to_string(),
                    }.into());
                }
            }
        }
        _ = cancel_watch.changed() => {
            return Err(AppError::Cancelled.into());
        }
    };

    // 注册敏感凭据，确保任何格式化输出均被脱敏为 [REDACTED]
    register_secret(&secret.client_secret);
    register_secret(&secret.refresh_token);

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();

    let client = hyper::Client::builder().build::<_, hyper::Body>(https);

    let build_fut = yup_oauth2::AuthorizedUserAuthenticator::with_client(secret, client).build();
    let auth = tokio::select! {
        res = tokio::time::timeout(OAUTH_OPERATION_TIMEOUT, build_fut) => {
            res.map_err(|_| AppError::YoutubeApiError {
                message: "初始化 YouTube OAuth2 认证器超时".to_string(),
            })?
            .context("Failed to initialize YouTube OAuth2 authenticator")?
        }
        _ = cancel_watch.changed() => {
            return Err(AppError::Cancelled.into());
        }
    };

    Ok(auth)
}

/// 获取有效的 Bearer Access Token（兼容无取消上下文调用方）。
#[allow(dead_code)]
pub async fn fetch_access_token(
    auth: &YoutubeAuthenticator,
    force_refresh: bool,
) -> Result<String> {
    let (_tx, rx) = watch::channel(false);
    fetch_access_token_cancellable(auth, force_refresh, &rx).await
}

/// 获取有效的 Bearer Access Token（`force_refresh = true` 时强制向 Google 刷新一次；同时受超时与取消信号控制）。
pub async fn fetch_access_token_cancellable(
    auth: &YoutubeAuthenticator,
    force_refresh: bool,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<String> {
    if *cancel_rx.borrow() {
        return Err(AppError::Cancelled.into());
    }

    let mut cancel_watch = cancel_rx.clone();
    let tok_res = if force_refresh {
        tokio::select! {
            res = tokio::time::timeout(OAUTH_OPERATION_TIMEOUT, auth.force_refreshed_token(YOUTUBE_SCOPES)) => res,
            _ = cancel_watch.changed() => return Err(AppError::Cancelled.into()),
        }
    } else {
        tokio::select! {
            res = tokio::time::timeout(OAUTH_OPERATION_TIMEOUT, auth.token(YOUTUBE_SCOPES)) => res,
            _ = cancel_watch.changed() => return Err(AppError::Cancelled.into()),
        }
    };

    let tok = match tok_res {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            return Err(AppError::YoutubeApiError {
                message: format!("OAuth 令牌获取/刷新失败: {}", redact_secrets(&e.to_string())),
            }
            .into());
        }
        Err(_) => {
            return Err(AppError::YoutubeApiError {
                message: "OAuth 令牌获取/刷新请求超时".to_string(),
            }
            .into());
        }
    };

    let raw = tok
        .token()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AppError::YoutubeApiError {
            message: "OAuth 认证器返回了空的访问令牌".to_string(),
        })?
        .to_string();

    register_secret(&raw);
    Ok(raw)
}
