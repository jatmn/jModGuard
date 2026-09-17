//! Selected moderation case recording with truthful Discord outcomes.
use crate::db;
use crate::db::guild_data::{
    CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource, TextEvidence,
};
use crate::state::BotState;

pub fn classify_discord(error: Option<&serenity::Error>) -> CaseOutcome {
    match error {
        None => CaseOutcome::Succeeded,
        Some(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response))) => {
            let status = response.status_code.as_u16();
            if (400..500).contains(&status) && status != 408 {
                CaseOutcome::Failed
            } else {
                CaseOutcome::Uncertain
            }
        }
        Some(_) => CaseOutcome::Uncertain,
    }
}

/// Selected Discord cases exist only after Discord was invoked.
pub fn discord_attempt_outcome(
    called: bool,
    error: Option<&serenity::Error>,
) -> Option<CaseOutcome> {
    called.then(|| classify_discord(error))
}

pub fn classify_db_commit<T>(result: &Result<T, anyhow::Error>) -> CaseOutcome {
    match result {
        Ok(_) => CaseOutcome::Succeeded,
        Err(error) if db::is_database_commit_indeterminate(error) => CaseOutcome::Uncertain,
        Err(_) => CaseOutcome::Failed,
    }
}

pub fn application_source() -> CaseSource {
    CaseSource::Application {
        operation_id: CaseId::new(),
    }
}

fn auditable_action(action: &CaseAction) -> bool {
    matches!(
        action,
        CaseAction::Ban | CaseAction::Unban | CaseAction::Kick
    )
}

fn pending_outcome(outcome: &CaseOutcome) -> bool {
    matches!(outcome, CaseOutcome::Succeeded | CaseOutcome::Uncertain)
}

async fn link_discord_audit(app: &BotState, link: crate::state::PendingAuditLink, entry_id: i64) {
    if entry_id <= 0 {
        return;
    }
    match app
        .tenant
        .link_selected_source(
            link.enrollment,
            link.case_id,
            CaseSource::Application {
                operation_id: link.operation_id,
            },
            CaseSource::DiscordAudit { entry_id },
        )
        .await
    {
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                guild_id = link.guild,
                "selected discord audit not linked: {error:#}"
            );
        }
    }
}

async fn remember_owned_application(
    app: &BotState,
    enrollment: Option<db::GuildEnrollment>,
    action: CaseAction,
    outcome: &CaseOutcome,
    subject: u64,
    case_id: CaseId,
    operation_id: CaseId,
) {
    if !auditable_action(&action) || !pending_outcome(outcome) || subject == 0 {
        return;
    }
    let Some(enrollment) = enrollment else {
        return;
    };
    let guild = enrollment.guild_id as u64;
    let link = crate::state::PendingAuditLink {
        action,
        guild,
        subject,
        case_id,
        operation_id,
        enrollment,
    };
    if let Some(entry_id) = app.remember_application_audit(link.clone()) {
        link_discord_audit(app, link, entry_id).await;
    }
}

pub(crate) async fn remember_recorded_application(
    app: &BotState,
    enrollment: db::GuildEnrollment,
    data: &CaseData,
    case_id: CaseId,
) {
    let CaseSource::Application { operation_id } = &data.source else {
        return;
    };
    let Ok(subject) = u64::try_from(data.subject_id) else {
        return;
    };
    remember_owned_application(
        app,
        Some(enrollment),
        data.action.clone(),
        &data.outcome,
        subject,
        case_id,
        operation_id.clone(),
    )
    .await;
}

/// Bot-owned Ban/Unban/Kick recording that later Discord audit events can bind.
#[allow(clippy::too_many_arguments)]
pub async fn record_application(
    app: &BotState,
    enrollment: Option<db::GuildEnrollment>,
    action: CaseAction,
    outcome: CaseOutcome,
    subject: u64,
    actor: Option<u64>,
    reason: Option<String>,
    evidence: CaseEvidence,
) -> Option<db::guild_data::CaseId> {
    let operation_id = CaseId::new();
    let pending_enrollment = enrollment.clone();
    let case_id = record(
        app,
        enrollment,
        action.clone(),
        outcome.clone(),
        subject,
        actor,
        reason,
        evidence,
        CaseSource::Application {
            operation_id: operation_id.clone(),
        },
    )
    .await?;
    remember_owned_application(
        app,
        pending_enrollment,
        action,
        &outcome,
        subject,
        case_id.clone(),
        operation_id,
    )
    .await;
    Some(case_id)
}

/// Bind a Discord audit entry only when this process executed the action.
pub async fn await_discord_audit(
    app: &BotState,
    bot_user: u64,
    executor: u64,
    action: CaseAction,
    guild: u64,
    subject: u64,
    entry_id: i64,
) {
    if executor != bot_user || !auditable_action(&action) || subject == 0 || entry_id <= 0 {
        return;
    }
    if let Some(link) = app.remember_discord_audit(action, guild, subject, entry_id) {
        link_discord_audit(app, link, entry_id).await;
    }
}

pub fn text_excerpt(channel_id: u64, message_id: u64, author_id: u64, text: &str) -> CaseEvidence {
    let text = text.trim();
    if channel_id == 0 || message_id == 0 || author_id == 0 || text.is_empty() {
        return CaseEvidence::Unavailable;
    }
    CaseEvidence::Text(vec![TextEvidence {
        channel_id: channel_id as i64,
        message_id: message_id as i64,
        author_id: author_id as i64,
        occurred_at_ms: chrono::Utc::now().timestamp_millis().max(1),
        text: text.chars().take(4000).collect(),
    }])
}

#[allow(clippy::too_many_arguments)]
pub async fn record(
    app: &BotState,
    enrollment: Option<db::GuildEnrollment>,
    action: CaseAction,
    outcome: CaseOutcome,
    subject: u64,
    actor: Option<u64>,
    reason: Option<String>,
    evidence: CaseEvidence,
    source: CaseSource,
) -> Option<db::guild_data::CaseId> {
    let enrollment = enrollment?;
    let occurred_at_ms = chrono::Utc::now().timestamp_millis();
    if occurred_at_ms <= 0 {
        return None;
    }
    let data = CaseData {
        source,
        action,
        outcome,
        subject_id: subject as i64,
        actor_id: actor
            .and_then(|id| i64::try_from(id).ok())
            .filter(|id| *id > 0),
        occurred_at_ms,
        reason,
        evidence,
    };
    record_data(&app.tenant, enrollment, data).await
}

pub(crate) async fn record_data(
    tenant: &crate::tenant::TenantRuntime,
    enrollment: db::GuildEnrollment,
    data: CaseData,
) -> Option<db::guild_data::CaseId> {
    let guild_id = enrollment.guild_id;
    match tenant.record_selected_case(enrollment, data).await {
        Ok(Some(result)) => match result {
            db::guild_data::RecordCase::Created(id) | db::guild_data::RecordCase::Existing(id) => {
                Some(id)
            }
            db::guild_data::RecordCase::Removed(_) => None,
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(guild_id, "selected case not recorded: {error:#}");
            None
        }
    }
}

/// Capture at selected-operation admission, before its asynchronous effects.
pub async fn capture(app: &BotState, guild: u64) -> Option<db::GuildEnrollment> {
    let guild = i64::try_from(guild).ok()?;
    db::guild_enrollment(&app.db, guild).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classify_db_commit_maps_indeterminate_errors() {
        let ok: Result<(), anyhow::Error> = Ok(());
        assert_eq!(classify_db_commit(&ok), CaseOutcome::Succeeded);

        let failed: Result<(), anyhow::Error> = Err(anyhow::anyhow!("write failed"));
        assert_eq!(classify_db_commit(&failed), CaseOutcome::Failed);

        let uncertain: Result<(), anyhow::Error> =
            Err(db::test_database_commit_indeterminate_error());
        assert_eq!(classify_db_commit(&uncertain), CaseOutcome::Uncertain);
    }

    #[test]
    fn discord_attempt_outcome_is_absent_until_discord_is_invoked() {
        assert_eq!(discord_attempt_outcome(false, None), None);
        assert_eq!(
            discord_attempt_outcome(true, None),
            Some(CaseOutcome::Succeeded)
        );
    }

    #[tokio::test]
    async fn delayed_record_keeps_original_membership() {
        let path =
            std::env::temp_dir().join(format!("delayed_record_{:032x}.db", rand::random::<u128>()));
        let database = db::Db::open(&path, 2).unwrap();
        let app = BotState::new(Default::default(), database).unwrap();
        let old = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(old.clone()).await.unwrap();
        // A behavior starts its Discord operation here; reply arrives after departure/rejoin.
        db::retire_guild_enrollment(&app.db, 10, 100).await.unwrap();
        app.tenant.schedule_erasure(old.clone()).await.unwrap();
        app.tenant.drive_erasure().await.unwrap();
        let new = db::enroll_guild(&app.db, 10, 101).await.unwrap().unwrap();
        app.tenant.activate(new.clone()).await.unwrap();
        assert!(
            record(
                &app,
                Some(old),
                CaseAction::Ban,
                CaseOutcome::Succeeded,
                99,
                Some(42),
                Some("old membership private reason".into()),
                CaseEvidence::Unavailable,
                application_source(),
            )
            .await
            .is_none()
        );
        let (s, scope) = app.tenant.live(new).await.unwrap();
        assert_eq!(
            s.case_count(scope.clone(), crate::tenant::case_limits())
                .await
                .unwrap(),
            0
        );
        assert!(
            record(
                &app,
                Some(scope.enrollment().clone()),
                CaseAction::Ban,
                CaseOutcome::Succeeded,
                99,
                None,
                None,
                CaseEvidence::Unavailable,
                application_source(),
            )
            .await
            .is_some()
        );
        assert_eq!(
            s.case_count(scope, crate::tenant::case_limits())
                .await
                .unwrap(),
            1
        );
        crate::tenant::remove_sidecars(&path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    async fn app_with_guild(
        guild: i64,
    ) -> (
        std::sync::Arc<BotState>,
        db::GuildEnrollment,
        std::path::PathBuf,
    ) {
        let path =
            std::env::temp_dir().join(format!("audit_sources_{:032x}.db", rand::random::<u128>()));
        let database = db::Db::open(&path, 2).unwrap();
        let app = BotState::new(Default::default(), database).unwrap();
        let enrollment = db::enroll_guild(&app.db, guild, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        (app, enrollment, path)
    }

    fn cleanup(path: &std::path::Path) {
        crate::tenant::remove_sidecars(path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    async fn linked(
        app: &BotState,
        enrollment: db::GuildEnrollment,
        source: CaseSource,
    ) -> Option<CaseId> {
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        store
            .case_for_source(scope, source)
            .await
            .unwrap()
            .map(|found| found.id)
    }

    #[tokio::test]
    async fn bot_owned_ban_links_matching_audit_without_subject_search() {
        let (app, enrollment, path) = app_with_guild(10).await;
        let case_id = record_application(
            &app,
            Some(enrollment.clone()),
            CaseAction::Ban,
            CaseOutcome::Succeeded,
            99,
            Some(42),
            Some("ban".into()),
            CaseEvidence::Unavailable,
        )
        .await
        .unwrap();
        await_discord_audit(&app, 7, 8, CaseAction::Ban, 10, 99, 123).await;
        assert_eq!(
            linked(
                &app,
                enrollment.clone(),
                CaseSource::DiscordAudit { entry_id: 123 }
            )
            .await,
            None
        );
        await_discord_audit(&app, 7, 7, CaseAction::Ban, 10, 88, 123).await;
        assert_eq!(
            linked(
                &app,
                enrollment.clone(),
                CaseSource::DiscordAudit { entry_id: 123 }
            )
            .await,
            None
        );
        await_discord_audit(&app, 7, 7, CaseAction::Ban, 10, 99, 123).await;
        assert_eq!(
            linked(
                &app,
                enrollment.clone(),
                CaseSource::DiscordAudit { entry_id: 123 }
            )
            .await
            .as_ref(),
            Some(&case_id)
        );
        assert_eq!(
            linked(
                &app,
                enrollment,
                CaseSource::Application {
                    operation_id: CaseId::new()
                }
            )
            .await,
            None
        );
        drop(app);
        cleanup(&path);
    }

    #[tokio::test]
    async fn audit_event_before_record_still_binds_the_owned_case() {
        let (app, enrollment, path) = app_with_guild(10).await;
        await_discord_audit(&app, 7, 7, CaseAction::Kick, 10, 99, 55).await;
        let unmarked = record(
            &app,
            Some(enrollment.clone()),
            CaseAction::Kick,
            CaseOutcome::Succeeded,
            99,
            None,
            Some("manual".into()),
            CaseEvidence::Unavailable,
            application_source(),
        )
        .await
        .unwrap();
        assert_eq!(
            linked(
                &app,
                enrollment.clone(),
                CaseSource::DiscordAudit { entry_id: 55 }
            )
            .await,
            None
        );
        let owned = record_application(
            &app,
            Some(enrollment.clone()),
            CaseAction::Kick,
            CaseOutcome::Succeeded,
            99,
            Some(42),
            Some("kick".into()),
            CaseEvidence::Unavailable,
        )
        .await
        .unwrap();
        assert_ne!(owned, unmarked);
        assert_eq!(
            linked(&app, enrollment, CaseSource::DiscordAudit { entry_id: 55 })
                .await
                .as_ref(),
            Some(&owned)
        );
        drop(app);
        cleanup(&path);
    }

    #[tokio::test]
    async fn failed_and_timeout_cases_do_not_consume_bot_audits() {
        let (app, enrollment, path) = app_with_guild(10).await;
        assert!(
            record_application(
                &app,
                Some(enrollment.clone()),
                CaseAction::Ban,
                CaseOutcome::Failed,
                99,
                None,
                None,
                CaseEvidence::Unavailable,
            )
            .await
            .is_some()
        );
        assert!(
            record_application(
                &app,
                Some(enrollment.clone()),
                CaseAction::Timeout,
                CaseOutcome::Succeeded,
                99,
                None,
                None,
                CaseEvidence::Unavailable,
            )
            .await
            .is_some()
        );
        await_discord_audit(&app, 7, 7, CaseAction::Ban, 10, 99, 9).await;
        assert_eq!(
            linked(&app, enrollment, CaseSource::DiscordAudit { entry_id: 9 }).await,
            None
        );
        drop(app);
        cleanup(&path);
    }
}
