//! Dedicated spam delete-and-ban path. Never uses the generic automod executor.
use crate::db::guild_data::{CaseAction, CaseEvidence, CaseOutcome, TextEvidence};
use crate::state::BotState;
use parking_lot::Mutex;
use serenity::client::Context;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEEP: Duration = Duration::from_secs(90);

fn release_gateway_ban_suppression(outcome: &CaseOutcome) -> bool {
    matches!(outcome, CaseOutcome::Failed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportDisposition {
    /// The match produced an operator-visible outcome or was deliberately
    /// cancelled by a current policy/lifecycle gate.
    Consumed,
    /// No outcome was produced because transient state or a stale test proof
    /// prevented delivery. Text detection may release its exact reservation.
    Retryable,
}

#[derive(Clone)]
pub enum MatchPolicy {
    Image(jmodguard::image_spam::Policy),
    Text(jmodguard::text_spam::Policy),
}

impl MatchPolicy {
    fn parse(kind: Kind, rule: &crate::types::RuleDef) -> Option<Self> {
        match kind {
            Kind::Image => jmodguard::image_spam::Policy::parse(rule)
                .ok()
                .map(Self::Image),
            Kind::Text => jmodguard::text_spam::Policy::parse(rule)
                .ok()
                .map(Self::Text),
        }
    }

    fn enforce(&self) -> bool {
        match self {
            Self::Image(policy) => policy.enforce,
            Self::Text(policy) => policy.enforce,
        }
    }

    fn still_proving(&self, app: &BotState, found: &Match, now: Instant) -> bool {
        self.still_proving_with_deletes(app, found, now, &[])
    }

    fn still_proving_with_deletes(
        &self,
        app: &BotState,
        found: &Match,
        now: Instant,
        own_deletes: &[crate::image_spam::OwnDelete],
    ) -> bool {
        match self {
            Self::Image(policy) => {
                let proof = crate::image_spam::MatchProof {
                    token: found.token,
                    messages: &found.messages,
                    evidence: &found.excerpts,
                    policy,
                    now,
                };
                if own_deletes.is_empty() {
                    app.image_spam
                        .match_still_proving(found.guild, found.author, proof)
                } else {
                    app.image_spam.enforcement_still_proving(
                        found.guild,
                        found.author,
                        proof,
                        own_deletes,
                    )
                }
            }
            Self::Text(policy) => app.text_spam.match_still_proving(
                found.guild,
                found.author,
                found.token,
                &found.messages,
                policy,
                now,
            ),
        }
    }
}

pub struct Match {
    pub kind: Kind,
    pub guild: u64,
    pub author: u64,
    pub channel: u64,
    pub policy: MatchPolicy,
    pub token: (u64, u64),
    pub messages: Vec<(u64, u64)>,
    pub excerpts: Vec<TextEvidence>,
    pub proving_hashes: Vec<jmodguard::image_spam::Hash>,
    pub attachments: Vec<ProvingAttachment>,
    pub reason: String,
}

#[derive(Clone)]
pub struct ProvingAttachment {
    pub channel: u64,
    pub message: u64,
    pub file: jmodguard::image_spam::Attachment,
    pub proving_hash: jmodguard::image_spam::Hash,
}

fn fetched_matches_proving(
    fetched: &jmodguard::image_spam::Hash,
    proving: &jmodguard::image_spam::Hash,
) -> bool {
    fetched == proving
}

struct Applied {
    until: Instant,
    banned: bool,
    banning: bool,
    deleted: BTreeSet<(u64, u64)>,
    deleting: BTreeSet<(u64, u64)>,
}

impl Applied {
    fn new(now: Instant) -> Self {
        Self {
            until: now + KEEP,
            banned: false,
            banning: false,
            deleted: BTreeSet::new(),
            deleting: BTreeSet::new(),
        }
    }
}

#[derive(Default)]
pub struct Gate {
    inner: Mutex<HashMap<(u64, u64), Applied>>,
}

impl Gate {
    fn prune(&self, now: Instant) {
        self.inner.lock().retain(|_, applied| applied.until > now);
    }

    fn claim_delete(
        &self,
        guild: u64,
        author: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> bool {
        self.prune(now);
        let mut inner = self.inner.lock();
        let applied = inner
            .entry((guild, author))
            .or_insert_with(|| Applied::new(now));
        applied.until = now + KEEP;
        let key = (channel, message);
        if applied.deleted.contains(&key) || applied.deleting.contains(&key) {
            return false;
        }
        applied.deleting.insert(key);
        true
    }

    fn finish_delete(
        &self,
        guild: u64,
        author: u64,
        channel: u64,
        message: u64,
        now: Instant,
        success: bool,
    ) {
        let mut inner = self.inner.lock();
        let Some(applied) = inner.get_mut(&(guild, author)) else {
            return;
        };
        applied.deleting.remove(&(channel, message));
        if success {
            applied.deleted.insert((channel, message));
        }
        applied.until = now + KEEP;
    }

    fn claim_ban(&self, guild: u64, author: u64, now: Instant) -> bool {
        self.prune(now);
        let mut inner = self.inner.lock();
        let applied = inner
            .entry((guild, author))
            .or_insert_with(|| Applied::new(now));
        applied.until = now + KEEP;
        if applied.banned || applied.banning {
            return false;
        }
        applied.banning = true;
        true
    }

    fn finish_ban(&self, guild: u64, author: u64, now: Instant, success: bool) {
        let mut inner = self.inner.lock();
        let Some(applied) = inner.get_mut(&(guild, author)) else {
            return;
        };
        applied.banning = false;
        if success {
            applied.banned = true;
        }
        applied.until = now + KEEP;
    }
}

pub(crate) async fn report(app: &Arc<BotState>, ctx: &Context, found: Match) -> ReportDisposition {
    let (rule, name) = match found.kind {
        Kind::Image => ("attachment_flood", "Exact image spam"),
        Kind::Text => ("repeat_text", "Repeated text"),
    };
    if !found.policy.enforce() {
        if !found.policy.still_proving(app, &found, Instant::now()) {
            return ReportDisposition::Retryable;
        }
        let vars = BTreeMap::from([
            ("rule".into(), format!("{name} test")),
            ("mention".into(), format!("<@{}>", found.author)),
            ("user_id".into(), found.author.to_string()),
            ("source_channel_id".into(), found.channel.to_string()),
            ("channel".into(), format!("<#{}>", found.channel)),
            ("action".into(), "log only; no punishment".into()),
            ("reason".into(), found.reason.clone()),
            ("content".into(), automod_evidence(&found)),
        ]);
        crate::logging::emit(app, ctx, GuildId::new(found.guild), "automod", vars).await;
        return ReportDisposition::Consumed;
    }
    enforce(
        app,
        std::sync::Arc::clone(&ctx.http),
        ctx.cache.as_ref(),
        found,
        rule,
        name,
    )
    .await
}

pub(crate) async fn enforce(
    app: &Arc<BotState>,
    http: Arc<serenity::http::Http>,
    cache: &serenity::cache::Cache,
    found: Match,
    rule: &str,
    name: &str,
) -> ReportDisposition {
    if app.poll_config_version().await.is_err() {
        return ReportDisposition::Retryable;
    }
    let Ok(cfg) = app.configs.get(&app.db, found.guild).await else {
        return ReportDisposition::Retryable;
    };
    let Some(rule_def) = cfg.rules.get(rule) else {
        return ReportDisposition::Consumed;
    };
    let policy = match found.kind {
        Kind::Image => crate::image_spam::policy_for_config(&cfg, rule_def).map(MatchPolicy::Image),
        Kind::Text => MatchPolicy::parse(found.kind, rule_def),
    };
    let Some(policy) = policy else {
        return ReportDisposition::Consumed;
    };
    if !policy.enforce() || !rule_def.enabled {
        return ReportDisposition::Consumed;
    }
    if crate::db::guild_stores::guild_is_write_fenced(&app.db, found.guild as i64)
        .await
        .unwrap_or(true)
    {
        return ReportDisposition::Retryable;
    }
    let enrollment = crate::cases::capture(app, found.guild).await;
    let Some(enrollment) = enrollment.clone() else {
        return ReportDisposition::Consumed;
    };
    if app.tenant.live(enrollment.clone()).await.is_err() {
        return ReportDisposition::Consumed;
    }
    let now = Instant::now();
    let token = (cfg.version, cfg.durable_version);
    if token != found.token {
        return ReportDisposition::Consumed;
    }
    if !policy.still_proving(app, &found, now) {
        let vars = BTreeMap::from([
            ("rule".into(), name.into()),
            ("mention".into(), format!("<@{}>", found.author)),
            ("user_id".into(), found.author.to_string()),
            ("source_channel_id".into(), found.channel.to_string()),
            ("channel".into(), format!("<#{}>", found.channel)),
            (
                "action".into(),
                "enforce skipped; matched observations were invalidated".into(),
            ),
            ("reason".into(), found.reason.clone()),
            (
                "content".into(),
                "one or more proving messages were edited, deleted, or expired".into(),
            ),
        ]);
        crate::logging::emit_to(
            app,
            http.as_ref(),
            GuildId::new(found.guild),
            "automod",
            vars,
        )
        .await;
        return ReportDisposition::Consumed;
    }
    punish_proven_match(app, http, cache, found, rule, name, rule_def, enrollment).await
}

// Member lookup, earlier deletes, and ban preparation can all outlive the
// admitted configuration or image window. Revalidate at each dispatch boundary.
async fn image_match_still_current(
    app: &BotState,
    found: &Match,
    own_deletes: &[crate::image_spam::OwnDelete],
) -> bool {
    if found.kind != Kind::Image {
        return true;
    }
    if app.poll_config_version().await.is_err() {
        return false;
    }
    let Ok(cfg) = app.configs.get(&app.db, found.guild).await else {
        return false;
    };
    (cfg.version, cfg.durable_version) == found.token
        && found
            .policy
            .still_proving_with_deletes(app, found, Instant::now(), own_deletes)
}

#[allow(clippy::too_many_arguments)]
async fn punish_proven_match(
    app: &Arc<BotState>,
    http: Arc<serenity::http::Http>,
    cache: &serenity::cache::Cache,
    found: Match,
    rule: &str,
    name: &str,
    rule_def: &jmodguard::types::RuleDef,
    enrollment: crate::db::GuildEnrollment,
) -> ReportDisposition {
    let member = tokio::time::timeout(
        Duration::from_secs(10),
        http.as_ref()
            .get_member(GuildId::new(found.guild), UserId::new(found.author)),
    )
    .await
    .ok()
    .and_then(Result::ok);
    if member.is_none() {
        let vars = BTreeMap::from([
            ("rule".into(), name.into()),
            ("mention".into(), format!("<@{}>", found.author)),
            ("user_id".into(), found.author.to_string()),
            ("source_channel_id".into(), found.channel.to_string()),
            ("channel".into(), format!("<#{}>", found.channel)),
            (
                "action".into(),
                "enforce skipped; membership could not be confirmed".into(),
            ),
            ("reason".into(), found.reason),
            ("content".into(), "member lookup failed or timed out".into()),
        ]);
        crate::logging::emit_to(
            app,
            http.as_ref(),
            GuildId::new(found.guild),
            "automod",
            vars,
        )
        .await;
        return ReportDisposition::Consumed;
    }
    if !found
        .policy
        .clone()
        .still_proving(app, &found, Instant::now())
    {
        let vars = BTreeMap::from([
            ("rule".into(), name.into()),
            ("mention".into(), format!("<@{}>", found.author)),
            ("user_id".into(), found.author.to_string()),
            ("source_channel_id".into(), found.channel.to_string()),
            ("channel".into(), format!("<#{}>", found.channel)),
            (
                "action".into(),
                "enforce skipped; matched observations were invalidated".into(),
            ),
            ("reason".into(), found.reason.clone()),
            (
                "content".into(),
                "one or more proving messages were edited, deleted, or expired".into(),
            ),
        ]);
        crate::logging::emit_to(
            app,
            http.as_ref(),
            GuildId::new(found.guild),
            "automod",
            vars,
        )
        .await;
        return ReportDisposition::Consumed;
    }
    if crate::db::guild_stores::guild_is_write_fenced(&app.db, found.guild as i64)
        .await
        .unwrap_or(true)
    {
        return ReportDisposition::Retryable;
    }
    let roles: Vec<u64> = member
        .as_ref()
        .map(|m| m.roles.iter().map(|r| r.get()).collect())
        .unwrap_or_default();
    let status = crate::commands::cached_moderator_status(
        app,
        cache,
        GuildId::new(found.guild),
        found.author,
        &roles,
    );
    let is_moderator = status == crate::commands::ModeratorStatus::Moderator;
    let withhold_ban = jmodguard::custom_rules::withhold_member_penalty(
        rule_def.skip_moderators,
        status == crate::commands::ModeratorStatus::Unknown,
    );
    if scope_blocks_enforcement(
        rule_def,
        found.author,
        found.messages.iter().map(|(channel, _)| *channel),
        &roles,
        is_moderator,
    ) {
        let vars = BTreeMap::from([
            ("rule".into(), name.into()),
            ("mention".into(), format!("<@{}>", found.author)),
            ("user_id".into(), found.author.to_string()),
            ("source_channel_id".into(), found.channel.to_string()),
            ("channel".into(), format!("<#{}>", found.channel)),
            (
                "action".into(),
                "enforce skipped; exemption recheck failed".into(),
            ),
            ("reason".into(), found.reason),
            (
                "content".into(),
                "matched channel/user/role is whitelisted".into(),
            ),
        ]);
        crate::logging::emit_to(
            app,
            http.as_ref(),
            GuildId::new(found.guild),
            "automod",
            vars,
        )
        .await;
        return ReportDisposition::Consumed;
    }
    let now = Instant::now();
    let mut delete_notes = Vec::new();
    let mut own_deletes = Vec::new();
    let mut proof_stale = false;
    for (channel, message) in &found.messages {
        if !app.activation.may_record() {
            break;
        }
        if !image_match_still_current(app, &found, &own_deletes).await {
            proof_stale = true;
            break;
        }
        if !app
            .spam_gate
            .claim_delete(found.guild, found.author, *channel, *message, now)
        {
            delete_notes.push(format!("{channel}/{message} already handled this window"));
            continue;
        }
        // Register before awaiting HTTP: Gateway may deliver the echo before
        // REST completes. Only a successful response makes this proof usable.
        let own_delete = if found.kind == Kind::Image {
            let Some(deleted) = app.image_spam.begin_delete(
                found.guild,
                found.author,
                found.token,
                *channel,
                *message,
            ) else {
                app.spam_gate.finish_delete(
                    found.guild,
                    found.author,
                    *channel,
                    *message,
                    now,
                    false,
                );
                proof_stale = true;
                break;
            };
            Some(deleted)
        } else {
            None
        };
        let delete_attempt =
            app.automod_delete_marks
                .insert(found.guild, *channel, *message, rule, name);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            crate::discord::race_discord_call(
                &app.activation,
                http.as_ref().delete_message(
                    ChannelId::new(*channel),
                    MessageId::new(*message),
                    Some("spam match"),
                ),
            ),
        )
        .await;
        if let Some(mut deleted) = own_delete {
            // An already-gone 404 is idempotent for reporting, but does not
            // prove this enforcement performed the deletion.
            deleted.finish(matches!(
                &result,
                Ok(crate::discord::DiscordCall::Finished(Ok(())))
            ));
            own_deletes.push(deleted);
        }
        let outcome = match result {
            Ok(crate::discord::DiscordCall::Finished(Ok(()))) => {
                if let Some(settlement) = app.automod_delete_marks.confirm(
                    found.guild,
                    *channel,
                    *message,
                    delete_attempt,
                ) {
                    crate::logging::spawn_reconciled_automod_delete(
                        std::sync::Arc::clone(app),
                        Arc::clone(&http),
                        found.guild,
                        *channel,
                        *message,
                        settlement.rule_id,
                        settlement.rule_name,
                        settlement.retained,
                    );
                }
                CaseOutcome::Succeeded
            }
            Ok(crate::discord::DiscordCall::Finished(Err(error))) => {
                let removal = app.automod_delete_marks.remove_unconfirmed(
                    found.guild,
                    *channel,
                    *message,
                    delete_attempt,
                );
                if removal.action
                    == crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
                {
                    crate::logging::emit_plain_message_delete(
                        app,
                        http.as_ref(),
                        GuildId::new(found.guild),
                        ChannelId::new(*channel),
                        MessageId::new(*message),
                        removal.retained.as_ref(),
                    )
                    .await;
                }
                delete_outcome(&error)
            }
            Ok(crate::discord::DiscordCall::Cancelled) | Err(_) => {
                if let Some(settlement) =
                    app.automod_delete_marks.take_uncertain_gateway_settlement(
                        found.guild,
                        *channel,
                        *message,
                        delete_attempt,
                    )
                {
                    crate::logging::spawn_reconciled_automod_delete(
                        std::sync::Arc::clone(app),
                        Arc::clone(&http),
                        found.guild,
                        *channel,
                        *message,
                        settlement.rule_id,
                        settlement.rule_name,
                        settlement.retained,
                    );
                } else {
                    let removal = app.automod_delete_marks.remove_unconfirmed(
                        found.guild,
                        *channel,
                        *message,
                        delete_attempt,
                    );
                    if removal.action
                        == crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
                    {
                        crate::logging::emit_plain_message_delete(
                            app,
                            http.as_ref(),
                            GuildId::new(found.guild),
                            ChannelId::new(*channel),
                            MessageId::new(*message),
                            removal.retained.as_ref(),
                        )
                        .await;
                    }
                }
                CaseOutcome::Uncertain
            }
        };
        app.spam_gate.finish_delete(
            found.guild,
            found.author,
            *channel,
            *message,
            now,
            matches!(outcome, CaseOutcome::Succeeded),
        );
        delete_notes.push(format!("{channel}/{message} {outcome:?}"));
        if !app.activation.may_record() {
            break;
        }
        crate::cases::record(
            app,
            Some(enrollment.clone()),
            CaseAction::Delete,
            outcome,
            found.author,
            None,
            Some(found.reason.clone()),
            message_evidence(&found.excerpts, *channel, *message, found.author),
            crate::cases::application_source(),
        )
        .await;
    }
    let mut ban_outcome = None;
    if withhold_ban {
        tracing::warn!(
            guild_id = found.guild,
            user_id = found.author,
            rule,
            "{}",
            jmodguard::custom_rules::UNKNOWN_MODERATOR_WARNING
        );
    } else if !proof_stale && app.spam_gate.claim_ban(found.guild, found.author, now) {
        let decision = crate::member_directory::observe_ban_handoff(
            app,
            http.as_ref(),
            found.guild,
            found.author,
            crate::member_directory::MarkerSave::Missing,
        )
        .await;
        match decision {
            crate::member_directory::BanContinuation::Stop => {}
            crate::member_directory::BanContinuation::AlreadyFinished { reason } => {
                crate::cases::settle_present_application_ban(
                    app,
                    Some(enrollment.clone()),
                    found.author,
                    reason.as_deref(),
                )
                .await;
            }
            crate::member_directory::BanContinuation::RecordOnly { reason } => {
                if continue_present_spam_ban(
                    app,
                    enrollment.clone(),
                    found.author,
                    rule,
                    &found.reason,
                    reason.as_deref(),
                )
                .await
                {
                    ban_outcome = Some(CaseOutcome::Succeeded);
                }
            }
            crate::member_directory::BanContinuation::BanThenRecord => {
                if app.activation.may_record() {
                    let pending_ban =
                        crate::member_directory::begin_ban_attempt(app, found.guild, found.author)
                            .await;
                    if app.activation.may_record() {
                        let attempted = match crate::db::guild_data::CaseId::try_from(
                            pending_ban.operation_id().to_owned(),
                        ) {
                            Ok(operation_id) => {
                                crate::cases::begin_application_attempt(
                                    app,
                                    Some(enrollment.clone()),
                                    CaseAction::Ban,
                                    found.author,
                                    None,
                                    Some(found.reason.clone()),
                                    evidence(found.excerpts.clone()),
                                    operation_id,
                                )
                                .await
                            }
                            Err(_) => None,
                        };
                        if !image_match_still_current(app, &found, &own_deletes).await {
                            app.spam_gate
                                .finish_ban(found.guild, found.author, now, false);
                            pending_ban.finish(app, CaseOutcome::Failed).await;
                            if let Some(attempt) = attempted {
                                attempt.finish(app, CaseOutcome::Failed).await;
                            }
                            proof_stale = true;
                        } else {
                            let discord_reason = crate::cases::ban_dispatch_reason(
                                &found.reason,
                                attempted.as_ref(),
                            );
                            if let Some(outcome) = dispatch_spam_ban(
                                app,
                                enrollment.clone(),
                                &found,
                                rule,
                                now,
                                pending_ban,
                                attempted,
                                GuildId::new(found.guild).ban_with_reason(
                                    http.as_ref(),
                                    UserId::new(found.author),
                                    0,
                                    &discord_reason,
                                ),
                            )
                            .await
                            {
                                ban_outcome = Some(outcome);
                            }
                        }
                    }
                }
            }
        }
    }
    let action = if proof_stale {
        "remaining actions skipped; image proof or configuration is no longer current".into()
    } else if withhold_ban {
        format!(
            "delete matched messages; {}",
            jmodguard::custom_rules::UNKNOWN_MODERATOR_WARNING
        )
    } else {
        match ban_outcome {
            None => "delete remaining; ban already applied this window".into(),
            Some(outcome) => format!("delete matched messages; ban {outcome:?}"),
        }
    };
    let notes = delete_notes.join("; ");
    let content = if found.kind == Kind::Image {
        let evidence = automod_evidence(&found);
        if notes.is_empty() {
            evidence
        } else if evidence.is_empty() {
            notes
        } else {
            format!("{evidence}; {notes}")
        }
    } else {
        notes
    };
    let vars = BTreeMap::from([
        ("rule".into(), name.into()),
        ("mention".into(), format!("<@{}>", found.author)),
        ("user_id".into(), found.author.to_string()),
        ("source_channel_id".into(), found.channel.to_string()),
        ("channel".into(), format!("<#{}>", found.channel)),
        ("action".into(), action),
        ("reason".into(), found.reason),
        ("content".into(), content),
    ]);
    crate::logging::emit_to(
        app,
        http.as_ref(),
        GuildId::new(found.guild),
        "automod",
        vars,
    )
    .await;
    ReportDisposition::Consumed
}

pub(crate) async fn continue_present_spam_ban(
    app: &BotState,
    enrollment: crate::db::GuildEnrollment,
    author: u64,
    rule: &str,
    reason: &str,
    discord_reason: Option<&str>,
) -> bool {
    crate::cases::settle_present_application_ban(
        app,
        Some(enrollment.clone()),
        author,
        discord_reason,
    )
    .await;
    app.activation.may_record()
        && crate::db::record_enrolled_ban_if_absent(
            &app.db,
            Some(enrollment),
            author as i64,
            None,
            None,
            None,
            None,
            &format!("[automod:{rule}] {reason}"),
            None,
            crate::db::BanRecord::Retry,
            None,
        )
        .await
        .ok()
        .flatten()
        .is_some()
}

fn delete_outcome(error: &serenity::Error) -> CaseOutcome {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = error
        && response.status_code.as_u16() == 404
    {
        return CaseOutcome::Succeeded;
    }
    crate::cases::classify_discord(Some(error))
}

fn proving_hashes(found: &Match) -> Vec<jmodguard::image_spam::Hash> {
    let mut hashes = found.proving_hashes.clone();
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

fn automod_evidence(found: &Match) -> String {
    match found.kind {
        Kind::Text => found
            .messages
            .iter()
            .map(|(channel, message)| format!("channel {channel}, message {message}"))
            .collect::<Vec<_>>()
            .join("; "),
        Kind::Image => {
            crate::image_spam::image_notice_evidence(&found.messages, &proving_hashes(found))
        }
    }
}

fn message_evidence(
    excerpts: &[TextEvidence],
    channel: u64,
    message: u64,
    author: u64,
) -> CaseEvidence {
    if let Some(item) = excerpts
        .iter()
        .find(|item| item.channel_id == channel as i64 && item.message_id == message as i64)
    {
        return CaseEvidence::Text(vec![item.clone()]);
    }
    crate::cases::text_excerpt(channel, message, author, "")
}

fn evidence(excerpts: Vec<TextEvidence>) -> CaseEvidence {
    let excerpts: Vec<_> = excerpts
        .into_iter()
        .filter(|item| {
            item.channel_id > 0
                && item.message_id > 0
                && item.author_id > 0
                && item.occurred_at_ms > 0
                && !item.text.is_empty()
        })
        .collect();
    if excerpts.is_empty() {
        CaseEvidence::Unavailable
    } else {
        CaseEvidence::Text(excerpts)
    }
}

struct RemoveCapture(std::path::PathBuf);
impl Drop for RemoveCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
static HOLD_CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static SPAM_PHASES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Discord ban, then the case and the core mirror. Capture is last so a slow
/// download cannot delay or drop the mirror.
#[allow(clippy::too_many_arguments)]
async fn dispatch_spam_ban<B>(
    app: &Arc<BotState>,
    enrollment: crate::db::GuildEnrollment,
    found: &Match,
    rule: &str,
    now: Instant,
    pending_ban: crate::member_directory::PendingBan,
    attempted: Option<crate::cases::ApplicationAttempt>,
    ban: B,
) -> Option<CaseOutcome>
where
    B: std::future::Future<Output = Result<(), serenity::Error>>,
{
    let operation_id = pending_ban.operation_id().to_owned();
    let suppression = app.suppress_next_gateway_case(
        crate::state::GatewayCaseAction::Ban,
        found.guild,
        found.author,
        Some(&operation_id),
    );
    let discord = tokio::time::timeout(
        Duration::from_secs(10),
        crate::discord::race_discord_call(&app.activation, ban),
    )
    .await;
    if matches!(discord, Ok(crate::discord::DiscordCall::Cancelled)) {
        app.spam_gate
            .finish_ban(found.guild, found.author, now, false);
        pending_ban.finish(app, CaseOutcome::Uncertain).await;
        return None;
    }
    let outcome = match discord {
        Ok(crate::discord::DiscordCall::Finished(Ok(()))) => CaseOutcome::Succeeded,
        Ok(crate::discord::DiscordCall::Finished(Err(error))) => {
            crate::cases::classify_discord(Some(&error))
        }
        Ok(crate::discord::DiscordCall::Cancelled) | Err(_) => CaseOutcome::Uncertain,
    };
    app.spam_gate.finish_ban(
        found.guild,
        found.author,
        now,
        matches!(outcome, CaseOutcome::Succeeded),
    );
    if release_gateway_ban_suppression(&outcome) {
        app.release_gateway_case_suppression(suppression);
    }
    if !app.activation.may_record() {
        return None;
    }
    let case_id = if let Some(attempt) = attempted {
        attempt.finish(app, outcome.clone()).await
    } else {
        None
    };
    pending_ban.finish(app, outcome.clone()).await;
    if matches!(outcome, CaseOutcome::Succeeded) {
        let _ = crate::db::record_enrolled_ban_if_absent(
            &app.db,
            Some(enrollment.clone()),
            found.author as i64,
            None,
            None,
            None,
            None,
            &format!("[automod:{rule}] {}", found.reason),
            None,
            crate::db::BanRecord::Fresh,
            Some(&operation_id),
        )
        .await;
    }
    if app.activation.may_record()
        && found.kind == Kind::Image
        && let Some(case_id) = case_id
    {
        capture_attachments(app, enrollment, case_id, &found.attachments).await;
    }
    Some(outcome)
}

async fn capture_attachments(
    app: &BotState,
    enrollment: crate::db::GuildEnrollment,
    case_id: crate::db::guild_data::CaseId,
    attachments: &[ProvingAttachment],
) {
    #[cfg(test)]
    {
        SPAM_PHASES.lock().push("capture-start");
        while HOLD_CAPTURE.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    if !app.tenant.evidence_enabled() {
        return;
    }
    let hasher = app.image_spam.hasher();
    let budget = app.image_spam.download_timeout();
    let started = Instant::now();
    let limits = crate::tenant::evidence_limits();
    let sizes: Vec<u64> = attachments.iter().map(|item| item.file.size).collect();
    let plan = crate::tenant::plan_case_evidence_captures(&sizes, limits);
    if plan.skipped_invalid > 0 || plan.skipped_item_limit > 0 || plan.skipped_byte_limit > 0 {
        tracing::warn!(
            guild_id = enrollment.guild_id,
            case_id = case_id.as_str(),
            proving_total = attachments.len(),
            capturing = plan.admitted.len(),
            skipped_invalid = plan.skipped_invalid,
            skipped_item_limit = plan.skipped_item_limit,
            skipped_byte_limit = plan.skipped_byte_limit,
            max_items_per_case = limits.max_items_per_case,
            max_file_bytes = limits.max_file_bytes,
            max_case_bytes = limits.max_case_bytes,
            "proving attachment capture plan excludes files that cannot be admitted to case evidence"
        );
    }
    let admitted_total = plan.admitted.len();
    let mut captured = 0usize;
    for index in plan.admitted {
        let item = &attachments[index];
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            tracing::warn!(
                guild_id = enrollment.guild_id,
                case_id = case_id.as_str(),
                proving_total = attachments.len(),
                admitted = admitted_total,
                captured_before_timeout = captured,
                "attachment capture budget exhausted"
            );
            break;
        }
        let origin = crate::db::guild_data::EvidenceOrigin {
            channel_id: item.channel as i64,
            message_id: item.message as i64,
            attachment_id: item.file.id as i64,
        };
        let path = std::env::temp_dir().join(format!(
            "jmodguard-attach-{:032x}.part",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        match hasher.fetch_file(&item.file, remaining, &path).await {
            Ok(digest) if fetched_matches_proving(&digest, &item.proving_hash) => {}
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
                tracing::warn!(
                    guild_id = enrollment.guild_id,
                    case_id = case_id.as_str(),
                    channel_id = item.channel,
                    message_id = item.message,
                    attachment_id = item.file.id,
                    "attachment re-fetch digest does not match proving hash"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(guild_id = enrollment.guild_id, "{error:#}");
                continue;
            }
        }
        let _guard = RemoveCapture(path.clone());
        if let Err(error) = app
            .tenant
            .capture_selected_attachment(
                enrollment.clone(),
                case_id.clone(),
                origin,
                item.file.size,
                path,
            )
            .await
        {
            tracing::warn!(
                guild_id = enrollment.guild_id,
                "attachment capture failed: {error:#}"
            );
        } else {
            captured += 1;
        }
    }
}

/// True when any matched channel is outside the rule's current scope.
/// Moderator status is the caller's lookup; a failed lookup is not a moderator.
fn scope_blocks_enforcement(
    rule: &jmodguard::types::RuleDef,
    author: u64,
    channels: impl IntoIterator<Item = u64>,
    roles: &[u64],
    is_moderator: bool,
) -> bool {
    channels.into_iter().any(|channel| {
        !jmodguard::custom_rules::spam_scope_admits(
            rule,
            author,
            channel,
            Some(roles),
            is_moderator,
        )
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) static IMAGE_ENFORCEMENT_TEST_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    #[test]
    fn image_notice_evidence_uses_hash_and_locations_without_a_url() {
        let hash = [1u8; 32];
        let found = Match {
            kind: Kind::Image,
            guild: 1,
            author: 2,
            channel: 10,
            policy: MatchPolicy::Image(jmodguard::image_spam::Policy {
                mode: "exact_image_test".into(),
                window_secs: 60,
                min_channels: 3,
                min_attachments: 2,
                known_bad_hashes: Default::default(),
                enforce: false,
            }),
            token: (0, 0),
            messages: vec![(10, 100), (11, 101)],
            excerpts: vec![],
            proving_hashes: vec![hash],
            attachments: Vec::new(),
            reason: "Verified identical image bytes".into(),
        };
        let evidence = automod_evidence(&found);
        assert!(evidence.contains("channel 10, message 100"));
        assert!(evidence.contains("channel 11, message 101"));
        assert!(evidence.contains("sha256 "));
        let sha = evidence.find("sha256 ").expect("hash marker");
        let first_channel = evidence.find("channel ").expect("location");
        assert!(
            sha < first_channel,
            "proving hashes must precede locations in delivered evidence: {evidence}"
        );
        assert!(evidence.contains(&"01".repeat(32)));
        assert!(!evidence.contains("http"));
        assert!(!evidence.contains("cdn"));
        let mut text = found;
        text.kind = Kind::Text;
        text.policy = MatchPolicy::Text(jmodguard::text_spam::Policy {
            mode: "exact_text".into(),
            window_secs: 60,
            min_channels: 3,
            min_length: 20,
            enforce: false,
        });
        let text_evidence = automod_evidence(&text);
        assert_eq!(
            text_evidence,
            "channel 10, message 100; channel 11, message 101"
        );
        assert!(!text_evidence.contains("sha256"));
    }

    #[test]
    fn re_fetch_digest_must_match_proving_hash() {
        let proving = [1u8; 32];
        let other = [2u8; 32];
        assert!(fetched_matches_proving(&proving, &proving));
        assert!(!fetched_matches_proving(&other, &proving));
    }

    #[test]
    fn failed_delete_and_ban_claims_can_be_retried() {
        let gate = Gate::default();
        let now = Instant::now();
        assert!(gate.claim_delete(1, 2, 10, 100, now));
        assert!(!gate.claim_delete(1, 2, 10, 100, now));
        gate.finish_delete(1, 2, 10, 100, now, false);
        assert!(gate.claim_delete(1, 2, 10, 100, now));
        gate.finish_delete(1, 2, 10, 100, now, true);
        assert!(!gate.claim_delete(1, 2, 10, 100, now));

        assert!(gate.claim_ban(1, 2, now));
        assert!(!gate.claim_ban(1, 2, now));
        gate.finish_ban(1, 2, now, false);
        assert!(gate.claim_ban(1, 2, now));
        gate.finish_ban(1, 2, now, true);
        assert!(!gate.claim_ban(1, 2, now));
    }

    #[test]
    fn uncertain_ban_keeps_gateway_suppression() {
        assert!(!release_gateway_ban_suppression(&CaseOutcome::Succeeded));
        assert!(!release_gateway_ban_suppression(&CaseOutcome::Uncertain));
        assert!(release_gateway_ban_suppression(&CaseOutcome::Failed));
    }

    #[tokio::test]
    async fn image_ban_future_finishes_before_capture_starts() {
        let _serial = IMAGE_ENFORCEMENT_TEST_LOCK.lock().await;
        use std::sync::atomic::Ordering;
        struct ReleaseHold;
        impl Drop for ReleaseHold {
            fn drop(&mut self) {
                HOLD_CAPTURE.store(false, Ordering::SeqCst);
                SPAM_PHASES.lock().clear();
            }
        }
        let _release = ReleaseHold;
        HOLD_CAPTURE.store(true, Ordering::SeqCst);
        SPAM_PHASES.lock().clear();
        let path =
            std::env::temp_dir().join(format!("spam_order_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let app_ban = std::sync::Arc::clone(&app);
        let enrollment_ban = enrollment.clone();
        let task = tokio::spawn(async move {
            let pending = crate::member_directory::begin_ban_attempt(&app_ban, 10, 99).await;
            let operation =
                crate::db::guild_data::CaseId::try_from(pending.operation_id().to_owned()).unwrap();
            let attempted = crate::cases::begin_application_attempt(
                &app_ban,
                Some(enrollment_ban.clone()),
                CaseAction::Ban,
                99,
                None,
                Some("image spam".into()),
                CaseEvidence::Unavailable,
                operation,
            )
            .await;
            let found = Match {
                kind: Kind::Image,
                guild: 10,
                author: 99,
                channel: 5,
                policy: MatchPolicy::Image(jmodguard::image_spam::Policy {
                    mode: "exact_image_test".into(),
                    window_secs: 60,
                    min_channels: 3,
                    min_attachments: 2,
                    known_bad_hashes: Default::default(),
                    enforce: true,
                }),
                token: (0, 0),
                messages: Vec::new(),
                excerpts: Vec::new(),
                proving_hashes: Vec::new(),
                attachments: Vec::new(),
                reason: "image spam".into(),
            };
            dispatch_spam_ban(
                &app_ban,
                enrollment_ban,
                &found,
                "attachment_flood",
                Instant::now(),
                pending,
                attempted,
                async {
                    SPAM_PHASES.lock().push("ban-enter");
                    SPAM_PHASES.lock().push("ban-complete");
                    Ok(())
                },
            )
            .await
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if SPAM_PHASES.lock().contains(&"capture-start") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "capture did not start: {:?}",
                SPAM_PHASES.lock().clone()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            SPAM_PHASES.lock().clone(),
            vec!["ban-enter", "ban-complete", "capture-start"]
        );
        let mirrored: i64 = app
            .db
            .with_conn(|conn| {
                use crate::db::SqlExecutor;
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM bans WHERE guild_id=?1 AND user_id=?2 AND active=1",
                    crate::db::params![10_i64, 99_i64],
                    |row| {
                        let count: i64 = row.get(0).map_err(|error| anyhow::anyhow!("{error}"))?;
                        Ok(count)
                    },
                )?;
                Ok(count)
            })
            .await
            .unwrap();
        assert_eq!(
            mirrored, 1,
            "the ban mirror must exist before capture starts"
        );
        HOLD_CAPTURE.store(false, Ordering::SeqCst);
        let outcome = task.await.unwrap();
        assert_eq!(outcome, Some(CaseOutcome::Succeeded));
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 20, crate::tenant::case_limits())
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
        let stored = store
            .case(scope, ids[0].clone(), crate::tenant::case_limits())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.outcome, CaseOutcome::Succeeded);
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[test]
    fn enforcement_recheck_sees_moderator_and_channel_scope_changes() {
        let mut rule = crate::defaults::default_rules().remove(1);
        rule.skip_moderators = true;
        rule.include_channels = vec![11];
        let blocks = |channels: &[(u64, u64)], is_moderator: bool| {
            scope_blocks_enforcement(
                &rule,
                5,
                channels.iter().map(|(channel, _)| *channel),
                &[],
                is_moderator,
            )
        };
        assert!(!blocks(&[(11, 1)], false));
        assert!(blocks(&[(11, 1)], true));
        assert!(blocks(&[(11, 1), (12, 2)], false));
    }

    const MEMBER_JSON: &str = r#"{
        "user": {
            "id": "99",
            "username": "member",
            "global_name": null,
            "avatar": null,
            "banner": null,
            "accent_color": null,
            "locale": null,
            "verified": null,
            "email": null,
            "public_flags": null,
            "member": null,
            "primary_guild": null,
            "avatar_decoration_data": null,
            "collectibles": null
        },
        "nick": null,
        "avatar": null,
        "banner": null,
        "roles": [],
        "joined_at": null,
        "premium_since": null,
        "deaf": false,
        "mute": false,
        "flags": 0,
        "permissions": null,
        "communication_disabled_until": null,
        "unusual_dm_activity_until": null,
        "avatar_decoration_data": null
    }"#;

    fn read_http_request(stream: &mut std::net::TcpStream) -> Option<(String, String, String)> {
        use std::io::Read;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => return None,
            }
            if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 65_536 {
                break;
            }
        }
        let header_end = buf.windows(4).position(|window| window == b"\r\n\r\n")?;
        let header = String::from_utf8_lossy(&buf[..header_end]);
        let mut parts = header.lines().next()?.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
        let length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buf.len() < header_end + 4 + length {
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = String::from_utf8(buf[header_end + 4..header_end + 4 + length].to_vec()).ok()?;
        Some((method, path, body))
    }

    pub(crate) fn serve_discord_stub<F>(
        listener: std::net::TcpListener,
        lines: Arc<Mutex<Vec<String>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        before_response: F,
    ) where
        F: FnMut(&str, &str, &str) -> Option<&'static str> + Send + 'static,
    {
        use std::io::Write;
        let before_response = Arc::new(Mutex::new(before_response));
        listener.set_nonblocking(true).unwrap();
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let lines = Arc::clone(&lines);
                    let before_response = Arc::clone(&before_response);
                    std::thread::spawn(move || {
                        let Some((method, path, request_body)) = read_http_request(&mut stream)
                        else {
                            return;
                        };
                        lines.lock().push(format!("{method} {path}"));
                        let status_override = before_response.lock()(&method, &path, &request_body);
                        let body = if method == "GET" && path.contains("/members/") {
                            MEMBER_JSON
                        } else if method == "POST" && path.ends_with("/messages") {
                            r#"{"id":"1000","channel_id":"999","author":{"id":"1","username":"fixture","discriminator":"0001","avatar":null},"content":"fixture","timestamp":"2026-01-01T00:00:00Z","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}"#
                        } else {
                            ""
                        };
                        let status = if let Some(status) = status_override {
                            status
                        } else if method == "GET" && path.contains("/bans/") {
                            "404 Not Found"
                        } else if body.is_empty() {
                            "204 No Content"
                        } else {
                            "200 OK"
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }

    #[tokio::test]
    async fn unknown_moderator_deletes_spam_message_without_kick_or_ban() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tracing::instrument::WithSubscriber;

        #[derive(Clone)]
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let lines = Arc::new(Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        struct Stop(Arc<AtomicBool>);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let stop_guard = Stop(Arc::clone(&stop));
        let server_lines = Arc::clone(&lines);
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            serve_discord_stub(listener, server_lines, server_stop, |_, _, _| None)
        });

        let path = std::env::temp_dir().join(format!(
            "spam_unknown_mod_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                std::sync::Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let mut rule = crate::defaults::default_rules().remove(2);
        rule.enabled = true;
        rule.skip_moderators = true;
        let policy = jmodguard::text_spam::Policy {
            mode: "exact_text".into(),
            window_secs: 60,
            min_channels: 3,
            min_length: 20,
            enforce: true,
        };
        let now = Instant::now();
        let text = "abcdefghijklmnopqrst";
        let messages = app
            .text_spam
            .seed_exact_text(
                10,
                99,
                (0, 0),
                &policy,
                now,
                &[(42, 11, text), (43, 12, text), (44, 13, text)],
            )
            .expect("three channels still prove the match");
        let found = Match {
            kind: Kind::Text,
            guild: 10,
            author: 99,
            channel: 11,
            policy: MatchPolicy::Text(policy),
            token: (0, 0),
            messages,
            excerpts: Vec::new(),
            proving_hashes: Vec::new(),
            attachments: Vec::new(),
            reason: "repeated text".into(),
        };
        let http = serenity::http::HttpBuilder::new("token")
            .proxy(format!("http://{addr}"))
            .ratelimiter_disabled(true)
            .build();
        let cache = serenity::cache::Cache::new();
        let log = CapturedLog(Arc::new(Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let disposition = punish_proven_match(
            &app,
            Arc::new(http),
            &cache,
            found,
            "repeat_text",
            "Repeated text",
            &rule,
            enrollment,
        )
        .with_subscriber(subscriber)
        .await;
        drop(stop_guard);
        let _ = server.join();
        assert_eq!(disposition, ReportDisposition::Consumed);
        let seen = lines.lock().clone();
        assert!(
            seen.iter()
                .any(|line| line.starts_with("DELETE ") && line.contains("/messages/42")),
            "the matched message must be deleted: {seen:?}"
        );
        assert!(
            seen.iter().all(|line| !line.contains("/bans/")),
            "unknown moderator status must not ban: {seen:?}"
        );
        assert!(
            seen.iter()
                .all(|line| !(line.starts_with("DELETE ") && line.contains("/members/"))),
            "unknown moderator status must not kick: {seen:?}"
        );
        let output = String::from_utf8(log.0.lock().clone()).unwrap();
        assert!(
            output.contains("WARN")
                && output.contains(jmodguard::custom_rules::UNKNOWN_MODERATOR_WARNING),
            "{output}"
        );
        let report = crate::logging::observed_delete_report(
            &app,
            GuildId::new(10),
            ChannelId::new(11),
            MessageId::new(42),
            None,
        )
        .expect("automod delete");
        assert_eq!(report.event, "automod_delete");
        assert_eq!(report.vars["rule"], "Repeated text");
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }
}
