//! Bounded aggregation of automod deletes. Enforcement reports effects here;
//! the delete marks still decide which path owns each message's report.

use super::*;
use crate::automod_delete_marks::PreparedAutomodDelete;
use crate::db::guild_data::{CaseId, CaseOutcome};
use tokio::time::Instant as Clock;

const MAX_INCIDENTS: usize = 64;
const MAX_MESSAGES: usize = 256;
const MAX_LIFETIME: Duration = Duration::from_secs(30);

pub(super) const SUMMARY_TEMPLATE: &str = "Automod cleanup\n\n**User:** {mention} (`{user_id}`)\n**Rule:** {rule} (`{rule_id}`)\n**Action:** {cleanup_action}\n**Deleted:** {cleanup_total}\n**Channels:** {cleanup_channels}\n**Time range:** {cleanup_first} – {cleanup_last}\n**Sample:** {cleanup_sample}\n**Evidence:** {cleanup_evidence}\n**Delete failures:** {cleanup_failures}";

type Subject = (u64, u64);
type Message = (u64, u64);

#[derive(Default)]
pub(crate) struct CleanupIncidents {
    inner: Mutex<Incidents>,
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct Incidents {
    next_id: u64,
    active: HashMap<Subject, Incident>,
}

pub(crate) struct CleanupRule<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub reason: &'a str,
    pub action: &'a str,
    pub sample: Option<(u64, u64, &'a str)>,
    pub hash: Option<&'a [u8; 32]>,
}

struct Incident {
    id: u64,
    rule_id: String,
    rule: String,
    reason: String,
    action: String,
    outcome: Option<CaseOutcome>,
    case: Option<CaseId>,
    delay: Duration,
    settle_at: Clock,
    deadline: Clock,
    running: usize,
    purge_from: Option<i64>,
    purge_until: Option<i64>,
    purge_receipt_until: Option<Clock>,
    sample: Option<(u64, u64, String)>,
    hash: Option<String>,
    messages: BTreeMap<Message, Delete>,
}

struct Delete {
    at: chrono::DateTime<chrono::Utc>,
    result: Result<Option<Box<MessageSnapshot>>, &'static str>,
    via_purge: bool,
}

pub(crate) struct Cleanup<'a> {
    incidents: &'a CleanupIncidents,
    subject: Subject,
    id: u64,
    rule_id: String,
    rule_name: String,
    reason: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        self.update(|incident| incident.running -= 1);
        self.incidents.wake.notify_one();
    }
}

impl Cleanup<'_> {
    fn update(&self, update: impl FnOnce(&mut Incident)) {
        let mut state = self.incidents.inner.lock();
        if let Some(incident) = state
            .active
            .get_mut(&self.subject)
            .filter(|i| i.id == self.id)
        {
            update(incident);
        }
    }

    pub(crate) fn prepare(
        &self,
        app: &BotState,
        channel: u64,
        message: u64,
    ) -> PreparedAutomodDelete {
        app.automod_delete_marks.prepare_delete(
            self.subject.0,
            channel,
            message,
            &self.rule_id,
            &self.rule_name,
            &self.reason,
        )
    }

    pub(crate) async fn complete_ungrouped(
        &self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
    ) {
        complete_prepared_automod_delete(
            app,
            sink,
            self.subject.0,
            channel,
            message,
            &prepared,
            &self.rule_id,
            &self.rule_name,
            &self.reason,
        )
        .await;
    }

    pub(crate) async fn release_ungrouped(
        &self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
    ) {
        let guild = self.subject.0;
        let removal =
            app.automod_delete_marks
                .remove_unconfirmed(guild, channel, message, prepared.attempt);
        if let Some(settlement) = removal.released_settlement.as_ref() {
            emit_reconciled_automod_delete(
                app,
                sink,
                guild,
                channel,
                message,
                &settlement.rule_id,
                &settlement.rule_name,
                &settlement.reason,
                settlement.retained.as_ref(),
                settlement.fallback_to_bulk,
            )
            .await;
        }
        replay_unconfirmed_plain_delete(
            app,
            sink,
            GuildId::new(guild),
            ChannelId::new(channel),
            MessageId::new(message),
            &removal,
        )
        .await;
    }

    pub(crate) fn begin_purge(&self, seconds: u32) {
        if seconds > 0 {
            self.update(|incident| {
                let now = chrono::Utc::now().timestamp_millis();
                let from = now - i64::from(seconds) * 1000;
                incident.purge_from = Some(incident.purge_from.map_or(from, |old| old.min(from)));
                incident.purge_until = Some(now);
                incident.purge_receipt_until = Some(incident.deadline);
                incident.touch(Clock::now());
            });
        }
    }

    pub(crate) fn outcome(&self, outcome: CaseOutcome, case: Option<CaseId>) {
        self.update(|incident| {
            if incident.purge_receipt_until.is_some() {
                incident.purge_receipt_until = (outcome != CaseOutcome::Failed)
                    .then(|| (Clock::now() + incident.delay).min(incident.deadline));
            }
            if incident.outcome != Some(CaseOutcome::Succeeded) || outcome == CaseOutcome::Succeeded
            {
                incident.outcome = Some(outcome);
                if case.is_some() {
                    incident.case = case;
                }
            }
            incident.touch(Clock::now());
        });
    }

    pub(crate) fn deleted(
        &self,
        app: &BotState,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
        gateway_required: bool,
    ) -> bool {
        let mut claimed = false;
        self.update(|incident| {
            if gateway_required
                && !incident
                    .purge_receipt_until
                    .is_some_and(|until| Clock::now() < until)
            {
                return;
            }
            claimed = incident.claim_delete(
                app,
                self.subject.0,
                channel,
                message,
                Some(prepared),
                None,
                gateway_required,
            );
        });
        claimed
    }

    pub(crate) fn failed(&self, channel: u64, message: u64, reason: &'static str) {
        self.update(|incident| {
            if incident.messages.len() < MAX_MESSAGES {
                incident
                    .messages
                    .entry((channel, message))
                    .or_insert_with(|| Delete {
                        at: chrono::Utc::now(),
                        result: Err(reason),
                        via_purge: false,
                    });
                incident.touch(Clock::now());
            }
        });
    }
}

impl Incident {
    fn touch(&mut self, now: Clock) {
        self.settle_at = (now + self.delay).min(self.deadline);
    }

    #[allow(clippy::too_many_arguments)]
    fn claim_delete(
        &mut self,
        app: &BotState,
        guild: u64,
        channel: u64,
        message: u64,
        prepared: Option<PreparedAutomodDelete>,
        retained: Option<&MessageSnapshot>,
        gateway_required: bool,
    ) -> bool {
        let target = (channel, message);
        if let Some(Delete {
            result: Ok(snapshot),
            via_purge,
            ..
        }) = self.messages.get_mut(&target)
        {
            if snapshot.is_none() {
                *snapshot = retained.cloned().map(Box::new);
            }
            if let Some(prepared) = prepared {
                if !gateway_required {
                    *via_purge = false;
                }
                app.automod_delete_marks.remove_unconfirmed(
                    guild,
                    channel,
                    message,
                    prepared.attempt,
                );
            }
            return true;
        }
        if Clock::now() >= self.deadline
            || (self.messages.len() >= MAX_MESSAGES && !self.messages.contains_key(&target))
        {
            return false;
        }
        let Some(snapshot) = app.automod_delete_marks.claim_cleanup_delete(
            (guild, channel, message),
            prepared,
            retained,
            gateway_required,
        ) else {
            return false;
        };
        self.messages.insert(
            target,
            Delete {
                at: chrono::Utc::now(),
                result: Ok(snapshot.map(Box::new)),
                via_purge: prepared.is_none() || gateway_required,
            },
        );
        self.touch(Clock::now());
        true
    }
}

pub(crate) async fn begin_cleanup<'a>(
    app: &'a BotState,
    guild: u64,
    user: u64,
    rule: CleanupRule<'_>,
) -> Option<Cleanup<'a>> {
    if !app.cfg.log.group_automod_cleanup_deletes || !app.activation.may_record() {
        return None;
    }
    let cfg = app.configs.get(&app.db, guild).await.ok()?;
    channel_for(&cfg, "automod_delete")?;
    let mut state = app.cleanup_incidents.inner.lock();
    if app.activation.is_shutting_down() {
        return None;
    }
    let subject = (guild, user);
    let now = Clock::now();
    let capacity = state.active.len() < MAX_INCIDENTS;
    let id = state.next_id;
    let handle = |id| Cleanup {
        incidents: &app.cleanup_incidents,
        subject,
        id,
        rule_id: rule.id.to_owned(),
        rule_name: rule.name.to_owned(),
        reason: rule.reason.to_owned(),
    };
    match state.active.entry(subject) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            let incident = entry.get_mut();
            if incident.rule_id != rule.id
                || incident.action != rule.action
                || now >= incident.settle_at
            {
                return None;
            }
            incident.running += 1;
            incident.touch(now);
            Some(handle(incident.id))
        }
        std::collections::hash_map::Entry::Vacant(entry) if capacity => {
            let delay = Duration::from_secs(app.cfg.log.automod_cleanup_settle_secs.clamp(5, 10));
            entry.insert(Incident {
                id,
                rule_id: rule.id.to_owned(),
                rule: clip(rule.name, 100),
                reason: rule.reason.to_owned(),
                action: rule.action.to_owned(),
                outcome: None,
                case: None,
                delay,
                settle_at: now + delay,
                deadline: now + MAX_LIFETIME,
                running: 1,
                purge_from: None,
                purge_until: None,
                purge_receipt_until: None,
                sample: rule
                    .sample
                    .filter(|(_, _, text)| !text.is_empty())
                    .map(|(channel, message, text)| (channel, message, clip(text, 240))),
                hash: rule
                    .hash
                    .map(|hash| hash.iter().map(|byte| format!("{byte:02x}")).collect()),
                messages: BTreeMap::new(),
            });
            state.next_id = id.wrapping_add(1);
            app.cleanup_incidents.wake.notify_one();
            Some(handle(id))
        }
        std::collections::hash_map::Entry::Vacant(_) => None,
    }
}

/// Raw Gateway deletes only have message IDs. Use a previously observed author
/// and the message creation time; unknown or unrelated authors stay in their
/// normal single or bulk path.
pub(crate) fn claim_cleanup_gateway(
    app: &BotState,
    guild: GuildId,
    channel: ChannelId,
    message: MessageId,
    retained: Option<&MessageSnapshot>,
) {
    if !app.cfg.log.group_automod_cleanup_deletes {
        return;
    }
    let guild = guild.get();
    let channel = channel.get();
    let id = message.get();
    let Some((author, created)) = app
        .recent_messages
        .identity(guild, channel, id)
        .or_else(|| {
            let row = retained?;
            Some((row.fields.author_id?.try_into().ok()?, row.created_at_ms))
        })
    else {
        return;
    };
    let mut state = app.cleanup_incidents.inner.lock();
    let Some(incident) = state.active.get_mut(&(guild, author)) else {
        return;
    };
    if incident
        .purge_receipt_until
        .is_some_and(|until| Clock::now() < until)
        && incident
            .purge_from
            .zip(incident.purge_until)
            .is_some_and(|(from, until)| (from..=until).contains(&created))
    {
        incident.claim_delete(app, guild, channel, id, None, retained, false);
    }
}

pub(crate) fn cleanup_delete_failure(error: &serenity::Error) -> &'static str {
    match crate::discord::http_error_response(error) {
        Some((403, _)) => "missing permission",
        Some((404, _)) => "message unavailable",
        Some(_) => "Discord rejected the delete",
        None => "delete outcome unknown",
    }
}

impl CleanupIncidents {
    async fn wait_until_due(&self) {
        let next = self
            .inner
            .lock()
            .active
            .values()
            .map(|incident| {
                if incident.running > 0 {
                    incident.deadline
                } else {
                    incident.settle_at
                }
            })
            .min();
        if let Some(next) = next {
            tokio::select! {
                _ = self.wake.notified() => {},
                _ = tokio::time::sleep_until(next) => {},
            }
        } else {
            self.wake.notified().await;
        }
    }

    fn take(&self, now: Clock, shutdown: bool) -> Vec<(Subject, Incident, bool)> {
        let mut state = self.inner.lock();
        state
            .active
            .extract_if(|_, incident| {
                shutdown
                    || now >= incident.deadline
                    || (incident.running == 0 && now >= incident.settle_at)
            })
            .map(|(subject, incident)| {
                let fallback = shutdown || incident.running > 0;
                (subject, incident, fallback)
            })
            .collect()
    }
}

fn channel_counts(counts: BTreeMap<u64, usize>) -> String {
    let parts = counts
        .into_iter()
        .map(|(channel, count)| format!("<#{channel}> {count}"))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "—".to_owned()
    } else {
        parts.join(", ")
    }
}

fn summary_vars(
    subject: Subject,
    incident: &Incident,
    filter: Option<&db::LogFilter>,
) -> Option<BTreeMap<String, String>> {
    let (_, user) = subject;
    let mut vars = BTreeMap::from([
        ("mention".into(), format!("<@{user}>")),
        ("user_id".into(), user.to_string()),
        ("rule".into(), incident.rule.clone()),
        ("rule_id".into(), clip(&incident.rule_id, 80)),
    ]);
    if filter.is_some_and(|filter| excluded(filter, &vars)) {
        return None;
    }
    let mut channels = BTreeMap::new();
    let mut source_channels = std::collections::BTreeSet::new();
    let mut failures = BTreeMap::<&str, BTreeMap<u64, usize>>::new();
    let mut first = None;
    let mut last = None;
    let mut sample = None;
    let mut evidence_visible = false;
    let mut total = 0;
    for (&(channel, message), item) in &incident.messages {
        if filter.is_some_and(|filter| channel_id_excluded(filter, Some(&channel.to_string()))) {
            continue;
        }
        source_channels.insert(channel);
        first = Some(first.map_or(item.at, |first| item.at.min(first)));
        last = Some(last.map_or(item.at, |last| item.at.max(last)));
        match &item.result {
            Ok(row) => {
                total += 1;
                *channels.entry(channel).or_default() += 1;
                if let Some((source, id, text)) = &incident.sample
                    && (*source, *id) == (channel, message)
                {
                    sample = Some(text.clone());
                } else if sample.is_none()
                    && let Some(row) = row.as_deref()
                    && row.expires_at_ms > chrono::Utc::now().timestamp_millis()
                {
                    sample = row
                        .fields
                        .content
                        .as_deref()
                        .filter(|text| !text.is_empty())
                        .map(|text| clip(text, 240));
                }
                evidence_visible = true;
            }
            Err(reason) => {
                *failures
                    .entry(reason)
                    .or_default()
                    .entry(channel)
                    .or_default() += 1
            }
        }
    }
    let format_time =
        |at: chrono::DateTime<chrono::Utc>| at.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let failure_count: usize = failures
        .values()
        .flat_map(|channels| channels.values())
        .sum();
    let details = failures
        .into_iter()
        .map(|(reason, channels)| format!("{reason} ({})", channel_counts(channels)))
        .collect::<Vec<_>>()
        .join("; ");
    let action = match (&*incident.action, &incident.outcome) {
        ("ban", Some(CaseOutcome::Succeeded)) => "Banned".into(),
        ("kick", Some(CaseOutcome::Succeeded)) => "Kicked".into(),
        ("ban", Some(CaseOutcome::Failed)) => "Ban failed".into(),
        ("kick", Some(CaseOutcome::Failed)) => "Kick failed".into(),
        (action, Some(CaseOutcome::Uncertain)) => format!("{action} outcome uncertain"),
        (action, Some(outcome)) => format!("{action} {outcome:?}"),
        (action, None) => format!("{action} outcome unavailable"),
    };
    vars.extend([
        ("cleanup_action".to_owned(), action),
        ("cleanup_total".to_owned(), total.to_string()),
        ("cleanup_channels".to_owned(), channel_counts(channels)),
        ("cleanup_first".to_owned(), format_time(first?)),
        ("cleanup_last".to_owned(), format_time(last?)),
        (
            "cleanup_sample".to_owned(),
            sample.unwrap_or_else(|| "unavailable".into()),
        ),
        (
            "cleanup_evidence".to_owned(),
            incident
                .hash
                .as_ref()
                .filter(|_| evidence_visible)
                .map(|hash| format!("SHA-256 `{hash}`"))
                .unwrap_or_else(|| "unavailable".into()),
        ),
        (
            "cleanup_failures".to_owned(),
            if failure_count == 0 {
                "0".into()
            } else {
                format!("{failure_count} · {}", clip(&details, 400))
            },
        ),
        (
            "cleanup_source_channels".to_owned(),
            source_channels
                .into_iter()
                .map(|channel| channel.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
    ]);
    Some(vars)
}

pub(crate) async fn flush(app: &Arc<BotState>, sink: &impl LogDelivery, shutdown: bool) {
    for (subject, mut incident, mut fallback) in app.cleanup_incidents.take(Clock::now(), shutdown)
    {
        let (guild, _) = subject;
        if incident.messages.is_empty() {
            continue;
        }
        // An echo can beat the ban HTTP result. A definite rejection leaves
        // those Gateway observations as ordinary deletes.
        if incident.outcome == Some(CaseOutcome::Failed) {
            let purge_echoes: Vec<_> = incident
                .messages
                .extract_if(.., |_, item| item.via_purge)
                .collect();
            for ((channel, message), item) in purge_echoes {
                if let Ok(retained) = item.result {
                    emit_individual(
                        app,
                        sink,
                        subject,
                        (channel, message),
                        &incident,
                        retained.as_deref(),
                        true,
                    )
                    .await;
                }
            }
        }
        if !fallback {
            match app.configs.get(&app.db, guild).await {
                Ok(cfg) if channel_for(&cfg, "automod_delete").is_some() => {
                    if let Some(mut vars) =
                        summary_vars(subject, &incident, log_filter_for(&cfg, "automod_delete"))
                    {
                        if let Some(case) = &incident.case {
                            let id = case.as_str();
                            let case_display = jmodguard::templates::public_webui_url(
                                &app.cfg.bot.about_webui_url,
                            )
                            .map(|base| {
                                format!("[Open case]({base}/?list=cases&guild={guild}&case={id})")
                            })
                            .unwrap_or_else(|| format!("Case `{id}`"));
                            vars.entry("cleanup_action".into()).and_modify(|action| {
                                action.push_str(" · ");
                                action.push_str(&case_display);
                            });
                        }
                        // One Discord embed has a fixed description budget.
                        // If the channel list will not fit, keep the individual reports.
                        if templates::render_bounded(SUMMARY_TEMPLATE, &vars, 4097)
                            .encode_utf16()
                            .count()
                            > 4096
                        {
                            fallback = true;
                        } else {
                            let delivery =
                                emit_to(app, sink, GuildId::new(guild), "automod_delete", vars)
                                    .await;
                            fallback =
                                matches!(delivery, Delivery::Unconfigured | Delivery::Blocked);
                        }
                    }
                }
                _ => fallback = true,
            }
        }
        if !fallback {
            continue;
        }
        for (&target, item) in &incident.messages {
            if let Ok(retained) = &item.result {
                emit_individual(
                    app,
                    sink,
                    subject,
                    target,
                    &incident,
                    retained.as_deref(),
                    false,
                )
                .await;
            }
        }
    }
}

/// Keep the normal event templates, admission and routing for individual replay.
async fn emit_individual(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    (guild, user): Subject,
    (channel, message): Message,
    incident: &Incident,
    retained: Option<&MessageSnapshot>,
    plain: bool,
) {
    let retained = settle_automod_retained(app, guild, channel, message, retained);
    let guild = GuildId::new(guild);
    let channel = ChannelId::new(channel);
    let id = MessageId::new(message);
    let mut report = if plain {
        let mut report = message_delete_report(app, guild, channel, id);
        apply_retained_delete(&mut report, retained.as_ref());
        report
    } else {
        automod_delete_report(
            app,
            guild,
            channel,
            id,
            &incident.rule_id,
            &incident.rule,
            &incident.reason,
            retained.as_ref(),
        )
    };
    report.vars.extend(templates::user_vars(
        user,
        &user.to_string(),
        &user.to_string(),
    ));
    let delivery = emit_to_observed(
        app,
        sink,
        guild,
        report.event,
        report.vars,
        None,
        Admission::WaitForShutdown,
    )
    .await;
    if matches!(delivery, Delivery::Unconfigured)
        && let Ok(cfg) = app.configs.get(&app.db, guild.get()).await
        && channel_for(&cfg, report.event).is_none()
        && channel_for(&cfg, "message_bulk_delete").is_some()
    {
        emit_failure_bulk(
            app,
            sink,
            &cfg,
            guild,
            channel,
            id,
            retained.as_ref(),
            Admission::WaitForShutdown,
        )
        .await;
    }
}

pub async fn run_cleanup_reports(app: Arc<BotState>, http: Arc<serenity::http::Http>) {
    if !app.cfg.log.group_automod_cleanup_deletes {
        return;
    }
    while !app.activation.is_shutting_down() {
        tokio::select! {
            _ = crate::shutdown_signal() => return,
            _ = app.cleanup_incidents.wait_until_due() => {}
        }
        if !app.activation.is_shutting_down() {
            flush(&app, http.as_ref(), false).await;
        }
    }
}

pub async fn flush_cleanup_on_shutdown(app: &Arc<BotState>, http: &serenity::http::Http) {
    flush(app, http, true).await;
}

#[cfg(test)]
mod tests;
