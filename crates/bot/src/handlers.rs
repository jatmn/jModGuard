//! Discord gateway event handlers: basic member/message reports,
//! automod pipeline, and command dispatch.

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::gateway::ActivityData;
use serenity::http::{LightMethod, RatelimitInfo};
use serenity::model::channel::{GuildChannel, Message, PartialGuildChannel};
use serenity::model::event::{GuildMemberUpdateEvent, MessageUpdateEvent, ResumedEvent};
use serenity::model::gateway::Ready;
use serenity::model::guild::{Guild, Member, Role, UnavailableGuild};
use serenity::model::id::{GuildId, MessageId, RoleId};
use serenity::model::prelude::{ChannelId, User};
use serenity::model::voice::VoiceState;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::automod::{MsgCtx, execute_pending};
use crate::commands;
use crate::db;
use crate::logging;
use crate::state::BotState;
use crate::templates;

pub struct Handler {
    pub app: Arc<BotState>,
}

impl Handler {
    fn app(&self) -> &Arc<BotState> {
        &self.app
    }

    // Serenity spawns this hook and does not await it before sleeping. The sleep
    // covers both a real provider throttle and a pre-emptive empty bucket. This
    // function awaits one pacing write attempt before it returns; the log sender
    // only schedules the coalesced persist task. A late commit cannot shorten
    // `request_after_ms` because the SQL keeps the max.
    #[allow(dead_code)]
    pub(crate) async fn record_rate_limit_wait(&self, wait: RateLimitWait) {
        apply_rate_limit_wait(&self.app, wait).await;
    }
}

pub(crate) fn observe_member_sync_rate_limit_wait(app: &Arc<BotState>, wait: RateLimitWait) {
    let _logged = note_rate_limit_wait(app, &wait, Instant::now());
    if !member_sync_rate_limit_route(wait.global, &redact_rate_limit_path(&wait.path)) {
        return;
    }
    let deadline = member_sync_deadline_ms(chrono::Utc::now().timestamp_millis(), wait.timeout);
    app.arm_member_sync_rate_limit(deadline);
    app.ensure_member_sync_rate_limit_persist_task();
}

pub(crate) async fn apply_rate_limit_wait(app: &Arc<BotState>, wait: RateLimitWait) {
    observe_member_sync_rate_limit_wait(app, wait);
    app.persist_pending_member_sync_rate_limit().await;
}

#[derive(Clone)]
pub(crate) struct RateLimitWait {
    pub timeout: Duration,
    #[allow(dead_code)]
    pub limit: i64,
    pub method: LightMethod,
    pub path: String,
    pub global: bool,
}

pub(crate) fn member_sync_deadline_ms(now_ms: i64, timeout: Duration) -> i64 {
    now_ms.saturating_add(timeout.as_millis().min(i64::MAX as u128) as i64)
}

pub(crate) fn member_sync_rate_limit_route(global: bool, path: &str) -> bool {
    global || path.contains("/members") || path.contains("/bans") || path.contains("/audit-logs")
}

fn scrub_rate_limit_query(query: &str, token: &str) -> String {
    query
        .split('&')
        .map(|piece| {
            if let Some((key, value)) = piece.split_once('=') {
                let value = if value == token { "{token}" } else { value };
                format!("{key}={value}")
            } else if piece == token {
                "{token}".to_string()
            } else {
                piece.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn redact_rate_limit_path(path: &str) -> String {
    let (route_plus_query, fragment_opt) = match path.split_once('#') {
        Some((route_plus_query, fragment)) => (route_plus_query, Some(fragment)),
        None => (path, None),
    };
    let (route, query_opt) = match route_plus_query.split_once('?') {
        Some((route, query)) => (route, Some(query)),
        None => (route_plus_query, None),
    };

    let (origin, path_part) = if let Some(rest) = route.strip_prefix("https://") {
        match rest.find('/') {
            Some(slash_idx) => (
                route[..8 + slash_idx].to_string(),
                route[8 + slash_idx..].to_string(),
            ),
            None => return path.to_string(),
        }
    } else if let Some(rest) = route.strip_prefix("http://") {
        match rest.find('/') {
            Some(slash_idx) => (
                route[..7 + slash_idx].to_string(),
                route[7 + slash_idx..].to_string(),
            ),
            None => return path.to_string(),
        }
    } else {
        (String::new(), route.to_string())
    };

    if !path_part.starts_with('/') {
        return path.to_string();
    }

    let segments: Vec<&str> = path_part.split('/').collect();
    let mut token_segment_idx: Option<usize> = None;
    let mut original_token: Option<&str> = None;
    let mut idx = 0;
    while idx < segments.len() {
        if segments[idx] == "webhooks" {
            let candidate = idx + 2;
            if candidate < segments.len() && !segments[candidate].is_empty() {
                token_segment_idx = Some(candidate);
                original_token = Some(segments[candidate]);
                break;
            }
        } else if segments[idx] == "interactions" {
            let token_candidate = idx + 2;
            let callback_candidate = idx + 3;
            if callback_candidate < segments.len()
                && segments[callback_candidate] == "callback"
                && token_candidate < segments.len()
                && !segments[token_candidate].is_empty()
            {
                token_segment_idx = Some(token_candidate);
                original_token = Some(segments[token_candidate]);
                break;
            }
        }
        idx += 1;
    }

    let Some(token_segment_idx) = token_segment_idx else {
        return path.to_string();
    };
    let token = original_token.unwrap();

    let mut redacted_segments = segments;
    redacted_segments[token_segment_idx] = "{token}";
    let redacted_path = redacted_segments.join("/");

    let mut result = format!("{origin}{redacted_path}");
    if let Some(query) = query_opt {
        result.push('?');
        result.push_str(&scrub_rate_limit_query(query, token));
    }
    if let Some(fragment) = fragment_opt {
        result.push('#');
        if fragment == token {
            result.push_str("{token}");
        } else {
            result.push_str(fragment);
        }
    }
    result
}

pub(crate) fn rate_limit_throttle_key(
    method: LightMethod,
    global: bool,
    redacted_path: &str,
) -> String {
    let scope = if global { "global" } else { "route" };
    format!(
        "{method:?} {scope} {}",
        normalize_rate_limit_route(redacted_path)
    )
}

fn normalize_rate_limit_route(path: &str) -> String {
    let without_fragment = path.split_once('#').map(|(head, _)| head).unwrap_or(path);
    let route = without_fragment
        .split_once('?')
        .map(|(head, _)| head)
        .unwrap_or(without_fragment);
    let mut out = String::with_capacity(route.len());
    for (index, segment) in route.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) {
            out.push_str("{id}");
        } else {
            out.push_str(segment);
        }
    }
    out
}

pub(crate) fn note_rate_limit_wait(
    app: &crate::state::BotState,
    wait: &RateLimitWait,
    now: Instant,
) -> bool {
    let path = redact_rate_limit_path(&wait.path);
    let key = rate_limit_throttle_key(wait.method, wait.global, &path);
    let mut logged_at = app.rate_limit_wait_logged_at.lock();
    logged_at.retain(|_, logged| now.saturating_duration_since(*logged) < Duration::from_secs(60));
    if logged_at.contains_key(&key) {
        return false;
    }
    logged_at.insert(key, now);
    tracing::warn!(
        method = ?wait.method,
        path = %path,
        global = wait.global,
        timeout_ms = u64::try_from(wait.timeout.as_millis()).unwrap_or(u64::MAX),
        "rate-limit wait"
    );
    true
}

pub(crate) fn clear_remembered_if_bot_roles_changed(
    app: &BotState,
    guild_id: u64,
    bot_user_id: serenity::model::id::UserId,
    member_user_id: serenity::model::id::UserId,
    old_roles: Option<&[RoleId]>,
    new_roles: &[RoleId],
) {
    if member_user_id != bot_user_id {
        return;
    }
    use std::collections::HashSet;
    let old: HashSet<u64> = old_roles
        .map(|roles| roles.iter().map(|role| role.get()).collect())
        .unwrap_or_default();
    let new_roles: HashSet<u64> = new_roles.iter().map(|role| role.get()).collect();
    if old != new_roles {
        app.remembered_discord_targets.clear_guild(guild_id);
    }
}

pub(crate) fn clear_remembered_if_subject_roles_changed(
    app: &BotState,
    guild_id: u64,
    user_id: u64,
    old_roles: Option<&[RoleId]>,
    new_roles: &[RoleId],
) {
    use std::collections::HashSet;
    let roles_changed = match old_roles {
        None => true,
        Some(old_roles) => {
            let old: HashSet<u64> = old_roles.iter().map(|role| role.get()).collect();
            let new_roles: HashSet<u64> = new_roles.iter().map(|role| role.get()).collect();
            old != new_roles
        }
    };
    if roles_changed {
        app.remembered_discord_targets
            .clear_member_moderation_rejections(guild_id, user_id);
    }
}

async fn emit_activity(
    app: &Arc<BotState>,
    ctx: &Context,
    guild_id: GuildId,
    reports: Vec<crate::activity_log::ActivityReport>,
) {
    if guild_id.get() == 0 {
        return;
    }
    for report in reports {
        logging::emit(app, ctx, guild_id, report.event, report.vars).await;
    }
}

fn log_message_diagnostic(msg: &Message, guild_id: GuildId) {
    tracing::debug!(
        "message {} in guild {} channel {}: {} bytes, {} attachments",
        msg.id,
        guild_id,
        msg.channel_id,
        msg.content.len(),
        msg.attachments.len()
    );
}

fn user_display(member: &Member) -> String {
    member.display_name().to_string()
}

fn audit_case_action(
    action: serenity::model::guild::audit_log::Action,
) -> Option<crate::db::guild_data::CaseAction> {
    use serenity::model::guild::audit_log::{Action, MemberAction};
    match action {
        Action::Member(MemberAction::Kick) => Some(crate::db::guild_data::CaseAction::Kick),
        Action::Member(MemberAction::BanAdd) => Some(crate::db::guild_data::CaseAction::Ban),
        Action::Member(MemberAction::BanRemove) => Some(crate::db::guild_data::CaseAction::Unban),
        _ => None,
    }
}

pub(crate) fn record_unresolved_gateway_observation(suppressed_by_owned_action: bool) -> bool {
    // Directory state is only a snapshot. Equal saved state does not identify
    // this delivered action after a missed opposite Gateway event, so recording
    // depends only on whether this delivery is the bot's own suppressed echo.
    !suppressed_by_owned_action
}

/// Audit attribution belongs only to the row this delivery inserted.
/// An existing bot row keeps the actor and reason the command already stored.
fn gateway_ban_row_to_attribute(write: db::GatewayBanWrite) -> Option<i64> {
    match write {
        db::GatewayBanWrite::Recorded { row_id } => Some(row_id),
        db::GatewayBanWrite::OwnedByBot { .. } | db::GatewayBanWrite::Existing { .. } => None,
    }
}

pub(crate) async fn persist_gateway_ban_observation(
    app: &BotState,
    enrollment: Option<db::GuildEnrollment>,
    user: u64,
    mut in_flight: crate::state::GatewayBanObservation,
    owned_by_bot: bool,
    write: impl std::future::Future<Output = anyhow::Result<db::GatewayBanWrite>> + Send + 'static,
) -> anyhow::Result<db::GatewayBanWrite> {
    let observed_at_ms = in_flight.observed.timestamp_millis();
    // Cancellation must retry with this captured membership, never a later one.
    in_flight.enrollment = enrollment.clone();
    // Keep the write future and its observation in one task. Cancelling the
    // gateway callback must not discard a result that SQL may already commit.
    let persistence = tokio::spawn(async move {
        let result = write.await;
        in_flight.finish().await;
        result
    });
    let result = persistence
        .await
        .map_err(|error| anyhow::anyhow!("gateway ban persistence task failed: {error}"))?;
    // finish retries queued audits independently of this callback's lifetime.
    if !owned_by_bot && let Ok(db::GatewayBanWrite::Existing { row_id }) = result {
        // Another audit callback may already have split and acknowledged this
        // observation. Resolve persisted lineage/time, not the last queue entry
        // or whichever ban happens to be active now.
        match db::gateway_ban_split_for_observation(
            &app.db,
            enrollment,
            user as i64,
            row_id,
            observed_at_ms,
        )
        .await
        {
            Ok(Some(row_id)) => return Ok(db::GatewayBanWrite::Recorded { row_id }),
            Ok(None) => {}
            Err(error) => tracing::warn!("gateway ban split lookup failed: {error:#}"),
        }
    }
    result
}

// Enqueue and reconcile with the enrollment captured by this audit callback.
pub(crate) async fn reconcile_received_gateway_ban_audit(
    app: &Arc<BotState>,
    enrollment: Option<db::GuildEnrollment>,
    guild: u64,
    user: u64,
    entry_id: i64,
    attribution: db::GatewayBanAuditAttribution,
) -> bool {
    app.queue_attributed_gateway_manual_ban_audit_entry(guild, user, entry_id, attribution);
    let app = Arc::clone(app);
    let reconcile = tokio::spawn(async move {
        let matched = app
            .reconcile_gateway_manual_ban_audit_entry(enrollment, guild, user, entry_id)
            .await?;
        if matched {
            app.drop_gateway_manual_ban_audit_entry(guild, user, entry_id);
        }
        Ok::<_, anyhow::Error>(matched)
    });
    match reconcile.await {
        Ok(Ok(matched)) => matched,
        Ok(Err(error)) => {
            tracing::warn!("gateway manual ban audit reconcile deferred: {error:#}");
            false
        }
        Err(error) => {
            tracing::warn!("gateway manual ban audit task failed: {error:#}");
            false
        }
    }
}

// Pending entries survive transient SQL errors and duplicate audit deliveries.
// Successful reconciliation (including an already persisted ID) is the only ack.
pub(crate) async fn reconcile_pending_gateway_ban_audits(
    app: &BotState,
    enrollment: Option<db::GuildEnrollment>,
    guild: u64,
    user: u64,
) {
    for entry_id in app.pending_gateway_manual_ban_audit_entries(guild, user) {
        match app
            .reconcile_gateway_manual_ban_audit_entry(enrollment.clone(), guild, user, entry_id)
            .await
        {
            Ok(true) => app.drop_gateway_manual_ban_audit_entry(guild, user, entry_id),
            Ok(false) => {}
            Err(error) => tracing::warn!("gateway manual ban audit reconcile failed: {error:#}"),
        }
    }
}

/// Member-directory precheck for a gateway ban must finish before the ban observation write.
pub(crate) async fn gateway_ban_directory_precheck_then_observe(
    app: &Arc<BotState>,
    guild_id: u64,
    user: &User,
    observed_at_ms: i64,
) -> bool {
    let member_was_banned =
        match crate::member_directory::member_directory_shows_banned(app, guild_id, user.id.get())
            .await
        {
            Some(was_banned) => was_banned,
            None => {
                tracing::warn!(
                    guild_id,
                    user_id = user.id.get(),
                    "ban mirror precheck unavailable; assuming still banned"
                );
                true
            }
        };
    crate::member_directory::observe_ban(app, guild_id, user, observed_at_ms).await;
    member_was_banned
}

fn departure_joined_at_ms(
    decision: Option<&crate::db::guild_data::MemberDepartureDecision>,
    payload_joined_at_ms: Option<i64>,
    observed_joined_at_ms: Option<i64>,
) -> Option<i64> {
    match decision {
        Some(crate::db::guild_data::MemberDepartureDecision::NeedsVerification) => None,
        Some(crate::db::guild_data::MemberDepartureDecision::Applied(departure)) => {
            // A successful persistence decision is authoritative, including unknown stays.
            departure
                .as_ref()
                .and_then(|departure| departure.joined_at_ms)
        }
        None => payload_joined_at_ms.or(observed_joined_at_ms),
    }
}

fn select_update_authors(
    event_author: Option<u64>,
    text_author: Option<u64>,
    image_author: Option<u64>,
    snippet_author: Option<u64>,
    snippet_webhook: bool,
    edited: bool,
) -> (Option<u64>, Option<u64>) {
    let detector_author = event_author.or(text_author).or(image_author);
    let known_author = detector_author.or(snippet_author);
    // A cached author identifies a message for logging, but an untimestamped
    // refresh cannot turn an untracked old message into a new spam candidate.
    let update_author = if edited && !snippet_webhook {
        known_author
    } else {
        detector_author
    };
    (known_author, update_author)
}

pub(crate) struct UpdateObservation {
    pub author: Option<u64>,
    pub received: std::time::Instant,
    pub received_at: chrono::DateTime<chrono::Utc>,
}

/// Throttle for the "message content is empty" warning (missing intent).
static LAST_EMPTY_WARN: parking_lot::Mutex<Option<std::time::Instant>> =
    parking_lot::Mutex::new(None);

fn warn_empty_content_once() {
    let mut last = LAST_EMPTY_WARN.lock();
    let now = std::time::Instant::now();
    if last
        .map(|t| now.duration_since(t) > std::time::Duration::from_secs(120))
        .unwrap_or(true)
    {
        *last = Some(now);
        tracing::warn!(
            "received messages with empty content. If commands/automod do not react, \
             enable the Server Members and Message Content intents in the Discord \
             developer portal (Bot -> Privileged Gateway Intents) and restart the bot."
        );
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ratelimit(&self, data: RatelimitInfo) {
        self.record_rate_limit_wait(RateLimitWait {
            timeout: data.timeout,
            limit: data.limit,
            method: data.method,
            path: data.path,
            global: data.global,
        })
        .await;
    }

    async fn resume(&self, _ctx: Context, _event: ResumedEvent) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        self.app.snapshots.install_collector(&self.app, &_ctx);
        self.app.snapshots.gap();
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        self.app.activation.note_discord_connected();
        self.app.activation.publish_process_ready();
        if !self.app.activation.allows_started_notice() {
            tracing::info!("discord connected before this bot was ready; not posting started");
            return;
        }
        self.app.note_shard_ready(ctx.shard_id.0);
        self.app.snapshots.install_collector(&self.app, &ctx);
        self.app.snapshots.gap();
        tracing::info!(
            "logged in as {} ({} guilds, shard {})",
            ready.user.name,
            ready.guilds.len(),
            ctx.shard_id
        );
        ctx.set_presence(
            Some(ActivityData::playing(&self.app.cfg.bot.status)),
            serenity::model::user::OnlineStatus::Online,
        );
        // The launcher must not accept a replacement before Discord has sent
        // Ready; constructing the client does not make commands available.
        if let Err(error) = jmodguard::mark_child_ready() {
            tracing::error!("failed to signal bot readiness: {error}");
        }

        // Per-shard startup: seed + sync each guild once.
        if !self
            .app
            .slash_synced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            let app = Arc::clone(self.app());
            let refresh_app = Arc::clone(&app);
            let refresh_cache = Arc::clone(&ctx.cache);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    for guild in refresh_cache.guilds() {
                        refresh_app
                            .snapshots
                            .refresh_scope(&refresh_app, guild.get())
                            .await;
                    }
                }
            });
            let maintenance_app = Arc::clone(&app);
            tokio::spawn(async move {
                loop {
                    let delay = maintenance_app.snapshots.maintain(&maintenance_app).await;
                    tokio::time::sleep(delay).await;
                }
            });
            let startup = async move {
                let guild_ids: Vec<GuildId> = ctx.cache.guilds();
                tracing::info!(
                    "connected as {} — {} guilds; commands are live. If commands do not respond, \
                     verify the Server Members + Message Content intents in the developer portal \
                     and restart.",
                    ctx.cache.current_user().name,
                    guild_ids.len()
                );
                let mut failed = false;
                for gid in guild_ids {
                    app.snapshots.refresh_scope(&app, gid.get()).await;
                    // Joining/ready does not insert guild_settings — inheritance
                    // from global must remain unblocked until an operator sets
                    // an intentional override.
                    if !commands::sync_slash_commands(&app, &ctx, gid).await {
                        failed = true;
                    }
                    logging::emit_system_notice(
                        &app,
                        &ctx.http,
                        gid,
                        format!("Bot started (v{})", env!("CARGO_PKG_VERSION")),
                    )
                    .await;
                }
                if failed {
                    app.retry_slash_registry_version();
                }
                tracing::info!("startup slash sync complete");
            };
            self.app.spawn_startup_notice_task(startup).await;
        }
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        self.app.member_joins.clear_guild(guild.id.get());
        let app = Arc::clone(self.app());
        let gid = guild.id;
        match crate::enrollment_repair::observe_gateway_membership(
            &app.db,
            &app.tenant,
            gid.get(),
            guild.joined_at.timestamp_nanos_opt(),
            guild.unavailable,
            true,
        )
        .await
        {
            Ok(()) => {
                app.snapshots.refresh_scope(&app, gid.get()).await;
                crate::cases::retry_deferred_discord_audit_links(&app, Some(gid.get())).await;
            }
            Err(error) => {
                tracing::warn!(
                    guild_id = gid.get(),
                    "guild membership observation deferred: {error:#}"
                );
            }
        }
        // Rejoin permits another attempt for a parked guild, never for a bad token.
        app.reset_slash_guild_retry(gid.get());
        app.remembered_discord_targets.clear_guild(gid.get());
        // No guild_settings seed on join — global inheritance stays unblocked.
        if !commands::sync_slash_commands(&app, &ctx, gid).await {
            app.retry_slash_registry_version();
        }

        if is_new.unwrap_or(false) {
            tracing::info!("joined new guild {gid} ({})", guild.name);
        }
        self.app.invalidate_member_sync_scope();
    }

    async fn guild_delete(&self, _ctx: Context, incomplete: UnavailableGuild, full: Option<Guild>) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        self.app.invalidate_member_sync_scope();
        if !incomplete.unavailable {
            self.app.forget_slash_guild(incomplete.id.get()).await;
        }
        self.app.snapshots.clear_guild(incomplete.id.get());
        self.app.member_joins.clear_guild(incomplete.id.get());
        self.app.image_spam.invalidate_guild(incomplete.id.get());
        self.app.text_spam.invalidate_guild(incomplete.id.get());
        if incomplete.unavailable {
            crate::member_directory::mark_guild_gap(
                self.app(),
                incomplete.id.get(),
                "guild gateway unavailable",
            )
            .await;
        }
        let joined = full
            .filter(|guild| guild.id == incomplete.id)
            .and_then(|guild| guild.joined_at.timestamp_nanos_opt());
        if let Err(error) = crate::enrollment_repair::observe_gateway_membership(
            &self.app().db,
            &self.app().tenant,
            incomplete.id.get(),
            joined,
            incomplete.unavailable,
            false,
        )
        .await
        {
            tracing::warn!(
                guild_id = incomplete.id.get(),
                "guild retirement lacks confirmed membership or database access: {error:#}"
            );
        }
    }

    async fn guild_member_addition(&self, ctx: Context, new_member: Member) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let app = Arc::clone(self.app());
        let guild_id = new_member.guild_id;
        let uid = new_member.user.id.get();
        app.member_joins.take(guild_id.get(), uid);
        app.automod.member_joined(guild_id.get(), uid);
        crate::member_directory::observe_join(
            &app,
            &new_member,
            chrono::Utc::now().timestamp_millis(),
        )
        .await;
        logging::emit_member_event(
            &app,
            &ctx,
            guild_id,
            "member_join",
            &new_member.user,
            logging::member_join_vars(new_member.joined_at, chrono::Utc::now()),
            None,
        )
        .await;
    }

    async fn guild_member_removal(
        &self,
        ctx: Context,
        guild_id: GuildId,
        user: User,
        member_data: Option<Member>,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        self.app
            .image_spam
            .invalidate_author(guild_id.get(), user.id.get());
        self.app
            .text_spam
            .invalidate_author(guild_id.get(), user.id.get());
        let app = Arc::clone(self.app());
        let observed_join = app.member_joins.take(guild_id.get(), user.id.get());
        let departed_at = chrono::Utc::now();
        let expected_joined_at_ms = member_data
            .as_ref()
            .and_then(|member| member.joined_at)
            .map(|joined| joined.timestamp_millis());
        let departed = crate::member_directory::observe_departure(
            &app,
            guild_id.get(),
            &user,
            departed_at.timestamp_millis(),
            expected_joined_at_ms,
        )
        .await;
        if matches!(
            departed,
            Some(crate::db::guild_data::MemberDepartureDecision::NeedsVerification)
        ) {
            // A paced target check will either discard this stale remove or
            // emit the leave with the saved duration once absence is proven.
            return;
        }
        let joined_at_ms = departure_joined_at_ms(
            departed.as_ref(),
            expected_joined_at_ms,
            observed_join.map(|joined| joined.timestamp_millis()),
        );
        let extra = logging::member_join_vars_ms(joined_at_ms, departed_at);
        logging::emit_member_event(&app, &ctx, guild_id, "member_leave", &user, extra, None).await;
    }

    async fn guild_ban_addition(&self, ctx: Context, guild_id: GuildId, banned_user: User) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        // Capture ownership before any await: an unban can clear remembered
        // ownership while this callback waits, but this already-started echo keeps
        // its own identity without restoring that state. Its dispatch can still
        // revoke ownership on a definitive failure before persistence.
        let echo = self
            .app
            .consume_gateway_ban_echo(guild_id.get(), banned_user.id.get());
        let _rule_hold = self.app.activation.hold_rule();
        let in_flight = self
            .app
            .begin_gateway_ban_observation(guild_id.get(), banned_user.id.get())
            .await;
        let observed = in_flight.observed;
        let observation = self.app.member_audits.observe_event(
            guild_id.get(),
            banned_user.id.get(),
            "member_ban",
            observed.timestamp_millis(),
        );
        let app = Arc::clone(self.app());
        let joined = ctx
            .cache
            .guild(guild_id)
            .and_then(|guild| guild.joined_at.timestamp_nanos_opt());
        let observed_at = observed.to_rfc3339();
        // The detached owner receives the actual payload before any SQL await.
        // It retries storage; the durable inbox completes partial work on restart.
        let persistence = crate::gateway_ban_persistence::receive_ban(
            Arc::clone(&app),
            in_flight,
            guild_id.get(),
            banned_user.clone(),
            echo,
            joined,
        );
        let (enrollment, ban_row_id) = match persistence.await {
            Ok(Ok((enrollment, write))) => (
                Some(enrollment),
                write.and_then(gateway_ban_row_to_attribute),
            ),
            Ok(Err(error)) => {
                tracing::warn!("gateway ban persistence deferred or membership retired: {error:#}");
                (None, None)
            }
            Err(error) => {
                tracing::error!("gateway ban persistence owner failed: {error:#}");
                (None, None)
            }
        };
        // Resolve the audit context even when no dedicated ban log channel is
        // configured, so the WebUI can retain the known moderator and reason.
        let audit = if let (Some(observation), Ok(_permit)) =
            (observation.as_ref(), app.log_delivery_slots.try_acquire())
        {
            logging::lookup_member_audit(&app, ctx.http.as_ref(), guild_id, observation).await
        } else {
            None
        };
        if let (Some(row_id), Some(context)) = (ban_row_id, audit.as_ref()) {
            let moderator_id = context
                .get("actor_id")
                .and_then(|id| id.parse::<i64>().ok());
            if let Some(moderator_id) = moderator_id {
                let _ = db::attribute_enrolled_ban(
                    &app.db,
                    enrollment.clone(),
                    row_id,
                    banned_user.id.get() as i64,
                    moderator_id,
                    context.get("actor_name").map(String::as_str),
                    context
                        .get("reason")
                        .map(String::as_str)
                        .unwrap_or("No reason provided"),
                )
                .await;
            }
        }
        let mut extra = audit.unwrap_or_default();
        extra.insert("time".into(), observed_at);
        let late_audit = logging::emit_member_event(
            &app,
            &ctx,
            guild_id,
            "member_ban",
            &banned_user,
            extra,
            observation,
        )
        .await;
        // If the audit entry appeared during the report's bounded retry, keep
        // the displayed ban row consistent with the report that used it.
        if let (Some(row_id), Some(context)) = (ban_row_id, late_audit) {
            let moderator_id = context
                .get("actor_id")
                .and_then(|id| id.parse::<i64>().ok());
            if let Some(moderator_id) = moderator_id {
                let _ = db::attribute_enrolled_ban(
                    &app.db,
                    enrollment,
                    row_id,
                    banned_user.id.get() as i64,
                    moderator_id,
                    context.get("actor_name").map(String::as_str),
                    context
                        .get("reason")
                        .map(String::as_str)
                        .unwrap_or("No reason provided"),
                )
                .await;
            }
        }
    }

    async fn guild_ban_removal(&self, ctx: Context, guild_id: GuildId, unbanned_user: User) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        // A ban after this unban is a new ban, even inside the echo window.
        self.app
            .clear_gateway_ban_echo(guild_id.get(), unbanned_user.id.get());
        let _rule_hold = self.app.activation.hold_rule();
        let observed = chrono::Utc::now();
        let observation = self.app.member_audits.observe_event(
            guild_id.get(),
            unbanned_user.id.get(),
            "member_unban",
            observed.timestamp_millis(),
        );
        let app = Arc::clone(self.app());
        let joined = ctx
            .cache
            .guild(guild_id)
            .and_then(|guild| guild.joined_at.timestamp_nanos_opt());
        let suppressed = app.consume_gateway_case_suppression(
            crate::state::GatewayCaseAction::Unban,
            guild_id.get(),
            unbanned_user.id.get(),
        );
        let persistence = crate::gateway_ban_persistence::receive_unban(
            Arc::clone(&app),
            guild_id.get(),
            unbanned_user.id.get(),
            observed,
            joined,
            suppressed,
            None,
        );
        crate::member_directory::observe_unban(
            &app,
            guild_id.get(),
            &unbanned_user,
            observed.timestamp_millis(),
        )
        .await;
        match persistence.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("gateway unban persistence deferred: {error:#}"),
            Err(error) => tracing::error!("gateway unban persistence owner failed: {error:#}"),
        }
        let mut extra = BTreeMap::new();
        extra.insert("time".into(), observed.to_rfc3339());
        logging::emit_member_event(
            &app,
            &ctx,
            guild_id,
            "member_unban",
            &unbanned_user,
            extra,
            observation,
        )
        .await;
    }

    async fn guild_audit_log_entry_create(
        &self,
        ctx: Context,
        entry: serenity::model::guild::audit_log::AuditLogEntry,
        guild_id: GuildId,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let Some(action) = audit_case_action(entry.action) else {
            return;
        };
        let Some(target) = entry.target_id else {
            return;
        };
        let Ok(entry_id) = i64::try_from(entry.id.get()) else {
            return;
        };
        let bot_user = ctx.cache.current_user().id.get();
        if entry.user_id.get() != bot_user
            && matches!(
                action,
                crate::db::guild_data::CaseAction::Ban | crate::db::guild_data::CaseAction::Unban
            )
        {
            if matches!(action, crate::db::guild_data::CaseAction::Ban) {
                let guild = guild_id.get();
                let user = target.get();
                let joined = ctx
                    .cache
                    .guild(guild_id)
                    .and_then(|guild| guild.joined_at.timestamp_nanos_opt());
                let persistence = crate::gateway_ban_persistence::receive_audit(
                    Arc::clone(&self.app),
                    guild,
                    user,
                    entry_id,
                    db::GatewayBanAuditAttribution {
                        moderator_id: entry.user_id.get() as i64,
                        moderator_name: ctx.cache.user(entry.user_id).map(|user| user.name.clone()),
                        reason: entry
                            .reason
                            .as_deref()
                            .map(jmodguard::ban_reason::without_tag)
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or("No reason provided")
                            .to_owned(),
                    },
                    entry
                        .reason
                        .as_ref()
                        .map(|reason| jmodguard::ban_reason::without_tag(reason).to_owned()),
                    joined,
                    None,
                );
                match persistence.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!("gateway ban audit persistence deferred: {error:#}")
                    }
                    Err(error) => {
                        tracing::error!("gateway ban audit persistence owner failed: {error:#}")
                    }
                }
            }
            // The audit snowflake is the durable action identity. A repeat ban
            // or unban has a different ID; a replay of this entry is idempotent.
            if matches!(action, crate::db::guild_data::CaseAction::Unban)
                && let Some(enrollment) = crate::cases::capture(self.app(), guild_id.get()).await
            {
                let data = crate::db::guild_data::CaseData {
                    ban_attempt_tag: None,
                    source: crate::db::guild_data::CaseSource::DiscordAudit { entry_id },
                    action: action.clone(),
                    outcome: crate::db::guild_data::CaseOutcome::Succeeded,
                    subject_id: target.get() as i64,
                    actor_id: i64::try_from(entry.user_id.get()).ok(),
                    occurred_at_ms: entry.id.created_at().timestamp_millis(),
                    reason: entry
                        .reason
                        .clone()
                        .map(|reason| jmodguard::ban_reason::without_tag(&reason).to_owned()),
                    evidence: crate::db::guild_data::CaseEvidence::Unavailable,
                };
                if crate::cases::record_data(&self.app().tenant, enrollment, data)
                    .await
                    .is_none()
                {
                    crate::member_directory::mark_user_gap(
                        self.app(),
                        guild_id.get(),
                        target.get() as i64,
                        "ban audit case persistence unavailable",
                    )
                    .await;
                }
            }
        }
        crate::cases::await_discord_audit(
            self.app(),
            bot_user,
            entry.user_id.get(),
            action,
            guild_id.get(),
            target.get(),
            entry_id,
        )
        .await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let received = std::time::Instant::now();
        let received_at = chrono::Utc::now();
        let image_revision = self.app.image_spam.revision();
        let text_revision = self.app.text_spam.revision();
        let app = Arc::clone(self.app());
        let Some(guild_id) = msg.guild_id else { return };
        if !app.snapshots.collector_active(&ctx) {
            app.snapshots.gap();
        }
        app.member_joins.remember(
            guild_id.get(),
            msg.author.id.get(),
            msg.member.as_ref().and_then(|member| member.joined_at),
        );

        // Capture the already-delivered identity before configuration loading
        // yields, so a fast partial edit can still apply its user-ID filter.
        crate::logging::remember_message_create(
            &app,
            guild_id.get(),
            msg.channel_id.get(),
            msg.id.get(),
            Some(msg.author.id.get()),
            Some(&msg.content),
            Some(
                &msg.attachments
                    .iter()
                    .map(|attachment| attachment.url.clone())
                    .collect::<Vec<_>>(),
            ),
            msg.webhook_id.is_some(),
        );

        if msg.content.is_empty() {
            warn_empty_content_once();
        }
        log_message_diagnostic(&msg, guild_id);

        app.automod.maybe_prune();

        let cfg = match app.configs.get(&app.db, guild_id.get()).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("message: config load failed: {e:#}");
                return;
            }
        };
        tracing::debug!(
            "config for guild {guild_id}: prefix={:?}, bot_name={:?}, {} commands, {} rules, {} templates",
            cfg.settings.prefix,
            cfg.settings.bot_name,
            cfg.commands.len(),
            cfg.rules.len(),
            cfg.templates.len()
        );

        crate::image_spam::observe(
            &app,
            &ctx,
            &cfg,
            &msg,
            received,
            received_at,
            image_revision,
        );
        crate::text_spam::observe(&app, &ctx, &cfg, &msg, received, received_at, text_revision);
        // Generic rules and prefix commands retain their existing bot exemption.
        if msg.author.bot {
            return;
        }

        // automod
        let member: Option<Member> = msg.member.clone().map(|m| Member::from(*m));
        let member_roles: Vec<u64> = msg
            .member
            .as_deref()
            .map(|partial| partial.roles.iter().map(|role| role.get()).collect())
            .unwrap_or_default();
        let joined_secs_ago = app
            .automod
            .member_joined_at(guild_id.get(), msg.author.id.get())
            .map(|t| t.elapsed().as_secs());
        let account_age_days = {
            let created = msg.author.created_at();
            let now = chrono::Utc::now().timestamp();
            (now - created.unix_timestamp()).max(0) as u64 / 86400
        };

        let (is_moderator, can_mention_everyone, mention_permission_unknown) = {
            let guild = ctx.cache.guild(guild_id);
            let scope = crate::commands::author_message_scope(
                app.cfg.bot.owner_id,
                guild.as_deref(),
                msg.channel_id,
                msg.author.id,
                msg.member.as_deref(),
            );
            (
                scope.is_moderator,
                scope.mention == crate::commands::MentionPermission::Allowed,
                scope.mention == crate::commands::MentionPermission::Unknown,
            )
        };

        let msg_ctx = MsgCtx {
            guild_id: guild_id.get(),
            message_id: msg.id.get(),
            channel_id: msg.channel_id.get(),
            author_id: msg.author.id.get(),
            content: &msg.content,
            attachment_count: msg.attachments.len(),
            everyone_mentions: usize::from(msg.mention_everyone),
            here_mentions: usize::from(msg.mention_everyone),
            role_mentions: msg.mention_roles.len(),
            joined_secs_ago,
            account_age_days: Some(account_age_days),
            member_roles: &member_roles,
            is_moderator,
            can_mention_everyone,
            mention_permission_unknown,
        };
        let mut stop_commands = false;
        if let Some(pending) = app.automod.evaluate(&cfg, &msg_ctx) {
            let display = member
                .as_ref()
                .map(user_display)
                .unwrap_or_else(|| msg.author.name.clone());
            execute_pending(
                &app,
                &ctx,
                guild_id,
                &msg.author,
                member.clone(),
                &display,
                Some(msg.id.get()),
                Some(msg.channel_id.get()),
                &pending,
            )
            .await;
            stop_commands = matches!(pending.action.action.as_str(), "ban" | "kick" | "timeout");
        }

        if !stop_commands {
            let cfg_arc = Arc::clone(&cfg);
            let _ = commands::handle_prefix(&app, &ctx.http, &ctx.cache, cfg_arc, &msg, Some(&ctx))
                .await;
        }
    }

    async fn message_update(
        &self,
        ctx: Context,
        _old: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let received = std::time::Instant::now();
        let received_at = chrono::Utc::now();
        let image_revision = self.app.image_spam.revision();
        let text_revision = self.app.text_spam.revision();
        let (known_author_id, detector_update_author_id) = event
            .guild_id
            .map(|guild| {
                let guild = guild.get();
                let channel = event.channel_id.get();
                let message = event.id.get();
                select_update_authors(
                    event.author.as_ref().map(|user| user.id.get()),
                    self.app.text_spam.author_of(guild, channel, message),
                    self.app.image_spam.author_of(guild, channel, message),
                    logging::known_message_author(&self.app, guild, channel, message),
                    logging::known_message_webhook(&self.app, guild, channel, message),
                    event.edited_timestamp.is_some(),
                )
            })
            .unwrap_or_default();
        logging::remember_message_update(&self.app, &event, known_author_id);
        let bot_user_id = ctx.cache.current_user().id.get();
        let hooked = self.app.snapshots.collector_active(&ctx);
        let retained_report = (!hooked)
            .then(|| {
                logging::message_update_report(&event, known_author_id, Some(bot_user_id))
                    .filter(|_| self.app.edit_reports.should_report(&event))
            })
            .flatten();
        if let Some(guild) = event.guild_id {
            if let Some(author) = known_author_id {
                self.app.member_joins.remember(
                    guild.get(),
                    author,
                    event
                        .member
                        .as_ref()
                        .and_then(|member| member.as_ref())
                        .and_then(|member| member.joined_at),
                );
            }
            let channel = event.channel_id.get();
            let message = event.id.get();
            let update = UpdateObservation {
                author: detector_update_author_id,
                received,
                received_at,
            };
            match self.app.configs.get(&self.app.db, guild.get()).await {
                Ok(cfg) => {
                    crate::image_spam::observe_update(
                        &self.app,
                        &ctx,
                        &cfg,
                        &event,
                        &update,
                        image_revision,
                    );
                    crate::text_spam::observe_update(
                        &self.app,
                        &ctx,
                        &cfg,
                        &event,
                        &update,
                        text_revision,
                    );
                }
                Err(e) => {
                    tracing::warn!("message_update: config load failed: {e:#}");
                    self.app
                        .image_spam
                        .invalidate(guild.get(), channel, message);
                    self.app.text_spam.invalidate(guild.get(), channel, message);
                }
            }
        }
        if !hooked && let Some(report) = retained_report {
            self.app.snapshots.gap();
            logging::emit(&self.app, &ctx, report.guild_id, report.event, report.vars).await;
        }
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let Some(guild_id) = guild_id else { return };
        let hooked = self.app.snapshots.collector_active(&ctx);
        if !hooked {
            self.app
                .snapshots
                .unhooked_delete(guild_id, channel_id, &[deleted_message_id]);
        }
        self.app
            .image_spam
            .deleted(guild_id.get(), channel_id.get(), deleted_message_id.get());
        self.app
            .text_spam
            .invalidate(guild_id.get(), channel_id.get(), deleted_message_id.get());
        if !hooked {
            let report =
                logging::message_delete_report(&self.app, guild_id, channel_id, deleted_message_id);
            logging::emit(&self.app, &ctx, guild_id, report.event, report.vars).await;
        }
    }

    async fn message_delete_bulk(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        multiple_deleted_messages_ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let Some(guild_id) = guild_id else { return };
        let hooked = self.app.snapshots.collector_active(&ctx);
        if !hooked {
            self.app.snapshots.unhooked_delete(
                guild_id,
                channel_id,
                &multiple_deleted_messages_ids,
            );
        }
        for id in &multiple_deleted_messages_ids {
            self.app
                .image_spam
                .invalidate(guild_id.get(), channel_id.get(), id.get());
            self.app
                .text_spam
                .invalidate(guild_id.get(), channel_id.get(), id.get());
        }
        if !hooked {
            let ids = crate::snapshot_capture::unique_ids(&multiple_deleted_messages_ids);
            let mut transcript = logging::BulkTranscript::new(ids.len());
            for id in ids {
                transcript.push_unknown(id);
            }
            logging::emit_retained_bulk(&self.app, &ctx, guild_id, channel_id, transcript).await;
        }
    }

    async fn guild_member_update(
        &self,
        ctx: Context,
        old_if_available: Option<Member>,
        new_if_available: Option<Member>,
        event: GuildMemberUpdateEvent,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        self.app
            .image_spam
            .invalidate_author(event.guild_id.get(), event.user.id.get());
        self.app
            .text_spam
            .invalidate_author(event.guild_id.get(), event.user.id.get());
        let app = Arc::clone(self.app());
        crate::member_directory::observe_update(
            &app,
            &event,
            chrono::Utc::now().timestamp_millis(),
        )
        .await;
        let Some(new) = new_if_available else { return };
        let guild_id = new.guild_id;
        clear_remembered_if_bot_roles_changed(
            &self.app,
            guild_id.get(),
            ctx.cache.current_user().id,
            new.user.id,
            old_if_available
                .as_ref()
                .map(|member| member.roles.as_slice()),
            &new.roles,
        );
        clear_remembered_if_subject_roles_changed(
            &self.app,
            guild_id.get(),
            new.user.id.get(),
            old_if_available
                .as_ref()
                .map(|member| member.roles.as_slice()),
            &new.roles,
        );
        let old_to = old_if_available
            .as_ref()
            .and_then(|member| member.communication_disabled_until);
        let new_to = new.communication_disabled_until;
        if old_to != new_to {
            let enrollment = crate::cases::capture(&app, guild_id.get()).await;
            let mut vars =
                templates::user_vars(new.user.id.get(), &new.user.name, new.display_name());
            if new_to.is_some() {
                vars.insert("duration".into(), "timed out".into());
                vars.insert("reason".into(), "moderation action".into());
                logging::emit(&app, &ctx, guild_id, "timeout_add", vars).await;
                crate::logging::log_persistence_result(
                    "timeout_add event",
                    db::insert_enrolled_event(
                        &app.db,
                        enrollment.clone(),
                        "timeout_add",
                        Some(new.user.id.get() as i64),
                        None,
                        &serde_json::json!({"until": new_to.map(|t| t.to_string())}),
                    )
                    .await,
                );
            } else {
                vars.insert("reason".into(), "timeout expired or removed".into());
                logging::emit(&app, &ctx, guild_id, "timeout_remove", vars).await;
                crate::logging::log_persistence_result(
                    "timeout_remove event",
                    db::insert_enrolled_event(
                        &app.db,
                        enrollment.clone(),
                        "timeout_remove",
                        Some(new.user.id.get() as i64),
                        None,
                        &serde_json::json!({}),
                    )
                    .await,
                );
            }
        }
        if let Some(old) = old_if_available {
            // Diff only. These reports do not write a row and do not wait
            // behind moderation: delivery uses the existing non-blocking slot.
            let reports = crate::activity_log::member_activity_reports(
                &crate::activity_log::MemberView::from_member(&old),
                &crate::activity_log::MemberView::from_member(&new),
            );
            emit_activity(&app, &ctx, guild_id, reports).await;

            let old_roles: std::collections::HashSet<u64> =
                old.roles.iter().map(|r| r.get()).collect();
            let added: Vec<u64> = new
                .roles
                .iter()
                .map(|r| r.get())
                .filter(|id| !old_roles.contains(id))
                .collect();
            if !added.is_empty() {
                let cfg = match app.configs.get(&app.db, guild_id.get()).await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let member_roles: Vec<u64> = new.roles.iter().map(|r| r.get()).collect();
                let (is_moderator, _) = ctx.cache.guild(guild_id).map_or((false, false), |g| {
                    let level = crate::commands::perm_level(&app, &g.roles, &new);
                    (level >= crate::commands::PermLevel::Mod, false)
                });
                if let Some((rule_id, action)) = jmodguard::custom_rules::honeypot_role_added(
                    &cfg.rules,
                    &added,
                    new.user.id.get(),
                    &member_roles,
                    is_moderator,
                ) {
                    let rule_name = cfg
                        .rules
                        .get(&rule_id)
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| rule_id.clone());
                    let pending = crate::automod::PendingAction {
                        rule_id,
                        rule_name,
                        action,
                        strikes: 1,
                        evidence: serde_json::json!({}),
                        mention_permission_unknown: false,
                    };
                    execute_pending(
                        &app,
                        &ctx,
                        guild_id,
                        &new.user,
                        Some(new.clone()),
                        new.display_name(),
                        None,
                        None,
                        &pending,
                    )
                    .await;
                }
            }
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let Some(guild_id) = new.guild_id else {
            return;
        };
        let member = crate::activity_log::MemberView::from_voice(&new);
        let old_channel = old
            .as_ref()
            .and_then(|state| state.channel_id.map(|id| id.get()));
        let new_channel = new.channel_id.map(|id| id.get());
        if let Some(report) = crate::activity_log::voice_report(&member, old_channel, new_channel) {
            emit_activity(&self.app, &ctx, guild_id, vec![report]).await;
        }
    }

    async fn guild_role_create(&self, ctx: Context, new: Role) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let view = crate::activity_log::RoleView::from_role(&new);
        if let Some(report) = crate::activity_log::role_change_report(
            crate::activity_log::RoleAction::Created,
            None,
            Some(&view),
        ) {
            emit_activity(&self.app, &ctx, new.guild_id, vec![report]).await;
        }
    }

    async fn guild_role_delete(
        &self,
        ctx: Context,
        guild_id: GuildId,
        removed_role_id: RoleId,
        removed_role_data_if_available: Option<Role>,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let view = removed_role_data_if_available
            .as_ref()
            .map(crate::activity_log::RoleView::from_role)
            .unwrap_or_else(|| {
                crate::activity_log::RoleView::unknown(removed_role_id.get(), guild_id.get())
            });
        if let Some(report) = crate::activity_log::role_change_report(
            crate::activity_log::RoleAction::Deleted,
            Some(&view),
            None,
        ) {
            emit_activity(&self.app, &ctx, guild_id, vec![report]).await;
        }
    }

    async fn guild_role_update(
        &self,
        ctx: Context,
        old_data_if_available: Option<Role>,
        new: Role,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        self.app
            .remembered_discord_targets
            .clear_guild(new.guild_id.get());
        let new_view = crate::activity_log::RoleView::from_role(&new);
        let old_view = old_data_if_available
            .as_ref()
            .map(crate::activity_log::RoleView::from_role);
        if let Some(report) = crate::activity_log::role_change_report(
            crate::activity_log::RoleAction::Updated,
            old_view.as_ref(),
            Some(&new_view),
        ) {
            emit_activity(&self.app, &ctx, new.guild_id, vec![report]).await;
        }
    }

    async fn channel_update(&self, _ctx: Context, old: Option<GuildChannel>, new: GuildChannel) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        if old.as_ref().map(|channel| &channel.permission_overwrites)
            != Some(&new.permission_overwrites)
        {
            self.app
                .remembered_discord_targets
                .clear_guild(new.guild_id.get());
        }
    }

    async fn thread_create(&self, ctx: Context, thread: GuildChannel) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let Some(view) = crate::activity_log::ThreadView::from_guild(&thread) else {
            return;
        };
        if let Some(report) = crate::activity_log::thread_created(&view) {
            emit_activity(&self.app, &ctx, thread.guild_id, vec![report]).await;
        }
    }

    async fn thread_update(&self, ctx: Context, old: Option<GuildChannel>, new: GuildChannel) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let Some(new_view) = crate::activity_log::ThreadView::from_guild(&new) else {
            return;
        };
        let old_view = old
            .as_ref()
            .and_then(crate::activity_log::ThreadView::from_guild);
        if let Some(report) = crate::activity_log::thread_updated(old_view.as_ref(), &new_view) {
            emit_activity(&self.app, &ctx, new.guild_id, vec![report]).await;
        }
    }

    async fn thread_delete(
        &self,
        ctx: Context,
        thread: PartialGuildChannel,
        full_thread_data: Option<GuildChannel>,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let Some(view) =
            crate::activity_log::ThreadView::from_deleted(&thread, full_thread_data.as_ref())
        else {
            return;
        };
        if let Some(report) = crate::activity_log::thread_deleted(&view) {
            emit_activity(&self.app, &ctx, thread.guild_id, vec![report]).await;
        }
    }

    async fn interaction_create(
        &self,
        ctx: Context,
        interaction: serenity::model::prelude::Interaction,
    ) {
        if !self.app.activation.allows_rule_trigger() {
            return;
        }
        let _rule_hold = self.app.activation.hold_rule();
        let app = Arc::clone(self.app());
        commands::handle_interaction(&app, &ctx, interaction).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn message_diagnostics_respect_configured_level() {
        const CHILD_LEVEL: &str = "JMODGUARD_TEST_MESSAGE_LOG_LEVEL";
        if let Ok(level) = std::env::var(CHILD_LEVEL) {
            let mut cfg = crate::config::LogConfig::default();
            if level != "default" {
                cfg.level = level;
            }
            cfg.directory = Default::default();
            crate::init_tracing(&cfg, "bot");
            let mut msg = Message::default();
            msg.id = MessageId::new(111111111111111111);
            msg.channel_id = ChannelId::new(222222222222222222);
            msg.author.id = serenity::model::id::UserId::new(333333333333333333);
            msg.content = "private message text".into();
            log_message_diagnostic(&msg, GuildId::new(444444444444444444));
            tracing::info!("startup log probe");
            tracing::error!("error log probe");
            return;
        }

        // Each subprocess has its own global subscriber and environment. Exercise
        // the production initializer and the diagnostic used by the handler.
        for (level, rust_log, diagnostic, info) in [
            ("default", None, false, true),
            ("info", None, false, true),
            ("info", Some("jmodguard=debug"), false, true),
            ("warn", Some("debug"), false, false),
            ("debug", Some("off"), true, true),
        ] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "handlers::tests::message_diagnostics_respect_configured_level",
                    "--nocapture",
                ])
                .env(CHILD_LEVEL, level)
                .env_remove("RUST_LOG");
            if let Some(rust_log) = rust_log {
                command.env("RUST_LOG", rust_log);
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let logs = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                logs.contains("message 111111111111111111"),
                diagnostic,
                "level={level}, RUST_LOG={rust_log:?}: {logs}"
            );
            assert_eq!(logs.contains("startup log probe"), info, "{logs}");
            assert!(logs.contains("error log probe"), "{logs}");
            assert!(!logs.contains("333333333333333333"), "{logs}");
            assert!(!logs.contains("private message text"), "{logs}");
        }
    }

    use super::{
        audit_case_action, clear_remembered_if_bot_roles_changed,
        clear_remembered_if_subject_roles_changed, departure_joined_at_ms,
        gateway_ban_row_to_attribute, log_message_diagnostic,
        record_unresolved_gateway_observation, select_update_authors,
    };
    use crate::{
        db::{self, Db},
        logging,
        state::BotState,
    };
    use serenity::model::channel::Message;
    use serenity::model::event::MessageUpdateEvent;
    use serenity::model::guild::audit_log::{Action, MemberAction};
    use serenity::model::id::{ChannelId, GuildId, MessageId};

    #[test]
    fn unsuppressed_gateway_actions_survive_equal_saved_state() {
        assert!(record_unresolved_gateway_observation(false));
        assert!(!record_unresolved_gateway_observation(true));
    }

    #[tokio::test]
    async fn exact_audit_replaces_inferred_attribution_but_not_confirmed_facts() {
        let directory =
            std::env::temp_dir().join(format!("exact_attribution_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let audit = 1_212_912_186_163_200_001;
        let write = db::record_gateway_ban(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some("2024-03-01T00:00:00Z"),
            false,
            None,
            false,
        )
        .await
        .unwrap();
        let row_id = gateway_ban_row_to_attribute(write).unwrap();
        db::attribute_enrolled_ban(
            &app.db,
            Some(scope.clone()),
            row_id,
            55,
            88,
            Some("inferred actor"),
            "inferred reason",
        )
        .await
        .unwrap();
        db::mark_enrolled_unbanned(&app.db, Some(scope.clone()), 55)
            .await
            .unwrap();
        super::reconcile_received_gateway_ban_audit(
            &app,
            Some(scope.clone()),
            10,
            55,
            audit,
            db::GatewayBanAuditAttribution {
                moderator_id: 77,
                moderator_name: None,
                reason: "exact reason".into(),
            },
        )
        .await;
        let rows = db::list_enrolled_bans(&app.db, scope.clone(), 10)
            .await
            .unwrap();
        assert_eq!(
            rows[0].moderator_id,
            Some(77),
            "exact audit must replace inferred actor"
        );
        assert_eq!(rows[0].reason.as_deref(), Some("exact reason"));
        assert!(
            rows[0].moderator_name.is_none(),
            "old actor's name must not follow the new actor"
        );
        assert!(!rows[0].active, "attribution must not undo unban");
        // Neither a conflicting replay nor a later heuristic may replace confirmed facts.
        super::reconcile_received_gateway_ban_audit(
            &app,
            Some(scope.clone()),
            10,
            55,
            audit,
            db::GatewayBanAuditAttribution {
                moderator_id: 99,
                moderator_name: Some("replay".into()),
                reason: "changed replay".into(),
            },
        )
        .await;
        db::attribute_enrolled_ban(
            &app.db,
            Some(scope.clone()),
            row_id,
            55,
            88,
            Some("late inferred actor"),
            "late inferred reason",
        )
        .await
        .unwrap();
        db::attribute_enrolled_ban(
            &app.db,
            Some(scope.clone()),
            row_id,
            55,
            77,
            Some("confirmed name"),
            "display reason",
        )
        .await
        .unwrap();
        let rows = db::list_enrolled_bans(&app.db, scope.clone(), 10)
            .await
            .unwrap();
        assert_eq!(rows[0].moderator_id, Some(77));
        assert_eq!(rows[0].moderator_name.as_deref(), Some("confirmed name"));
        assert_eq!(rows[0].reason.as_deref(), Some("exact reason"));
        let events = db::query_enrolled_events(&app.db, scope, Some("member_ban"), Some(55), 10)
            .await
            .unwrap();
        assert_eq!(events[0].actor_id, Some(77));
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cancelled_callback_keeps_write_result_and_observation_together() {
        let directory =
            std::env::temp_dir().join(format!("cancelled_write_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let audit = 1_212_912_186_163_200_001;
        let mut observation = app.begin_gateway_ban_observation(10, 55).await;
        observation.observed = chrono::DateTime::parse_from_rfc3339("2024-03-01T00:00:00Z")
            .unwrap()
            .to_utc();
        app.queue_attributed_gateway_manual_ban_audit_entry(
            10,
            55,
            audit,
            db::GatewayBanAuditAttribution {
                moderator_id: 77,
                moderator_name: None,
                reason: "committed reason".into(),
            },
        );
        let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let callback = {
            let app = std::sync::Arc::clone(&app);
            let scope = scope.clone();
            tokio::spawn(async move {
                let write_app = std::sync::Arc::clone(&app);
                let write_scope = scope.clone();
                super::persist_gateway_ban_observation(
                    &app,
                    Some(scope),
                    55,
                    observation,
                    false,
                    async move {
                        let result = db::record_gateway_ban(
                            &write_app.db,
                            Some(write_scope),
                            55,
                            None,
                            None,
                            Some("2024-03-01T00:00:00Z"),
                            false,
                            None,
                            false,
                        )
                        .await;
                        let _ = committed_tx.send(());
                        let _ = release_rx.await;
                        result
                    },
                )
                .await
            })
        };
        committed_rx.await.unwrap();
        callback.abort();
        assert!(callback.await.unwrap_err().is_cancelled());
        let _ = release_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !app
                .pending_gateway_manual_ban_audit_entries(10, 55)
                .is_empty()
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("caller cancellation must not discard a committed write result");
        let rows = db::list_enrolled_bans(&app.db, scope, 10).await.unwrap();
        assert_eq!(rows[0].moderator_id, Some(77));
        assert_eq!(rows[0].reason.as_deref(), Some("committed reason"));
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn existing_ban_observation_survives_audit_after_snapshot_or_write() {
        for after_write in [false, true] {
            let directory = std::env::temp_dir()
                .join(format!("existing_audit_{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(&directory).unwrap();
            let app = BotState::new(
                Default::default(),
                Db::open(&directory.join("core.db"), 2).unwrap(),
            )
            .unwrap();
            let scope = crate::db::enroll_guild(&app.db, 10, 100)
                .await
                .unwrap()
                .unwrap();
            crate::db::record_gateway_ban(
                &app.db,
                Some(scope.clone()),
                55,
                None,
                None,
                Some("2024-03-01T00:00:00Z"),
                false,
                None,
                false,
            )
            .await
            .unwrap();
            // No unban: the old active mirror and empty pending snapshot are
            // exactly the stale-mirror case. Audit arrives on either side of SQL.
            let pending = app.pending_gateway_manual_ban_audit_entries(10, 55);
            assert!(pending.is_empty());
            let audit = 1_212_912_311_992_320_001;
            let mut in_flight = app.begin_gateway_ban_observation(10, 55).await;
            in_flight.observed = chrono::DateTime::parse_from_rfc3339("2024-03-01T00:00:30Z")
                .unwrap()
                .to_utc();
            let result = super::persist_gateway_ban_observation(
                &app,
                Some(scope.clone()),
                55,
                in_flight,
                false,
                {
                    let app = std::sync::Arc::clone(&app);
                    let scope = scope.clone();
                    async move {
                        if !after_write {
                            app.queue_attributed_gateway_manual_ban_audit_entry(
                                10,
                                55,
                                audit,
                                crate::db::GatewayBanAuditAttribution {
                                    moderator_id: 77,
                                    moderator_name: None,
                                    reason: "second ban reason".into(),
                                },
                            );
                            super::reconcile_pending_gateway_ban_audits(
                                &app,
                                Some(scope.clone()),
                                10,
                                55,
                            )
                            .await;
                            assert_eq!(
                                app.pending_gateway_manual_ban_audit_entries(10, 55),
                                vec![audit]
                            );
                        }
                        crate::db::record_gateway_ban_with_audit(
                            &app.db,
                            Some(scope.clone()),
                            55,
                            None,
                            None,
                            Some("2024-03-01T00:00:30Z"),
                            false,
                            None,
                            true,
                            crate::db::GatewayBanAuditEntries::Pending(pending),
                        )
                        .await
                    }
                },
            )
            .await
            .unwrap();
            if !after_write {
                gateway_ban_row_to_attribute(result)
                    .expect("post-write split must hand its row to attribution");
            } else {
                assert!(matches!(
                    result,
                    crate::db::GatewayBanWrite::Existing { .. }
                ));
            }
            if after_write {
                app.queue_attributed_gateway_manual_ban_audit_entry(
                    10,
                    55,
                    audit,
                    crate::db::GatewayBanAuditAttribution {
                        moderator_id: 77,
                        moderator_name: None,
                        reason: "second ban reason".into(),
                    },
                );
                super::reconcile_pending_gateway_ban_audits(&app, Some(scope.clone()), 10, 55)
                    .await;
            }
            assert!(
                app.pending_gateway_manual_ban_audit_entries(10, 55)
                    .is_empty()
            );
            let rows = crate::db::list_enrolled_bans(&app.db, scope.clone(), 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows.iter().filter(|r| r.active).count(), 1);
            {
                let active = rows.iter().find(|row| row.active).unwrap();
                assert_eq!(active.moderator_id, Some(77));
                assert_eq!(active.reason.as_deref(), Some("second ban reason"));
                assert!(active.moderator_name.is_none());
                // The later HTTP lookup can enrich the same actor's name,
                // without replacing the reason supplied by the exact audit.
                crate::db::attribute_enrolled_ban(
                    &app.db,
                    Some(scope.clone()),
                    active.id,
                    55,
                    77,
                    Some("moderator"),
                    "later display reason",
                )
                .await
                .unwrap();
                crate::db::attribute_enrolled_ban(
                    &app.db,
                    Some(scope.clone()),
                    active.id,
                    55,
                    88,
                    Some("other actor"),
                    "unrelated reason",
                )
                .await
                .unwrap();
                let enriched = crate::db::list_enrolled_bans(&app.db, scope.clone(), 10)
                    .await
                    .unwrap();
                let enriched = enriched.iter().find(|row| row.id == active.id).unwrap();
                assert_eq!(enriched.moderator_id, Some(77));
                assert_eq!(enriched.moderator_name.as_deref(), Some("moderator"));
                assert_eq!(enriched.reason.as_deref(), Some("second ban reason"));
                assert!(
                    rows.iter()
                        .filter(|row| !row.active)
                        .all(|row| row.moderator_id.is_none())
                );
            }
            let events =
                crate::db::query_enrolled_events(&app.db, scope, Some("member_ban"), Some(55), 50)
                    .await
                    .unwrap();
            assert_eq!(events.len(), 2);
            let audit_text = audit.to_string();
            let matched: Vec<_> = events
                .iter()
                .filter(|e| {
                    let data: serde_json::Value =
                        serde_json::from_str(e.data.as_deref().unwrap()).unwrap();
                    data["audit_entry_id"] == audit_text
                })
                .collect();
            assert_eq!(matched.len(), 1);
            assert!(matched[0].created_at.starts_with("2024-03-01T00:00:30"));
            drop(app);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn callback_starting_after_fence_sample_keeps_its_audit_pending() {
        let directory =
            std::env::temp_dir().join(format!("late_flight_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        let mut first = app.begin_gateway_ban_observation(10, 55).await;
        let at = first.observed + chrono::Duration::seconds(2);
        first.observed = at;
        // A has already sampled the fence for its asynchronous write.
        let cutoff = first.audit_after_ms();
        // B's action precedes A's delayed observation, but B's callback
        // starts while A's write is outstanding and remains unpersisted.
        let mut second = app.begin_gateway_ban_observation(10, 55).await;
        second.observed = at + chrono::Duration::seconds(1);
        let audit = (((at - chrono::Duration::seconds(1)).timestamp_millis() - 1_420_070_400_000)
            << 22)
            + 1;
        crate::db::record_gateway_ban_with_audit(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&at.to_rfc3339()),
            false,
            None,
            false,
            crate::db::GatewayBanAuditEntries::Observed {
                pending: vec![],
                audit_after_ms: cutoff,
            },
        )
        .await
        .unwrap();
        first.finish().await;
        app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
        super::reconcile_pending_gateway_ban_audits(&app, Some(scope.clone()), 10, 55).await;
        assert_eq!(
            app.pending_gateway_manual_ban_audit_entries(10, 55),
            vec![audit]
        );
        let events =
            crate::db::query_enrolled_events(&app.db, scope, Some("member_ban"), Some(55), 50)
                .await
                .unwrap();
        assert_eq!(events.len(), 1);
        let data: serde_json::Value =
            serde_json::from_str(events[0].data.as_deref().unwrap()).unwrap();
        assert!(data.get("audit_entry_id").is_none());
        drop(second);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn in_flight_same_subject_audit_never_binds_to_the_younger_commit() {
        for completion in ["saved", "failed", "cancelled", "restart"] {
            let directory = std::env::temp_dir()
                .join(format!("in_flight_audit_{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("core.db");
            let mut app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
            let scope = crate::db::enroll_guild(&app.db, 10, 100)
                .await
                .unwrap()
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let first = app.begin_gateway_ban_observation(10, 55).await;
            let at = first.observed;
            let user: serenity::model::user::User = serde_json::from_value(serde_json::json!({"id":"55","username":"received-name","discriminator":"0","avatar":null})).unwrap();
            let audit = ((at.timestamp_millis() - 1_420_070_400_000) << 22) + 1;
            crate::db::mark_enrolled_unbanned(&app.db, Some(scope.clone()), 55)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let second = app.begin_gateway_ban_observation(10, 55).await;
            crate::gateway_ban_persistence::receive_ban(
                std::sync::Arc::clone(&app),
                second,
                10,
                user.clone(),
                app.consume_gateway_ban_echo(10, 55),
                Some(100),
            )
            .await
            .unwrap()
            .unwrap();
            // A remains paused before persistence; B is the sole committed row.
            app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            super::reconcile_pending_gateway_ban_audits(&app, Some(scope.clone()), 10, 55).await;
            assert_eq!(
                app.pending_gateway_manual_ban_audit_entries(10, 55),
                vec![audit]
            );
            if completion == "saved" {
                crate::gateway_ban_persistence::receive_ban(
                    std::sync::Arc::clone(&app),
                    first,
                    10,
                    user,
                    app.consume_gateway_ban_echo(10, 55),
                    Some(100),
                )
                .await
                .unwrap()
                .unwrap();
            } else if completion == "cancelled" {
                drop(first);
            } else {
                first.finish().await;
            }
            if completion == "restart" {
                drop(app);
                app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
                app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            }
            super::reconcile_pending_gateway_ban_audits(&app, Some(scope.clone()), 10, 55).await;
            assert_eq!(
                app.pending_gateway_manual_ban_audit_entries(10, 55),
                vec![audit],
                "{completion}"
            );
            let events =
                crate::db::query_enrolled_events(&app.db, scope, Some("member_ban"), Some(55), 50)
                    .await
                    .unwrap();
            assert!(
                events.iter().all(|event| {
                    let data: serde_json::Value =
                        serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
                    data.get("audit_entry_id").is_none()
                }),
                "{completion}"
            );
            drop(app);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn pending_ban_audits_cover_delivery_interleavings_and_errors() {
        // These steps use the same pending reconciliation passes and SQL write
        // as the handlers, with deterministic scheduling at the former consume
        // boundary. No Discord client supplies the persistence effect.
        for arrival in [
            "before_gateway",
            "before_commit",
            "after_commit",
            "read_error",
            "restart",
        ] {
            let directory = std::env::temp_dir().join(format!(
                "audit_interleaving_{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("core.db");
            let mut app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
            let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
                .await
                .unwrap()
                .unwrap();
            let audit = 1_212_912_186_163_200_001;
            if arrival == "before_gateway" {
                app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
                super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55)
                    .await;
            }
            super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55)
                .await;
            let first = super::persist_gateway_ban_observation(
                &app,
                Some(enrollment.clone()),
                55,
                app.begin_gateway_ban_observation(10, 55).await,
                false,
                {
                    let app = std::sync::Arc::clone(&app);
                    let enrollment = enrollment.clone();
                    async move {
                        if matches!(arrival, "before_commit" | "read_error") {
                            if arrival == "read_error" {
                                app.db
                                    .with_conn(|conn| {
                                        conn.execute_batch_bounded(
                                            "ALTER TABLE events RENAME TO unavailable_events",
                                            4096,
                                        )?;
                                        Ok(())
                                    })
                                    .await
                                    .unwrap();
                            }
                            app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
                            super::reconcile_pending_gateway_ban_audits(
                                &app,
                                Some(enrollment.clone()),
                                10,
                                55,
                            )
                            .await;
                            assert_eq!(
                                app.pending_gateway_manual_ban_audit_entries(10, 55),
                                vec![audit]
                            );
                            if arrival == "read_error" {
                                app.db
                                    .with_conn(|conn| {
                                        conn.execute_batch_bounded(
                                            "ALTER TABLE unavailable_events RENAME TO events",
                                            4096,
                                        )?;
                                        Ok(())
                                    })
                                    .await
                                    .unwrap();
                            }
                        }
                        crate::db::record_gateway_ban(
                            &app.db,
                            Some(enrollment.clone()),
                            55,
                            None,
                            None,
                            Some("2024-03-01T00:00:00Z"),
                            false,
                            None,
                            false,
                        )
                        .await
                    }
                },
            )
            .await
            .unwrap();
            if arrival == "restart" {
                // Crash after queueing but before reconciliation loses the queue.
                // A replay must recover from persisted observations alone.
                app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
                drop(app);
                app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
                assert!(
                    app.pending_gateway_manual_ban_audit_entries(10, 55)
                        .is_empty()
                );
            }
            if matches!(arrival, "after_commit" | "restart") {
                app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
                super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55)
                    .await;
            }
            assert!(
                app.pending_gateway_manual_ban_audit_entries(10, 55)
                    .is_empty(),
                "{arrival}"
            );
            // A replay after persistence is acknowledged, not offered to the next ban.
            app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            assert_eq!(
                app.pending_gateway_manual_ban_audit_entries(10, 55),
                vec![audit]
            );
            super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55)
                .await;
            crate::db::mark_enrolled_unbanned(&app.db, Some(enrollment.clone()), 55)
                .await
                .unwrap();
            let second = crate::db::record_gateway_ban(
                &app.db,
                Some(enrollment.clone()),
                55,
                None,
                None,
                Some("2024-03-01T00:00:30Z"),
                false,
                None,
                true,
            )
            .await
            .unwrap();
            assert_ne!(
                first, second,
                "{arrival}: the next ban must get its own row"
            );
            let events = crate::db::query_enrolled_events(
                &app.db,
                enrollment,
                Some("member_ban"),
                Some(55),
                50,
            )
            .await
            .unwrap();
            assert_eq!(events.len(), 2);
            for event in events {
                let data: serde_json::Value =
                    serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
                if event.created_at == "2024-03-01T00:00:00Z" {
                    assert_eq!(data["audit_entry_id"], audit.to_string(), "{arrival}");
                } else {
                    assert!(data.get("audit_entry_id").is_none(), "{arrival}");
                }
            }
            drop(app);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn pending_ban_audits_survive_failed_row_commit_and_isolate_concurrent_users() {
        let directory =
            std::env::temp_dir().join(format!("audit_retry_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("core.db");
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        let audit = 1_212_912_186_163_200_001;
        app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
        app.queue_gateway_manual_ban_audit_entry(10, 56, audit + 1);
        app.db.with_conn(|conn| {
            conn.execute_batch_bounded("CREATE TRIGGER fail_ban_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'injected ban write failure'); END;", 4096)?;
            Ok(())
        }).await.unwrap();
        let error = crate::db::record_gateway_ban(
            &app.db,
            Some(enrollment.clone()),
            55,
            None,
            None,
            Some("2024-03-01T00:00:00Z"),
            false,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected ban write failure"));
        super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55).await;
        assert_eq!(
            app.pending_gateway_manual_ban_audit_entries(10, 55),
            vec![audit]
        );
        assert!(
            crate::db::list_enrolled_bans(&app.db, enrollment.clone(), 10)
                .await
                .unwrap()
                .is_empty()
        );
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TRIGGER fail_ban_event", 4096)?;
                Ok(())
            })
            .await
            .unwrap();
        let save = |user| {
            let app = &app;
            let enrollment = enrollment.clone();
            async move {
                crate::db::record_gateway_ban(
                    &app.db,
                    Some(enrollment.clone()),
                    user,
                    None,
                    None,
                    Some("2024-03-01T00:00:00Z"),
                    false,
                    None,
                    false,
                )
                .await
                .unwrap();
                super::reconcile_pending_gateway_ban_audits(app, Some(enrollment), 10, user as u64)
                    .await;
            }
        };
        tokio::join!(save(55), save(56));
        for (user, expected) in [(55, audit), (56, audit + 1)] {
            assert!(
                app.pending_gateway_manual_ban_audit_entries(10, user)
                    .is_empty()
            );
            let events = crate::db::query_enrolled_events(
                &app.db,
                enrollment.clone(),
                Some("member_ban"),
                Some(user as i64),
                50,
            )
            .await
            .unwrap();
            assert_eq!(events.len(), 1);
            let data: serde_json::Value =
                serde_json::from_str(events[0].data.as_deref().unwrap()).unwrap();
            assert_eq!(data["audit_entry_id"], expected.to_string());
        }
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_ban_live_path_records_distinct_manual_bans_when_audit_arrives_late() {
        use serenity::all::{User, UserId};

        let directory = std::env::temp_dir().join(format!(
            "gateway_ban_late_audit_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("core.db");
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = db_path.clone();
        let app = BotState::new(cfg, Db::open(&db_path, 2).unwrap()).unwrap();
        app.reset_gateway_ban_path_test_phases();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        drop(store);
        let mut user = User::default();
        user.id = UserId::new(55);
        user.name = "member".into();

        let precheck =
            super::gateway_ban_directory_precheck_then_observe(&app, 10, &user, 5_000).await;
        assert!(!precheck);
        let first = crate::db::record_gateway_ban_with_audit(
            &app.db,
            Some(enrollment.clone()),
            55,
            Some("member"),
            None,
            Some("2024-03-01T00:00:00+00:00"),
            false,
            None,
            precheck,
            None,
        )
        .await
        .unwrap();
        let crate::db::GatewayBanWrite::Recorded { row_id: first_id } = first else {
            panic!("first live-path ban must insert, got {first:?}");
        };
        assert!(
            crate::db::reconcile_gateway_manual_ban_audit_entry(
                &app.db,
                Some(enrollment.clone()),
                10,
                55,
                1_212_912_186_163_200_001,
            )
            .await
            .unwrap()
        );
        app.drop_gateway_manual_ban_audit_entry(10, 55, 1_212_912_186_163_200_001);

        let precheck =
            super::gateway_ban_directory_precheck_then_observe(&app, 10, &user, 35_000).await;
        assert!(precheck);
        assert!(
            app.pending_gateway_manual_ban_audit_entries(10, 55)
                .is_empty()
        );
        crate::db::mark_enrolled_unbanned(&app.db, Some(enrollment.clone()), 55)
            .await
            .unwrap();
        let second = crate::db::record_gateway_ban_with_audit(
            &app.db,
            Some(enrollment.clone()),
            55,
            Some("member"),
            None,
            Some("2024-03-01T00:00:30+00:00"),
            false,
            None,
            precheck,
            None,
        )
        .await
        .unwrap();
        let crate::db::GatewayBanWrite::Recorded { row_id: second_id } = second else {
            panic!("second live-path ban must insert after late audit reconcile, got {second:?}");
        };
        assert_ne!(first_id, second_id);
        let rows = crate::db::list_enrolled_bans(&app.db, enrollment, 10)
            .await
            .unwrap();
        assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
        assert_eq!(rows.iter().find(|row| row.active).unwrap().id, second_id);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_ban_live_path_records_distinct_manual_bans_within_sixty_seconds() {
        use serenity::all::{User, UserId};

        let directory = std::env::temp_dir().join(format!(
            "gateway_ban_distinct_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("core.db");
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = db_path.clone();
        let app = BotState::new(cfg, Db::open(&db_path, 2).unwrap()).unwrap();
        app.reset_gateway_ban_path_test_phases();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        drop(store);
        let mut user = User::default();
        user.id = UserId::new(55);
        user.name = "member".into();

        app.queue_gateway_manual_ban_audit_entry(10, 55, 1_212_912_186_163_200_001);
        let precheck =
            super::gateway_ban_directory_precheck_then_observe(&app, 10, &user, 5_000).await;
        assert!(!precheck);
        super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55).await;
        let first = crate::db::record_gateway_ban_with_audit(
            &app.db,
            Some(enrollment.clone()),
            55,
            Some("member"),
            None,
            Some("2024-03-01T00:00:00+00:00"),
            false,
            None,
            precheck,
            crate::db::GatewayBanAuditEntries::Pending(
                app.pending_gateway_manual_ban_audit_entries(10, 55),
            ),
        )
        .await
        .unwrap();
        super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55).await;
        let crate::db::GatewayBanWrite::Recorded { row_id: first_id } = first else {
            panic!("first live-path ban must insert, got {first:?}");
        };

        app.queue_gateway_manual_ban_audit_entry(10, 55, 1_212_912_311_992_320_001);
        let precheck =
            super::gateway_ban_directory_precheck_then_observe(&app, 10, &user, 35_000).await;
        assert!(
            precheck,
            "directory must show banned after the first observe"
        );
        super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55).await;
        let second = crate::db::record_gateway_ban_with_audit(
            &app.db,
            Some(enrollment.clone()),
            55,
            Some("member"),
            None,
            Some("2024-03-01T00:00:30+00:00"),
            false,
            None,
            precheck,
            crate::db::GatewayBanAuditEntries::Pending(
                app.pending_gateway_manual_ban_audit_entries(10, 55),
            ),
        )
        .await
        .unwrap();
        super::reconcile_pending_gateway_ban_audits(&app, Some(enrollment.clone()), 10, 55).await;
        let crate::db::GatewayBanWrite::Recorded { row_id: second_id } = second else {
            panic!("second live-path ban must insert, got {second:?}");
        };
        assert_ne!(first_id, second_id);
        let rows = crate::db::list_enrolled_bans(&app.db, enrollment, 10)
            .await
            .unwrap();
        assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
        assert_eq!(rows.iter().find(|row| row.active).unwrap().id, second_id);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_ban_live_path_precheck_runs_before_observe_ban() {
        use serenity::all::{User, UserId};

        let directory = std::env::temp_dir().join(format!(
            "gateway_ban_precheck_order_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("core.db");
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = db_path.clone();
        let app = BotState::new(cfg, Db::open(&db_path, 2).unwrap()).unwrap();
        app.reset_gateway_ban_path_test_phases();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        drop(store);
        let mut user = User::default();
        user.id = UserId::new(55);
        user.name = "member".into();
        super::gateway_ban_directory_precheck_then_observe(&app, 10, &user, 5_000).await;
        assert_eq!(app.gateway_ban_path_test_phases(), ["precheck", "observe"]);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bot_role_change_clears_remembered_discord_targets_for_guild() {
        use crate::discord::DiscordRestTarget;
        use serenity::model::id::{RoleId, UserId};
        use std::time::Instant;

        let path = std::env::temp_dir().join(format!(
            "jmg-bot-role-clear-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        let now = Instant::now();
        let guild = 42_u64;
        let channel = DiscordRestTarget::GuildChannel { guild, channel: 99 };
        app.remembered_discord_targets
            .remember_permanent_for_test(channel, now);
        assert!(app.remembered_discord_targets.blocked(channel, now));
        let bot = UserId::new(1);
        let other = UserId::new(2);
        let role_a = RoleId::new(10);
        let role_b = RoleId::new(11);
        clear_remembered_if_bot_roles_changed(&app, guild, bot, other, Some(&[role_a]), &[role_a]);
        assert!(app.remembered_discord_targets.blocked(channel, now));
        clear_remembered_if_bot_roles_changed(&app, guild, bot, bot, Some(&[role_a]), &[role_b]);
        assert!(!app.remembered_discord_targets.blocked(channel, now));
        drop(app);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn subject_role_change_clears_remembered_kick_ban_and_timeout_only() {
        use crate::discord::{AutomodRestAction, DiscordRestTarget};
        use serenity::model::id::RoleId;
        use std::time::Instant;

        let path = std::env::temp_dir().join(format!(
            "jmg-subject-role-clear-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        let now = Instant::now();
        let guild = 42_u64;
        let user = 99_u64;
        let kick = DiscordRestTarget::AutomodMember {
            guild,
            user,
            action: AutomodRestAction::Kick,
        };
        let ban = DiscordRestTarget::AutomodMember {
            guild,
            user,
            action: AutomodRestAction::Ban,
        };
        let timeout = DiscordRestTarget::AutomodMember {
            guild,
            user,
            action: AutomodRestAction::Timeout,
        };
        let delete = DiscordRestTarget::AutomodDelete { guild, channel: 5 };
        for target in [kick, ban, timeout, delete] {
            app.remembered_discord_targets
                .remember_permanent_for_test(target, now);
        }
        let role_a = RoleId::new(10);
        let role_b = RoleId::new(11);
        clear_remembered_if_subject_roles_changed(&app, guild, user, Some(&[role_a]), &[role_a]);
        assert!(app.remembered_discord_targets.blocked(kick, now));
        assert!(app.remembered_discord_targets.blocked(ban, now));
        assert!(app.remembered_discord_targets.blocked(timeout, now));
        clear_remembered_if_subject_roles_changed(
            &app,
            guild,
            user,
            Some(&[role_a]),
            &[role_a, role_b],
        );
        assert!(!app.remembered_discord_targets.blocked(kick, now));
        assert!(!app.remembered_discord_targets.blocked(ban, now));
        assert!(!app.remembered_discord_targets.blocked(timeout, now));
        assert!(app.remembered_discord_targets.blocked(delete, now));
        drop(app);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn subject_role_cache_miss_clears_remembered_moderation_actions() {
        use crate::discord::{AutomodRestAction, DiscordRestTarget};
        use serenity::model::id::RoleId;
        use std::time::Instant;

        let path = std::env::temp_dir().join(format!(
            "jmg-subject-role-miss-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        let now = Instant::now();
        let guild = 42_u64;
        let user = 99_u64;
        let kick = DiscordRestTarget::AutomodMember {
            guild,
            user,
            action: AutomodRestAction::Kick,
        };
        app.remembered_discord_targets
            .remember_permanent_for_test(kick, now);
        assert!(app.remembered_discord_targets.blocked(kick, now));
        let role_a = RoleId::new(10);
        clear_remembered_if_subject_roles_changed(&app, guild, user, None, &[role_a]);
        assert!(!app.remembered_discord_targets.blocked(kick, now));
        drop(app);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn only_a_newly_recorded_manual_ban_is_attributed() {
        assert_eq!(
            gateway_ban_row_to_attribute(crate::db::GatewayBanWrite::Recorded { row_id: 4 }),
            Some(4)
        );
        assert_eq!(
            gateway_ban_row_to_attribute(crate::db::GatewayBanWrite::Existing { row_id: 4 }),
            None
        );
        assert_eq!(
            gateway_ban_row_to_attribute(crate::db::GatewayBanWrite::OwnedByBot { row_id: 4 }),
            None
        );
    }

    #[tokio::test]
    async fn failed_dispatch_after_ban_capture_preserves_the_manual_record() {
        use crate::{db, state::GatewayCaseAction};

        // Production state, enrolled SQLite, case routing and attribution.
        // Schedule the REST completion during the handler's initial awaits;
        // no live Discord Gateway or audit lookup is used.
        let directory =
            std::env::temp_dir().join(format!("released_ban_echo_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        assert!(
            db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let operation = db::guild_data::CaseId::new();
        let token = app.suppress_next_gateway_case(
            GatewayCaseAction::Ban,
            10,
            20,
            Some(operation.as_str()),
        );
        let captured = app.consume_gateway_ban_echo(10, 20);
        // The pending bot request definitively fails, so its production
        // dispatch path releases this exact token before the mirror write.
        app.release_gateway_case_suppression(token);
        let echo = captured.resolve();
        let write = db::record_gateway_ban(
            &app.db,
            Some(enrollment.clone()),
            20,
            Some("member"),
            None,
            None,
            echo.owned_by_bot,
            echo.operation_id.as_deref(),
            true,
        )
        .await
        .unwrap();
        let recorded_case = if record_unresolved_gateway_observation(echo.suppress_case) {
            crate::cases::record(
                &app,
                Some(enrollment.clone()),
                db::guild_data::CaseAction::Ban,
                db::guild_data::CaseOutcome::Succeeded,
                20,
                None,
                None,
                db::guild_data::CaseEvidence::Unavailable,
                db::guild_data::CaseSource::GatewayObservation {
                    observation_id: db::guild_data::CaseId::new(),
                },
            )
            .await
        } else {
            None
        };
        let events = db::query_enrolled_events(
            &app.db,
            enrollment.clone(),
            Some("member_ban"),
            Some(20),
            10,
        )
        .await
        .unwrap();
        assert_eq!(events.len(), 1, "the manual ban needs its event");
        let case_id = recorded_case.expect("the manual ban needs its Gateway case");
        let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
        let case = store
            .case(scope, case_id, crate::tenant::case_limits())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            case.source,
            db::guild_data::CaseSource::GatewayObservation { .. }
        ));
        let row_id = gateway_ban_row_to_attribute(write).expect("manual row permits attribution");
        db::attribute_enrolled_ban(
            &app.db,
            Some(enrollment.clone()),
            row_id,
            20,
            77,
            Some("moderator"),
            "manual reason",
        )
        .await
        .unwrap();
        let rows = db::list_enrolled_bans(&app.db, enrollment, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moderator_id, Some(77));
        assert_eq!(rows[0].reason.as_deref(), Some("manual reason"));
        assert!(!echo.owned_by_bot);
        assert!(echo.operation_id.is_none());
        assert!(app.gateway_ban_echo_operation(10, 20).is_none());
        drop(store);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn delayed_ban_echo_preserves_manual_and_bot_rebans_after_unban() {
        use crate::{db, state::GatewayCaseAction};

        // Real suppression state, enrolled SQLite writes, and attribution.
        // Schedule the two handler phases explicitly; no live Discord Gateway.
        for (next_operation, application_writes_first) in [
            (None, false),
            (Some("new-attempt"), false),
            (Some("new-attempt"), true),
        ] {
            let directory = std::env::temp_dir()
                .join(format!("delayed_ban_echo_{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("core.db");
            let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
            let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
            db::record_enrolled_ban_with_identity(
                &app.db,
                Some(enrollment.clone()),
                20,
                Some("member"),
                None,
                Some(42),
                Some("bot"),
                "bot reason",
                None,
            )
            .await
            .unwrap();
            app.suppress_next_gateway_case(GatewayCaseAction::Ban, 10, 20, Some("old-attempt"));
            let delayed = app.consume_gateway_ban_echo(10, 20);
            if let Some(operation) = next_operation {
                // The next REST dispatch registers before the preceding
                // unban callback starts; its Gateway echo has not started yet.
                app.suppress_next_gateway_case(GatewayCaseAction::Ban, 10, 20, Some(operation));
            }
            app.clear_gateway_ban_echo(10, 20);
            let delayed = delayed.resolve();
            assert!(delayed.suppress_case);
            assert!(delayed.owned_by_bot);
            assert_eq!(delayed.operation_id.as_deref(), Some("old-attempt"));
            let old_write = db::record_gateway_ban(
                &app.db,
                Some(enrollment.clone()),
                20,
                Some("member"),
                None,
                None,
                delayed.owned_by_bot,
                delayed.operation_id.as_deref(),
                true,
            )
            .await
            .unwrap();
            assert!(matches!(old_write, db::GatewayBanWrite::Existing { .. }));
            // Complete the observed unban, then deliver the next ban inside TTL.
            db::mark_enrolled_unbanned(&app.db, Some(enrollment.clone()), 20)
                .await
                .unwrap();
            let application_row = if application_writes_first {
                db::record_enrolled_ban_if_absent(
                    &app.db,
                    Some(enrollment.clone()),
                    20,
                    Some("member"),
                    None,
                    Some(42),
                    Some("bot"),
                    "new bot reason",
                    None,
                    db::BanRecord::Fresh,
                    next_operation,
                )
                .await
                .unwrap()
            } else {
                None
            };
            let next = app.consume_gateway_ban_echo(10, 20).resolve();
            assert_eq!(
                record_unresolved_gateway_observation(next.suppress_case),
                next_operation.is_none(),
                "only a manual reban creates an unresolved Gateway case"
            );
            assert_eq!(next.owned_by_bot, next_operation.is_some());
            assert_eq!(next.operation_id.as_deref(), next_operation);
            let write = db::record_gateway_ban(
                &app.db,
                Some(enrollment.clone()),
                20,
                Some("member"),
                None,
                None,
                next.owned_by_bot,
                next.operation_id.as_deref(),
                true,
            )
            .await
            .unwrap();
            let events = db::query_enrolled_events(
                &app.db,
                enrollment.clone(),
                Some("member_ban"),
                Some(20),
                10,
            )
            .await
            .unwrap();
            if let Some(operation) = next_operation {
                assert!(
                    events.is_empty(),
                    "bot reban must not create a manual event"
                );
                assert!(gateway_ban_row_to_attribute(write).is_none());
                let row_id = match (write, application_row) {
                    (db::GatewayBanWrite::Existing { row_id }, Some(saved)) => {
                        assert_eq!(row_id, saved);
                        row_id
                    }
                    (db::GatewayBanWrite::OwnedByBot { row_id }, None) => {
                        let completed = db::record_enrolled_ban_if_absent(
                            &app.db,
                            Some(enrollment.clone()),
                            20,
                            Some("member"),
                            None,
                            Some(42),
                            Some("bot"),
                            "new bot reason",
                            None,
                            db::BanRecord::Fresh,
                            Some(operation),
                        )
                        .await
                        .unwrap();
                        assert_eq!(completed, Some(row_id), "dispatch fills its own echo row");
                        row_id
                    }
                    other => panic!("unexpected bot reban writes: {other:?}"),
                };
                let rows = db::list_enrolled_bans(&app.db, enrollment, 10)
                    .await
                    .unwrap();
                assert_eq!(rows.len(), 2, "no extra mirror row for the same bot action");
                assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
                let row = rows.iter().find(|row| row.id == row_id).unwrap();
                assert!(row.active);
                assert_eq!(row.moderator_id, Some(42));
                assert_eq!(row.reason.as_deref(), Some("new bot reason"));
            } else {
                assert_eq!(events.len(), 1, "manual reban must have one event");
                let row_id =
                    gateway_ban_row_to_attribute(write).expect("manual reban can be attributed");
                db::attribute_enrolled_ban(
                    &app.db,
                    Some(enrollment.clone()),
                    row_id,
                    20,
                    77,
                    Some("moderator"),
                    "manual reason",
                )
                .await
                .unwrap();
                let rows = db::list_enrolled_bans(&app.db, enrollment, 10)
                    .await
                    .unwrap();
                assert_eq!(rows.len(), 2);
                assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
                let manual_row = rows.iter().find(|row| row.id == row_id).unwrap();
                assert!(manual_row.active);
                assert_eq!(manual_row.moderator_id, Some(77));
                assert_eq!(manual_row.reason.as_deref(), Some("manual reason"));
            }
            drop(app);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn committed_unknown_departure_keeps_leave_duration_unavailable() {
        use crate::db::guild_data::{MemberDepartureDecision, MemberObservation};
        use serenity::all::{Timestamp, User, UserId};

        let directory =
            std::env::temp_dir().join(format!("departure_unknown_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = directory.join("core.db");
        let app = BotState::new(cfg.clone(), Db::open(&cfg.db.path, 2).unwrap()).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        store
            .initialize_member_directory(scope.clone(), 42, 1000)
            .await
            .unwrap();
        store
            .observe_member(
                scope,
                MemberObservation {
                    user_id: 7,
                    username: None,
                    global_name: None,
                    nickname: None,
                    joined_at_ms: None,
                    observed_at_ms: 2000,
                    is_join: false,
                },
            )
            .await
            .unwrap();
        let mut user = User::default();
        user.id = UserId::new(7);
        let saved = crate::member_directory::observe_departure(&app, 10, &user, 5000, None)
            .await
            .unwrap();
        assert!(
            matches!(&saved, MemberDepartureDecision::Applied(Some(d)) if d.joined_at_ms.is_none())
        );
        let no_stay = crate::member_directory::observe_departure(&app, 10, &user, 5000, Some(1000))
            .await
            .unwrap();
        assert!(matches!(no_stay, MemberDepartureDecision::Applied(None)));
        let departed_at = chrono::DateTime::from_timestamp_millis(5000).unwrap();
        let mut rendered = Vec::new();
        for decision in [&no_stay, &saved] {
            app.member_joins
                .remember(10, 7, Some(Timestamp::from_unix_timestamp(1).unwrap()));
            let cached = app
                .member_joins
                .take(10, 7)
                .map(|joined| joined.timestamp_millis());
            // A payload can accompany Applied(None); an unknown saved stay
            // reaches this path without a payload, but can have cached context.
            let payload = if matches!(decision, MemberDepartureDecision::Applied(None)) {
                Some(1000)
            } else {
                None
            };
            rendered.push(logging::member_join_vars_ms(
                departure_joined_at_ms(Some(decision), payload, cached),
                departed_at,
            ));
        }
        drop((store, app));
        std::fs::remove_dir_all(directory).unwrap();
        for vars in rendered {
            assert_eq!(vars["joined"], "Unavailable");
            assert_eq!(vars["duration"], "Unavailable");
        }
        assert_eq!(
            departure_joined_at_ms(None, Some(1000), Some(2000)),
            Some(1000)
        );
        assert_eq!(departure_joined_at_ms(None, None, Some(2000)), Some(2000));
    }

    #[test]
    fn ambiguous_departure_does_not_log_a_new_stays_duration() {
        use crate::db::guild_data::{MemberDeparture, MemberDepartureDecision};
        assert_eq!(
            departure_joined_at_ms(
                Some(&MemberDepartureDecision::NeedsVerification),
                Some(200),
                Some(100)
            ),
            None,
        );
        let saved = MemberDepartureDecision::Applied(Some(MemberDeparture {
            user_id: 7,
            joined_at_ms: Some(100),
            departed_at_ms: 300,
            completed_stay_ms: Some(200),
        }));
        // A normal seeded stay still supplies the duration when Serenity did
        // not include the optional member payload or a recent cache entry.
        assert_eq!(departure_joined_at_ms(Some(&saved), None, None), Some(100));
        assert_eq!(
            departure_joined_at_ms(Some(&saved), Some(200), None),
            Some(100)
        );
    }

    #[test]
    fn update_author_prefers_event_then_detector_and_only_uses_snippet_for_real_edits() {
        assert_eq!(
            select_update_authors(Some(1), Some(2), Some(3), Some(4), false, false),
            (Some(1), Some(1))
        );
        assert_eq!(
            select_update_authors(None, Some(2), Some(3), Some(4), false, false),
            (Some(2), Some(2))
        );
        assert_eq!(
            select_update_authors(None, None, Some(3), Some(4), false, false),
            (Some(3), Some(3))
        );
        assert_eq!(
            select_update_authors(None, None, None, Some(4), false, false),
            (Some(4), None)
        );
        assert_eq!(
            select_update_authors(None, None, None, Some(4), false, true),
            (Some(4), Some(4))
        );
        assert_eq!(
            select_update_authors(None, None, None, None, false, true),
            (None, None)
        );
    }

    #[test]
    fn webhook_snippet_author_stays_available_to_logging_but_not_detectors() {
        let path =
            std::env::temp_dir().join(format!("webhook_snip_{:032x}.db", rand::random::<u128>()));
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        logging::remember_message_create(&app, 789, 456, 123, Some(42), Some(""), None, true);
        // A partial edit can omit webhook_id, including while changing attachments.
        let update: MessageUpdateEvent = serde_json::from_value(serde_json::json!({
            "id":"123", "channel_id":"456", "guild_id":"789",
            "edited_timestamp":"2026-09-21T00:00:01.000Z",
            "attachments":[{
                "id":"1", "filename":"image.jpg", "url":"https://cdn.discordapp.com/attachments/456/1/image.jpg",
                "proxy_url":"https://cdn.discordapp.com/attachments/456/1/image.jpg", "size":100
            }]
        })).unwrap();
        logging::remember_message_update(&app, &update, Some(42));
        let author = logging::known_message_author(&app, 789, 456, 123);
        let webhook = logging::known_message_webhook(&app, 789, 456, 123);
        assert_eq!(author, Some(42));
        assert!(webhook);
        assert_eq!(
            select_update_authors(None, None, None, author, webhook, true),
            (Some(42), None)
        );
        assert_eq!(
            select_update_authors(None, None, None, author, webhook, false),
            (Some(42), None)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_case_action_maps_bot_owned_member_events_only() {
        assert_eq!(
            audit_case_action(Action::Member(MemberAction::Kick)),
            Some(crate::db::guild_data::CaseAction::Kick)
        );
        assert_eq!(
            audit_case_action(Action::Member(MemberAction::BanAdd)),
            Some(crate::db::guild_data::CaseAction::Ban)
        );
        assert_eq!(
            audit_case_action(Action::Member(MemberAction::BanRemove)),
            Some(crate::db::guild_data::CaseAction::Unban)
        );
        assert_eq!(
            audit_case_action(Action::Member(MemberAction::Update)),
            None
        );
        assert_eq!(audit_case_action(Action::GuildUpdate), None);
    }

    #[tokio::test]
    async fn dashboard_unban_echo_records_one_member_unban_with_dashboard_actor() {
        use crate::db;
        use crate::state::GatewayCaseAction;
        use jmodguard::db::SqlExecutor;

        let directory = std::env::temp_dir().join(format!(
            "dashboard_unban_echo_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        app.activation.mark_lock_held(true);
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        db::record_enrolled_ban_with_identity(
            &app.db,
            Some(enrollment.clone()),
            99,
            Some("member"),
            None,
            Some(1),
            Some("mod"),
            "spam",
            None,
        )
        .await
        .unwrap();
        let _token = app.suppress_next_gateway_case(GatewayCaseAction::Unban, 10, 99, None);
        db::mark_enrolled_unbanned(&app.db, Some(enrollment.clone()), 99)
            .await
            .unwrap();
        let suppressed = app.consume_gateway_case_suppression(GatewayCaseAction::Unban, 10, 99);
        assert!(suppressed);
        if !suppressed {
            db::record_gateway_unban_event(&app.db, Some(enrollment.clone()), 99, None)
                .await
                .unwrap();
        }
        db::insert_event(
            &app.db,
            10,
            "member_unban",
            Some(99),
            Some(42),
            &serde_json::json!({"source": "dashboard"}),
        )
        .await
        .unwrap();
        let (count, actor): (i64, Option<i64>) = app
            .db
            .with_conn(|conn| {
                let count = conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE guild_id=10 AND event_type='member_unban'",
                    (),
                    |row| Ok(row.get(0)?),
                )?;
                let actor = conn.query_row(
                    "SELECT actor_id FROM events WHERE guild_id=10 AND event_type='member_unban' LIMIT 1",
                    (),
                    |row| Ok(row.get(0)?),
                )?;
                Ok((count, actor))
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(actor, Some(42));
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::{
        Handler, RateLimitWait, member_sync_deadline_ms, note_rate_limit_wait,
        redact_rate_limit_path,
    };
    use crate::config::BotProcessConfig;
    use crate::db::Db;
    use crate::logging::persistence_log_tests::CapturedLog;
    use crate::state::BotState;
    use jmodguard::db::SqlExecutor;
    use serenity::http::LightMethod;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use tracing::instrument::WithSubscriber;

    fn wait(timeout: Duration, method: LightMethod, path: &str, global: bool) -> RateLimitWait {
        RateLimitWait {
            timeout,
            limit: 5,
            method,
            path: path.into(),
            global,
        }
    }

    async fn test_state() -> (std::path::PathBuf, Arc<BotState>, Handler) {
        let directory =
            std::env::temp_dir().join(format!("rate_limit_hook_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("core.db");
        let mut cfg = BotProcessConfig::default();
        cfg.db.path = db_path;
        let app = BotState::new(cfg, Db::open(&directory.join("core.db"), 2).unwrap()).unwrap();
        let handler = Handler {
            app: Arc::clone(&app),
        };
        (directory, app, handler)
    }

    fn rate_limit_wait_lines(text: &str) -> usize {
        text.lines()
            .filter(|line| line.contains("rate-limit wait"))
            .count()
    }

    async fn wait_epoch_persisted(app: &BotState, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
                == app
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("member sync pacing epoch did not persist within {timeout:?}");
    }

    async fn pacing_deadline(app: &BotState, bot_id: i64) -> Option<i64> {
        app.db
            .with_conn(move |conn| {
                let present: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM member_sync_pacing WHERE bot_id=?1",
                    [bot_id],
                    |row| Ok(row.get(0)?),
                )?;
                if present == 0 {
                    return Ok(None);
                }
                conn.query_row(
                    "SELECT request_after_ms FROM member_sync_pacing WHERE bot_id=?1",
                    [bot_id],
                    |row| Ok(row.get(0)?),
                )
                .map(Some)
            })
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hook_saves_simulated_member_deadline() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        let member_wait = wait(
            Duration::from_secs(60),
            LightMethod::Get,
            "/guilds/1/members",
            false,
        );
        let before = chrono::Utc::now().timestamp_millis();
        handler
            .record_rate_limit_wait(member_wait.clone())
            .with_subscriber(log.dispatch())
            .await;
        let after = chrono::Utc::now().timestamp_millis();
        let stored = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(stored >= before + 60_000 && stored <= after + 60_000);
        assert_eq!(pacing_deadline(&app, 42).await, Some(stored));
        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 1);
        assert!(text.contains("WARN"));
        assert!(text.contains("/guilds/1/members"));
        assert!(text.contains("Get"));
        assert!(!text.contains("429"));
        let first_stored = stored;
        handler
            .record_rate_limit_wait(member_wait)
            .with_subscriber(log.dispatch())
            .await;
        assert_eq!(rate_limit_wait_lines(&log.text()), 1);
        assert!(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed) >= first_stored);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saved_deadline_blocks_claims_after_reopen() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(60),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(log.dispatch())
            .await;
        let deadline = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        let db_path = app.db.path().to_path_buf();
        drop(handler);
        drop(app);

        let reopened = Db::open(&db_path, 2).unwrap();
        assert!(
            crate::db::claim_member_sync_request(
                &reopened,
                42,
                "after-restart".into(),
                deadline - 1,
                15_000,
                90_000,
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            crate::db::claim_member_sync_request(
                &reopened,
                42,
                "after-restart".into(),
                deadline,
                15_000,
                90_000,
            )
            .await
            .unwrap()
            .is_some()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn route_filter_matches_the_replaced_callback() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        let dispatch = log.dispatch();

        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(5),
                LightMethod::Get,
                "/channels/1/messages",
                false,
            ))
            .with_subscriber(dispatch.clone())
            .await;
        assert_eq!(
            app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed),
            0
        );
        assert_eq!(pacing_deadline(&app, 42).await, None);
        assert_eq!(rate_limit_wait_lines(&log.text()), 1);

        let mut previous = 0;
        for (timeout_secs, path, global) in [
            (30u64, "/guilds/1/members", false),
            (60, "/guilds/1/bans", false),
            (90, "/guilds/1/audit-logs", false),
            (120, "/gateway/bot", true),
        ] {
            handler
                .record_rate_limit_wait(wait(
                    Duration::from_secs(timeout_secs),
                    LightMethod::Get,
                    path,
                    global,
                ))
                .with_subscriber(dispatch.clone())
                .await;
            let stored = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
            assert!(
                stored > previous,
                "{path} must extend the member-sync deadline"
            );
            assert_eq!(pacing_deadline(&app, 42).await, Some(stored));
            previous = stored;
        }
        // `thread-members` does not contain the `/members` needle, so the old
        // callback did not arm the gate for it. The wait is still logged.
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(150),
                LightMethod::Get,
                "/channels/1/thread-members",
                false,
            ))
            .with_subscriber(dispatch.clone())
            .await;
        assert_eq!(
            app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed),
            previous
        );
        assert_eq!(pacing_deadline(&app, 42).await, Some(previous));
        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 6);
        assert!(!text.contains("429"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limit_wait_log_throttles_per_route() {
        let (directory, app, _handler) = test_state().await;
        let log = CapturedLog::default();
        let dispatch = log.dispatch();
        let t0 = Instant::now();
        let members = wait(
            Duration::from_secs(1),
            LightMethod::Get,
            "/guilds/1/members",
            false,
        );
        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(&app, &members, t0));
            assert!(!note_rate_limit_wait(
                &app,
                &members,
                t0 + Duration::from_secs(30)
            ));
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Post,
                    "/guilds/1/members",
                    false,
                ),
                t0,
            ));
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Get,
                    "/channels/1/messages",
                    false,
                ),
                t0,
            ));
            assert!(note_rate_limit_wait(
                &app,
                &members,
                t0 + Duration::from_secs(60),
            ));
        });
        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 4);
        assert!(!text.contains("429"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_routes_commit_the_max_deadline() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        let dispatch = log.dispatch();
        let handler = Arc::new(handler);
        let tasks = [
            (Duration::from_secs(30), "/guilds/1/members"),
            (Duration::from_secs(90), "/guilds/1/bans"),
            (Duration::from_secs(120), "/channels/9/messages"),
        ];
        let mut joins = Vec::new();
        for (timeout, path) in tasks {
            let handler = Arc::clone(&handler);
            let dispatch = dispatch.clone();
            joins.push(tokio::spawn(async move {
                handler
                    .record_rate_limit_wait(wait(timeout, LightMethod::Get, path, false))
                    .with_subscriber(dispatch)
                    .await;
            }));
        }
        for join in joins {
            join.await.unwrap();
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let stored = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        let bans_deadline = member_sync_deadline_ms(now_ms, Duration::from_secs(90));
        let messages_deadline = member_sync_deadline_ms(now_ms, Duration::from_secs(120));
        assert!(stored >= member_sync_deadline_ms(now_ms, Duration::from_secs(30)));
        assert!(stored <= bans_deadline + 2_000);
        assert!(stored < messages_deadline - 10_000);
        assert_eq!(pacing_deadline(&app, 42).await, Some(stored));
        assert_eq!(rate_limit_wait_lines(&log.text()), 3);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn late_write_back_cannot_shorten_the_deadline() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        let dispatch = log.dispatch();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(90),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(dispatch.clone())
            .await;
        let high = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(10),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(dispatch.clone())
            .await;
        assert_eq!(
            app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed),
            high
        );
        assert_eq!(pacing_deadline(&app, 42).await, Some(high));
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(120),
                LightMethod::Get,
                "/guilds/1/bans",
                false,
            ))
            .with_subscriber(dispatch)
            .await;
        let newer = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(newer > high);
        assert_eq!(pacing_deadline(&app, 42).await, Some(newer));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_pacing_write_rolls_back_and_retry_persists() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(30),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(log.dispatch())
            .await;
        let kept = pacing_deadline(&app, 42).await.unwrap();
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "CREATE TRIGGER reject_pacing_extend
BEFORE UPDATE ON member_sync_pacing
BEGIN SELECT RAISE(ABORT, 'fixture pacing failure'); END;",
                    65_536,
                )
            })
            .await
            .unwrap();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(90),
                LightMethod::Get,
                "/guilds/1/bans",
                false,
            ))
            .with_subscriber(log.dispatch())
            .await;
        let advanced = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(advanced > kept);
        assert_eq!(pacing_deadline(&app, 42).await, Some(kept));
        assert!(
            app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
                > app
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire)
        );
        let text = log.text();
        assert!(text.contains("/guilds/1/bans"));
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TRIGGER reject_pacing_extend;", 65_536)
            })
            .await
            .unwrap();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(90),
                LightMethod::Get,
                "/guilds/1/bans",
                false,
            ))
            .await;
        let final_deadline = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(final_deadline > kept);
        assert_eq!(pacing_deadline(&app, 42).await, Some(final_deadline));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_bot_id_does_not_persist_or_log_a_pacing_error() {
        let (directory, app, handler) = test_state().await;
        let log = CapturedLog::default();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(60),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(log.dispatch())
            .await;
        assert!(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed) > 0);
        assert_eq!(pacing_deadline(&app, 42).await, None);
        assert_eq!(pacing_deadline(&app, 0).await, None);
        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 1);
        assert!(!text.contains("failed to persist"));
        assert!(!text.contains("429"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pending_deadline_is_persisted_when_the_member_sync_loop_publishes_the_bot_id() {
        use serenity::all::{Cache, CurrentUser, UserUpdateEvent};
        use std::sync::Arc;

        let (directory, app, handler) = test_state().await;
        assert_eq!(app.member_sync_bot_id.load(Ordering::Relaxed), 0);
        let log = CapturedLog::default();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(60),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(log.dispatch())
            .await;
        let stored = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(stored > 0);
        let epoch = app.member_sync_rate_limit_epoch.load(Ordering::Acquire);
        assert!(epoch > 0);
        assert_eq!(
            app.member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire),
            0
        );
        assert_eq!(pacing_deadline(&app, 42).await, None);
        assert!(!log.text().contains("failed to persist"));

        let cache = Arc::new(Cache::new());
        let mut user = CurrentUser::default();
        user.id = serenity::model::id::UserId::new(42);
        let mut event: UserUpdateEvent =
            serde_json::from_value(serde_json::to_value(user).unwrap()).unwrap();
        cache.update(&mut event);

        let worker = tokio::spawn(crate::member_sync::run(
            app.clone(),
            Arc::new(serenity::http::Http::new("unused")),
            cache,
        ));
        let poll = tokio::time::timeout(Duration::from_secs(5), async {
            // The committed row can be visible before the worker resumes to
            // acknowledge it. Wait for that acknowledgement before aborting.
            while app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
                != epoch
                || pacing_deadline(&app, 42).await != Some(stored)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        worker.abort();
        let _ = worker.await;
        assert!(poll.is_ok());
        assert_eq!(pacing_deadline(&app, 42).await, Some(stored));
        assert_eq!(
            app.member_sync_rate_limit_epoch.load(Ordering::Relaxed),
            app.member_sync_rate_limit_persisted_epoch
                .load(Ordering::Relaxed)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_pacing_write_retries_when_the_bot_id_is_published_again() {
        let (directory, app, handler) = test_state().await;
        app.publish_member_sync_bot_id(42).await;
        assert_eq!(pacing_deadline(&app, 42).await, None);

        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(30),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .await;
        let kept = pacing_deadline(&app, 42).await.unwrap();

        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "CREATE TRIGGER reject_pacing_extend
BEFORE UPDATE ON member_sync_pacing
BEGIN SELECT RAISE(ABORT, 'fixture pacing failure'); END;",
                    65_536,
                )
            })
            .await
            .unwrap();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(90),
                LightMethod::Get,
                "/guilds/1/bans",
                false,
            ))
            .await;
        let advanced = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(advanced > kept);
        assert_eq!(pacing_deadline(&app, 42).await, Some(kept));
        assert!(
            app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
                > app
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire)
        );

        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TRIGGER reject_pacing_extend;", 65_536)
            })
            .await
            .unwrap();

        app.publish_member_sync_bot_id(42).await;
        assert_eq!(
            pacing_deadline(&app, 42).await,
            Some(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed))
        );
        assert!(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed) > kept);
        assert_eq!(
            app.member_sync_rate_limit_epoch.load(Ordering::Relaxed),
            app.member_sync_rate_limit_persisted_epoch
                .load(Ordering::Relaxed)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_sync_rate_limit_persist_exit_reclaims_epoch_armed_during_handoff() {
        use super::observe_member_sync_rate_limit_wait;
        use std::sync::atomic::AtomicBool;

        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(2),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .await;
        wait_epoch_persisted(&app, Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_secs(3)).await;

        let at_exit = Arc::new(AtomicBool::new(false));
        let release_exit = Arc::new(AtomicBool::new(false));
        app.set_member_sync_rate_limit_persist_exit_hook(Some(Arc::new({
            let at_exit = Arc::clone(&at_exit);
            let release_exit = Arc::clone(&release_exit);
            move || {
                at_exit.store(true, Ordering::SeqCst);
                while !release_exit.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        })));

        app.ensure_member_sync_rate_limit_persist_task();
        let wait_until = tokio::time::Instant::now() + Duration::from_secs(5);
        while !at_exit.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() > wait_until {
                panic!("persist task did not reach exit handoff");
            }
            tokio::task::yield_now().await;
        }

        observe_member_sync_rate_limit_wait(
            &app,
            wait(
                Duration::from_secs(90),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ),
        );
        assert!(
            app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
                > app
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire),
            "new pacing epoch must stay dirty while the exiting task is still marked running"
        );

        release_exit.store(true, Ordering::SeqCst);
        wait_epoch_persisted(&app, Duration::from_secs(5)).await;
        assert_eq!(
            pacing_deadline(&app, 42).await,
            Some(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed))
        );
        app.set_member_sync_rate_limit_persist_exit_hook(None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handler_failed_pacing_write_retries_without_another_wait() {
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(30),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .await;
        let kept = pacing_deadline(&app, 42).await.unwrap();
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "CREATE TRIGGER reject_pacing_extend
BEFORE UPDATE ON member_sync_pacing
BEGIN SELECT RAISE(ABORT, 'fixture pacing failure'); END;",
                    65_536,
                )
            })
            .await
            .unwrap();
        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(90),
                LightMethod::Get,
                "/guilds/1/bans",
                false,
            ))
            .await;
        assert!(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed) > kept);
        assert_eq!(pacing_deadline(&app, 42).await, Some(kept));
        assert!(
            app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
                > app
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire)
        );
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TRIGGER reject_pacing_extend;", 65_536)
            })
            .await
            .unwrap();
        let target = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        let poll = tokio::time::timeout(Duration::from_secs(5), async {
            while pacing_deadline(&app, 42).await != Some(target) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(poll.is_ok());
        assert_eq!(
            app.member_sync_rate_limit_epoch.load(Ordering::Relaxed),
            app.member_sync_rate_limit_persisted_epoch
                .load(Ordering::Relaxed)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn global_wait_logs_after_a_local_wait_on_the_same_route() {
        let (directory, app, _handler) = test_state().await;
        let log = CapturedLog::default();
        let dispatch = log.dispatch();
        let t0 = Instant::now();
        let members = wait(
            Duration::from_secs(1),
            LightMethod::Get,
            "/guilds/1/members",
            false,
        );
        let global_members = wait(
            Duration::from_secs(1),
            LightMethod::Get,
            "/guilds/1/members",
            true,
        );
        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(&app, &members, t0));
            assert!(note_rate_limit_wait(
                &app,
                &global_members,
                t0 + Duration::from_secs(30)
            ));
            assert!(!note_rate_limit_wait(
                &app,
                &global_members,
                t0 + Duration::from_secs(40)
            ));
            assert!(!note_rate_limit_wait(
                &app,
                &members,
                t0 + Duration::from_secs(30)
            ));
        });
        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 2);
        assert!(text.contains("global=true"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_resource_ids_share_one_throttle_key() {
        use serenity::http::Route;
        use serenity::model::id::InteractionId;

        let (directory, app, _handler) = test_state().await;
        let log = CapturedLog::default();
        let dispatch = log.dispatch();
        let t0 = Instant::now();
        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Get,
                    "/guilds/1/members",
                    false,
                ),
                t0,
            ));
            assert!(!note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Get,
                    "/guilds/999/members",
                    false,
                ),
                t0 + Duration::from_secs(1),
            ));
        });
        let keys = app
            .rate_limit_wait_logged_at
            .lock()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], "Get route /guilds/{id}/members");
        assert!(!keys[0].contains("/guilds/1"));
        assert!(!keys[0].contains("/guilds/999"));
        assert!(log.text().contains("/guilds/1/members"));

        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Get,
                    "/guilds/3/bans",
                    false,
                ),
                t0 + Duration::from_secs(1),
            ));
        });

        let post_path_a = Route::InteractionResponse {
            interaction_id: InteractionId::new(200),
            token: TOKEN_A,
        }
        .path()
        .into_owned();
        let post_path_b = Route::InteractionResponse {
            interaction_id: InteractionId::new(201),
            token: TOKEN_A,
        }
        .path()
        .into_owned();
        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Post,
                    &post_path_a,
                    false
                ),
                t0 + Duration::from_secs(1),
            ));
            assert!(!note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Post,
                    &post_path_b,
                    false
                ),
                t0 + Duration::from_secs(2),
            ));
        });
        let text = log.text();
        assert!(!text.contains(TOKEN_A));
        let keys = app
            .rate_limit_wait_logged_at
            .lock()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let interaction_key =
            "Post route https://discord.com/api/v10/interactions/{id}/{token}/callback";
        assert!(keys.iter().any(|key| key == interaction_key));
        assert!(
            keys.iter()
                .all(|key| !key.contains("/interactions/200/")
                    && !key.contains("/interactions/201/"))
        );
        for key in &keys {
            assert!(!key.contains(TOKEN_A));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deadline_millis_clamps_to_i64() {
        assert_eq!(member_sync_deadline_ms(10, Duration::MAX), i64::MAX);
        assert_eq!(member_sync_deadline_ms(10, Duration::from_millis(5)), 15);
        assert_eq!(
            member_sync_deadline_ms(i64::MAX, Duration::from_millis(1)),
            i64::MAX
        );
    }

    const TOKEN_A: &str = "synthetic_token_r4303412353";
    const TOKEN_B: &str = "synthetic_token_r4303412353_other";

    #[test]
    fn redact_rate_limit_path_keeps_safe_routes_and_edges() {
        use serenity::http::Route;
        use serenity::model::id::{ApplicationId, InteractionId, MessageId, WebhookId};

        let serenity_pairs = [
            (
                Route::WebhookOriginalInteractionResponse {
                    application_id: ApplicationId::new(100),
                    token: TOKEN_A,
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/webhooks/100/{token}/messages/@original".to_string(),
            ),
            (
                Route::InteractionResponse {
                    interaction_id: InteractionId::new(200),
                    token: TOKEN_A,
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/interactions/200/{token}/callback".to_string(),
            ),
            (
                Route::WebhookWithToken {
                    webhook_id: WebhookId::new(300),
                    token: TOKEN_A,
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/webhooks/300/{token}".to_string(),
            ),
            (
                Route::WebhookFollowupMessages {
                    application_id: ApplicationId::new(100),
                    token: TOKEN_A,
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/webhooks/100/{token}".to_string(),
            ),
            (
                Route::WebhookMessage {
                    webhook_id: WebhookId::new(300),
                    token: TOKEN_A,
                    message_id: MessageId::new(55),
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/webhooks/300/{token}/messages/55".to_string(),
            ),
            (
                Route::WebhookFollowupMessage {
                    application_id: ApplicationId::new(100),
                    token: TOKEN_A,
                    message_id: MessageId::new(55),
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/webhooks/100/{token}/messages/55".to_string(),
            ),
            (
                Route::Webhook {
                    webhook_id: WebhookId::new(300),
                }
                .path()
                .into_owned(),
                "https://discord.com/api/v10/webhooks/300".to_string(),
            ),
        ];
        for (input, expected) in serenity_pairs {
            assert!(
                input.contains(TOKEN_A) || !expected.contains("{token}"),
                "Serenity must expand live tokens in route paths"
            );
            let redacted = redact_rate_limit_path(&input);
            assert_eq!(redacted, expected);
            assert!(!redacted.contains(TOKEN_A));
            if expected.contains("messages/55") {
                assert!(redacted.contains("55"));
            }
        }

        let pairs = [
            (
                "/webhooks/100/synthetic_token_r4303412353/messages/@original",
                "/webhooks/100/{token}/messages/@original",
            ),
            (
                "/interactions/200/synthetic_token_r4303412353/callback",
                "/interactions/200/{token}/callback",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353/messages/55",
                "/webhooks/100/{token}/messages/55",
            ),
            (
                "/webhooks/300/synthetic_token_r4303412353/slack",
                "/webhooks/300/{token}/slack",
            ),
            (
                "/webhooks/300/synthetic_token_r4303412353/github",
                "/webhooks/300/{token}/github",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353/messages/@original/extra",
                "/webhooks/100/{token}/messages/@original/extra",
            ),
            (
                "/interactions/200/synthetic_token_r4303412353/callback/extra",
                "/interactions/200/{token}/callback/extra",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353/",
                "/webhooks/100/{token}/",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353/messages/@original/",
                "/webhooks/100/{token}/messages/@original/",
            ),
            (
                "/webhooks//synthetic_token_r4303412353/messages/@original",
                "/webhooks//{token}/messages/@original",
            ),
            (
                "https://discord.com/api/v10/webhooks/100/synthetic_token_r4303412353/messages/@original?wait=true",
                "https://discord.com/api/v10/webhooks/100/{token}/messages/@original?wait=true",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353?wait=true&token=synthetic_token_r4303412353",
                "/webhooks/100/{token}?wait=true&token={token}",
            ),
            (
                "/interactions/200/synthetic_token_r4303412353/callback?wait=true",
                "/interactions/200/{token}/callback?wait=true",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353/messages/@original?",
                "/webhooks/100/{token}/messages/@original?",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353/messages/@original#frag",
                "/webhooks/100/{token}/messages/@original#frag",
            ),
            (
                "/webhooks/100/synthetic_token_r4303412353#synthetic_token_r4303412353",
                "/webhooks/100/{token}#{token}",
            ),
            (
                "http://discord.com:443/api/v10/webhooks/100/synthetic_token_r4303412353/messages/@original",
                "http://discord.com:443/api/v10/webhooks/100/{token}/messages/@original",
            ),
        ];
        for (input, expected) in pairs {
            let redacted = redact_rate_limit_path(input);
            assert_eq!(redacted, expected);
            assert!(!redacted.contains(TOKEN_A));
            if expected.contains("messages/55") || input.contains("/messages/55") {
                assert!(redacted.contains("55"));
            }
        }

        let unchanged = [
            "",
            "/guilds/1/members",
            "/guilds/1/bans",
            "/guilds/1/audit-logs",
            "/channels/1/messages",
            "/channels/1/thread-members",
            "/gateway/bot",
            "/channels/1/webhooks",
            "/guilds/1/webhooks",
            "/webhooks/300",
            "/webhooks/300/",
            "/interactions/200",
            "/interactions/200/",
            "/interactions/200/callback",
            "/interactions/200/synthetic_token_r4303412353",
            "webhooks/100/synthetic_token_r4303412353",
            "https://discord.com/api/v10",
            "https://discord.com/api/v10/guilds/1/members",
            "https://discord.com/api/v10/channels/1/webhooks",
            "/guilds/1/members?limit=1000",
            "/guilds/1/members?token=synthetic_token_r4303412353",
        ];
        for input in unchanged {
            assert_eq!(redact_rate_limit_path(input), input);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limit_wait_log_redacts_interaction_tokens() {
        use serenity::http::Route;
        use serenity::model::id::{ApplicationId, InteractionId};

        let (directory, app, _handler) = test_state().await;
        let log = CapturedLog::default();
        let dispatch = log.dispatch();
        let t0 = Instant::now();

        let patch_path = Route::WebhookOriginalInteractionResponse {
            application_id: ApplicationId::new(100),
            token: TOKEN_A,
        }
        .path()
        .into_owned();
        let post_path = Route::InteractionResponse {
            interaction_id: InteractionId::new(200),
            token: TOKEN_A,
        }
        .path()
        .into_owned();
        assert!(patch_path.contains(TOKEN_A));
        assert!(post_path.contains(TOKEN_A));

        let patch_b_path = Route::WebhookOriginalInteractionResponse {
            application_id: ApplicationId::new(100),
            token: TOKEN_B,
        }
        .path()
        .into_owned();
        let post_b_path = Route::InteractionResponse {
            interaction_id: InteractionId::new(200),
            token: TOKEN_B,
        }
        .path()
        .into_owned();

        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Patch,
                    &patch_path,
                    false
                ),
                t0,
            ));
            assert!(note_rate_limit_wait(
                &app,
                &wait(Duration::from_secs(1), LightMethod::Post, &post_path, false),
                t0,
            ));
            assert!(!note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Patch,
                    &patch_b_path,
                    false
                ),
                t0 + Duration::from_secs(30),
            ));
            assert!(!note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Post,
                    &post_b_path,
                    false
                ),
                t0 + Duration::from_secs(30),
            ));
        });

        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 2);
        assert!(text.contains("WARN"));
        assert!(text.contains("Patch"));
        assert!(text.contains("Post"));
        assert!(text.contains("{token}"));
        assert!(text.contains("/messages/@original"));
        assert!(text.contains("/callback"));
        assert!(!text.contains(TOKEN_A));
        assert!(!text.contains(TOKEN_B));
        assert!(!text.contains("429"));

        let keys = app
            .rate_limit_wait_logged_at
            .lock()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 2);
        let expected_keys = [
            "Patch route https://discord.com/api/v10/webhooks/{id}/{token}/messages/@original"
                .to_string(),
            "Post route https://discord.com/api/v10/interactions/{id}/{token}/callback".to_string(),
        ];
        for key in &expected_keys {
            assert!(keys.contains(key));
            assert!(!key.contains(TOKEN_A));
            assert!(!key.contains(TOKEN_B));
        }

        tracing::dispatcher::with_default(&dispatch, || {
            assert!(note_rate_limit_wait(
                &app,
                &wait(
                    Duration::from_secs(1),
                    LightMethod::Patch,
                    &patch_path,
                    false
                ),
                t0 + Duration::from_secs(60),
            ));
        });
        let text = log.text();
        assert_eq!(rate_limit_wait_lines(&text), 3);
        assert!(!text.contains(TOKEN_A));
        assert!(!text.contains(TOKEN_B));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_token_segment_does_not_arm_member_sync() {
        const RAW_TOKEN: &str = "synthetic_token_r4303412353";
        let (directory, app, handler) = test_state().await;
        app.member_sync_bot_id.store(42, Ordering::Relaxed);
        let log = CapturedLog::default();
        let dispatch = log.dispatch();

        for path in [
            "https://discord.com/api/v10/webhooks/42/members",
            "https://discord.com/api/v10/webhooks/42/bans",
            "https://discord.com/api/v10/webhooks/42/audit-logs",
        ] {
            handler
                .record_rate_limit_wait(wait(
                    Duration::from_secs(30),
                    LightMethod::Get,
                    path,
                    false,
                ))
                .with_subscriber(dispatch.clone())
                .await;
            assert_eq!(
                app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed),
                0
            );
            assert_eq!(pacing_deadline(&app, 42).await, None);
        }

        let text = log.text();
        assert!(!text.contains("/webhooks/42/members"));
        assert!(!text.contains("/webhooks/42/bans"));
        assert!(!text.contains("/webhooks/42/audit-logs"));

        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(60),
                LightMethod::Get,
                "/guilds/1/members",
                false,
            ))
            .with_subscriber(dispatch.clone())
            .await;
        let members_deadline = app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        assert!(members_deadline > 0);
        assert_eq!(pacing_deadline(&app, 42).await, Some(members_deadline));

        handler
            .record_rate_limit_wait(wait(
                Duration::from_secs(90),
                LightMethod::Get,
                &format!("https://discord.com/api/v10/webhooks/42/{}", RAW_TOKEN),
                true,
            ))
            .with_subscriber(dispatch)
            .await;
        assert!(app.member_sync_rate_limit_until_ms.load(Ordering::Relaxed) > members_deadline);
        let text = log.text();
        assert!(!text.contains(RAW_TOKEN));
        assert!(!text.contains("/webhooks/42/members"));
        assert!(!text.contains("/webhooks/42/bans"));
        assert!(!text.contains("/webhooks/42/audit-logs"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
