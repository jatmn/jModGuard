//! Scoped live directory observations. These writes are independent of log delivery.

use crate::db::guild_data::{
    CaseOutcome, MemberDepartureDecision, MemberObservation, MemberVerifyKind,
};
use crate::{db, state::BotState};
use anyhow::{Context as _, Result};
use serenity::model::{event::GuildMemberUpdateEvent, guild::Member, user::User};
use std::time::Duration;

fn timestamp_ms(value: serenity::model::Timestamp) -> Option<i64> {
    value
        .timestamp_millis()
        .gt(&0)
        .then_some(value.timestamp_millis())
}

pub fn from_member(member: &Member, observed_at_ms: i64, is_join: bool) -> MemberObservation {
    MemberObservation {
        user_id: member.user.id.get() as i64,
        username: Some(Some(member.user.name.clone())),
        global_name: Some(member.user.global_name.clone()),
        nickname: Some(member.nick.clone()),
        joined_at_ms: member.joined_at.and_then(timestamp_ms),
        observed_at_ms,
        is_join,
    }
}

fn from_update(event: &GuildMemberUpdateEvent, observed_at_ms: i64) -> MemberObservation {
    MemberObservation {
        user_id: event.user.id.get() as i64,
        username: Some(Some(event.user.name.clone())),
        global_name: Some(event.user.global_name.clone()),
        nickname: Some(event.nick.clone()),
        joined_at_ms: timestamp_ms(event.joined_at),
        observed_at_ms,
        is_join: false,
    }
}

async fn live(
    app: &BotState,
    guild_id: u64,
) -> Option<(
    db::guild_data::GuildDataStore,
    db::guild_data::GuildDataScope,
)> {
    let enrollment = crate::cases::capture(app, guild_id).await?;
    match app.tenant.live(enrollment).await {
        Ok(scope) => Some(scope),
        Err(error) => {
            tracing::warn!(guild_id, "member directory scope unavailable: {error:#}");
            None
        }
    }
}

async fn live_observation(
    app: &BotState,
    guild_id: u64,
    user_id: Option<i64>,
    reason: &str,
) -> Option<(
    db::guild_data::GuildDataStore,
    db::guild_data::GuildDataScope,
)> {
    let enrollment = match db::guild_enrollment(&app.db, guild_id as i64).await {
        Ok(Some(enrollment)) => enrollment,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                guild_id,
                "member directory enrollment unavailable: {error:#}"
            );
            app.remember_member_gap(guild_id, None, user_id, reason);
            return None;
        }
    };
    match app.tenant.live(enrollment.clone()).await {
        Ok(scope) => Some(scope),
        Err(error) => {
            tracing::warn!(guild_id, "member directory scope unavailable: {error:#}");
            app.remember_member_gap(guild_id, Some(enrollment.id), user_id, reason);
            None
        }
    }
}

async fn mark_gap(
    app: &BotState,
    store: &db::guild_data::GuildDataStore,
    scope: db::guild_data::GuildDataScope,
    user_id: Option<i64>,
    reason: &str,
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Err(error) = store
        .mark_member_reconciliation_needed(scope.clone(), reason, now_ms)
        .await
    {
        tracing::warn!("member directory coverage marker deferred: {error:#}");
        app.remember_member_gap(
            scope.guild_id() as u64,
            Some(scope.enrollment().id.clone()),
            user_id,
            reason,
        );
        return;
    }
    if let Some(user_id) = user_id {
        let kind = if reason.contains("ban") {
            MemberVerifyKind::Ban
        } else {
            MemberVerifyKind::Member
        };
        if let Err(error) = store
            .queue_member_target_check(scope.clone(), user_id, kind, reason, now_ms)
            .await
        {
            tracing::warn!(
                guild_id = scope.guild_id(),
                user_id,
                "member target check deferred: {error:#}"
            );
            app.remember_member_gap(
                scope.guild_id() as u64,
                Some(scope.enrollment().id.clone()),
                Some(user_id),
                reason,
            );
        }
    }
}

pub async fn mark_guild_gap(app: &BotState, guild_id: u64, reason: &str) {
    let Some((store, scope)) = live_observation(app, guild_id, None, reason).await else {
        return;
    };
    mark_gap(app, &store, scope, None, reason).await;
}

pub async fn mark_user_gap(app: &BotState, guild_id: u64, user_id: i64, reason: &str) {
    let Some((store, scope)) = live_observation(app, guild_id, Some(user_id), reason).await else {
        return;
    };
    mark_gap(app, &store, scope, Some(user_id), reason).await;
}

pub async fn flush_pending_gaps(
    app: &BotState,
    store: &db::guild_data::GuildDataStore,
    scope: &db::guild_data::GuildDataScope,
) {
    for gap in app.pending_member_gaps(scope.guild_id() as u64) {
        // An old event cannot be applied to a replacement enrollment.
        if gap
            .enrollment_id
            .as_deref()
            .is_some_and(|id| id != scope.enrollment().id)
        {
            app.acknowledge_member_gap(&gap);
            continue;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(error) = store
            .mark_member_reconciliation_needed(scope.clone(), &gap.reason, now_ms)
            .await
        {
            tracing::warn!(
                guild_id = gap.guild_id,
                "member coverage gap retry failed: {error:#}"
            );
            continue;
        }
        let mut queued = true;
        for (user_id, is_ban) in &gap.users {
            let kind = if *is_ban {
                MemberVerifyKind::Ban
            } else {
                MemberVerifyKind::Member
            };
            if let Err(error) = store
                .queue_member_target_check(scope.clone(), *user_id, kind, &gap.reason, now_ms)
                .await
            {
                tracing::warn!(
                    guild_id = gap.guild_id,
                    user_id,
                    "member target check retry failed: {error:#}"
                );
                queued = false;
                break;
            }
        }
        if queued {
            app.acknowledge_member_gap(&gap);
        }
    }
}

/// How long the ban-in-progress marker may delay the Discord ban.
///
/// A save that misses this deadline is abandoned. The remote transaction can
/// still commit after the client stops waiting; settlement treats that row
/// like a marker that never became visible.
pub const BAN_MARKER_SAVE_DEADLINE: Duration = Duration::from_secs(1);

/// Fences ordinary-departure cleanup while an outbound bot ban is in flight.
/// The marker is durable so a crash cannot erase a possible confirmed ban.
/// Failure or timeout of the insert does not cancel the Discord ban.
pub struct PendingBan {
    guild_id: u64,
    user_id: i64,
    operation_id: String,
    located: Option<(
        db::guild_data::GuildDataStore,
        db::guild_data::GuildDataScope,
    )>,
    save: MarkerSave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkerSave {
    /// The insert committed before the ban.
    Saved,
    /// The insert failed and will not commit.
    Missing,
    /// The client stopped waiting, or the commit response was lost.
    /// The insert may still commit.
    MaybeLate,
}

pub async fn begin_ban_attempt(app: &BotState, guild_id: u64, user_id: u64) -> PendingBan {
    let operation_id = db::guild_data::CaseId::new().as_str().to_owned();
    let user_key = user_id as i64;
    let located = std::sync::Mutex::new(None);
    let save = async {
        let (store, scope) = live(app, guild_id)
            .await
            .context("member directory scope unavailable")?;
        *located.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((store.clone(), scope.clone()));
        store
            .begin_member_ban_attempt(
                scope,
                user_key,
                operation_id.clone(),
                chrono::Utc::now().timestamp_millis(),
            )
            .await?;
        Ok(())
    };
    let save = reserve_ban_marker(save).await;
    if save != MarkerSave::Saved {
        tracing::warn!(
            guild_id,
            user_id,
            "ban retention marker was not confirmed; banning on Discord anyway"
        );
    }
    PendingBan {
        guild_id,
        user_id: user_key,
        operation_id,
        located: located
            .into_inner()
            .unwrap_or_else(|error| error.into_inner()),
        save,
    }
}

impl PendingBan {
    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscordBanView {
    Absent,
    Present { reason: Option<String> },
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BanContinuation {
    Stop,
    AlreadyFinished { reason: Option<String> },
    RecordOnly { reason: Option<String> },
    BanThenRecord,
}

/// Lease loss stops the attempt, including a marker that may still commit.
/// A ban Discord already shows is recorded once and never sent again.
pub(crate) async fn observe_ban_handoff(
    app: &BotState,
    http: &serenity::http::Http,
    guild_id: u64,
    user_id: u64,
    save: MarkerSave,
) -> BanContinuation {
    if !app.activation.may_record() {
        return BanContinuation::Stop;
    }
    let view = discord_ban_view(http, guild_id, user_id).await;
    let row_exists = db::active_ban_recorded(&app.reads, guild_id as i64, user_id as i64)
        .await
        .unwrap_or(true);
    ban_continuation(app.activation.may_record(), save, view, row_exists)
}

async fn discord_ban_view(
    http: &serenity::http::Http,
    guild_id: u64,
    user_id: u64,
) -> DiscordBanView {
    let guild = serenity::model::id::GuildId::new(guild_id);
    let user = serenity::model::id::UserId::new(user_id);
    match tokio::time::timeout(Duration::from_secs(5), http.get_ban(guild, user)).await {
        Ok(Ok(Some(ban))) => DiscordBanView::Present { reason: ban.reason },
        Ok(Ok(_)) => DiscordBanView::Absent,
        Ok(Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(
            response,
        )))) if response.status_code.as_u16() == 404 => DiscordBanView::Absent,
        _ => DiscordBanView::Unknown,
    }
}

pub(crate) fn ban_continuation(
    may_record: bool,
    _save: MarkerSave,
    view: DiscordBanView,
    row_exists: bool,
) -> BanContinuation {
    if !may_record {
        return BanContinuation::Stop;
    }
    match view {
        DiscordBanView::Unknown => BanContinuation::Stop,
        DiscordBanView::Present { reason } if row_exists => {
            BanContinuation::AlreadyFinished { reason }
        }
        DiscordBanView::Present { reason } => BanContinuation::RecordOnly { reason },
        DiscordBanView::Absent => BanContinuation::BanThenRecord,
    }
}

/// Classifies the marker insert. A lost commit or a client deadline is
/// `MaybeLate` because the server may have committed after this process gave up.
async fn reserve_ban_marker<F>(save: F) -> MarkerSave
where
    F: std::future::Future<Output = Result<()>>,
{
    match tokio::time::timeout(BAN_MARKER_SAVE_DEADLINE, save).await {
        Ok(Ok(())) => MarkerSave::Saved,
        Ok(Err(error)) if commit_may_still_land(&error) => MarkerSave::MaybeLate,
        Ok(Err(_)) => MarkerSave::Missing,
        Err(_elapsed) => MarkerSave::MaybeLate,
    }
}

fn commit_may_still_land(error: &anyhow::Error) -> bool {
    db::is_database_commit_indeterminate(error) || db::is_database_operation_timeout(error)
}

fn directory_repair_needed(outcome: &CaseOutcome) -> bool {
    matches!(
        outcome,
        CaseOutcome::Succeeded | CaseOutcome::Uncertain | CaseOutcome::Attempted
    )
}

impl PendingBan {
    pub async fn finish(self, app: &BotState, outcome: CaseOutcome) {
        let Some((store, scope)) = self.located else {
            if directory_repair_needed(&outcome) {
                app.remember_member_gap(
                    self.guild_id,
                    None,
                    Some(self.user_id),
                    "ban retention marker unavailable",
                );
            }
            return;
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(error) = store
            .finish_member_ban_attempt(
                scope.clone(),
                self.user_id,
                &self.operation_id,
                outcome.clone(),
                now_ms,
            )
            .await
        {
            tracing::warn!(
                guild_id = self.guild_id,
                user_id = self.user_id,
                "ban attempt marker settlement failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(self.user_id),
                "ban attempt settlement unavailable",
            )
            .await;
            return;
        }
        // A confirmed save keeps today's settlement and does not queue a second repair.
        if self.save == MarkerSave::Saved || !directory_repair_needed(&outcome) {
            return;
        }
        if member_records_ban(&store, &scope, self.user_id).await {
            return;
        }
        // A row that is already visible is the pending-ban verification's job.
        // Queue the missing-marker gap only when that fence is absent.
        let marker_visible = store
            .next_member_pending_ban_verification(scope.clone(), i64::MAX)
            .await
            .ok()
            .flatten()
            .is_some_and(|row| {
                row.operation_id == self.operation_id && row.user_id == self.user_id
            });
        if marker_visible {
            return;
        }
        mark_gap(
            app,
            &store,
            scope,
            Some(self.user_id),
            "ban retention marker unavailable",
        )
        .await;
    }
}

async fn member_records_ban(
    store: &db::guild_data::GuildDataStore,
    scope: &db::guild_data::GuildDataScope,
    user_id: i64,
) -> bool {
    store
        .get_member(scope.clone(), user_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|member| member.is_banned)
}

pub async fn observe_join(app: &BotState, member: &Member, observed_at_ms: i64) {
    let Some((store, scope)) = live_observation(
        app,
        member.guild_id.get(),
        Some(member.user.id.get() as i64),
        "member join persistence unavailable",
    )
    .await
    else {
        return;
    };
    if let Err(error) = store
        .observe_member(scope.clone(), from_member(member, observed_at_ms, true))
        .await
    {
        tracing::warn!(
            guild_id = member.guild_id.get(),
            user_id = member.user.id.get(),
            "member join observation failed: {error:#}"
        );
        mark_gap(
            app,
            &store,
            scope,
            Some(member.user.id.get() as i64),
            "member join persistence unavailable",
        )
        .await;
    }
}

pub async fn observe_update(app: &BotState, event: &GuildMemberUpdateEvent, observed_at_ms: i64) {
    let Some((store, scope)) = live_observation(
        app,
        event.guild_id.get(),
        Some(event.user.id.get() as i64),
        "member update persistence unavailable",
    )
    .await
    else {
        return;
    };
    if let Err(error) = store
        .observe_member(scope.clone(), from_update(event, observed_at_ms))
        .await
    {
        tracing::warn!(
            guild_id = event.guild_id.get(),
            user_id = event.user.id.get(),
            "member name observation failed: {error:#}"
        );
        mark_gap(
            app,
            &store,
            scope,
            Some(event.user.id.get() as i64),
            "member update persistence unavailable",
        )
        .await;
    }
}

pub async fn observe_departure(
    app: &BotState,
    guild_id: u64,
    user: &User,
    departed_at_ms: i64,
    expected_joined_at_ms: Option<i64>,
) -> Option<MemberDepartureDecision> {
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user.id.get() as i64),
        "member departure persistence unavailable",
    )
    .await?;
    match store
        .depart_member_checked(
            scope.clone(),
            user.id.get() as i64,
            departed_at_ms,
            expected_joined_at_ms,
        )
        .await
    {
        Ok(MemberDepartureDecision::NeedsVerification) => {
            // The fenced departure transaction queued the targeted check.
            Some(MemberDepartureDecision::NeedsVerification)
        }
        Ok(result) => Some(result),
        Err(error) => {
            tracing::warn!(
                guild_id,
                user_id = user.id.get(),
                "member departure observation failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(user.id.get() as i64),
                "member departure persistence unavailable",
            )
            .await;
            None
        }
    }
}

/// Whether the live member directory already shows this user as banned.
pub(crate) async fn member_directory_shows_banned(
    app: &BotState,
    guild_id: u64,
    user_id: u64,
) -> Option<bool> {
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user_id as i64),
        "ban mirror precheck unavailable",
    )
    .await?;
    #[cfg(test)]
    app.record_gateway_ban_path_test_phase("precheck");
    match store.get_member(scope, user_id as i64).await {
        Ok(Some(member)) => Some(member.is_banned),
        Ok(None) => Some(false),
        Err(error) => {
            tracing::warn!(guild_id, user_id, "ban mirror precheck failed: {error:#}");
            None
        }
    }
}

pub async fn observe_ban(
    app: &BotState,
    guild_id: u64,
    user: &User,
    observed_at_ms: i64,
) -> Option<bool> {
    #[cfg(test)]
    app.record_gateway_ban_path_test_phase("observe");
    let user_id = user.id.get();
    let contested = app.begin_ban_observation(guild_id, user_id);
    let _end = BanObservation::new(app, guild_id, user_id);
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user_id as i64),
        "ban observation persistence unavailable",
    )
    .await?;
    let saved = if contested {
        store
            .observe_ban_contested(
                scope.clone(),
                user_id as i64,
                observed_at_ms,
                user.name.clone(),
                user.global_name.clone(),
            )
            .await
    } else {
        store
            .observe_ban_with_identity(
                scope.clone(),
                user_id as i64,
                observed_at_ms,
                user.name.clone(),
                user.global_name.clone(),
            )
            .await
    };
    match saved {
        Ok(changed) => Some(changed),
        Err(error) => {
            tracing::warn!(
                guild_id,
                user_id,
                "member ban observation failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(user_id as i64),
                "ban observation persistence unavailable",
            )
            .await;
            None
        }
    }
}

struct BanObservation<'a> {
    app: &'a BotState,
    guild_id: u64,
    user_id: u64,
}

impl<'a> BanObservation<'a> {
    fn new(app: &'a BotState, guild_id: u64, user_id: u64) -> Self {
        Self {
            app,
            guild_id,
            user_id,
        }
    }
}

impl Drop for BanObservation<'_> {
    fn drop(&mut self) {
        self.app.end_ban_observation(self.guild_id, self.user_id);
    }
}

/// Serializes overlapping moderator-initiated bans for one guild member.
pub(crate) struct CommandBanInProgress<'a> {
    _observation: BanObservation<'a>,
}

pub(crate) fn begin_command_ban_in_progress(
    app: &BotState,
    guild_id: u64,
    user_id: u64,
) -> Result<CommandBanInProgress<'_>> {
    if app.begin_ban_observation(guild_id, user_id) {
        anyhow::bail!("another ban for this user is already in progress");
    }
    Ok(CommandBanInProgress {
        _observation: BanObservation::new(app, guild_id, user_id),
    })
}

pub async fn observe_unban(
    app: &BotState,
    guild_id: u64,
    user: &User,
    observed_at_ms: i64,
) -> Option<bool> {
    let user_id = user.id.get();
    let contested = app.begin_ban_observation(guild_id, user_id);
    let _end = BanObservation::new(app, guild_id, user_id);
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user_id as i64),
        "unban observation persistence unavailable",
    )
    .await?;
    let saved = if contested {
        store
            .observe_unban_contested(
                scope.clone(),
                user_id as i64,
                observed_at_ms,
                user.name.clone(),
                user.global_name.clone(),
            )
            .await
    } else {
        store
            .observe_unban_with_identity(
                scope.clone(),
                user_id as i64,
                observed_at_ms,
                user.name.clone(),
                user.global_name.clone(),
            )
            .await
    };
    match saved {
        Ok(changed) => Some(changed),
        Err(error) => {
            tracing::warn!(
                guild_id,
                user_id = user.id.get(),
                "member unban observation failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(user.id.get() as i64),
                "unban observation persistence unavailable",
            )
            .await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::db::guild_data::{
        CaseAction, CaseEvidence, CaseOutcome, MemberBanSnapshot, MemberVerification,
    };
    use crate::state::BotState;
    use crate::{db, tenant};

    use super::{
        BAN_MARKER_SAVE_DEADLINE, BanContinuation, DiscordBanView, MarkerSave, PendingBan,
        ban_continuation, begin_ban_attempt, reserve_ban_marker,
    };

    #[test]
    fn expired_lease_ignores_a_late_marker_and_cutoff_does_not_double_ban() {
        assert_eq!(
            ban_continuation(false, MarkerSave::MaybeLate, DiscordBanView::Absent, false),
            BanContinuation::Stop,
            "a late marker after lease loss must not authorize a ban or a log"
        );
        assert_eq!(
            ban_continuation(true, MarkerSave::MaybeLate, DiscordBanView::Absent, false),
            BanContinuation::BanThenRecord
        );
        assert_eq!(
            ban_continuation(
                false,
                MarkerSave::Saved,
                DiscordBanView::Present { reason: None },
                false
            ),
            BanContinuation::Stop
        );
        assert_eq!(
            ban_continuation(
                true,
                MarkerSave::Saved,
                DiscordBanView::Present { reason: None },
                false
            ),
            BanContinuation::RecordOnly { reason: None },
            "a successor records the missing row and does not ban again"
        );
        assert_eq!(
            ban_continuation(
                true,
                MarkerSave::Saved,
                DiscordBanView::Present { reason: None },
                true
            ),
            BanContinuation::AlreadyFinished { reason: None },
            "a recorded ban is not logged a second time"
        );
        assert_eq!(
            ban_continuation(true, MarkerSave::Saved, DiscordBanView::Unknown, false),
            BanContinuation::Stop
        );
    }

    const GUILD: i64 = 10;
    const USER: i64 = 91;

    struct TempDb(PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            tenant::remove_sidecars(&self.0);
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn open_bot() -> (TempDb, Arc<BotState>, db::GuildEnrollment) {
        let path =
            std::env::temp_dir().join(format!("ban_marker_{:032x}.db", rand::random::<u128>()));
        let app = BotState::new(Default::default(), db::Db::open(&path, 2).unwrap()).unwrap();
        let enrollment = db::enroll_guild(&app.db, GUILD, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        (TempDb(path), app, enrollment)
    }

    async fn ready_directory(
        app: &BotState,
        enrollment: &db::GuildEnrollment,
    ) -> (
        db::guild_data::GuildDataStore,
        db::guild_data::GuildDataScope,
    ) {
        let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        (store, scope)
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Recorded {
        bans: i64,
        cases: usize,
        mod_logs: i64,
        history: Vec<String>,
    }

    async fn recorded(
        app: &BotState,
        store: &db::guild_data::GuildDataStore,
        scope: &db::guild_data::GuildDataScope,
    ) -> Recorded {
        let stats = db::guild_stats(&app.db, GUILD).await.unwrap();
        let cases = store
            .case_ids(scope.clone(), None, 20, tenant::case_limits())
            .await
            .unwrap()
            .len();
        let mod_logs = app
            .db
            .with_conn(|conn| {
                use jmodguard::db::SqlExecutor as _;
                conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE guild_id=?1 AND event_type='mod_action'",
                    libsql::params![GUILD],
                    |row| Ok(row.get(0)?),
                )
            })
            .await
            .unwrap();
        let history = store
            .member_history(scope.clone(), USER, None, 50)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.kind)
            .collect();
        Recorded {
            bans: stats.bans,
            cases,
            mod_logs,
            history,
        }
    }

    /// The caller's one ban record: case, ban mirror, and mod-log row.
    async fn record_ban_once(app: &BotState, enrollment: &db::GuildEnrollment) {
        crate::cases::record_application(
            app,
            Some(enrollment.clone()),
            CaseAction::Ban,
            CaseOutcome::Succeeded,
            USER as u64,
            Some(7),
            Some("retention marker".into()),
            CaseEvidence::Unavailable,
        )
        .await;
        db::record_enrolled_ban(
            &app.db,
            Some(enrollment.clone()),
            USER,
            Some(7),
            "retention marker",
        )
        .await
        .unwrap();
        db::insert_enrolled_event(
            &app.db,
            Some(enrollment.clone()),
            "mod_action",
            Some(USER),
            Some(7),
            &serde_json::json!({"action": "ban"}),
        )
        .await
        .unwrap();
    }

    async fn settle_queued_repair(
        store: &db::guild_data::GuildDataStore,
        scope: &db::guild_data::GuildDataScope,
    ) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        // The missing-marker target check is the repair. Settle it before a late
        // fence so that fence cannot apply a second directory observation.
        if let Some(check) = store
            .next_member_target_check(scope.clone(), now_ms.saturating_add(60_000))
            .await
            .unwrap()
        {
            store
                .settle_member_target_check(
                    scope.clone(),
                    check.clone(),
                    MemberVerification {
                        user_id: check.user_id,
                        revision: check.revision,
                        kind: check.kind,
                        present: true,
                        member: None,
                        ban: Some(MemberBanSnapshot {
                            user_id: check.user_id,
                            username: None,
                            global_name: None,
                            reason: None,
                        }),
                    },
                    now_ms,
                )
                .await
                .unwrap();
        }
        if let Some(candidate) = store
            .next_member_pending_ban_verification(scope.clone(), i64::MAX)
            .await
            .unwrap()
        {
            store
                .settle_member_pending_ban_verification(scope.clone(), candidate, true, now_ms)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn failed_marker_save_still_bans_and_repairs_the_directory() {
        let (_db, app, enrollment) = open_bot().await;
        let (store, scope) = ready_directory(&app, &enrollment).await;
        let mut discord_bans = 0;
        let pending = PendingBan {
            guild_id: GUILD as u64,
            user_id: USER,
            operation_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            located: Some((store.clone(), scope.clone())),
            save: MarkerSave::Missing,
        };
        discord_bans += 1;
        record_ban_once(&app, &enrollment).await;
        let after_ban = recorded(&app, &store, &scope).await;
        pending.finish(&app, CaseOutcome::Succeeded).await;
        settle_queued_repair(&store, &scope).await;
        let member = store
            .get_member(scope.clone(), USER)
            .await
            .unwrap()
            .unwrap();
        assert!(member.is_banned);
        assert_eq!(discord_bans, 1);
        assert_eq!(recorded(&app, &store, &scope).await, after_ban);
        assert_eq!(after_ban.bans, 1);
        assert_eq!(after_ban.cases, 1);
        assert_eq!(after_ban.mod_logs, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_marker_save_bans_within_the_deadline() {
        let start = tokio::time::Instant::now();
        let task = tokio::spawn(async {
            let save = reserve_ban_marker(std::future::pending::<anyhow::Result<()>>()).await;
            (save, tokio::time::Instant::now())
        });
        tokio::time::sleep(BAN_MARKER_SAVE_DEADLINE + Duration::from_millis(20)).await;
        let (save, banned_at) = task.await.unwrap();
        assert_eq!(save, MarkerSave::MaybeLate);
        let elapsed = banned_at.saturating_duration_since(start);
        assert!(
            elapsed >= BAN_MARKER_SAVE_DEADLINE,
            "ban ran before the marker deadline: {elapsed:?}"
        );
        assert!(
            elapsed <= BAN_MARKER_SAVE_DEADLINE + Duration::from_millis(50),
            "ban waited past the marker deadline: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn late_marker_matches_a_missing_marker() {
        let (_db, app, enrollment) = open_bot().await;
        let (store, scope) = ready_directory(&app, &enrollment).await;
        let operation_id = "cccccccccccccccccccccccccccccccc".to_string();
        let mut discord_bans = 0;
        let pending = PendingBan {
            guild_id: GUILD as u64,
            user_id: USER,
            operation_id: operation_id.clone(),
            located: Some((store.clone(), scope.clone())),
            save: MarkerSave::MaybeLate,
        };
        discord_bans += 1;
        record_ban_once(&app, &enrollment).await;
        let after_ban = recorded(&app, &store, &scope).await;
        pending.finish(&app, CaseOutcome::Succeeded).await;
        // The abandoned insert commits only after the ban and its log were recorded.
        store
            .begin_member_ban_attempt(
                scope.clone(),
                USER,
                operation_id,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            .unwrap();
        settle_queued_repair(&store, &scope).await;
        settle_queued_repair(&store, &scope).await;
        let repaired = recorded(&app, &store, &scope).await;
        assert_eq!(discord_bans, 1);
        assert_eq!(repaired.bans, after_ban.bans);
        assert_eq!(repaired.cases, after_ban.cases);
        assert_eq!(repaired.mod_logs, after_ban.mod_logs);
        assert_eq!(repaired.bans, 1);
        assert_eq!(repaired.cases, 1);
        assert_eq!(repaired.mod_logs, 1);
        let ban_rows = repaired
            .history
            .iter()
            .filter(|kind| {
                matches!(
                    kind.as_str(),
                    "ban" | "ban_present" | "ban_present_state_observation"
                )
            })
            .count();
        assert_eq!(ban_rows, 1, "{:?}", repaired.history);
        let member = store.get_member(scope, USER).await.unwrap().unwrap();
        assert!(member.is_banned);
    }

    #[tokio::test]
    async fn healthy_marker_save_still_settles_without_a_gap() {
        let (_db, app, enrollment) = open_bot().await;
        let (store, scope) = ready_directory(&app, &enrollment).await;
        let pending = begin_ban_attempt(&app, GUILD as u64, USER as u64).await;
        assert_eq!(pending.save, MarkerSave::Saved);
        let marker = store
            .next_member_pending_ban_verification(scope.clone(), i64::MAX)
            .await
            .unwrap();
        assert!(marker.is_some());
        record_ban_once(&app, &enrollment).await;
        let after_ban = recorded(&app, &store, &scope).await;
        pending.finish(&app, CaseOutcome::Succeeded).await;
        let member = store
            .get_member(scope.clone(), USER)
            .await
            .unwrap()
            .unwrap();
        assert!(member.ever_banned);
        assert!(member.is_banned);
        assert!(
            store
                .next_member_pending_ban_verification(scope.clone(), i64::MAX)
                .await
                .unwrap()
                .is_none()
        );
        let status = store
            .member_sync_status(scope.clone(), 42, chrono::Utc::now().timestamp_millis())
            .await
            .unwrap();
        assert!(!status.reconciliation_needed);
        assert_eq!(recorded(&app, &store, &scope).await, after_ban);
    }

    #[tokio::test]
    async fn begin_ban_attempt_does_not_fail_the_ban_when_the_save_is_refused() {
        let (_db, app, enrollment) = open_bot().await;
        let destination = db::guild_stores::StoreId::new();
        db::guild_stores::register_store(
            &app.db,
            destination.clone(),
            db::guild_stores::StorePolicy::Dedicated { guild_id: GUILD },
        )
        .await
        .unwrap();
        {
            use jmodguard::db::SqlExecutor as _;
            app.db
                .with_conn(move |conn| {
                    conn.execute_bounded(
                        "INSERT INTO guild_write_fences(
                            guild_id,generation,phase,destination_store_id,destination_revision,created_at
                         ) VALUES(?1,?2,'freeze',?3,?2,unixepoch())",
                        libsql::params![GUILD, format!("{:032x}", 9u128), destination.as_str()],
                        64 * 1024 * 1024,
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let pending = begin_ban_attempt(&app, GUILD as u64, USER as u64).await;
        assert_eq!(pending.save, MarkerSave::Missing);
        let mut discord_bans = 0;
        discord_bans += 1;
        pending.finish(&app, CaseOutcome::Succeeded).await;
        assert_eq!(discord_bans, 1);
        let gaps = app.pending_member_gaps(GUILD as u64);
        assert!(
            gaps.iter()
                .any(|gap| gap.users.iter().any(|(user, ban)| *user == USER && *ban)),
            "refused marker save must keep an in-memory repair"
        );
        let _ = enrollment;
    }

    #[tokio::test]
    async fn lost_commit_is_classified_as_a_late_marker() {
        let missing = reserve_ban_marker(async { Err(anyhow::anyhow!("disk full")) }).await;
        assert_eq!(missing, MarkerSave::Missing);
        let late =
            reserve_ban_marker(async { Err(db::test_database_commit_indeterminate_error()) }).await;
        assert_eq!(late, MarkerSave::MaybeLate);
        let timed_out =
            reserve_ban_marker(async { Err(db::test_database_operation_timeout_error()) }).await;
        assert_eq!(timed_out, MarkerSave::MaybeLate);
    }

    #[test]
    fn each_ban_path_uses_the_shared_marker_helper() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for rel in [
            "src/automod.rs",
            "src/spam_actions.rs",
            "src/commands/behaviors.rs",
            "src/commands/plans.rs",
        ] {
            let source = std::fs::read_to_string(root.join(rel)).unwrap();
            assert!(
                source.contains("member_directory::begin_ban_attempt"),
                "{rel} must ban through the shared marker helper"
            );
            assert!(
                !source.contains("deferred until retention marker"),
                "{rel} must not skip the Discord ban when the marker save fails"
            );
        }
        let helper = include_str!("member_directory.rs");
        assert!(helper.contains("reserve_ban_marker(save)"));
        assert!(helper.contains("BAN_MARKER_SAVE_DEADLINE"));
    }

    #[tokio::test]
    async fn stale_active_ban_is_replaced_when_discord_has_no_ban() {
        assert_eq!(
            ban_continuation(true, MarkerSave::Missing, DiscordBanView::Absent, true),
            BanContinuation::BanThenRecord
        );
        let path =
            std::env::temp_dir().join(format!("stale_ban_{:032x}.db", rand::random::<u128>()));
        let database = db::Db::open(&path, 2).unwrap();
        let enrollment = db::enroll_guild(&database, 10, 100).await.unwrap().unwrap();
        db::record_enrolled_ban_with_identity(
            &database,
            Some(enrollment.clone()),
            55,
            None,
            None,
            Some(1),
            None,
            "old reason",
            Some("2020-01-01T00:00:00+00:00"),
        )
        .await
        .unwrap();
        let occurred = "2024-02-02T00:00:00+00:00";
        let id = db::record_enrolled_ban_if_absent(
            &database,
            Some(enrollment.clone()),
            55,
            None,
            None,
            Some(9),
            None,
            "new reason",
            Some(occurred),
            db::BanRecord::Retry,
            None,
        )
        .await
        .unwrap();
        assert!(id.is_some(), "a new ban must not be suppressed");
        let rows = db::list_enrolled_bans(&database, enrollment.clone(), 10)
            .await
            .unwrap();
        let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].reason.as_deref(), Some("new reason"));
        assert_eq!(active[0].moderator_id, Some(9));
        assert!(
            rows.iter()
                .any(|row| !row.active && row.moderator_id == Some(1))
        );
        let again = db::record_enrolled_ban_if_absent(
            &database,
            Some(enrollment.clone()),
            55,
            None,
            None,
            Some(9),
            None,
            "new reason",
            Some(occurred),
            db::BanRecord::Retry,
            None,
        )
        .await
        .unwrap();
        assert_eq!(again, None);
        assert_eq!(
            db::list_enrolled_bans(&database, enrollment, 10)
                .await
                .unwrap()
                .iter()
                .filter(|row| row.active)
                .count(),
            1
        );
        drop(database);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// A Discord call already inside its rate-limit wait is cancelled by cutoff.
    /// The ban settles as uncertain: no bans row and no report. The next holder
    /// records that ban once from Discord's view.
    #[tokio::test(flavor = "multi_thread")]
    async fn cutoff_during_discord_wait_cancels_the_ban_and_the_next_holder_records_once() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll};

        struct Stuck {
            polled: Arc<AtomicBool>,
            dropped: Arc<AtomicBool>,
            started: Arc<tokio::sync::Notify>,
        }

        impl Future for Stuck {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                self.polled.store(true, Ordering::SeqCst);
                self.started.notify_waiters();
                Poll::Pending
            }
        }

        impl Drop for Stuck {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let activation = crate::activation::Activation::new();
        activation.mark_lock_held(true);
        let polled = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let notified = started.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let task = tokio::spawn({
            let activation = Arc::clone(&activation);
            let polled = Arc::clone(&polled);
            let dropped = Arc::clone(&dropped);
            let started = Arc::clone(&started);
            async move {
                crate::discord::race_discord_call(
                    &activation,
                    Stuck {
                        polled,
                        dropped,
                        started,
                    },
                )
                .await
            }
        });
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                panic!("the discord future must be polled while it is still pending");
            }
            _ = &mut notified => {}
        }
        assert!(polled.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        activation.cut_off();
        let raced = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cutoff must cancel the discord future")
            .unwrap();
        assert!(matches!(raced, crate::discord::DiscordCall::Cancelled));
        assert!(
            dropped.load(Ordering::SeqCst),
            "dropping the future is the cancellation"
        );

        let (_tmp, app, enrollment) = open_bot().await;
        let (store, scope) = ready_directory(&app, &enrollment).await;
        let pending = begin_ban_attempt(&app, GUILD as u64, USER as u64).await;
        assert_eq!(pending.save, MarkerSave::Saved);
        pending.finish(&app, CaseOutcome::Uncertain).await;
        let settled = recorded(&app, &store, &scope).await;
        assert_eq!(
            settled.bans, 0,
            "a cancelled ban must not insert a bans row"
        );
        assert_eq!(
            settled.mod_logs, 0,
            "a cancelled ban must not publish a report"
        );
        assert_eq!(settled.cases, 0);
        let now_ms = chrono::Utc::now().timestamp_millis();
        assert!(
            store
                .next_member_pending_ban_verification(scope.clone(), now_ms.saturating_add(400_000))
                .await
                .unwrap()
                .is_some(),
            "uncertain settlement leaves the pending marker"
        );
        assert_eq!(
            ban_continuation(
                true,
                MarkerSave::Saved,
                DiscordBanView::Present { reason: None },
                false
            ),
            BanContinuation::RecordOnly { reason: None }
        );
        let inserted = db::record_enrolled_ban_if_absent(
            &app.db,
            Some(enrollment.clone()),
            USER,
            None,
            None,
            Some(7),
            None,
            "handoff",
            None,
            db::BanRecord::Retry,
            None,
        )
        .await
        .unwrap();
        assert!(inserted.is_some());
        db::insert_enrolled_event(
            &app.db,
            Some(enrollment.clone()),
            "mod_action",
            Some(USER),
            Some(7),
            &serde_json::json!({"action": "ban"}),
        )
        .await
        .unwrap();
        let again = db::record_enrolled_ban_if_absent(
            &app.db,
            Some(enrollment.clone()),
            USER,
            None,
            None,
            Some(7),
            None,
            "handoff",
            None,
            db::BanRecord::Retry,
            None,
        )
        .await
        .unwrap();
        assert!(again.is_none(), "the same dispatch must not publish again");
        let reconciled = recorded(&app, &store, &scope).await;
        assert_eq!(reconciled.bans, 1);
        assert_eq!(reconciled.mod_logs, 1);
    }
}
