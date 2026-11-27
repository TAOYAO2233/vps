use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct LiveTask {
    #[serde(alias = "live_id")]
    pub(crate) id: String,
    #[serde(alias = "raw_url")]
    pub(crate) url: Option<String>,         // 部分API可能在不同字段中返回URL
    pub(crate) live_url: Option<String>,    // 兼容可能存在的不同字段
    pub(crate) host_name: Option<String>,
    pub(crate) room_name: Option<String>,
    pub(crate) recording: bool,
    pub(crate) listening: bool,
}

#[derive(Deserialize)]
pub(crate) struct LiveDetail {
    pub(crate) host_name: Option<String>,
    pub(crate) room_name: Option<String>,
    #[serde(alias = "url", alias = "live_url")]
    pub(crate) raw_url: Option<String>,
    pub(crate) listening: Option<bool>,
    pub(crate) recording: Option<bool>,
    pub(crate) recording_preparing: Option<bool>,
    pub(crate) live_start_time: Option<String>,
    pub(crate) last_record_time: Option<String>,
    pub(crate) recorder_status: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub(crate) struct AddLivePayload {
    pub(crate) url: String,
    pub(crate) listen: bool,
}

// Accept numeric strings used by recorder statistics without weakening the outer schema.
pub(crate) fn number(v: &serde_json::Value, key: &str) -> Option<f64> {
    let v = v.get(key)?;
    let n = v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok())?;
    if n.is_finite() && n >= 0.0 { Some(n) } else { None }
}
