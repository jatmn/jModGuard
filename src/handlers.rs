//! Discord gateway event handlers: member lifecycle, message tracking,
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
use crate::state::AppState;
use crate::templates;

pub struct Handler {
    pub app: Arc<AppState>,
}

impl Handler {
    fn app(&self) -> &Arc<AppState> {
        &self.app
    }
}

fn attachments_json(msg: &Message) -> String {
    serde_json::to_string(
        &msg.attachments
            .iter()
            .map(|a| serde_json::json!({"filename": a.filename, "url": a.url, "size": a.size, "content_type": a.content_type}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into())
}

fn truncate_2000(s: &str) -> String {
    let t: String = s.chars().take(1900).collect();
    if t.chars().count() >= 1900 {
        format!("{t}…")
    } else {
        t
    }
}

fn user_display(member: &Member) -> String {
    member.display_name().to_string()
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
                    let _ = db::insert_event(
                        &app.db,
                        gid.get() as i64,
                        "system",
                        None,
                        None,
                        &serde_json::json!({
                            "content": format!("Bot started (v{})", env!("CARGO_PKG_VERSION"))
                        }),
                    )
                    .await;
                }
                if failed {
                    app.retry_slash_registry_version();
                }
                tracing::info!("startup slash sync + backfill complete");
            });
        }
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        let app = Arc::clone(self.app());
        let gid = guild.id;
        // No guild_settings seed on join — global inheritance stays unblocked.
        if !commands::sync_slash_commands(&app, &ctx, gid).await {
            app.retry_slash_registry_version();
        }

        // Guild lifecycle is distinct from settings/explicit repair. A
        // reconnect preserves terminal operator errors; dormant work is
        // reactivated, and a genuinely new guild receives initial work.
        if let Err(error) = db::activate_nickname_reconcile_for_guild_create(
            &app.db,
            gid.get() as i64,
            is_new.unwrap_or(false),
        )
        .await
        {
            tracing::warn!(
                guild_id = gid.get(),
                "failed to queue nickname reconciliation on guild_create: {error:#}"
            );
        }

        // Backfill member tracking from cache (bounded batch).
        if !guild.members.is_empty() {
            let members: Vec<(u64, String)> = guild
                .members
                .values()
                .map(|m| (m.user.id.get(), m.user.name.clone()))
                .collect();
            for (uid, name) in members {
                let _ = db::backfill_member(&app.db, gid.get() as i64, uid as i64, &name).await;
            }
        }

        if is_new.unwrap_or(false) {
            tracing::info!("joined new guild {gid} ({})", guild.name);
        }
    }

    async fn guild_delete(
        &self,
        _ctx: Context,
        incomplete: UnavailableGuild,
        _full: Option<Guild>,
    ) {
        // `unavailable=true` is a temporary Discord outage, not a departure.
        // A real removal remains dormant until guild_create reactivates it.
        if !incomplete.unavailable
            && let Err(error) = db::dormancy_nickname_reconcile(
                &self.app().db,
                incomplete.id.get() as i64,
                "bot removed from guild",
            )
            .await
        {
            tracing::warn!(
                guild_id = incomplete.id.get(),
                "failed to mark nickname work dormant: {error:#}"
            );
        }
    }

    async fn guild_member_addition(&self, ctx: Context, new_member: Member) {
        let app = Arc::clone(self.app());
        let guild_id = new_member.guild_id;
        let uid = new_member.user.id.get();
        app.automod.member_joined(guild_id.get(), uid);
        let _ = db::member_joined(
            &app.db,
            guild_id.get() as i64,
            uid as i64,
            &new_member.user.name,
        )
        .await;
        let _ = db::upsert_user(
            &app.db,
            uid as i64,
            &new_member.user.name,
            new_member.user.global_name.as_deref(),
            None,
        )
        .await;
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
        _member_data: Option<Member>,
    ) {
        let app = Arc::clone(self.app());
        let _ = db::member_left(&app.db, guild_id.get() as i64, user.id.get() as i64).await;
        let mut extra = BTreeMap::new();
        if let Ok(Some(row)) =
            db::member_row(&app.db, guild_id.get() as i64, user.id.get() as i64).await
            && let (Some(first), Some(last)) = (&row.first_joined_at, &row.last_joined_at)
        {
            extra.insert(
                "duration".into(),
                format!("{first} → {last} ({} joins)", row.join_count),
            );
        }
        logging::emit_member_event(&app, &ctx, guild_id, "member_leave", &user, extra).await;
    }

    async fn guild_ban_addition(&self, ctx: Context, guild_id: GuildId, banned_user: User) {
        let app = Arc::clone(self.app());
        let _ = db::record_ban(
            &app.db,
            guild_id.get() as i64,
            banned_user.id.get() as i64,
            None,
            "banned (audit)",
        )
        .await;
        let mut extra = BTreeMap::new();
        extra.insert("reason".into(), "banned".into());
        logging::emit_member_event(&app, &ctx, guild_id, "member_ban", &banned_user, extra).await;
    }

    async fn guild_ban_removal(&self, ctx: Context, guild_id: GuildId, unbanned_user: User) {
        let app = Arc::clone(self.app());
        let _ = db::mark_unbanned(
            &app.db,
            guild_id.get() as i64,
            unbanned_user.id.get() as i64,
        )
        .await;
        let mut extra = BTreeMap::new();
        extra.insert("reason".into(), "unbanned".into());
        logging::emit_member_event(&app, &ctx, guild_id, "member_unban", &unbanned_user, extra)
            .await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let app = Arc::clone(self.app());
        let Some(guild_id) = msg.guild_id else { return };

        // ignore other bots and ourselves
        if msg.author.bot {
            return;
        }

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

        // Best-effort tracking runs after moderation and command dispatch, and
        // uses one transaction/deadline rather than three sequential remote
        // mutations. A database outage must not hold the enforcement path.
        let _ = db::record_message_activity(
            &app.db,
            msg.id.get() as i64,
            guild_id.get() as i64,
            msg.channel_id.get() as i64,
            msg.author.id.get() as i64,
            &msg.author.name,
            msg.author.global_name.as_deref(),
            &msg.content,
            &attachments_json(&msg),
            &msg.timestamp.to_string(),
        )
        .await;
    }

    async fn message_update(
        &self,
        ctx: Context,
        _old: Option<Message>,
        new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        let app = Arc::clone(self.app());
        let Some(guild_id) = event.guild_id else {
            return;
        };
        let Some(new_content) = new
            .as_ref()
            .map(|m| m.content.clone())
            .or_else(|| event.content.clone())
        else {
            return;
        };
        let id = event.id.get() as i64;
        if let Ok(Some((old_content, _))) =
            db::mark_edited_with_previous(&app.db, id, &new_content).await
        {
            if old_content != new_content {
                let mut vars = BTreeMap::new();
                vars.insert("channel".into(), format!("<#{}>", event.channel_id));
                vars.insert("before".into(), truncate_2000(&old_content));
                vars.insert("after".into(), truncate_2000(&new_content));
                if let Some(author) = &event.author {
                    vars.insert("user".into(), author.name.clone());
                    vars.insert("mention".into(), format!("<@{}>", author.id));
                }
                logging::emit(&app, &ctx, guild_id, "message_edit", vars).await;
                let _ = db::insert_event(
                    &app.db,
                    guild_id.get() as i64,
                    "message_edit",
                    event.author.as_ref().map(|a| a.id.get() as i64),
                    None,
                    &serde_json::json!({"message_id": id, "before": old_content, "after": new_content, "channel_id": event.channel_id.get()}),
                )
                .await;
            }
        }
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        let app = Arc::clone(self.app());
        let Some(guild_id) = guild_id else { return };
        let id = deleted_message_id.get() as i64;
        let content = db::mark_deleted_with_previous(&app.db, id)
            .await
            .ok()
            .flatten()
            .map(|(c, _)| c);
        let mut vars = BTreeMap::new();
        vars.insert("channel".into(), format!("<#{}>", channel_id));
        vars.insert(
            "content".into(),
            truncate_2000(content.as_deref().unwrap_or("*unknown*")),
        );
        logging::emit(&app, &ctx, guild_id, "message_delete", vars).await;
        let _ = db::insert_event(
            &app.db,
            guild_id.get() as i64,
            "message_delete",
            None,
            None,
            &serde_json::json!({"message_id": id, "channel_id": channel_id.get(), "content": content}),
        )
        .await;
    }

    async fn message_delete_bulk(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        multiple_deleted_messages_ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        let app = Arc::clone(self.app());
        let Some(guild_id) = guild_id else { return };
        let n = multiple_deleted_messages_ids.len();
        for id in &multiple_deleted_messages_ids {
            let _ = db::mark_deleted(&app.db, id.get() as i64).await;
        }
        let mut vars = BTreeMap::new();
        vars.insert("channel".into(), format!("<#{}>", channel_id));
        vars.insert("count".into(), n.to_string());
        logging::emit(&app, &ctx, guild_id, "message_bulk_delete", vars).await;
        let _ = db::insert_event(
            &app.db,
            guild_id.get() as i64,
            "message_bulk_delete",
            None,
            None,
            &serde_json::json!({"channel_id": channel_id.get(), "count": n}),
        )
        .await;
    }

    async fn guild_member_update(
        &self,
        ctx: Context,
        old_if_available: Option<Member>,
        new_if_available: Option<Member>,
        _event: GuildMemberUpdateEvent,
    ) {
        let app = Arc::clone(self.app());
        let Some(new) = new_if_available else { return };
        let guild_id = new.guild_id;
        let old_to = old_if_available.and_then(|o| o.communication_disabled_until);
        let new_to = new.communication_disabled_until;
        if old_to != new_to {
            let mut vars =
                templates::user_vars(new.user.id.get(), &new.user.name, new.display_name());
            if new_to.is_some() {
                vars.insert("duration".into(), "timed out".into());
                vars.insert("reason".into(), "moderation action".into());
                logging::emit(&app, &ctx, guild_id, "timeout_add", vars).await;
                let _ = db::insert_event(
                    &app.db,
                    guild_id.get() as i64,
                    "timeout_add",
                    Some(new.user.id.get() as i64),
                    None,
                    &serde_json::json!({"until": new_to.map(|t| t.to_string())}),
                )
                .await;
            } else {
                vars.insert("reason".into(), "timeout expired or removed".into());
                logging::emit(&app, &ctx, guild_id, "timeout_remove", vars).await;
                let _ = db::insert_event(
                    &app.db,
                    guild_id.get() as i64,
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
