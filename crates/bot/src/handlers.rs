//! Discord gateway event handlers: basic member/message reports,
//! automod pipeline, and command dispatch.

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::gateway::ActivityData;
use serenity::model::channel::{GuildChannel, Message, PartialGuildChannel};
use serenity::model::event::{GuildMemberUpdateEvent, MessageUpdateEvent, ResumedEvent};
use serenity::model::gateway::Ready;
use serenity::model::guild::{Guild, Member, Role, UnavailableGuild};
use serenity::model::id::{GuildId, MessageId, RoleId};
use serenity::model::prelude::{ChannelId, User};
use serenity::model::voice::VoiceState;
use std::collections::BTreeMap;
use std::sync::Arc;

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

fn record_unresolved_gateway_observation(suppressed_by_owned_action: bool) -> bool {
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
            self.app.reset_slash_guild_retry(incomplete.id.get());
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
        let observed = chrono::Utc::now();
        let observation = self.app.member_audits.observe_event(
            guild_id.get(),
            banned_user.id.get(),
            "member_ban",
            observed.timestamp_millis(),
        );
        let app = Arc::clone(self.app());
        let enrollment = crate::cases::capture(&app, guild_id.get()).await;
        crate::member_directory::observe_ban(
            &app,
            guild_id.get(),
            &banned_user,
            observed.timestamp_millis(),
        )
        .await;
        let echo = echo.resolve();
        let observed_at = observed.to_rfc3339();
        // A bot echo is not a second ban and not a manual moderation record.
        // Consumed echoes stay remembered until expiry, unban or release.
        // If Discord applied the ban but the command never stored a row, that
        // owned echo still inserts the one active row, marked with this
        // attempt's operation id, and does not add a member_ban event. A
        // manual delivery inserts one row and one
        // member_ban event before the audit wait, so an unban during that
        // wait can mark the row inactive. A later delivery does not insert
        // and does not replace the actor or reason already stored.
        let ban_write = crate::logging::log_persistence_result(
            "gateway ban row",
            db::record_gateway_ban(
                &app.db,
                enrollment.clone(),
                banned_user.id.get() as i64,
                Some(&banned_user.name),
                banned_user.global_name.as_deref(),
                Some(&observed_at),
                echo.owned_by_bot,
                echo.operation_id.as_deref(),
            )
            .await,
        );
        let ban_row_id = ban_write.and_then(gateway_ban_row_to_attribute);
        // Every unsuppressed delivery is a distinct unresolved observation.
        // A matching saved status is not an action identity: an opposite event
        // may have been missed while Gateway coverage was unavailable.
        if record_unresolved_gateway_observation(echo.suppress_case) {
            let recorded = crate::cases::record(
                &app,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Ban,
                crate::db::guild_data::CaseOutcome::Succeeded,
                banned_user.id.get(),
                None,
                Some("banned".into()),
                crate::db::guild_data::CaseEvidence::Unavailable,
                crate::db::guild_data::CaseSource::GatewayObservation {
                    observation_id: crate::db::guild_data::CaseId::new(),
                },
            )
            .await;
            if recorded.is_none() {
                crate::member_directory::mark_user_gap(
                    &app,
                    guild_id.get(),
                    banned_user.id.get() as i64,
                    "ban case persistence unavailable",
                )
                .await;
            }
        }
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
        let enrollment = crate::cases::capture(&app, guild_id.get()).await;
        crate::member_directory::observe_unban(
            &app,
            guild_id.get(),
            &unbanned_user,
            observed.timestamp_millis(),
        )
        .await;
        let _ =
            db::mark_enrolled_unbanned(&app.db, enrollment.clone(), unbanned_user.id.get() as i64)
                .await;
        let suppressed = app.consume_gateway_case_suppression(
            crate::state::GatewayCaseAction::Unban,
            guild_id.get(),
            unbanned_user.id.get(),
        );
        if record_unresolved_gateway_observation(suppressed) {
            let recorded = crate::cases::record(
                &app,
                enrollment,
                crate::db::guild_data::CaseAction::Unban,
                crate::db::guild_data::CaseOutcome::Succeeded,
                unbanned_user.id.get(),
                None,
                Some("unbanned".into()),
                crate::db::guild_data::CaseEvidence::Unavailable,
                crate::db::guild_data::CaseSource::GatewayObservation {
                    observation_id: crate::db::guild_data::CaseId::new(),
                },
            )
            .await;
            if recorded.is_none() {
                crate::member_directory::mark_user_gap(
                    &app,
                    guild_id.get(),
                    unbanned_user.id.get() as i64,
                    "unban case persistence unavailable",
                )
                .await;
            }
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
            // The audit snowflake is the durable action identity. A repeat ban
            // or unban has a different ID; a replay of this entry is idempotent.
            if let Some(enrollment) = crate::cases::capture(self.app(), guild_id.get()).await {
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
            .invalidate(guild_id.get(), channel_id.get(), deleted_message_id.get());
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
        audit_case_action, departure_joined_at_ms, gateway_ban_row_to_attribute,
        log_message_diagnostic, record_unresolved_gateway_observation, select_update_authors,
    };
    use crate::{db::Db, logging, state::BotState};
    use serenity::model::channel::Message;
    use serenity::model::event::MessageUpdateEvent;
    use serenity::model::guild::audit_log::{Action, MemberAction};
    use serenity::model::id::{ChannelId, GuildId, MessageId};

    #[test]
    fn unsuppressed_gateway_actions_survive_equal_saved_state() {
        assert!(record_unresolved_gateway_observation(false));
        assert!(!record_unresolved_gateway_observation(true));
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
}
