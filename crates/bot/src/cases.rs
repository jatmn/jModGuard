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

#[allow(clippy::too_many_arguments)]
pub async fn record(
    app: &BotState,
    guild: u64,
    action: CaseAction,
    outcome: CaseOutcome,
    subject: u64,
    actor: Option<u64>,
    reason: Option<String>,
    evidence: CaseEvidence,
    source: CaseSource,
) {
    let Ok(guild_id) = i64::try_from(guild) else {
        return;
    };
    let Some(enrollment) = db::guild_enrollment(&app.db, guild_id).await.ok().flatten() else {
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
