//! Independent owner Discord webhook delivery. No guild log channels or secrets in payloads.
use anyhow::{Context as _, Result};

use crate::state::Conductor;
use crate::store::now_ms;

pub async fn deliver(conductor: &Conductor, kind: &str, payload_json: &str) -> Result<()> {
    let settings = conductor.settings().await?;
    anyhow::ensure!(settings.notify_enabled, "owner notifications are disabled");
    anyhow::ensure!(
        !settings.notify_webhook_url.is_empty(),
        "owner notification webhook is not configured"
    );
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let category =
        payload
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or(if kind == "notify_test" {
                "health"
            } else {
                "jobs"
            });
    anyhow::ensure!(
        settings.notify_categories.iter().any(|c| c == category),
        "notification category is not enabled"
    );
    if settings.notify_verbosity == "errors"
        && kind != "notify_test"
        && payload.get("severity").and_then(|v| v.as_str()) != Some("error")
        && category != "updates"
    {
        return Ok(());
    }
    let since = now_ms()
        - i64::try_from(settings.notify_dedup_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    if kind != "notify_test"
        && conductor
            .store()?
            .last_notify_dedup(category, since)
            .await?
    {
        return Ok(());
    }
    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("jModGuard conductor notification");
    anyhow::ensure!(
        !message.contains("token") && !message.contains("secret"),
        "notification payload rejected"
    );
    let body = notification_body(message, settings.notify_mentions);
    let response = conductor
        .http
        .post(&settings.notify_webhook_url)
        .json(&body)
        .send()
        .await
        .context("owner notification delivery failed")?;
    let status = response.status();
    let store = conductor.store()?;
    if status.is_success() {
        store
            .insert_notify_receipt(category, "sent", Some(&status.as_u16().to_string()))
            .await?;
        Ok(())
    } else {
        let detail = format!("http {}", status.as_u16());
        store
            .insert_notify_receipt(category, "failed", Some(&detail))
            .await?;
        anyhow::bail!("owner notification delivery failed: {detail}");
    }
}

fn notification_body(message: &str, allow_mentions: bool) -> serde_json::Value {
    let parse: &[&str] = if allow_mentions {
        &["users", "roles", "everyone"]
    } else {
        &[]
    };
    serde_json::json!({
        "content": message,
        "allowed_mentions": { "parse": parse },
    })
}

#[cfg(test)]
mod tests {
    use super::notification_body;

    #[test]
    fn notification_mentions_follow_the_applied_setting() {
        assert_eq!(
            notification_body("hello <@42>", false)["allowed_mentions"]["parse"],
            serde_json::json!([])
        );
        assert_eq!(
            notification_body("hello <@42>", true)["allowed_mentions"]["parse"],
            serde_json::json!(["users", "roles", "everyone"])
        );
    }
}
