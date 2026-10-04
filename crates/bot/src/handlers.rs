//! Discord gateway event handlers: basic member/message reports,
//! automod pipeline, and command dispatch.

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::gateway::ActivityData;
use serenity::model::channel::Message;
use serenity::model::event::{GuildMemberUpdateEvent, MessageUpdateEvent, ResumedEvent};
use serenity::model::gateway::Ready;
use serenity::model::guild::{Guild, Member, UnavailableGuild};
use serenity::model::id::{GuildId, MessageId};
use serenity::model::prelude::{ChannelId, User};
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
        self.app.snapshots.install_collector(&self.app, &_ctx);
        self.app.snapshots.gap();
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
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
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    maintenance_app.snapshots.maintain(&maintenance_app).await;
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
        self.app.invalidate_member_sync_scope();
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
        let observed_at = observed.to_rfc3339();
        // Record the supplied identity before any audit HTTP wait. An unban
        // observed during that wait must be able to mark this row inactive.
        let ban_row_id = db::record_enrolled_ban_with_identity(
            &app.db,
            enrollment.clone(),
            banned_user.id.get() as i64,
            Some(&banned_user.name),
            banned_user.global_name.as_deref(),
            None,
            None,
            "Reason unavailable",
            Some(&observed_at),
        )
        .await
        .ok();
        // Decide whether this Gateway event is the bot's own echo before the
        // best-effort audit lookup can outlive its bounded suppression token.
        let suppressed = app.consume_gateway_case_suppression(
            crate::state::GatewayCaseAction::Ban,
            guild_id.get(),
            banned_user.id.get(),
        );
        // Every unsuppressed delivery is a distinct unresolved observation.
        // A matching saved status is not an action identity: an opposite event
        // may have been missed while Gateway coverage was unavailable.
        if record_unresolved_gateway_observation(suppressed) {
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
                    source: crate::db::guild_data::CaseSource::DiscordAudit { entry_id },
                    action: action.clone(),
                    outcome: crate::db::guild_data::CaseOutcome::Succeeded,
                    subject_id: target.get() as i64,
                    actor_id: i64::try_from(entry.user_id.get()).ok(),
                    occurred_at_ms: entry.id.created_at().timestamp_millis(),
                    reason: entry.reason.clone(),
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
        tracing::debug!(
            "message {} from {} in guild {} channel {}: {} bytes, {} attachments",
            msg.id,
            msg.author.id,
            guild_id,
            msg.channel_id,
            msg.content.len(),
            msg.attachments.len()
        );

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
        let member_roles: Vec<u64> = member
            .as_ref()
            .map(|m| m.roles.iter().map(|r| r.get()).collect())
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
            let _ = commands::handle_prefix(&app, &ctx, cfg_arc, &msg).await;
        }
    }

    async fn message_update(
        &self,
        ctx: Context,
        _old: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
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
        let old_to = old_if_available.and_then(|o| o.communication_disabled_until);
        let new_to = new.communication_disabled_until;
        if old_to != new_to {
            let enrollment = crate::cases::capture(&app, guild_id.get()).await;
            let mut vars =
                templates::user_vars(new.user.id.get(), &new.user.name, new.display_name());
            if new_to.is_some() {
                vars.insert("duration".into(), "timed out".into());
                vars.insert("reason".into(), "moderation action".into());
                logging::emit(&app, &ctx, guild_id, "timeout_add", vars).await;
                let _ = db::insert_enrolled_event(
                    &app.db,
                    enrollment.clone(),
                    "timeout_add",
                    Some(new.user.id.get() as i64),
                    None,
                    &serde_json::json!({"until": new_to.map(|t| t.to_string())}),
                )
                .await;
            } else {
                vars.insert("reason".into(), "timeout expired or removed".into());
                logging::emit(&app, &ctx, guild_id, "timeout_remove", vars).await;
                let _ = db::insert_enrolled_event(
                    &app.db,
                    enrollment.clone(),
                    "timeout_remove",
                    Some(new.user.id.get() as i64),
                    None,
                    &serde_json::json!({}),
                )
                .await;
            }
        }
    }

    async fn interaction_create(
        &self,
        ctx: Context,
        interaction: serenity::model::prelude::Interaction,
    ) {
        let app = Arc::clone(self.app());
        commands::handle_interaction(&app, &ctx, interaction).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        audit_case_action, departure_joined_at_ms, record_unresolved_gateway_observation,
        select_update_authors,
    };
    use crate::{db::Db, logging, state::BotState};
    use serenity::model::event::MessageUpdateEvent;
    use serenity::model::guild::audit_log::{Action, MemberAction};

    #[test]
    fn unsuppressed_gateway_actions_survive_equal_saved_state() {
        assert!(record_unresolved_gateway_observation(false));
        assert!(!record_unresolved_gateway_observation(true));
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
