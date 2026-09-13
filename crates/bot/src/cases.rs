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

pub(crate) async fn record_data(
    tenant: &crate::tenant::TenantRuntime,
    enrollment: db::GuildEnrollment,
    data: CaseData,
) {
    let guild_id = enrollment.guild_id;
    if let Err(error) = tenant.record_selected_case(enrollment, data).await {
        tracing::warn!(guild_id, "selected case not recorded: {error:#}");
    }
}

/// Capture at selected-operation admission, before its asynchronous effects.
pub async fn capture(app: &BotState, guild: u64) -> Option<db::GuildEnrollment> {
    let guild = i64::try_from(guild).ok()?;
    db::guild_enrollment(&app.db, guild).await.ok().flatten()
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
) {
    let Some(enrollment) = enrollment else {
        return;
    };
    let occurred_at_ms = chrono::Utc::now().timestamp_millis();
    if occurred_at_ms <= 0 {
        return;
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
    record_data(&app.tenant, enrollment, data).await;
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
        .await;
        let (s, scope) = app.tenant.live(new).await.unwrap();
        assert_eq!(
            s.case_count(scope.clone(), crate::tenant::case_limits())
                .await
                .unwrap(),
            0
        );
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
        .await;
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
}
