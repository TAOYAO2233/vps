use std::{collections::HashMap, time::{Duration, Instant}};
use crate::models::{number, LiveDetail};

const SAMPLE_TTL: Duration = Duration::from_secs(600);
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

struct FileSample {
    path: String,
    size: f64,
    at: Instant,
    record_start: String,
}

pub(crate) enum SpeedEstimate {
    Unavailable,
    Sampling,
    Rate { bytes_per_second: f64, seconds: f64 },
}

#[derive(Default)]
pub(crate) struct SpeedTracker {
    samples: HashMap<String, FileSample>,
}

impl SpeedTracker {
    pub(crate) fn reset(&mut self, id: &str) {
        self.samples.remove(id);
    }

    pub(crate) fn estimate(&mut self, id: &str, detail: &LiveDetail) -> SpeedEstimate {
        let status = detail.recorder_status.as_ref();
        let path = status.and_then(|v| v.get("file_path"))
            .and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let size = status.and_then(|v| number(v, "file_size"));
        self.observe(
            id,
            detail.recording == Some(true),
            path.zip(size),
            detail.last_record_time.as_deref().unwrap_or(""),
            Instant::now(),
        )
    }

    fn observe(
        &mut self,
        id: &str,
        recording: bool,
        file: Option<(&str, f64)>,
        record_start: &str,
        now: Instant,
    ) -> SpeedEstimate {
        self.samples.retain(|_, sample| now.duration_since(sample.at) < SAMPLE_TTL);
        let (path, size) = match (recording, file) {
            (true, Some(file)) => file,
            _ => {
                self.reset(id);
                return SpeedEstimate::Unavailable;
            }
        };
        let mut estimate = SpeedEstimate::Sampling;
        if let Some(previous) = self.samples.get(id) {
            let elapsed = now.duration_since(previous.at);
            if previous.path == path && previous.record_start == record_start && size >= previous.size {
                if elapsed < MIN_SAMPLE_INTERVAL {
                    return estimate;
                }
                let seconds = elapsed.as_secs_f64();
                estimate = SpeedEstimate::Rate {
                    bytes_per_second: (size - previous.size) / seconds,
                    seconds,
                };
            }
        }
        self.samples.insert(id.into(), FileSample {
            path: path.into(), size, at: now, record_start: record_start.into(),
        });
        estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_click_preserves_baseline_and_calculates_rate() {
        let mut tracker = SpeedTracker::default();
        let now = Instant::now();
        assert!(matches!(tracker.observe("id", true, Some(("a", 0.0)), "session", now), SpeedEstimate::Sampling));
        assert!(matches!(tracker.observe("id", true, Some(("a", 20.0)), "session", now + Duration::from_millis(500)), SpeedEstimate::Sampling));
        match tracker.observe("id", true, Some(("a", 200.0)), "session", now + Duration::from_secs(2)) {
            SpeedEstimate::Rate { bytes_per_second, .. } => assert_eq!(bytes_per_second, 100.0),
            _ => panic!("expected rate"),
        }
    }

    #[test]
    fn changed_file_session_size_and_expired_sample_reset_baseline() {
        let mut tracker = SpeedTracker::default();
        let now = Instant::now();
        let cases = [("a", 100.0, "s1", 0), ("b", 200.0, "s1", 2),
                     ("b", 300.0, "s2", 4), ("b", 10.0, "s2", 6),
                     ("b", 100.0, "s2", 606)];
        for (path, size, session, seconds) in cases {
            assert!(matches!(tracker.observe("id", true, Some((path, size)), session, now + Duration::from_secs(seconds)), SpeedEstimate::Sampling));
        }
        assert!(matches!(tracker.observe("id", false, None, "", now + Duration::from_secs(607)), SpeedEstimate::Unavailable));
        assert!(tracker.samples.is_empty());
    }
}
