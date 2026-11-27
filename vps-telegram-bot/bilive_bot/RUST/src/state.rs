use tokio::sync::Mutex;
use crate::{api::BiliLiveClient, config::Config, speed::SpeedTracker};

/// Dependencies owned by one bot instance and shared with handlers and monitoring.
pub(crate) struct AppState {
    pub(crate) config: Config,
    pub(crate) api: BiliLiveClient,
    pub(crate) speed: Mutex<SpeedTracker>,
}

impl AppState {
    pub(crate) fn new(config: Config) -> Result<Self, reqwest::Error> {
        let api = BiliLiveClient::new(config.api_base_url.clone())?;
        Ok(Self { config, api, speed: Mutex::new(SpeedTracker::default()) })
    }
}
