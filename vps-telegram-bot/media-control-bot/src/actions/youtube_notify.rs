//! Bounded delivery of upload results, independent of progress message edits.

use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::warn;

use crate::utils::redact::redact_secrets;
use crate::youtube::results::YoutubeResultStore;

/// Separate message delivery outcome; upload completion is unaffected.
pub(super) struct NotificationReport {
    pub delivered: bool,
    pub attempts: u32,
    pub last_error: Option<String>,
}

/// Update the task card and send a separate notification. Delivery failures must
/// never turn a confirmed upload into a failed upload or trigger another upload.
pub(super) async fn notify_result(
    bot: &Bot,
    progress: &Message,
    html: &str,
    record: Option<(&YoutubeResultStore, &str)>,
) -> NotificationReport {
    match tokio::time::timeout(
        super::TELEGRAM_FINAL_EDIT_TIMEOUT,
        bot.edit_message_text(progress.chat.id, progress.id, html)
            .parse_mode(ParseMode::Html),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!(error = %redact_secrets(&e.to_string()), "YouTube result card edit failed; sending separate notification"),
        Err(_) => warn!("YouTube result card edit timed out; sending separate notification"),
    }

    send_result_only(bot, progress.chat.id, html, record).await
}

/// Send a separate notification without overwriting the originating list panel
/// (e.g. `/history` or `/pending` inline keyboard messages).
pub(super) async fn send_result_only(
    bot: &Bot,
    chat_id: ChatId,
    html: &str,
    record: Option<(&YoutubeResultStore, &str)>,
) -> NotificationReport {
    let mut report = NotificationReport { delivered: false, attempts: 0, last_error: None };
    for attempt in 1..=3 {
        if let Some((store, task_id)) = record {
            if let Err(e) = store.begin_notification_attempt(task_id).await {
                warn!(task_id, error = %redact_secrets(&e.to_string()), "Cannot persist notification attempt; continuing delivery");
            }
        }
        report.attempts = attempt as u32;
        let retry_delay = match tokio::time::timeout(
            super::TELEGRAM_SEND_TIMEOUT,
            bot.send_message(chat_id, html).parse_mode(ParseMode::Html),
        )
        .await
        {
            Ok(Ok(_)) => {
                report.delivered = true;
                report.last_error = None;
                break;
            }
            Ok(Err(e)) => {
                report.last_error = Some(redact_secrets(&e.to_string()));
                warn!(attempt, error = %redact_secrets(&e.to_string()), "YouTube result notification failed");
                match e {
                    teloxide::RequestError::RetryAfter(seconds) => seconds.duration(),
                    _ => Duration::from_secs(attempt),
                }
            }
            Err(_) => {
                report.last_error = Some("Telegram result notification timed out".to_string());
                warn!(attempt, "YouTube result notification timed out");
                Duration::from_secs(attempt)
            }
        };
        // Keep task shutdown bounded without violating Telegram's Retry-After.
        if attempt == 3 || retry_delay > Duration::from_secs(30) {
            break;
        }
        tokio::time::sleep(retry_delay).await;
    }
    if let Some((store, task_id)) = record {
        if let Err(e) = store.finish_notification(task_id, report.delivered, report.last_error.as_deref()).await {
            warn!(task_id, error = %redact_secrets(&e.to_string()), "Cannot persist notification outcome");
        }
    }
    if !report.delivered {
        warn!(chat_id = %chat_id, attempts = report.attempts, "YouTube result notification undelivered; consult upload result logs and /history");
    }
    report
}

/// Fast single-shot card update for user-cancelled uploads so `/stop` never blocks on multi-attempt retries.
pub(super) async fn notify_cancelled_fast(bot: &Bot, progress: &Message, html: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        bot.edit_message_text(progress.chat.id, progress.id, html)
            .parse_mode(ParseMode::Html),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Response, Server};
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    async fn exercise_delivery(edit_ok: bool, send_failures: usize, broken_storage: bool) -> Vec<String> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_server = Arc::clone(&calls);
        let progress_json = serde_json::json!({
            "message_id": 7, "date": 0,
            "chat": {"id": 1, "type": "private", "first_name": "Admin"},
            "text": "upload progress"
        });
        let response_message = progress_json.clone();
        let service = make_service_fn(move |_| {
            let calls = Arc::clone(&calls_server);
            let message = response_message.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req: hyper::Request<Body>| {
                    let mut calls = calls.lock().unwrap();
                    let path = req.uri().path().to_ascii_lowercase();
                    let is_edit = path.ends_with("editmessagetext");
                    let sends = calls.iter().filter(|s: &&String| s.ends_with("sendmessage")).count();
                    calls.push(path);
                    let ok = if is_edit { edit_ok } else { sends >= send_failures };
                    let payload = if ok {
                        serde_json::json!({"ok": true, "result": message})
                    } else {
                        serde_json::json!({"ok": false, "error_code": 500, "description": "Internal Server Error"})
                    };
                    async move { Ok::<_, Infallible>(Response::new(Body::from(payload.to_string()))) }
                }))
            }
        });
        let server = Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
        let addr = server.local_addr();
        let handle = tokio::spawn(server);
        let bot = Bot::new("123456:test").set_api_url(format!("http://{addr}/").parse().unwrap());
        let progress = serde_json::from_value(progress_json).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");
        let results = YoutubeResultStore::beside_queue(&queue_path);
        results.save_confirmed(crate::youtube::results::UploadResultRecord::confirmed(
            "task", "video.mp4", 1, "video123",
        )).await.unwrap();
        if broken_storage {
            std::fs::write(dir.path().join("queue.json.results.json"), b"broken").unwrap();
        }
        let report = notify_result(&bot, &progress, "Upload complete: video123", Some((&results, "task"))).await;
        assert_eq!(report.delivered, send_failures < 3);
        assert_eq!(report.attempts as usize, (send_failures.saturating_add(1)).min(3));
        if !broken_storage {
            use crate::youtube::results::NotificationStatus;
            let stored = YoutubeResultStore::beside_queue(&queue_path).load().await.unwrap();
            assert_eq!(stored[0].notification_attempts, report.attempts);
            assert_eq!(stored[0].notification_status,
                if report.delivered { NotificationStatus::Sent } else { NotificationStatus::Failed });
        }
        handle.abort();
        let result = calls.lock().unwrap().clone();
        result
    }

    #[tokio::test]
    async fn sends_notification_even_when_edit_succeeds() {
        let calls = exercise_delivery(true, 0, false).await;
        assert_eq!(calls.len(), 2);
        assert!(calls[0].ends_with("editmessagetext"));
        assert!(calls[1].ends_with("sendmessage"));
    }

    #[tokio::test]
    async fn failed_edit_does_not_prevent_notification_retry() {
        let calls = exercise_delivery(false, 1, false).await;
        assert_eq!(calls.len(), 3);
        assert!(calls[1..].iter().all(|path| path.ends_with("sendmessage")));
    }

    #[tokio::test]
    async fn repeated_notification_failure_stops_after_three_attempts() {
        let calls = exercise_delivery(false, usize::MAX, false).await;
        assert_eq!(calls.len(), 4);
    }

    #[tokio::test]
    async fn persistence_failure_does_not_skip_notification() {
        let calls = exercise_delivery(false, 0, true).await;
        assert_eq!(calls.len(), 2);
    }
}
