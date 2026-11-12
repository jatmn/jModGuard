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
        #[serde(default)]
        clear_guild_override: bool,
    },
    DeleteRule {
        request: ScopeWire,
        target: ScopeWire,
        id: String,
    },
    AddKnownBadImage {
        scope: ScopeWire,
        hash: String,
        note: Option<String>,
    },
    RemoveKnownBadImage {
        scope: ScopeWire,
        hash: String,
    },
    AddKnownSpam {
        request: ScopeWire,
        target: ScopeWire,
        kind: String,
        value: String,
        note: Option<String>,
    },
    DeleteKnownSpam {
        request: ScopeWire,
        target: ScopeWire,
        id: i64,
    },
    AddBadword {
        request: ScopeWire,
        target: ScopeWire,
        pattern: String,
        note: Option<String>,
    },
    AddBadwords {
        request: ScopeWire,
        target: ScopeWire,
        words: String,
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
    RecordBakedComponentRunning {
        component: String,
        observation: ComponentObservation,
        now: i64,
    },
    /// Read a stored result. This never starts an apply.
    LookupResult,
    RecordComponentFailure {
        component: String,
        failed: ComponentRelease,
        error: String,
    },
    RecordComponentRollback {
        component: String,
        failed: ComponentRelease,
        previous: Option<ComponentRelease>,
        observation: ComponentObservation,
        error: String,
        now: i64,
    },
    RecordBootMismatch {
        record: BootMismatchRecord,
    },
    ClearBootMismatch {
        component: String,
    },
    ObserveLauncher {
        component: String,
        revision: u32,
    },
    ClearSupersededLauncher {
        component: String,
        release: ComponentRelease,
    },
    ClearUncontractedLauncher {
        component: String,
        release: ComponentRelease,
    },
    RecordLauncherRunning {
        component: String,
        release: ComponentRelease,
        hash: String,
        now: i64,
    },
    RecordBakedLauncher {
        component: String,
        hash: String,
        now: i64,
    },
    RecordLauncherFailure {
        component: String,
        digest: String,
        error: String,
    },
    /// `operation` names the paired upgrade that needs this launcher.
    SelectRequiredLauncher {
        release: ComponentRelease,
        operation: Option<String>,
    },
    RecordWebuiDrift {
        error: String,
    },
    SetWebuiPurgePending {
        error: Option<String>,
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
    SetUpdateAlertChannel {
        channel_id: Option<u64>,
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
        scope: ScopeWire,
        case_id: String,
        entry_id: String,
        actor_id: Option<i64>,
        text: String,
    },
    RemoveCase {
        scope: ScopeWire,
        case_id: String,
    },
    SetCaseDeleteRoles {
        scope: ScopeWire,
        revision: String,
        roles: Vec<i64>,
    },
    CleanupEvidence {
        scope: ScopeWire,
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
    /// API paired-upgrade steps. Only the bot launcher link applies these.
    JoinUpgrade {
        id: String,
        role: String,
        instance: String,
    },
    AcknowledgeUpgrade {
        id: String,
        role: String,
        instance: String,
        action: UpgradeAcknowledgement,
    },
    RecordUpgradeError {
        id: String,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlReply {
    Saved {
        body: serde_json::Value,
    },
    Unchanged {
        body: serde_json::Value,
    },
    Waiting,
    Busy,
    StillWaiting,
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedGuildActor {
    pub guild_owner_id: i64,
    pub permissions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlActorNeed {
    Process,
    Owner,
    ManageGuild(i64),
    GuildAdmin(i64),
}

const MANAGE_GUILD_BIT: u64 = 1 << 5;
const ADMINISTRATOR_BIT: u64 = 1 << 3;

pub fn control_actor_need(op: &ControlOp) -> ControlActorNeed {
    match op {
        ControlOp::PatchSettings { scope, .. }
        | ControlOp::AddKnownBadImage { scope, .. }
        | ControlOp::RemoveKnownBadImage { scope, .. } => scope_actor_need(scope),
        ControlOp::UpsertRule { target, .. }
        | ControlOp::DeleteRule { target, .. }
        | ControlOp::AddKnownSpam { target, .. }
        | ControlOp::DeleteKnownSpam { target, .. }
        | ControlOp::AddBadword { target, .. }
        | ControlOp::AddBadwords { target, .. }
        | ControlOp::DeleteBadword { target, .. }
        | ControlOp::UpsertCommand { target, .. }
        | ControlOp::DeleteCommand { target, .. }
        | ControlOp::UpsertTemplate { target, .. }
        | ControlOp::DeleteTemplate { target, .. }
        | ControlOp::PutLogFilter { target, .. } => scope_actor_need(target),
        ControlOp::CopyGuildConfig { destination, .. } => scope_actor_need(destination),
        ControlOp::AppendCaseNote { scope, .. } => ControlActorNeed::ManageGuild(scope.guild_id),
        ControlOp::RemoveCase { scope, .. }
        | ControlOp::SetCaseDeleteRoles { scope, .. }
        | ControlOp::CleanupEvidence { scope } => ControlActorNeed::GuildAdmin(scope.guild_id),
        ControlOp::EnqueueBotAction { .. }
        | ControlOp::CancelBotAction { .. }
        | ControlOp::StoreRetryDeadlines { .. }
        | ControlOp::ClaimUpdateLease { .. }
        | ControlOp::RenewUpdateLease { .. }
        | ControlOp::ReleaseUpdateLease { .. }
        | ControlOp::RecordUpdateCheck { .. }
        | ControlOp::RecordComponentRunning { .. }
        | ControlOp::RecordBakedComponentRunning { .. }
        | ControlOp::LookupResult
        | ControlOp::RecordComponentFailure { .. }
        | ControlOp::RecordComponentRollback { .. }
        | ControlOp::RecordBootMismatch { .. }
        | ControlOp::ClearBootMismatch { .. }
        | ControlOp::ObserveLauncher { .. }
        | ControlOp::ClearSupersededLauncher { .. }
        | ControlOp::ClearUncontractedLauncher { .. }
        | ControlOp::RecordLauncherRunning { .. }
        | ControlOp::RecordBakedLauncher { .. }
        | ControlOp::RecordLauncherFailure { .. }
        | ControlOp::SelectRequiredLauncher { .. }
        | ControlOp::RecordWebuiDrift { .. }
        | ControlOp::SetWebuiPurgePending { .. }
        | ControlOp::RecordWebuiInstalledVerified { .. }
        | ControlOp::TrackWebuiAssets { .. }
        | ControlOp::UntrackWebuiAssets { .. }
        | ControlOp::SetUpdatePolicy { .. }
        | ControlOp::SetUpdateAlertChannel { .. }
        | ControlOp::RequestDueAutomaticInstall { .. }
        | ControlOp::RequestComponentInstall { .. }
        | ControlOp::RequestLauncherInstall { .. }
        | ControlOp::SettleInterruptedWebui { .. }
        | ControlOp::AcquirePairCheck { .. }
        | ControlOp::RecordPairCheck { .. }
        | ControlOp::ReleasePairCheck { .. }
        | ControlOp::RequestUpgrade { .. }
        | ControlOp::AbortUpgrade { .. }
        | ControlOp::JoinUpgrade { .. }
        | ControlOp::AcknowledgeUpgrade { .. }
        | ControlOp::RecordUpgradeError { .. } => ControlActorNeed::Process,
    }
}

/// Every scope a request must authorize. Copy keeps source and destination.
/// Other request-plus-target configuration ops keep the request guild and the
/// target, without repeating the same need.
pub fn control_actor_needs(op: &ControlOp) -> Vec<ControlActorNeed> {
    match op {
        ControlOp::CopyGuildConfig {
            source,
            destination,
            ..
        } => pair_needs(scope_actor_need(source), scope_actor_need(destination)),
        ControlOp::UpsertRule {
            request, target, ..
        }
        | ControlOp::DeleteRule {
            request, target, ..
        }
        | ControlOp::AddKnownSpam {
            request, target, ..
        }
        | ControlOp::DeleteKnownSpam {
            request, target, ..
        }
        | ControlOp::AddBadword {
            request, target, ..
        }
        | ControlOp::AddBadwords {
            request, target, ..
        }
        | ControlOp::DeleteBadword {
            request, target, ..
        }
        | ControlOp::UpsertCommand {
            request, target, ..
        }
        | ControlOp::DeleteCommand {
            request, target, ..
        }
        | ControlOp::UpsertTemplate {
            request, target, ..
        }
        | ControlOp::DeleteTemplate {
            request, target, ..
        }
        | ControlOp::PutLogFilter {
            request, target, ..
        } => pair_needs(scope_actor_need(request), scope_actor_need(target)),
        other => vec![control_actor_need(other)],
    }
}

fn pair_needs(first: ControlActorNeed, second: ControlActorNeed) -> Vec<ControlActorNeed> {
    if first == second {
        vec![first]
    } else {
        vec![first, second]
    }
}

fn scope_actor_need(scope: &ScopeWire) -> ControlActorNeed {
    if scope.global {
        ControlActorNeed::Owner
    } else {
        ControlActorNeed::ManageGuild(scope.guild_id)
    }
}

pub fn decide_control_actor(
    need: ControlActorNeed,
    actor_user_id: i64,
    owner_id: u64,
    cached: Option<CachedGuildActor>,
) -> Result<(), &'static str> {
    match need {
        ControlActorNeed::Process => Ok(()),
        ControlActorNeed::Owner => {
            if owner_id != 0 && actor_user_id > 0 && actor_user_id as u64 == owner_id {
                Ok(())
            } else {
                Err("owner is required")
            }
        }
        ControlActorNeed::ManageGuild(_) => guild_actor(actor_user_id, cached, true),
        ControlActorNeed::GuildAdmin(_) => guild_actor(actor_user_id, cached, false),
    }
}

fn guild_actor(
    actor_user_id: i64,
    cached: Option<CachedGuildActor>,
    manage_guild: bool,
) -> Result<(), &'static str> {
    if actor_user_id <= 0 {
        return Err("actor is not allowed");
    }
    let Some(cached) = cached else {
        return Err("actor is not allowed");
    };
    if cached.guild_owner_id == actor_user_id {
        return Ok(());
    }
    if cached.permissions & ADMINISTRATOR_BIT != 0 {
        return Ok(());
    }
    if manage_guild && cached.permissions & MANAGE_GUILD_BIT != 0 {
        return Ok(());
    }
    Err("actor is not allowed")
}

struct LeaseFence<'a> {
    db: &'a Db,
    previous: Option<String>,
}

impl Drop for LeaseFence<'_> {
    fn drop(&mut self) {
        self.db.restore_lease_owner(self.previous.take());
    }
}

pub async fn apply_control(
    db: &Db,
    tenant: Option<&TenantRuntime>,
    op: ControlOp,
    instance: Option<&str>,
) -> Result<ControlReply> {
    let _fence = instance.map(|owner| {
        let previous = db.leased_owner();
        db.arm_lease_owner(owner);
        LeaseFence { db, previous }
    });
    match apply_control_inner(db, tenant, op).await {
        Err(error) if lease_rejected(&error) => Ok(ControlReply::Waiting),
        other => other,
    }
}

fn lease_rejected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<crate::db_compat::ActiveBotLeaseRejected>()
            .is_some()
    })
}

async fn apply_control_inner(
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
            clear_guild_override,
        } => {
            let saved = if clear_guild_override {
                anyhow::ensure!(
                    target.global,
                    "override clearing requires a global rule target"
                );
                upsert_global_rule_and_clear_enrolled_override(db, scope_of(request)?, &id, &rule)
                    .await?
            } else {
                upsert_enrolled_rule(db, write_scope(request, target)?, &id, &rule).await?
            };
            Ok(ControlReply::Saved {
                body: serde_json::to_value(saved)?,
            })
        }
        ControlOp::DeleteRule {
            request,
            target,
            id,
        } => {
            delete_enrolled_rule(db, write_scope(request, target)?, &id).await?;
            saved_null()
        }
        ControlOp::AddKnownBadImage { scope, hash, note } => {
            let scope = scope_of(scope)?;
            let changed = add_enrolled_known_bad_image(
                db,
                ConfigurationWriteScope::new(scope.clone(), scope)?,
                &hash,
                note,
            )
            .await?;
            let body = serde_json::json!({"changed": changed});
            Ok(if changed {
                ControlReply::Saved { body }
            } else {
                ControlReply::Unchanged { body }
            })
        }
        ControlOp::RemoveKnownBadImage { scope, hash } => {
            let scope = scope_of(scope)?;
            let changed = remove_enrolled_known_bad_image(
                db,
                ConfigurationWriteScope::new(scope.clone(), scope)?,
                &hash,
            )
            .await?;
            let body = serde_json::json!({"changed": changed});
            Ok(if changed {
                ControlReply::Saved { body }
            } else {
                ControlReply::Unchanged { body }
            })
        }
        ControlOp::AddKnownSpam {
            request,
            target,
            kind,
            value,
            note,
        } => {
            let id = add_enrolled_known_spam(
                db,
                write_scope(request, target)?,
                &kind,
                &value,
                note.as_deref(),
            )
            .await?;
            saved_value(id)
        }
        ControlOp::DeleteKnownSpam {
            request,
            target,
            id,
        } => {
            delete_enrolled_known_spam(db, write_scope(request, target)?, id).await?;
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
        ControlOp::AddBadwords {
            request,
            target,
            words,
            note,
        } => {
            let rows =
                add_enrolled_badwords(db, write_scope(request, target)?, &words, note.as_deref())
                    .await?;
            // Bound the acknowledgement independently of patterns and notes,
            // including notes retained by reused rows. The dashboard reloads them.
            saved_value(rows.into_iter().map(|row| row.id).collect::<Vec<_>>())
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
        ControlOp::RecordBakedComponentRunning {
            component,
            observation,
            now,
        } => {
            record_baked_component_running(db, &component, observation, now).await?;
            saved_null()
        }
        ControlOp::LookupResult => Ok(ControlReply::Failed {
            code: None,
            message: crate::control::OUTCOME_UNKNOWN.into(),
        }),
        ControlOp::RecordComponentFailure {
            component,
            failed,
            error,
        } => {
            record_component_failure(db, &component, &failed, &error).await?;
            saved_null()
        }
        ControlOp::RecordComponentRollback {
            component,
            failed,
            previous,
            observation,
            error,
            now,
        } => {
            record_component_rollback(db, &component, &failed, previous, observation, &error, now)
                .await?;
            saved_null()
        }
        ControlOp::RecordBootMismatch { record } => {
            record_boot_mismatch(db, record).await?;
            saved_null()
        }
        ControlOp::ClearBootMismatch { component } => {
            clear_boot_mismatch(db, &component).await?;
            saved_null()
        }
        ControlOp::ObserveLauncher {
            component,
            revision,
        } => {
            record_launcher_observation(db, &component, revision).await?;
            saved_null()
        }
        ControlOp::ClearSupersededLauncher { component, release } => {
            record_clear_superseded_launcher(db, &component, &release).await?;
            saved_null()
        }
        ControlOp::ClearUncontractedLauncher { component, release } => {
            record_clear_uncontracted_launcher(db, &component, &release).await?;
            saved_null()
        }
        ControlOp::RecordLauncherRunning {
            component,
            release,
            hash,
            now,
        } => {
            record_launcher_running(db, &component, &release, &hash, now).await?;
            saved_null()
        }
        ControlOp::RecordBakedLauncher {
            component,
            hash,
            now,
        } => {
            record_baked_launcher(db, &component, &hash, now).await?;
            saved_null()
        }
        ControlOp::RecordLauncherFailure {
            component,
            digest,
            error,
        } => {
            record_launcher_failure(db, &component, &digest, &error).await?;
            saved_null()
        }
        ControlOp::SelectRequiredLauncher { release, operation } => {
            record_required_launcher(db, &release, operation).await?;
            saved_null()
        }
        ControlOp::RecordWebuiDrift { error } => {
            record_webui_drift(db, &error).await?;
            saved_null()
        }
        ControlOp::SetWebuiPurgePending { error } => {
            set_webui_purge_pending(db, error).await?;
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
        ControlOp::SetUpdateAlertChannel { channel_id } => {
            set_update_alert_channel(db, channel_id).await?;
            saved_null()
        }
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
            scope,
            case_id,
            entry_id,
            actor_id,
            text,
        } => {
            let (store, scope) = captured_guild(tenant, scope).await.map_err(as_enrollment)?;
            let recorded = store
                .append_case_note(
                    scope,
                    case_id.try_into()?,
                    entry_id.try_into()?,
                    actor_id,
                    text,
                    crate::tenant::history_limits(),
                )
                .await
                .map_err(as_enrollment)?;
            saved_value(format!("{recorded:?}"))
        }
        ControlOp::RemoveCase { scope, case_id } => {
            let (store, scope) = captured_guild(tenant, scope).await.map_err(as_enrollment)?;
            let id: crate::db::guild_data::CaseId = case_id.try_into()?;
            let removed = store
                .remove_case(scope.clone(), id.clone())
                .await
                .map_err(as_enrollment)?;
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
            scope,
            revision,
            roles,
        } => {
            let (store, scope) = captured_guild(tenant, scope).await.map_err(as_enrollment)?;
            saved_value(
                store
                    .set_case_delete_roles(scope, &revision, roles)
                    .await
                    .map_err(as_enrollment)?,
            )
        }
        ControlOp::CleanupEvidence { scope } => {
            let (store, scope) = captured_guild(tenant, scope).await.map_err(as_enrollment)?;
            store
                .confirm_active_enrollment(scope.clone())
                .await
                .map_err(as_enrollment)?;
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
        // The bot launcher applies these under its own fence. The bot process
        // is stopped for part of a paired upgrade, so it never takes them.
        ControlOp::JoinUpgrade { .. }
        | ControlOp::AcknowledgeUpgrade { .. }
        | ControlOp::RecordUpgradeError { .. } => anyhow::bail!("write failed"),
    }
}

pub async fn apply_control_bytes(
    db: &Db,
    tenant: Option<&TenantRuntime>,
    bytes: &[u8],
) -> Result<ControlReply> {
    let op: ControlOp = serde_json::from_slice(bytes).context("write failed")?;
    apply_control(db, tenant, op, None).await
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

async fn captured_guild(
    tenant: Option<&TenantRuntime>,
    scope: ScopeWire,
) -> Result<(
    crate::db::guild_data::GuildDataStore,
    crate::db::guild_data::GuildDataScope,
)> {
    let tenant = tenant.context("write failed")?;
    let ConfigurationScope::Guild(enrollment) = scope_of(scope)? else {
        return Err(EnrollmentChanged.into());
    };
    let scope = tenant.scope(enrollment).await?;
    let store = tenant.store(&scope).await?;
    Ok((store, scope))
}

fn as_enrollment(error: anyhow::Error) -> anyhow::Error {
    let changed = error.chain().any(|cause| {
        cause.downcast_ref::<EnrollmentChanged>().is_some()
            || matches!(
                cause.downcast_ref::<crate::db::guild_data::TenantDataError>(),
                Some(crate::db::guild_data::TenantDataError::Membership)
            )
    });
    if changed {
        anyhow::Error::from(EnrollmentChanged)
    } else {
        error
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoked_actor_is_rejected_and_owner_gates_global_ops() {
        let manage = ControlActorNeed::ManageGuild(10);
        let allowed = CachedGuildActor {
            guild_owner_id: 1,
            permissions: MANAGE_GUILD_BIT,
        };
        assert!(decide_control_actor(manage, 5, 9, Some(allowed)).is_ok());
        let revoked = CachedGuildActor {
            guild_owner_id: 1,
            permissions: 0,
        };
        assert!(decide_control_actor(manage, 5, 9, Some(revoked)).is_err());
        assert!(decide_control_actor(manage, 5, 9, None).is_err());
        assert!(decide_control_actor(ControlActorNeed::Owner, 4, 9, None).is_err());
        assert!(decide_control_actor(ControlActorNeed::Owner, 9, 9, None).is_ok());
        assert!(decide_control_actor(ControlActorNeed::Owner, 9, 0, None).is_err());
        assert!(
            decide_control_actor(ControlActorNeed::GuildAdmin(10), 5, 9, Some(allowed)).is_err()
        );
        let admin = CachedGuildActor {
            guild_owner_id: 1,
            permissions: ADMINISTRATOR_BIT,
        };
        assert!(decide_control_actor(ControlActorNeed::GuildAdmin(10), 5, 9, Some(admin)).is_ok());
        assert!(decide_control_actor(ControlActorNeed::Process, 0, 0, None).is_ok());
    }

    #[tokio::test]
    async fn control_commit_rejects_foreign_or_expired_lease() {
        let path =
            std::env::temp_dir().join(format!("control_lease_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let owner = "a".repeat(32);
        let other = "b".repeat(32);
        let op = ControlOp::PatchSettings {
            scope: ConfigurationScope::Global.to_wire(),
            prefix: FieldPatch::Set("?".into()),
            bot_name: FieldPatch::Absent,
            reconcile: false,
        };
        write_lease(&db, &other, crate::control::now_ms() + 60_000).await;
        let reply = apply_control(&db, None, op.clone(), Some(&owner))
            .await
            .unwrap();
        assert!(matches!(reply, ControlReply::Waiting));
        assert_ne!(get_settings(&db, 0).await.unwrap().prefix, "?");

        write_lease(&db, &owner, crate::control::now_ms() - 1_000).await;
        let reply = apply_control(&db, None, op.clone(), Some(&owner))
            .await
            .unwrap();
        assert!(matches!(reply, ControlReply::Waiting));
        assert_ne!(get_settings(&db, 0).await.unwrap().prefix, "?");

        write_lease(&db, &owner, crate::control::now_ms() + 60_000).await;
        let reply = apply_control(&db, None, op, Some(&owner)).await.unwrap();
        assert!(matches!(
            reply,
            ControlReply::Saved { .. } | ControlReply::Unchanged { .. }
        ));
        assert_eq!(get_settings(&db, 0).await.unwrap().prefix, "?");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn armed_owner_rejects_commits_after_the_lease_is_released() {
        let path =
            std::env::temp_dir().join(format!("armed_lease_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        db.with_conn(|conn| {
            let mut tx = conn.transaction_primary_write()?;
            tx.ensure_mutation_receipt()?;
            tx.execute_bounded(
                "INSERT INTO meta (key, value) VALUES ('lease_probe', 'open')",
                params![],
                4096,
            )?;
            tx.commit()
        })
        .await
        .unwrap();
        let owner = "cd".repeat(16);
        assert!(
            claim_active_bot(
                &db,
                &owner,
                crate::control::now_ms(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        release_active_bot(&db, &owner).await.unwrap();
        let error = db
            .with_conn(|conn| {
                let mut tx = conn.transaction_primary_write()?;
                tx.ensure_mutation_receipt()?;
                tx.execute_bounded(
                    "UPDATE meta SET value='closed' WHERE key='lease_probe'",
                    params![],
                    4096,
                )?;
                tx.commit()
            })
            .await
            .unwrap_err();
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<crate::db_compat::ActiveBotLeaseRejected>()
                .is_some()),
            "{error:#}"
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn fresh_claim_records_how_the_previous_bot_stopped() {
        let path =
            std::env::temp_dir().join(format!("previous_stop_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let refuse = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let [first, second, third, fourth] = ["a1", "b2", "c3", "d4"].map(|id| id.repeat(16));
        let now = crate::control::now_ms();
        assert!(claim_active_bot(&db, &first, now, refuse()).await.unwrap());
        assert_eq!(previous_active_bot_stop(&db).await.unwrap(), None);

        // A shutdown signal then release is a planned stop dated by the marker.
        record_planned_active_bot_stop(&db, &first, now + 100)
            .await
            .unwrap();
        release_active_bot(&db, &first).await.unwrap();
        assert!(claim_active_bot(&db, &second, now, refuse()).await.unwrap());
        let planned = previous_active_bot_stop(&db).await.unwrap();
        assert_eq!(
            planned,
            Some(PreviousBotStop {
                last_alive_ms: now + 100,
                planned: true
            })
        );
        // Renewal by the same owner keeps the record.
        assert!(
            claim_active_bot(&db, &second, now + 1, refuse())
                .await
                .unwrap()
        );
        assert_eq!(previous_active_bot_stop(&db).await.unwrap(), planned);

        // An owner that never released was last alive one TTL before expiry. Its
        // own planned-stop marker does not make that unconfirmed stop planned.
        record_planned_active_bot_stop(&db, &second, now + 1)
            .await
            .unwrap();
        let expires = now + 1 + ACTIVE_BOT_LEASE_TTL_MS;
        let later = expires + 5_000;
        assert!(
            claim_active_bot(&db, &third, later, refuse())
                .await
                .unwrap()
        );
        assert_eq!(
            previous_active_bot_stop(&db).await.unwrap(),
            Some(PreviousBotStop {
                last_alive_ms: now + 1,
                planned: false
            })
        );

        // A release without a marker for its own generation has no known stop time.
        write_lease(&db, &third, crate::control::now_ms() + 60_000).await;
        release_active_bot(&db, &third).await.unwrap();
        assert!(
            claim_active_bot(&db, &fourth, crate::control::now_ms(), refuse())
                .await
                .unwrap()
        );
        assert_eq!(previous_active_bot_stop(&db).await.unwrap(), None);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    async fn write_lease(db: &Db, owner: &str, expires: i64) {
        let value = format!("{owner} {expires}");
        db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_write()?;
            tx.ensure_mutation_receipt()?;
            tx.execute_bounded(
                "INSERT INTO meta (key, value) VALUES ('active_bot_lease', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![value],
                4096,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    }
}
