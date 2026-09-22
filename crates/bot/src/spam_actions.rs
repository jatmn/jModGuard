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
        match self {
            Self::Image(policy) => app.image_spam.match_still_proving(
                found.guild,
                found.author,
                crate::image_spam::MatchProof {
                    token: found.token,
                    messages: &found.messages,
                    evidence: &found.excerpts,
                    policy,
                    now,
                },
            ),
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
            (
                "content".into(),
                found
                    .messages
                    .iter()
                    .map(|(c, m)| format!("channel {c}, message {m}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        ]);
        crate::logging::emit(app, ctx, GuildId::new(found.guild), "automod", vars).await;
        return ReportDisposition::Consumed;
    }
    enforce(app, ctx, found, rule, name).await
}

async fn enforce(
    app: &Arc<BotState>,
    ctx: &Context,
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
    let Some(policy) = MatchPolicy::parse(found.kind, rule_def) else {
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
    let member = tokio::time::timeout(
        Duration::from_secs(10),
        ctx.http
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
        crate::logging::emit(app, ctx, GuildId::new(found.guild), "automod", vars).await;
        return ReportDisposition::Consumed;
    }
    let roles: Vec<u64> = member
        .as_ref()
        .map(|m| m.roles.iter().map(|r| r.get()).collect())
        .unwrap_or_default();
    if found
        .messages
        .iter()
        .any(|(channel, _)| !eligible(rule_def, found.author, *channel, &roles))
    {
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
        crate::logging::emit(app, ctx, GuildId::new(found.guild), "automod", vars).await;
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
        crate::logging::emit(app, ctx, GuildId::new(found.guild), "automod", vars).await;
        return ReportDisposition::Consumed;
    }
    if crate::db::guild_stores::guild_is_write_fenced(&app.db, found.guild as i64)
        .await
        .unwrap_or(true)
    {
        return ReportDisposition::Retryable;
    }
    let http = ctx.http.as_ref();
    let mut delete_notes = Vec::new();
    for (channel, message) in &found.messages {
        if !app
            .spam_gate
            .claim_delete(found.guild, found.author, *channel, *message, now)
        {
            delete_notes.push(format!("{channel}/{message} already handled this window"));
            continue;
        }
        let outcome = match tokio::time::timeout(
            Duration::from_secs(10),
            http.delete_message(
                ChannelId::new(*channel),
                MessageId::new(*message),
                Some("spam match"),
            ),
        )
        .await
        {
            Ok(Ok(())) => CaseOutcome::Succeeded,
            Ok(Err(error)) => delete_outcome(&error),
            Err(_) => CaseOutcome::Uncertain,
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
    let mut ban_case = None;
    if app.spam_gate.claim_ban(found.guild, found.author, now) {
        let suppression = app.suppress_next_gateway_case(
            crate::state::GatewayCaseAction::Ban,
            found.guild,
            found.author,
        );
        let discord = tokio::time::timeout(
            Duration::from_secs(10),
            GuildId::new(found.guild).ban_with_reason(
                http,
                UserId::new(found.author),
                0,
                &found.reason,
            ),
        )
        .await;
        let outcome = match discord {
            Ok(Ok(())) => CaseOutcome::Succeeded,
            Ok(Err(error)) => crate::cases::classify_discord(Some(&error)),
            Err(_) => CaseOutcome::Uncertain,
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
        if matches!(outcome, CaseOutcome::Succeeded) {
            let _ = crate::db::record_enrolled_ban(
                &app.db,
                Some(enrollment.clone()),
                found.author as i64,
                None,
                &format!("[automod:{rule}] {}", found.reason),
            )
            .await;
        }
        ban_outcome = Some(outcome.clone());
        ban_case = crate::cases::record_application(
            app,
            Some(enrollment.clone()),
            CaseAction::Ban,
            outcome,
            found.author,
            None,
            Some(found.reason.clone()),
            evidence(found.excerpts.clone()),
        )
        .await;
    }
    if found.kind == Kind::Image
        && let Some(case_id) = ban_case
    {
        capture_attachments(app, enrollment, case_id, &found.attachments).await;
    }
    let action = match ban_outcome {
        None => "delete remaining; ban already applied this window".into(),
        Some(outcome) => format!("delete matched messages; ban {outcome:?}"),
    };
    let vars = BTreeMap::from([
        ("rule".into(), name.into()),
        ("mention".into(), format!("<@{}>", found.author)),
        ("user_id".into(), found.author.to_string()),
        ("source_channel_id".into(), found.channel.to_string()),
        ("channel".into(), format!("<#{}>", found.channel)),
        ("action".into(), action),
        ("reason".into(), found.reason),
        ("content".into(), delete_notes.join("; ")),
    ]);
    crate::logging::emit(app, ctx, GuildId::new(found.guild), "automod", vars).await;
    ReportDisposition::Consumed
}

fn eligible(rule: &crate::types::RuleDef, author: u64, channel: u64, roles: &[u64]) -> bool {
    rule.enabled
        && !rule.whitelist_users.contains(&author)
        && !rule.whitelist_channels.contains(&channel)
        && !rule.whitelist_roles.iter().any(|r| roles.contains(r))
}

fn delete_outcome(error: &serenity::Error) -> CaseOutcome {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = error
        && response.status_code.as_u16() == 404
    {
        return CaseOutcome::Succeeded;
    }
    crate::cases::classify_discord(Some(error))
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

async fn capture_attachments(
    app: &BotState,
    enrollment: crate::db::GuildEnrollment,
    case_id: crate::db::guild_data::CaseId,
    attachments: &[ProvingAttachment],
) {
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
                tracing::warn!(
                    guild_id = enrollment.guild_id,
                    "attachment download failed: {error:#}"
                );
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
