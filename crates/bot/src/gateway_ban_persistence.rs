//! Retain received ban lifecycle facts until durable, then finish from the inbox.
use crate::{db, state::BotState};
use anyhow::Result;
use db::guild_data::{CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource};
use std::{sync::Arc, time::Duration};

fn gateway_case(id: &str, user: i64, at: i64, action: CaseAction) -> CaseData {
    CaseData {
        ban_attempt_tag: None,
        source: CaseSource::GatewayObservation {
            observation_id: CaseId::try_from(id.to_owned()).expect("generated delivery id"),
        },
        reason: Some(
            if action == CaseAction::Ban {
                "banned"
            } else {
                "unbanned"
            }
            .to_owned(),
        ),
        action,
        outcome: CaseOutcome::Succeeded,
        subject_id: user,
        actor_id: None,
        occurred_at_ms: at,
        evidence: CaseEvidence::Unavailable,
    }
}

/// Initial capture may itself fail during an outage. Retain the payload while
/// retrying, but never adopt a membership that began after this observation.
async fn capture(
    app: &BotState,
    guild: u64,
    observed: chrono::DateTime<chrono::Utc>,
    joined: Option<i64>,
) -> Result<db::GuildEnrollment> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match db::guild_enrollment(&app.db, guild as i64).await {
            Ok(Some(enrollment)) => {
                anyhow::ensure!(
                    enrollment.joined_at_ns <= observed.timestamp_nanos_opt().unwrap_or(i64::MAX),
                    db::EnrollmentChanged
                );
                if let Some(joined) = joined {
                    anyhow::ensure!(
                        crate::enrollment_repair::membership_joined_at_matches(
                            joined,
                            enrollment.joined_at_ns
                        ) || crate::enrollment_repair::membership_joined_at_matches(
                            enrollment.joined_at_ns,
                            joined
                        ),
                        db::EnrollmentChanged
                    );
                }
                return Ok(enrollment);
            }
            Ok(None) => tracing::warn!(guild, "gateway ban delivery waiting for its membership"),
            Err(error) => {
                tracing::warn!(guild, "gateway ban membership lookup will retry: {error:#}")
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn save(db: &db::Db, delivery: db::GatewayBanDelivery) -> Result<db::GatewayBanDelivery> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match db::save_gateway_ban_delivery(db, delivery.clone()).await {
            Ok(saved) => return Ok(saved),
            Err(error) if error.downcast_ref::<db::EnrollmentChanged>().is_some() => {
                return Err(error);
            }
            Err(error) => {
                tracing::warn!(delivery_id=%delivery.id,"gateway ban delivery retained; save will retry: {error:#}")
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Spawn before the handler's next await. Callback cancellation detaches this
/// owner; it cannot discard a payload whose save has not been confirmed.
pub(crate) fn receive_ban(
    app: Arc<BotState>,
    mut observation: crate::state::GatewayBanObservation,
    guild: u64,
    user: serenity::model::user::User,
    echo: crate::state::GatewayBanEchoCapture,
    joined: Option<i64>,
) -> tokio::task::JoinHandle<Result<(db::GuildEnrollment, Option<db::GatewayBanWrite>)>> {
    tokio::spawn(async move {
        let id = observation.delivery_id.clone();
        let user_id = user.id.get();
        let observed = observation.observed;
        let enrollment = capture(&app, guild, observed, joined).await?;
        observation.enrollment = Some(enrollment.clone());
        let member_was_banned = crate::handlers::gateway_ban_directory_precheck_then_observe(
            &app,
            guild,
            &user,
            observed.timestamp_millis(),
        )
        .await;
        let audit_after_ms = observation.audit_after_ms();
        let pending_audits = app.pending_gateway_manual_ban_audit_entries(guild, user.id.get());
        let echo = echo.resolve();
        let delivery = db::GatewayBanDelivery {
            id: id.as_str().to_owned(),
            enrollment: enrollment.clone(),
            user_id: user.id.get() as i64,
            observed_at: observed.to_rfc3339(),
            payload: db::GatewayBanPayload::Ban {
                user_name: user.name.clone(),
                user_display_name: user.global_name.clone(),
                owned_by_bot: echo.owned_by_bot,
                operation_id: echo.operation_id,
                member_was_banned,
                pending_audits,
                overlapping_deliveries: observation.overlapping_deliveries(&enrollment),
                audit_after_ms,
            },
            case: crate::handlers::record_unresolved_gateway_observation(echo.suppress_case).then(
                || {
                    gateway_case(
                        id.as_str(),
                        user.id.get() as i64,
                        observed.timestamp_millis(),
                        CaseAction::Ban,
                    )
                },
            ),
        };
        drop(user);
        let delivery = save(&app.db, delivery).await?;
        let write_app = Arc::clone(&app);
        let delivery_id = delivery.id.clone();
        let owned_by_bot = matches!(
            delivery.payload,
            db::GatewayBanPayload::Ban {
                owned_by_bot: true,
                ..
            }
        );
        // Only the retry identity remains in memory after confirmed storage.
        drop(delivery);
        let result = crate::handlers::persist_gateway_ban_observation(
            &app,
            Some(enrollment.clone()),
            user_id,
            observation,
            owned_by_bot,
            async move {
                let mut backoff = Duration::from_secs(1);
                loop {
                    match db::apply_gateway_ban_delivery(&write_app.db, delivery_id.clone()).await {
                        Ok(Some(result)) => return Ok(result),
                        Ok(None) => anyhow::bail!("ban delivery has no result"),
                        Err(error) if error.downcast_ref::<db::EnrollmentChanged>().is_some() => {
                            return Err(error);
                        }
                        Err(error) => {
                            tracing::warn!("saved gateway ban projection will retry: {error:#}")
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            },
        )
        .await?;
        let result = Some(result);
        Ok((enrollment, result))
    })
}

pub(crate) fn receive_unban(
    app: Arc<BotState>,
    guild: u64,
    user: u64,
    observed: chrono::DateTime<chrono::Utc>,
    joined: Option<i64>,
    suppressed: bool,
    enrollment: Option<db::GuildEnrollment>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let id = CaseId::new();
        let enrollment = match enrollment {
            Some(e) => e,
            None => capture(&app, guild, observed, joined).await?,
        };
        let delivery = db::GatewayBanDelivery {
            id: id.as_str().to_owned(),
            enrollment,
            user_id: user as i64,
            observed_at: observed.to_rfc3339(),
            payload: db::GatewayBanPayload::Unban { suppressed },
            case: crate::handlers::record_unresolved_gateway_observation(suppressed).then(|| {
                gateway_case(
                    id.as_str(),
                    user as i64,
                    observed.timestamp_millis(),
                    CaseAction::Unban,
                )
            }),
        };
        let delivery = save(&app.db, delivery).await?;
        apply(&app, delivery, false, false).await?;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn receive_audit(
    app: Arc<BotState>,
    guild: u64,
    user: u64,
    entry_id: i64,
    attribution: db::GatewayBanAuditAttribution,
    case_reason: Option<String>,
    joined: Option<i64>,
    enrollment: Option<db::GuildEnrollment>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let observed =
            chrono::DateTime::from_timestamp_millis((entry_id >> 22) + 1_420_070_400_000)
                .ok_or_else(|| anyhow::anyhow!("invalid audit timestamp"))?;
        let enrollment = match enrollment {
            Some(e) => e,
            None => capture(&app, guild, observed, joined).await?,
        };
        let case = CaseData {
            ban_attempt_tag: None,
            source: CaseSource::DiscordAudit { entry_id },
            action: CaseAction::Ban,
            outcome: CaseOutcome::Succeeded,
            subject_id: user as i64,
            actor_id: Some(attribution.moderator_id),
            occurred_at_ms: observed.timestamp_millis(),
            reason: case_reason,
            evidence: CaseEvidence::Unavailable,
        };
        let delivery = db::GatewayBanDelivery {
            id: format!("audit:{guild}:{entry_id}"),
            enrollment,
            user_id: user as i64,
            observed_at: observed.to_rfc3339(),
            payload: db::GatewayBanPayload::Audit {
                entry_id,
                attribution,
            },
            case: Some(case),
        };
        let delivery = save(&app.db, delivery).await?;
        apply(&app, delivery, false, false).await?;
        Ok(())
    })
}

async fn apply(
    app: &Arc<BotState>,
    delivery: db::GatewayBanDelivery,
    applied: bool,
    case_saved: bool,
) -> Result<Option<db::GatewayBanWrite>> {
    let result = if matches!(delivery.payload, db::GatewayBanPayload::Audit { .. }) {
        if !applied
            && let db::GatewayBanPayload::Audit {
                entry_id,
                attribution,
            } = &delivery.payload
            && crate::handlers::reconcile_received_gateway_ban_audit(
                app,
                Some(delivery.enrollment.clone()),
                delivery.enrollment.guild_id as u64,
                delivery.user_id as u64,
                *entry_id,
                attribution.clone(),
            )
            .await
        {
            db::acknowledge_gateway_ban_audit(&app.db, delivery.clone()).await?;
        }
        None
    } else {
        db::apply_gateway_ban_delivery(&app.db, delivery.id.clone()).await?
    };
    if !case_saved {
        if let Some(case) = delivery.case.clone() {
            // Stable source and immutable original time make retries idempotent,
            // even when the core write succeeded but the guild store did not.
            let recorded = app
                .tenant
                .record_selected_case(delivery.enrollment.clone(), case)
                .await?;
            if recorded.is_none() {
                return Ok(result);
            }
        }
        db::acknowledge_gateway_ban_case(&app.db, delivery).await?;
    }
    Ok(result)
}

/// Preserve a confirmed direct unban even if its ordinary receipt write fails.
/// This only retries database persistence; it never calls Discord.
pub(crate) fn retain_confirmed_unban(db: db::Db, enrollment: db::GuildEnrollment, user_id: i64) {
    let observed_at = chrono::Utc::now().to_rfc3339();
    let delivery = db::GatewayBanDelivery {
        id: CaseId::new().as_str().to_owned(),
        enrollment,
        user_id,
        observed_at,
        payload: db::GatewayBanPayload::Unban { suppressed: true },
        case: None,
    };
    tokio::spawn(async move {
        let result = async {
            let delivery = save(&db, delivery).await?;
            let mut backoff = Duration::from_secs(1);
            loop {
                match db::apply_gateway_ban_delivery(&db, delivery.id.clone()).await {
                    Ok(_) => return Ok::<_, anyhow::Error>(()),
                    Err(error) if error.downcast_ref::<db::EnrollmentChanged>().is_some() => {
                        return Err(error);
                    }
                    Err(error) => {
                        tracing::warn!("confirmed unban projection will retry: {error:#}")
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
        .await;
        if let Err(error) = result {
            tracing::warn!("confirmed unban belongs to retired membership: {error:#}");
        }
    });
}

/// Startup and periodic recovery use the same durable work, without retaining
/// old payloads in the callback or a time/capacity-limited audit cache.
pub async fn drain(app: &Arc<BotState>, cursor: &mut i64) -> Result<()> {
    let pending = db::pending_gateway_ban_deliveries(&app.db, *cursor).await?;
    if pending.is_empty() {
        *cursor = 0;
        return Ok(());
    }
    for (sequence, delivery, applied, case_saved) in pending {
        *cursor = sequence;
        if let Err(error) = apply(app, delivery, applied, case_saved).await {
            tracing::warn!("saved gateway ban delivery will retry: {error:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::{SqlExecutor, params};

    async fn setup() -> (std::path::PathBuf, Arc<BotState>, db::GuildEnrollment) {
        let dir =
            std::env::temp_dir().join(format!("gateway_delivery_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let app = BotState::new(
            Default::default(),
            db::Db::open(&dir.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        (dir, app, enrollment)
    }
    async fn sql(app: &BotState, sql: &'static str) {
        app.db
            .with_conn(move |conn| {
                conn.execute_batch_bounded(sql, 1024 * 1024)?;
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn count(app: &BotState, table: &'static str) -> i64 {
        app.db
            .with_conn(move |conn| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), params![], |row| {
                    Ok(row.get(0)?)
                })
            })
            .await
            .unwrap()
    }
    fn ban(scope: &db::GuildEnrollment, at: i64) -> db::GatewayBanDelivery {
        db::GatewayBanDelivery {
            id: CaseId::new().as_str().to_owned(),
            enrollment: scope.clone(),
            user_id: 55,
            observed_at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
            payload: db::GatewayBanPayload::Ban {
                user_name: "received-name".into(),
                user_display_name: Some("Received display".into()),
                owned_by_bot: false,
                operation_id: None,
                member_was_banned: false,
                pending_audits: vec![],
                overlapping_deliveries: vec![],
                audit_after_ms: 0,
            },
            case: None,
        }
    }
    fn audit(scope: &db::GuildEnrollment, at: i64) -> db::GatewayBanDelivery {
        let entry_id = ((at - 1_420_070_400_000) << 22) + 1;
        db::GatewayBanDelivery {
            id: format!("audit:10:{entry_id}"),
            enrollment: scope.clone(),
            user_id: 55,
            observed_at: chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
            payload: db::GatewayBanPayload::Audit {
                entry_id,
                attribution: db::GatewayBanAuditAttribution {
                    moderator_id: 77,
                    moderator_name: Some("moderator".into()),
                    reason: format!("reason at {at}"),
                },
            },
            case: None,
        }
    }

    #[tokio::test]
    async fn callback_cancellation_and_failed_initial_lookup_and_save_retain_actual_event() {
        let (dir, app, scope) = setup().await;
        app.tenant.activate(scope.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let user:serenity::model::user::User=serde_json::from_value(serde_json::json!({"id":"55","username":"received-name","global_name":"Received display","discriminator":"0","avatar":null})).unwrap();
        sql(
            &app,
            "ALTER TABLE guild_enrollments RENAME TO temporarily_unavailable_enrollments",
        )
        .await;
        let observation = app.begin_gateway_ban_observation(10, 55).await;
        let observed = observation.observed;
        let task = receive_ban(
            Arc::clone(&app),
            observation,
            10,
            user,
            app.consume_gateway_ban_echo(10, 55),
            Some(100),
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(
            !task.is_finished(),
            "lookup failure must retain the delivery"
        );
        sql(&app,"ALTER TABLE temporarily_unavailable_enrollments RENAME TO guild_enrollments; CREATE TRIGGER fail_delivery_save BEFORE INSERT ON gateway_ban_deliveries BEGIN SELECT RAISE(FAIL,'storage unavailable'); END").await;
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(count(&app, "gateway_ban_deliveries").await, 0);
        assert!(
            !task.is_finished(),
            "save failure must retain the actual payload"
        );
        // Dropping the waiting callback does not cancel the persistence owner.
        drop(task);
        sql(&app, "DROP TRIGGER fail_delivery_save").await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while count(&app, "events").await != 1 {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let rows =
            db::query_enrolled_events(&app.db, scope.clone(), Some("member_ban"), Some(55), 10)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&rows[0].created_at)
                .unwrap()
                .timestamp_millis(),
            observed.timestamp_millis()
        );
        let received = audit(&scope, observed.timestamp_millis());
        let db::GatewayBanPayload::Audit {
            entry_id,
            attribution,
        } = received.payload
        else {
            unreachable!()
        };
        receive_audit(
            Arc::clone(&app),
            10,
            55,
            entry_id,
            attribution.clone(),
            Some(attribution.reason.clone()),
            Some(100),
            Some(scope.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        receive_audit(
            Arc::clone(&app),
            10,
            55,
            entry_id,
            attribution.clone(),
            Some(attribution.reason.clone()),
            Some(100),
            Some(scope.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        let mut cursor = 0;
        for _ in 0..3 {
            drain(&app, &mut cursor).await.unwrap();
        }
        assert_eq!(count(&app, "events").await, 1);
        let (name,display,moderator,reason)=app.db.with_conn(|conn|conn.query_row("SELECT user_name,user_display_name,moderator_id,reason FROM bans WHERE user_id=55",params![],|row|Ok((row.get::<String>(0)?,row.get::<String>(1)?,row.get::<i64>(2)?,row.get::<String>(3)?)))).await.unwrap();
        assert_eq!(
            (name.as_str(), display.as_str(), moderator),
            ("received-name", "Received display", 77)
        );
        assert_eq!(reason, format!("reason at {}", observed.timestamp_millis()));
        let (store, tenant) = app.tenant.live(scope).await.unwrap();
        assert_eq!(
            store
                .case_count(tenant, crate::tenant::case_limits())
                .await
                .unwrap(),
            2,
            "one gateway source plus one audit source; duplicate delivery adds neither"
        );
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saved_payload_recovers_after_restart_and_partial_case_write_without_duplicates() {
        let (dir, app, scope) = setup().await;
        let at = chrono::Utc::now().timestamp_millis() - 20_000;
        let mut delivery = ban(&scope, at);
        delivery.case = Some(gateway_case(&delivery.id, 55, at, CaseAction::Ban));
        let delivery = save(&app.db, delivery).await.unwrap();
        sql(&app,"CREATE TRIGGER fail_ban_projection BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL,'projection unavailable'); END").await;
        assert!(
            db::apply_gateway_ban_delivery(&app.db, delivery.id.clone())
                .await
                .is_err()
        );
        assert_eq!(
            count(&app, "bans").await,
            0,
            "failed projection rolls back its ban insert"
        );
        sql(&app, "DROP TRIGGER fail_ban_projection").await;
        // Commit the core row, but leave its tenant case unavailable.
        let result = db::apply_gateway_ban_delivery(&app.db, delivery.id.clone())
            .await
            .unwrap();
        assert_eq!(
            db::apply_gateway_ban_delivery(&app.db, delivery.id.clone())
                .await
                .unwrap(),
            result
        );
        drop(app);
        let app = BotState::new(
            Default::default(),
            db::Db::open(&dir.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        app.tenant.activate(scope.clone()).await.unwrap();
        let mut cursor = 0;
        drain(&app, &mut cursor).await.unwrap();
        // Recreate the gap after a tenant commit but before its core acknowledgment.
        sql(&app, "UPDATE gateway_ban_deliveries SET case_saved=0").await;
        cursor = 0;
        drain(&app, &mut cursor).await.unwrap();
        assert_eq!(count(&app, "events").await, 1);
        assert_eq!(count(&app, "bans").await, 1);
        let (store, tenant) = app.tenant.live(scope.clone()).await.unwrap();
        assert_eq!(
            store
                .case_count(tenant, crate::tenant::case_limits())
                .await
                .unwrap(),
            1
        );
        db::mark_enrolled_unbanned(&app.db, Some(scope.clone()), 55)
            .await
            .unwrap();
        assert_eq!(
            db::apply_gateway_ban_delivery(&app.db, delivery.id.clone())
                .await
                .unwrap(),
            result,
            "uncertain original write returns its original row after unban"
        );
        assert_eq!(
            app.db
                .with_conn(|conn| conn.query_row(
                    "SELECT count(*) FROM bans WHERE active=1",
                    params![],
                    |r| Ok(r.get::<i64>(0)?)
                ))
                .await
                .unwrap(),
            0
        );
        db::retire_guild_enrollment(&app.db, 10, 100).await.unwrap();
        db::enroll_guild(&app.db, 10, 200).await.unwrap();
        assert!(
            db::save_gateway_ban_delivery(&app.db, delivery)
                .await
                .is_err(),
            "never rebind an old delivery into a new membership"
        );
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn out_of_order_recovery_preserves_ban_unban_reban_and_direct_unban() {
        let (dir, app, scope) = setup().await;
        let at = chrono::Utc::now().timestamp_millis() - 30_000;
        let old = db::save_gateway_ban_delivery(&app.db, ban(&scope, at))
            .await
            .unwrap();
        let mut unban = ban(&scope, at + 10_000);
        unban.payload = db::GatewayBanPayload::Unban { suppressed: true };
        let unban = db::save_gateway_ban_delivery(&app.db, unban).await.unwrap();
        let new = db::save_gateway_ban_delivery(&app.db, ban(&scope, at + 20_000))
            .await
            .unwrap();
        for item in [&new, &unban, &old, &old, &new] {
            db::apply_gateway_ban_delivery(&app.db, item.id.clone())
                .await
                .unwrap();
        }
        let rows = app
            .db
            .with_conn(|conn| {
                conn.prepare("SELECT created_at,active FROM bans ORDER BY created_at")?
                    .query_map(params![], |r| Ok((r.get::<String>(0)?, r.get::<bool>(1)?)))?
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (old.observed_at.clone(), false),
                (new.observed_at.clone(), true)
            ]
        );
        // No existing row for this earlier delivery: direct unban must still win.
        let mut delayed = ban(&scope, at + 25_000);
        delayed.user_id = 66;
        let delayed = db::save_gateway_ban_delivery(&app.db, delayed)
            .await
            .unwrap();
        db::mark_enrolled_unbanned(&app.db, Some(scope), 66)
            .await
            .unwrap();
        db::apply_gateway_ban_delivery(&app.db, delayed.id)
            .await
            .unwrap();
        assert!(
            !app.db
                .with_conn(|conn| conn.query_row(
                    "SELECT active FROM bans WHERE user_id=66",
                    params![],
                    |r| Ok(r.get::<bool>(0)?)
                ))
                .await
                .unwrap()
        );
        assert_eq!(count(&app, "events").await, 3);
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn durable_audits_outlive_cache_capacity_unban_and_restart() {
        let (dir, app, scope) = setup().await;
        let at = chrono::Utc::now().timestamp_millis() - 400_000;
        let mut ids = Vec::new();
        for index in 0..13 {
            let time = at + index * 10_000;
            let delivery = db::save_gateway_ban_delivery(&app.db, ban(&scope, time))
                .await
                .unwrap();
            db::apply_gateway_ban_delivery(&app.db, delivery.id)
                .await
                .unwrap();
            let delivery = audit(&scope, time);
            let db::GatewayBanPayload::Audit { entry_id, .. } = delivery.payload else {
                unreachable!()
            };
            ids.push(entry_id);
            db::save_gateway_ban_delivery(&app.db, delivery)
                .await
                .unwrap();
            app.queue_gateway_manual_ban_audit_entry(10, 55, entry_id);
        }
        assert_eq!(
            app.pending_gateway_manual_ban_audit_entries(10, 55).len(),
            8
        );
        db::mark_enrolled_unbanned(&app.db, Some(scope.clone()), 55)
            .await
            .unwrap();
        app.clear_gateway_manual_ban_audit_entries(10, 55);
        drop(app);
        let app = BotState::new(
            Default::default(),
            db::Db::open(&dir.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let mut cursor = 0;
        for _ in 0..4 {
            drain(&app, &mut cursor).await.unwrap();
        }
        for entry_id in ids {
            let count=app.db.with_conn(move |conn|conn.query_row("SELECT count(*) FROM events WHERE CAST(json_extract(data,'$.audit_entry_id') AS INTEGER)=?1 AND actor_id=77",params![entry_id],|r|Ok(r.get::<i64>(0)?))).await.unwrap();
            assert_eq!(
                count, 1,
                "every durable audit is attached exactly once despite empty/lossy memory"
            );
        }
        assert_eq!(count(&app, "bans").await, 13);
        assert_eq!(
            app.db
                .with_conn(|conn| conn.query_row(
                    "SELECT count(*) FROM bans WHERE active=1",
                    params![],
                    |r| Ok(r.get::<i64>(0)?)
                ))
                .await
                .unwrap(),
            0
        );
        assert!(
            db::pending_gateway_ban_deliveries(&app.db, 0)
                .await
                .unwrap()
                .is_empty()
        );
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn separate_same_user_bans_retained_together_both_receive_their_audits() {
        let (dir, app, scope) = setup().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        sql(&app, "CREATE TRIGGER fail_delivery_save BEFORE INSERT ON gateway_ban_deliveries BEGIN SELECT RAISE(FAIL,'storage unavailable'); END").await;
        let user: serenity::model::user::User = serde_json::from_value(serde_json::json!({"id":"55","username":"received-name","discriminator":"0","avatar":null})).unwrap();
        let first = app.begin_gateway_ban_observation(10, 55).await;
        let first_at = first.observed.timestamp_millis();
        let first = receive_ban(
            Arc::clone(&app),
            first,
            10,
            user.clone(),
            app.consume_gateway_ban_echo(10, 55),
            Some(100),
        );
        // Distinct match windows, but both callbacks remain retained in flight.
        tokio::time::sleep(Duration::from_millis(5100)).await;
        let second = app.begin_gateway_ban_observation(10, 55).await;
        let second_at = second.observed.timestamp_millis();
        let second = receive_ban(
            Arc::clone(&app),
            second,
            10,
            user,
            app.consume_gateway_ban_echo(10, 55),
            Some(100),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!first.is_finished() && !second.is_finished());
        sql(&app, "DROP TRIGGER fail_delivery_save").await;
        tokio::time::timeout(Duration::from_secs(10), async {
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();
        })
        .await
        .unwrap();
        for time in [first_at, second_at] {
            let received = audit(&scope, time);
            let db::GatewayBanPayload::Audit {
                entry_id,
                attribution,
            } = received.payload
            else {
                unreachable!()
            };
            receive_audit(
                Arc::clone(&app),
                10,
                55,
                entry_id,
                attribution.clone(),
                Some(attribution.reason.clone()),
                Some(100),
                Some(scope.clone()),
            )
            .await
            .unwrap()
            .unwrap();
        }
        let mut cursor = 0;
        for _ in 0..3 {
            drain(&app, &mut cursor).await.unwrap();
        }
        let rows = app
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM bans WHERE user_id=55 AND moderator_id=77",
                    params![],
                    |row| Ok(row.get::<i64>(0)?),
                )
            })
            .await
            .unwrap();
        assert_eq!(
            rows, 2,
            "retained overlapping callbacks must not permanently fence out their own distinct audits"
        );
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn restart_remembers_a_competing_callback_that_has_not_reached_storage() {
        let (dir, app, scope) = setup().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        let first = app.begin_gateway_ban_observation(10, 55).await;
        let first_at = first.observed.timestamp_millis();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = app.begin_gateway_ban_observation(10, 55).await;
        let second_at = second.observed.timestamp_millis();
        let user: serenity::model::user::User = serde_json::from_value(serde_json::json!({"id":"55","username":"received-name","discriminator":"0","avatar":null})).unwrap();
        receive_ban(
            Arc::clone(&app),
            first,
            10,
            user.clone(),
            app.consume_gateway_ban_echo(10, 55),
            Some(100),
        )
        .await
        .unwrap()
        .unwrap();
        let received = audit(&scope, first_at);
        db::save_gateway_ban_delivery(&app.db, received.clone())
            .await
            .unwrap();
        // A separate process state has no live registration for the unsaved B.
        // The old guard is still live; only persisted peer identity is visible here.
        let restarted = BotState::new(
            Default::default(),
            db::Db::open(&dir.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let db::GatewayBanPayload::Audit {
            entry_id,
            attribution,
        } = received.payload
        else {
            unreachable!()
        };
        let enrollment = scope.clone();
        assert!(
            !restarted
                .db
                .with_conn(
                    move |conn| db::reconcile_gateway_manual_ban_audit_entry_conn(
                        conn,
                        &enrollment,
                        10,
                        55,
                        entry_id,
                        Some(&attribution)
                    )
                )
                .await
                .unwrap(),
            "a saved audit cannot attach to A while its known competing B is absent after restart"
        );
        // While B is still absent, its dependency only fences its own match
        // window; an unrelated later ban can still reconcile.
        let later = ban(&scope, second_at + 20_000);
        db::save_gateway_ban_delivery(&restarted.db, later.clone())
            .await
            .unwrap();
        db::apply_gateway_ban_delivery(&restarted.db, later.id)
            .await
            .unwrap();
        let later = audit(&scope, second_at + 20_000);
        let db::GatewayBanPayload::Audit {
            entry_id,
            attribution,
        } = later.payload
        else {
            unreachable!()
        };
        let enrollment = scope.clone();
        assert!(
            restarted
                .db
                .with_conn(
                    move |conn| db::reconcile_gateway_manual_ban_audit_entry_conn(
                        conn,
                        &enrollment,
                        10,
                        55,
                        entry_id,
                        Some(&attribution)
                    )
                )
                .await
                .unwrap()
        );
        // Completing B discharges the stored dependency. Its later action time
        // uniquely identifies B; A cannot become that audit's substitute.
        receive_ban(
            Arc::clone(&app),
            second,
            10,
            user,
            app.consume_gateway_ban_echo(10, 55),
            Some(100),
        )
        .await
        .unwrap()
        .unwrap();
        let later = audit(&scope, second_at);
        let db::GatewayBanPayload::Audit {
            entry_id,
            attribution,
        } = later.payload
        else {
            unreachable!()
        };
        let enrollment = scope.clone();
        assert!(
            restarted
                .db
                .with_conn(
                    move |conn| db::reconcile_gateway_manual_ban_audit_entry_conn(
                        conn,
                        &enrollment,
                        10,
                        55,
                        entry_id,
                        Some(&attribution)
                    )
                )
                .await
                .unwrap()
        );
        drop(restarted);
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn replay_preserves_an_existing_audit_case_with_no_reason() {
        let (dir, app, scope) = setup().await;
        app.tenant.activate(scope.clone()).await.unwrap();
        let at = chrono::Utc::now().timestamp_millis() - 20_000;
        let entry_id = ((at - 1_420_070_400_000) << 22) + 1;
        let existing = CaseData {
            ban_attempt_tag: None,
            source: CaseSource::DiscordAudit { entry_id },
            action: CaseAction::Ban,
            outcome: CaseOutcome::Succeeded,
            subject_id: 55,
            actor_id: Some(77),
            occurred_at_ms: at,
            reason: None,
            evidence: CaseEvidence::Unavailable,
        };
        app.tenant
            .record_selected_case(scope.clone(), existing)
            .await
            .unwrap()
            .unwrap();
        receive_audit(
            Arc::clone(&app),
            10,
            55,
            entry_id,
            db::GatewayBanAuditAttribution {
                moderator_id: 77,
                moderator_name: None,
                reason: "No reason provided".into(),
            },
            None,
            Some(100),
            Some(scope.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        let saved = app
            .db
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT case_saved FROM gateway_ban_deliveries WHERE id=?1",
                    params![format!("audit:10:{entry_id}")],
                    |row| Ok(row.get::<bool>(0)?),
                )
            })
            .await
            .unwrap();
        assert!(
            saved,
            "the original None reason must remain compatible with its immutable case source"
        );
        let (store, tenant) = app.tenant.live(scope).await.unwrap();
        assert_eq!(
            store
                .case_count(tenant, crate::tenant::case_limits())
                .await
                .unwrap(),
            1
        );
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
