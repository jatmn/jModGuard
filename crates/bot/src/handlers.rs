//! Discord gateway event handlers: basic member/message reports,
//! automod pipeline, and command dispatch.

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::gateway::ActivityData;
use serenity::model::channel::Message;
use serenity::model::event::{GuildMemberUpdateEvent, MessageUpdateEvent};
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

fn select_update_author(
    event_author: Option<u64>,
    text_author: Option<u64>,
    image_author: Option<u64>,
) -> Option<u64> {
    event_author.or(text_author).or(image_author)
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
    async fn ready(&self, ctx: Context, ready: Ready) {
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

        // Per-shard startup: seed + sync each guild once.
        if !self
            .app
            .slash_synced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            let app = Arc::clone(self.app());
            tokio::spawn(async move {
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
                    // Joining/ready does not insert guild_settings — inheritance
                    // from global must remain unblocked until an operator sets
                    // an intentional override.
                    if !commands::sync_slash_commands(&app, &ctx, gid).await {
                        failed = true;
                    }
                    let mut vars = BTreeMap::new();
                    vars.insert(
                        "content".into(),
                        format!("Bot started (v{})", env!("CARGO_PKG_VERSION")),
                    );
                    logging::emit(&app, &ctx, gid, "system", vars).await;
                }
                if failed {
                    app.retry_slash_registry_version();
                }
                tracing::info!("startup slash sync complete");
            });
        }
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        let app = Arc::clone(self.app());
        let gid = guild.id;
        if let Err(error) = crate::enrollment_repair::observe_gateway_membership(
            &app.db,
            &app.tenant,
            gid.get(),
            guild.joined_at.timestamp_nanos_opt(),
            guild.unavailable,
            true,
        )
        .await
        {
            tracing::warn!(
                guild_id = gid.get(),
                "guild membership observation deferred: {error:#}"
            );
        }
        // No guild_settings seed on join — global inheritance stays unblocked.
        if !commands::sync_slash_commands(&app, &ctx, gid).await {
            app.retry_slash_registry_version();
        }

        if is_new.unwrap_or(false) {
            tracing::info!("joined new guild {gid} ({})", guild.name);
        }
    }

    async fn guild_delete(&self, _ctx: Context, incomplete: UnavailableGuild, full: Option<Guild>) {
        self.app.image_spam.invalidate_guild(incomplete.id.get());
        self.app.text_spam.invalidate_guild(incomplete.id.get());
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
        app.automod.member_joined(guild_id.get(), uid);
        logging::emit_member_event(
            &app,
            &ctx,
            guild_id,
            "member_join",
            &new_member.user,
            BTreeMap::new(),
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
        let mut extra = BTreeMap::new();
        extra.insert(
            "duration".into(),
            member_data
                .and_then(|m| m.joined_at)
                .map(|joined| format!("joined at {joined}"))
                .unwrap_or_else(|| "unavailable".into()),
        );
        logging::emit_member_event(&app, &ctx, guild_id, "member_leave", &user, extra).await;
    }

    async fn guild_ban_addition(&self, ctx: Context, guild_id: GuildId, banned_user: User) {
        let app = Arc::clone(self.app());
        let enrollment = crate::cases::capture(&app, guild_id.get()).await;
        let _ = db::record_enrolled_ban(
            &app.db,
            enrollment.clone(),
            banned_user.id.get() as i64,
            None,
            "banned (audit)",
        )
        .await;
        if !app.consume_gateway_case_suppression(
            crate::state::GatewayCaseAction::Ban,
            guild_id.get(),
            banned_user.id.get(),
        ) {
            crate::cases::record(
                &app,
                enrollment,
                crate::db::guild_data::CaseAction::Ban,
                crate::db::guild_data::CaseOutcome::Succeeded,
                banned_user.id.get(),
                None,
                Some("banned".into()),
                crate::db::guild_data::CaseEvidence::Unavailable,
                crate::cases::application_source(),
            )
            .await;
        }
        let mut extra = BTreeMap::new();
        extra.insert("reason".into(), "banned".into());
        logging::emit_member_event(&app, &ctx, guild_id, "member_ban", &banned_user, extra).await;
    }

    async fn guild_ban_removal(&self, ctx: Context, guild_id: GuildId, unbanned_user: User) {
        let app = Arc::clone(self.app());
        let enrollment = crate::cases::capture(&app, guild_id.get()).await;
        let _ =
            db::mark_enrolled_unbanned(&app.db, enrollment.clone(), unbanned_user.id.get() as i64)
                .await;
        if !app.consume_gateway_case_suppression(
            crate::state::GatewayCaseAction::Unban,
            guild_id.get(),
            unbanned_user.id.get(),
        ) {
            crate::cases::record(
                &app,
                enrollment,
                crate::db::guild_data::CaseAction::Unban,
                crate::db::guild_data::CaseOutcome::Succeeded,
                unbanned_user.id.get(),
                None,
                Some("unbanned".into()),
                crate::db::guild_data::CaseEvidence::Unavailable,
                crate::cases::application_source(),
            )
            .await;
        }
        let mut extra = BTreeMap::new();
        extra.insert("reason".into(), "unbanned".into());
        logging::emit_member_event(&app, &ctx, guild_id, "member_unban", &unbanned_user, extra)
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
        crate::logging::remember_message(
            &app,
            guild_id.get(),
            msg.channel_id.get(),
            msg.id.get(),
            Some(msg.author.id.get()),
            &msg.content,
        );
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
        if let Some(guild) = event.guild_id {
            let channel = event.channel_id.get();
            let message = event.id.get();
            let update = UpdateObservation {
                author: select_update_author(
                    event.author.as_ref().map(|user| user.id.get()),
                    self.app.text_spam.author_of(guild.get(), channel, message),
                    self.app.image_spam.author_of(guild.get(), channel, message),
                ),
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
                    if let Some(content) = event.content.as_deref() {
                        crate::logging::remember_message(
                            &self.app,
                            guild.get(),
                            event.channel_id.get(),
                            event.id.get(),
                            update.author,
                            content,
                        );
                    }
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
        if let Some(report) = logging::message_update_report(&event) {
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
        self.app
            .image_spam
            .invalidate(guild_id.get(), channel_id.get(), deleted_message_id.get());
        self.app
            .text_spam
            .invalidate(guild_id.get(), channel_id.get(), deleted_message_id.get());
        let report =
            logging::message_delete_report(&self.app, guild_id, channel_id, deleted_message_id);
        logging::emit(&self.app, &ctx, guild_id, report.event, report.vars).await;
    }

    async fn message_delete_bulk(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        multiple_deleted_messages_ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        let Some(guild_id) = guild_id else { return };
        for id in &multiple_deleted_messages_ids {
            self.app
                .image_spam
                .invalidate(guild_id.get(), channel_id.get(), id.get());
            self.app
                .text_spam
                .invalidate(guild_id.get(), channel_id.get(), id.get());
        }
        let mut vars = BTreeMap::new();
        vars.insert("channel".into(), format!("<#{}>", channel_id));
        vars.insert(
            "count".into(),
            multiple_deleted_messages_ids.len().to_string(),
        );
        logging::emit(&self.app, &ctx, guild_id, "message_bulk_delete", vars).await;
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
    use super::{audit_case_action, select_update_author};
    use serenity::model::guild::audit_log::{Action, MemberAction};

    #[test]
    fn update_author_prefers_event_then_text_then_image_observation() {
        assert_eq!(select_update_author(Some(1), Some(2), Some(3)), Some(1));
        assert_eq!(select_update_author(None, Some(2), Some(3)), Some(2));
        assert_eq!(select_update_author(None, None, Some(3)), Some(3));
        assert_eq!(select_update_author(None, None, None), None);
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
