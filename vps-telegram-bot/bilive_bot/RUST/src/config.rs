use std::{collections::HashSet, env, error::Error, fmt};
use reqwest::Url;

pub(crate) struct Config {
    pub(crate) api_base_url: String,
    pub(crate) admin_ids: Vec<i64>,
    pub(crate) polling_interval: u64,
    telegram_token: String,
}

#[derive(Debug)]
pub(crate) struct ConfigError(&'static str);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Error for ConfigError {}

impl Config {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        let telegram_token = env::var("TELOXIDE_TOKEN")
            .map_err(|_| ConfigError("必须设置 TELOXIDE_TOKEN"))?;
        let telegram_token = telegram_token.trim().to_owned();
        if telegram_token.is_empty() {
            return Err(ConfigError("TELOXIDE_TOKEN 不能为空"));
        }
        let api_base_url = env::var("API_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000/api".into());
        let api_base_url = api_base_url.trim().trim_end_matches('/').to_owned();
        let url = Url::parse(&api_base_url)
            .map_err(|_| ConfigError("API_BASE_URL 必须是有效 HTTP(S) URL"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
            || url.query().is_some() || url.fragment().is_some()
        {
            return Err(ConfigError("API_BASE_URL 必须是无查询参数和片段的 HTTP(S) URL"));
        }
        let raw_ids = env::var("ADMIN_IDS")
            .map_err(|_| ConfigError("必须设置 ADMIN_IDS"))?;
        let mut seen = HashSet::new();
        let mut admin_ids = Vec::new();
        for raw in raw_ids.split(',') {
            let id = raw.trim().parse::<i64>()
                .map_err(|_| ConfigError("ADMIN_IDS 必须为逗号分隔的正整数用户 ID"))?;
            if id <= 0 {
                return Err(ConfigError("ADMIN_IDS 必须为正整数用户 ID"));
            }
            if seen.insert(id) {
                admin_ids.push(id);
            }
        }
        let polling_interval = env::var("POLLING_INTERVAL")
            .unwrap_or_else(|_| "30".into())
            .parse::<u64>()
            .map_err(|_| ConfigError("POLLING_INTERVAL 必须为正整数秒数"))?;
        if polling_interval == 0 {
            return Err(ConfigError("POLLING_INTERVAL 必须大于零"));
        }
        Ok(Self { api_base_url, admin_ids, polling_interval, telegram_token })
    }

    pub(crate) fn token(&self) -> &str {
        &self.telegram_token
    }

    pub(crate) fn is_admin(&self, user_id: u64) -> bool {
        i64::try_from(user_id).ok()
            .map_or(false, |id| self.admin_ids.contains(&id))
    }
}
