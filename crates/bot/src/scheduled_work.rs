//! Guild-store scheduled work poller (softban unban arm).
use crate::db::guild_data::transient_backoff_ms;
use crate::softban::{apply_unban_row_result, dispatch_softban_unban, parse_softban_unban_payload};
use crate::{db, state::BotState};
use serenity::model::id::{GuildId, UserId};
use std::sync::Arc;
use std::time::Duration;

const CLAIM_KINDS: &[&str] = &["softban_unban"];

pub async fn tick(app: &Arc<BotState>, http: &serenity::http::Http) {
    if !app.activation.may_record() {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let stores = app.tenant.open_configured_stores().await;
    for store in stores {
        if !app.activation.may_record() {
            return;
        }
        let _ = store.release_expired_scheduled_claims(now).await;
        let claimed = match store.claim_due_scheduled_work(CLAIM_KINDS, now, 4).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!("scheduled work claim failed: {error:#}");
                continue;
            }
        };
        for row in claimed {
            if !app.activation.may_record() {
                return;
            }
            if db::guild_stores::guild_is_write_fenced(&app.db, row.guild_id)
                .await
                .unwrap_or(true)
            {
                continue;
            }
            let enrollment = match db::guild_membership_observation(&app.db, row.guild_id).await {
                Ok(Some((enrollment, true))) => enrollment,
                _ => {
                    let _ = store.finish_scheduled_work(&row, "failed", None, now).await;
                    continue;
                }
            };
            if enrollment.id != row.enrollment_id
                || !store
                    .scheduled_work_membership_active(
                        row.guild_id,
                        &row.enrollment_id,
                        enrollment.joined_at_ns,
                    )
                    .await
                    .unwrap_or(false)
            {
                let _ = store.finish_scheduled_work(&row, "failed", None, now).await;
                continue;
            }
            let payload = match parse_softban_unban_payload(&row.payload) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!("scheduled softban payload invalid: {error:#}");
                    let _ = store.finish_scheduled_work(&row, "failed", None, now).await;
                    continue;
                }
            };
            let guild_id = match u64::try_from(row.guild_id) {
                Ok(id) => GuildId::new(id),
                Err(_) => continue,
            };
            let target = UserId::new(row.subject_id as u64);
            let dispatch = match dispatch_softban_unban(
                app,
                http,
                guild_id,
                target,
                &payload.reason,
                Some(enrollment),
                None,
                None,
                "",
                None,
                0,
            )
            .await
            {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    tracing::warn!("scheduled softban unban failed: {error:#}");
                    if row.attempt >= 8 {
                        let _ = store.finish_scheduled_work(&row, "failed", None, now).await;
                    } else {
                        let due = now + transient_backoff_ms(row.attempt, None);
                        let _ = store
                            .finish_scheduled_work(&row, "pending", Some(due), now)
                            .await;
                    }
                    continue;
                }
            };
            if dispatch.cancelled {
                continue;
            }
            if matches!(
                dispatch.retry_class,
                Some(crate::discord::RetryClass::Transient)
            ) && row.attempt >= 8
            {
                let _ = store.finish_scheduled_work(&row, "failed", None, now).await;
                continue;
            }
            let _ = apply_unban_row_result(&store, &row, &dispatch, now).await;
        }
    }
}

pub async fn cleanup(app: &Arc<BotState>) {
    if !app.activation.may_record() {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    for store in app.tenant.open_configured_stores().await {
        let _ = store.prune_finished_scheduled_work(now).await;
    }
}

pub async fn run(app: Arc<BotState>, http: Arc<serenity::http::Http>) {
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    let mut cleanup_tick = tokio::time::interval(Duration::from_secs(60));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    cleanup_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = jmodguard::shutdown_signal() => break,
            _ = poll.tick() => tick(&app, &http).await,
            _ = cleanup_tick.tick() => cleanup(&app).await,
        }
    }
}
