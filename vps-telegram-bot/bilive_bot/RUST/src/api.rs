use std::time::Duration;
use reqwest::{Client, Method};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use crate::{callbacks::ControlAction, error::ApiError, models::{AddLivePayload, LiveDetail, LiveTask}};

/// HTTP transport only; UI, Telegram and sampling state do not belong here.
pub(crate) struct BiliLiveClient {
    client: Client,
    base_url: String,
}

impl BiliLiveClient {
    pub(crate) fn new(base_url: String) -> Result<Self, reqwest::Error> {
        let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
        Ok(Self { client, base_url })
    }

    async fn request<T: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> Result<R, ApiError> {
        let mut request = self.client.request(method, format!("{}{}", self.base_url, path));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(ApiError::Transport)?;
        if !response.status().is_success() {
            return Err(ApiError::Http(response.status()));
        }
        let bytes = response.bytes().await.map_err(ApiError::Transport)?;
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(ApiError::Decode)?
        };
        if let Some(code) = value.get("err_no").and_then(Value::as_i64) {
            if code != 0 {
                return Err(ApiError::Remote(code));
            }
        }
        serde_json::from_value(value).map_err(ApiError::Decode)
    }

    async fn sync_config(&self) -> Result<(), ApiError> {
        self.request::<(), Value>(Method::PUT, "/config", None).await
            .map(|_| ())
            .map_err(|error| ApiError::Sync(Box::new(error)))
    }

    pub(crate) async fn get_lives(&self) -> Result<Vec<LiveTask>, ApiError> {
        self.request::<(), Vec<LiveTask>>(Method::GET, "/lives", None).await
    }

    pub(crate) async fn get_detail(&self, id: &str) -> Result<LiveDetail, ApiError> {
        self.request::<(), LiveDetail>(Method::GET, &format!("/lives/{}", id), None).await
    }

    pub(crate) async fn add_live(&self, url: &str) -> Result<(), ApiError> {
        let payload = [AddLivePayload { url: url.into(), listen: true }];
        self.request::<_, Value>(Method::POST, "/lives", Some(&payload)).await?;
        self.sync_config().await
    }

    pub(crate) async fn delete_live(&self, id: &str) -> Result<(), ApiError> {
        self.request::<(), Value>(Method::DELETE, &format!("/lives/{}", id), None).await?;
        self.sync_config().await
    }

    pub(crate) async fn control_task(&self, action: ControlAction, id: &str) -> Result<(), ApiError> {
        let path = format!("/lives/{}/{}", id, action.as_str());
        self.request::<(), Value>(Method::GET, &path, None).await?;
        self.sync_config().await
    }
}
