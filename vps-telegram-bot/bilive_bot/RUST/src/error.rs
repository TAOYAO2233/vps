use std::{error::Error, fmt};

/// Technical sources are retained without displaying URLs or response bodies.
#[derive(Debug)]
pub(crate) enum ApiError {
    Transport(reqwest::Error),
    Http(reqwest::StatusCode),
    Decode(serde_json::Error),
    Remote(i64),
    Sync(Box<ApiError>),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(_) => f.write_str("无法连接 API 或读取响应超时"),
            Self::Http(status) if *status == reqwest::StatusCode::NOT_FOUND =>
                f.write_str("接口或任务不存在（HTTP 404），请检查服务版本及任务列表"),
            Self::Http(status) => write!(f, "API 返回 HTTP {}", status),
            Self::Decode(_) => f.write_str("API 响应 JSON 或字段不兼容"),
            Self::Remote(code) => write!(f, "API 报告操作失败（错误码 {}）", code),
            Self::Sync(_) => f.write_str("操作已执行，但配置同步失败，请检查服务日志"),
        }
    }
}

impl Error for ApiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            Self::Decode(e) => Some(e),
            Self::Sync(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}
