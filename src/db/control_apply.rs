//! Bot-side application of dashboard writes. The API sends [`ControlOp`]; only
//! the process that holds the writer lease calls these functions.

use super::*;
use crate::bot_actions::{Action, Limits};
use crate::tenant::TenantRuntime;
use crate::types::{CommandDef, RuleDef, SettingsPatchField};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldPatch<T> {
    Absent,
    Clear,
    Set(T),
}

impl<T> FieldPatch<T> {
    pub fn from_settings(field: SettingsPatchField<T>) -> Self {
        match field {
            None => Self::Absent,
            Some(None) => Self::Clear,
            Some(Some(value)) => Self::Set(value),
        }
    }

    fn into_settings(self) -> SettingsPatchField<T> {
        match self {
            Self::Absent => None,
            Self::Clear => Some(None),
            Self::Set(value) => Some(Some(value)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyAck {
    pub copied: Vec<String>,
    pub skipped_badwords: usize,
    pub skipped_logchannels: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlOp {
    PatchSettings {
        scope: ScopeWire,
        prefix: FieldPatch<String>,
        bot_name: FieldPatch<String>,
        reconcile: bool,
    },
    UpsertRule {
        request: ScopeWire,
        target: ScopeWire,
        id: String,
        rule: RuleDef,
    },
    DeleteRule {
        request: ScopeWire,
        target: ScopeWire,
        id: String,
    },
    AddBadword {
        request: ScopeWire,
        target: ScopeWire,
        pattern: String,
        note: Option<String>,
    },
    DeleteBadword {
        request: ScopeWire,
        target: ScopeWire,
        id: i64,
    },
    UpsertCommand {
        request: ScopeWire,
        target: ScopeWire,
        name: String,
        command: CommandDef,
    },
    DeleteCommand {
        request: ScopeWire,
        target: ScopeWire,
        name: String,
    },
    UpsertTemplate {
        request: ScopeWire,
        target: ScopeWire,
        event: String,
        template: String,
    },
    DeleteTemplate {
        request: ScopeWire,
        target: ScopeWire,
        event: String,
    },
    PutLogFilter {
        request: ScopeWire,
        target: ScopeWire,
        event: String,
        filter: LogFilter,
    },
    CopyGuildConfig {
        source: ScopeWire,
        destination: ScopeWire,
        sections: Vec<String>,
    },
    EnqueueBotAction {
        enrollment: GuildEnrollment,
        actor: i64,
        request: String,
        action: Action,
        now: i64,
        limits: Limits,
    },
    CancelBotAction {
        enrollment: GuildEnrollment,
        id: String,
    },
    StoreRetryDeadlines {
        json: String,
    },
    ClaimUpdateLease {
        component: String,
        owner: String,
        now: i64,
        ttl: i64,
    },
    RenewUpdateLease {
        component: String,
        owner: String,
        now: i64,
        ttl: i64,
    },
    ReleaseUpdateLease {
        component: String,
        owner: String,
    },
    RecordUpdateCheck {
        component: String,
        channel: String,
        candidate: Option<ComponentRelease>,
        error: Option<String>,
        now: i64,
    },
    RecordComponentRunning {
        component: String,
        release: Option<ComponentRelease>,
        observation: ComponentObservation,
        now: i64,
    },
    RecordComponentFailure {
        component: String,
        failed: ComponentRelease,
        error: String,
    },
    RecordWebuiDrift {
        error: String,
    },
    RecordWebuiInstalledVerified {
        release: ComponentRelease,
        observation: ComponentObservation,
        now: i64,
    },
    TrackWebuiAssets {
        sha: String,
        now: i64,
    },
    UntrackWebuiAssets {
        sha: String,
    },
    SetUpdatePolicy {
        component: String,
        channel: String,
        cadence: String,
        unattended_allowed: bool,
    },
    RequestDueAutomaticInstall {
        component: String,
        expected_digest: String,
        now: i64,
        unattended_allowed: bool,
    },
    RequestComponentInstall {
        component: String,
        digest: String,
        now: i64,
    },
    RequestLauncherInstall {
        component: String,
        digest: String,
        now: i64,
    },
    SettleInterruptedWebui {
        release: ComponentRelease,
    },
    AppendCaseNote {
        guild_id: i64,
        case_id: String,
        entry_id: String,
        actor_id: Option<i64>,
        text: String,
    },
    RemoveCase {
        guild_id: i64,
        case_id: String,
    },
    SetCaseDeleteRoles {
        guild_id: i64,
        revision: String,
        roles: Vec<i64>,
    },
    CleanupEvidence {
        guild_id: i64,
    },
    AcquirePairCheck {
        now: i64,
    },
    RecordPairCheck {
        owner: String,
        channel: String,
        bot: ComponentRelease,
        api: ComponentRelease,
        now: i64,
    },
    ReleasePairCheck {
        owner: String,
    },
    RequestUpgrade {
        bot_digest: String,
        api_digest: String,
        recover: bool,
        now: i64,
    },
    AbortUpgrade {
        id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlReply {
    Saved { body: serde_json::Value },
    Unchanged { body: serde_json::Value },
    Waiting,
    Busy,
    StillWaiting,
    Failed { message: String },
}

pub async fn apply_control(
    db: &Db,
    tenant: Option<&TenantRuntime>,
    op: ControlOp,
) -> Result<ControlReply> {
    match op {
        ControlOp::PatchSettings {
            scope,
            prefix,
            bot_name,
            reconcile,
        } => {
            let before = get_config_version(db).await?;
            let settings = patch_enrolled_settings(
                db,
                scope_of(scope)?,
                prefix.into_settings(),
                bot_name.into_settings(),
                reconcile,
            )
            .await?;
            let after = get_config_version(db).await?;
            let body = serde_json::to_value(settings)?;
            if !reconcile && before == after {
                Ok(ControlReply::Unchanged { body })
            } else {
                Ok(ControlReply::Saved { body })
            }
        }
        ControlOp::UpsertRule {
            request,
            target,
            id,
            rule,
        } => {
            upsert_enrolled_rule(db, write_scope(request, target)?, &id, &rule).await?;
            saved_null()
        }
        ControlOp::DeleteRule {
            request,
            target,
            id,
        } => {
            delete_enrolled_rule(db, write_scope(request, target)?, &id).await?;
            saved_null()
        }
        ControlOp::AddBadword {
            request,
            target,
            pattern,
            note,
        } => {
            let id =
                add_enrolled_badword(db, write_scope(request, target)?, &pattern, note.as_deref())
                    .await?;
            saved_value(id)
        }
        ControlOp::DeleteBadword {
            request,
            target,
            id,
        } => {
            delete_enrolled_badword(db, write_scope(request, target)?, id).await?;
            saved_null()
        }
        ControlOp::UpsertCommand {
            request,
            target,
            name,
            command,
        } => {
            upsert_enrolled_command(db, write_scope(request, target)?, &name, &command).await?;
            saved_null()
        }
        ControlOp::DeleteCommand {
            request,
            target,
            name,
        } => {
            delete_enrolled_command(db, write_scope(request, target)?, &name).await?;
            saved_null()
        }
        ControlOp::UpsertTemplate {
            request,
            target,
            event,
            template,
        } => {
            upsert_enrolled_template(db, write_scope(request, target)?, &event, &template).await?;
            saved_null()
        }
        ControlOp::DeleteTemplate {
            request,
            target,
            event,
        } => {
            delete_enrolled_template(db, write_scope(request, target)?, &event).await?;
            saved_null()
        }
        ControlOp::PutLogFilter {
            request,
            target,
            event,
            filter,
        } => {
            let filter =
                put_enrolled_log_filter(db, write_scope(request, target)?, &event, filter).await?;
            saved_value(filter)
        }
        ControlOp::CopyGuildConfig {
            source,
            destination,
            sections,
        } => {
            let result =
                copy_enrolled_guild_config(db, scope_of(source)?, scope_of(destination)?, sections)
                    .await?;
            saved_value(CopyAck {
                copied: result.copied.into_iter().map(str::to_owned).collect(),
                skipped_badwords: result.skipped_badwords,
                skipped_logchannels: result.skipped_logchannels,
            })
        }
        ControlOp::EnqueueBotAction {
            enrollment,
            actor,
            request,
            action,
            now,
            limits,
        } => {
            let job =
                enqueue_enrolled_bot_action(db, &enrollment, actor, &request, action, now, limits)
                    .await?;
            saved_value(job)
        }
        ControlOp::CancelBotAction { enrollment, id } => {
            saved_value(cancel_enrolled_bot_action(db, &enrollment, &id).await?)
        }
        ControlOp::StoreRetryDeadlines { json } => {
            store_retry_deadlines(db, json).await?;
            saved_null()
        }
        ControlOp::ClaimUpdateLease {
            component,
            owner,
            now,
            ttl,
        } => saved_value(claim_update_lease(db, &component, &owner, now, ttl).await?),
        ControlOp::RenewUpdateLease {
            component,
            owner,
            now,
            ttl,
        } => saved_value(renew_update_lease(db, &component, &owner, now, ttl).await?),
        ControlOp::ReleaseUpdateLease { component, owner } => {
            release_update_lease(db, &component, &owner).await?;
            saved_null()
        }
        ControlOp::RecordUpdateCheck {
            component,
            channel,
            candidate,
            error,
            now,
        } => {
            saved_value(record_update_check(db, &component, &channel, candidate, error, now).await?)
        }
        ControlOp::RecordComponentRunning {
            component,
            release,
            observation,
            now,
        } => {
            record_component_running(db, &component, release, observation, now).await?;
            saved_null()
        }
        ControlOp::RecordComponentFailure {
            component,
            failed,
            error,
        } => {
            record_component_failure(db, &component, &failed, &error).await?;
            saved_null()
        }
        ControlOp::RecordWebuiDrift { error } => {
            record_webui_drift(db, &error).await?;
            saved_null()
        }
        ControlOp::RecordWebuiInstalledVerified {
            release,
            observation,
            now,
        } => {
            record_webui_installed_verified(db, &release, observation, now).await?;
            saved_null()
        }
        ControlOp::TrackWebuiAssets { sha, now } => {
            track_webui_assets(db, &sha, now).await?;
            saved_null()
        }
        ControlOp::UntrackWebuiAssets { sha } => {
            untrack_webui_assets(db, &sha).await?;
            saved_null()
        }
        ControlOp::SetUpdatePolicy {
            component,
            channel,
            cadence,
            unattended_allowed,
        } => saved_value(
            set_update_policy(db, &component, &channel, &cadence, unattended_allowed).await?,
        ),
        ControlOp::RequestDueAutomaticInstall {
            component,
            expected_digest,
            now,
            unattended_allowed,
        } => saved_value(
            request_due_automatic_install(
                db,
                &component,
                &expected_digest,
                now,
                unattended_allowed,
            )
            .await?,
        ),
        ControlOp::RequestComponentInstall {
            component,
            digest,
            now,
        } => saved_value(request_component_install(db, &component, &digest, now).await?),
        ControlOp::RequestLauncherInstall {
            component,
            digest,
            now,
        } => saved_value(request_launcher_install(db, &component, &digest, now).await?),
        ControlOp::SettleInterruptedWebui { release } => {
            settle_interrupted_webui(db, &release).await?;
            saved_null()
        }
        ControlOp::AppendCaseNote {
            guild_id,
            case_id,
            entry_id,
            actor_id,
            text,
        } => {
            let (store, scope) = live_guild(db, tenant, guild_id).await?;
            let recorded = store
                .append_case_note(
                    scope,
                    case_id.try_into()?,
                    entry_id.try_into()?,
                    actor_id,
                    text,
                    crate::tenant::history_limits(),
                )
                .await?;
            saved_value(format!("{recorded:?}"))
        }
        ControlOp::RemoveCase { guild_id, case_id } => {
            let (store, scope) = live_guild(db, tenant, guild_id).await?;
            let id: crate::db::guild_data::CaseId = case_id.try_into()?;
            let removed = store.remove_case(scope.clone(), id.clone()).await?;
            let cleanup_pending = match tenant
                .context("write failed")?
                .cleanup_owned_evidence(&store, &scope)
                .await
            {
                Ok(()) => false,
                Err(error) => {
                    tracing::warn!(
                        "case record removed; private evidence cleanup is still pending: {error:#}"
                    );
                    true
                }
            };
            saved_value(serde_json::json!({
                "removed": removed,
                "cleanup_pending": cleanup_pending,
                "discord_action_reversed": false,
            }))
        }
        ControlOp::SetCaseDeleteRoles {
            guild_id,
            revision,
            roles,
        } => {
            let (store, scope) = live_guild(db, tenant, guild_id).await?;
            saved_value(store.set_case_delete_roles(scope, &revision, roles).await?)
        }
        ControlOp::CleanupEvidence { guild_id } => {
            let (store, scope) = live_guild(db, tenant, guild_id).await?;
            tenant
                .context("write failed")?
                .cleanup_owned_evidence(&store, &scope)
                .await?;
            saved_null()
        }
        ControlOp::AcquirePairCheck { now } => saved_value(
            UpdateControl::from_application(db)
                .acquire_pair_check(now)
                .await?,
        ),
        ControlOp::RecordPairCheck {
            owner,
            channel,
            bot,
            api,
            now,
        } => {
            UpdateControl::from_application(db)
                .record_pair_check(&owner, &channel, bot, api, now)
                .await?;
            saved_null()
        }
        ControlOp::ReleasePairCheck { owner } => {
            UpdateControl::from_application(db)
                .release_pair_check(&owner)
                .await?;
            saved_null()
        }
        ControlOp::RequestUpgrade {
            bot_digest,
            api_digest,
            recover,
            now,
        } => saved_value(
            UpdateControl::from_application(db)
                .request_upgrade(&bot_digest, &api_digest, recover, now)
                .await?,
        ),
        ControlOp::AbortUpgrade { id } => {
            UpdateControl::from_application(db)
                .abort_upgrade(&id)
                .await?;
            saved_null()
        }
    }
}

pub async fn apply_control_bytes(
    db: &Db,
    tenant: Option<&TenantRuntime>,
    bytes: &[u8],
) -> Result<ControlReply> {
    let op: ControlOp = serde_json::from_slice(bytes).context("write failed")?;
    apply_control(db, tenant, op).await
}

fn scope_of(wire: ScopeWire) -> Result<ConfigurationScope> {
    if wire.global {
        return Ok(ConfigurationScope::Global);
    }
    anyhow::ensure!(wire.guild_id > 0, EnrollmentChanged);
    Ok(ConfigurationScope::Guild(GuildEnrollment {
        guild_id: wire.guild_id,
        id: wire.enrollment_id,
        joined_at_ns: wire.joined_at_ns,
    }))
}

fn write_scope(request: ScopeWire, target: ScopeWire) -> Result<ConfigurationWriteScope> {
    ConfigurationWriteScope::new(scope_of(request)?, scope_of(target)?)
}

async fn live_guild(
    db: &Db,
    tenant: Option<&TenantRuntime>,
    guild_id: i64,
) -> Result<(
    crate::db::guild_data::GuildDataStore,
    crate::db::guild_data::GuildDataScope,
)> {
    let tenant = tenant.context("write failed")?;
    let enrollment = guild_enrollment(db, guild_id)
        .await?
        .context("bot membership is not enrolled")?;
    tenant.live(enrollment).await
}

async fn store_retry_deadlines(db: &Db, json: String) -> Result<()> {
    anyhow::ensure!(
        json.len() <= 8192,
        "discord retry deadline record exceeds 8192 bytes"
    );
    db.with_conn(move |conn| {
        conn.execute_bounded(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params!["discord_oauth_retry_until", json],
            16 * 1024,
        )?;
        Ok(())
    })
    .await
}

fn saved_null() -> Result<ControlReply> {
    Ok(ControlReply::Saved {
        body: serde_json::Value::Null,
    })
}

fn saved_value(value: impl Serialize) -> Result<ControlReply> {
    Ok(ControlReply::Saved {
        body: serde_json::to_value(value)?,
    })
}
