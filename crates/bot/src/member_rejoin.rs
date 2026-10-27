//! Optional prior-case note when a member joins and the guild store already has cases.

use crate::db::guild_data::{GuildDataScope, GuildDataStore, MemberHistoryEntry};
use crate::logging;
use crate::state::BotState;
#[cfg(test)]
use crate::templates;
use serenity::model::id::GuildId;
use serenity::model::user::User;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const PRIOR_CASE_LIMIT: u32 = 8;
const REASON_UNITS: usize = 160;
const REJOIN_LOG_UTF16_BUDGET: usize = 2000;
const REJOIN_NOTICE_UTF16_RESERVE: usize = 320;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RejoinNote {
    Cases {
        lines: Vec<String>,
        truncated: bool,
        index_incomplete: bool,
    },
    IndexNotReady,
    BanHistoryOnly,
}

#[derive(Debug, Clone)]
pub(crate) struct PriorCases {
    pub entries: Vec<MemberHistoryEntry>,
    pub truncated: bool,
    pub index_complete: bool,
}

pub(crate) fn build_rejoin_note(
    cases: PriorCases,
    ever_banned: Option<bool>,
) -> Option<RejoinNote> {
    if cases.entries.is_empty() {
        if !cases.index_complete {
            return ever_banned
                .is_some_and(|ever_banned| ever_banned)
                .then_some(RejoinNote::IndexNotReady);
        }
        return match ever_banned {
            Some(true) => Some(RejoinNote::BanHistoryOnly),
            Some(false) | None => None,
        };
    }
    let lines = cases.entries.iter().map(format_case_line).collect();
    Some(RejoinNote::Cases {
        lines,
        truncated: cases.truncated,
        index_incomplete: !cases.index_complete,
    })
}

pub(crate) async fn collect_prior_cases(
    store: &GuildDataStore,
    scope: GuildDataScope,
    user_id: i64,
) -> Result<PriorCases, anyhow::Error> {
    let index_complete = store.member_case_index_complete(scope.clone()).await?;
    let (entries, truncated) = if index_complete {
        store
            .member_case_entries(scope.clone(), user_id, PRIOR_CASE_LIMIT)
            .await?
    } else {
        store
            .member_subject_case_entries(scope, user_id, PRIOR_CASE_LIMIT)
            .await?
    };
    Ok(PriorCases {
        entries,
        truncated,
        index_complete,
    })
}

/// Loads case lines for a member rejoin report after log delivery admission.
pub(crate) async fn enrich_member_rejoin_vars(
    app: &Arc<BotState>,
    guild_id: GuildId,
    user: &User,
    vars: &mut BTreeMap<String, String>,
) -> bool {
    let guild = i64::try_from(guild_id.get()).unwrap_or(0);
    let enrollment = match crate::db::guild_enrollment(&app.db, guild).await {
        Ok(Some(enrollment)) => enrollment,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                "member rejoin enrollment lookup failed: {error:#}"
            );
            return false;
        }
    };
    let (store, scope) = match app.tenant.live(enrollment).await {
        Ok(live) => live,
        Err(error) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                "member rejoin guild store unavailable: {error:#}"
            );
            return false;
        }
    };
    let user_id = user.id.get() as i64;
    let ever_banned = match store.get_member(scope.clone(), user_id).await {
        Ok(Some(profile)) => Some(profile.ever_banned),
        Ok(None) => Some(false),
        Err(error) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                user_id,
                "member rejoin profile unavailable: {error:#}"
            );
            None
        }
    };
    let collected = collect_prior_cases(&store, scope, user_id).await;
    let note = match collected {
        Ok(cases) => build_rejoin_note(cases, ever_banned),
        Err(error) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                user_id,
                "member rejoin case history unavailable: {error:#}"
            );
            None
        }
    };
    let Some(note) = note else {
        return false;
    };
    vars.extend(rejoin_note_vars(&note));
    true
}

pub(crate) fn rejoin_note_vars(note: &RejoinNote) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    match note {
        RejoinNote::IndexNotReady => {
            vars.insert("cases".into(), String::new());
            vars.insert(
                "notice".into(),
                "Recorded ban history exists; indexed case lines are not ready yet.".into(),
            );
        }
        RejoinNote::BanHistoryOnly => {
            vars.insert("cases".into(), String::new());
            vars.insert(
                "notice".into(),
                "Recorded ban history exists; no case details are available to list.".into(),
            );
        }
        RejoinNote::Cases {
            lines,
            truncated,
            index_incomplete,
        } => {
            let mut notice = String::new();
            if *index_incomplete {
                notice =
                    "Case index backfill is still running; additional history may appear later."
                        .into();
            }
            if *truncated {
                let omitted = "Older cases are omitted from this note.";
                notice = if notice.is_empty() {
                    omitted.into()
                } else {
                    format!("{notice} {omitted}")
                };
            }
            let notice_reserve = REJOIN_NOTICE_UTF16_RESERVE.max(notice.encode_utf16().count());
            vars.insert(
                "cases".into(),
                bound_rejoin_case_lines(lines, notice_reserve),
            );
            vars.insert("notice".into(), notice);
        }
    }
    vars
}

#[cfg(test)]
pub(crate) fn member_rejoin_vars(user: &User, note: &RejoinNote) -> BTreeMap<String, String> {
    let display = user
        .global_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(user.name.as_str());
    let mut vars = templates::user_vars(user.id.get(), &user.name, display);
    vars.extend(rejoin_note_vars(note));
    vars
}

fn format_case_line(entry: &MemberHistoryEntry) -> String {
    let action = &entry.kind;
    let outcome = entry.outcome.as_deref().unwrap_or("unknown");
    let time = format_case_time(entry.occurred_at_ms);
    let mut line = format!("- `{action}` · {outcome} · `{time}`");
    if let Some(reason) = entry.reason.as_deref().filter(|value| !value.is_empty()) {
        let clean = jmodguard::ban_reason::without_tag(reason);
        let clipped = clip_units(&sanitize_reason_for_report(clean), REASON_UNITS);
        if !clipped.is_empty() {
            line.push_str(&format!(" — {clipped}"));
        }
    }
    line
}

fn sanitize_reason_for_report(text: &str) -> String {
    let single_line = text.replace(['\r', '\n'], " ");
    single_line
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('_', "\\_")
}

fn format_case_time(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| "unavailable".into())
}

fn bound_rejoin_case_lines(lines: &[String], notice_reserve: usize) -> String {
    let budget = REJOIN_LOG_UTF16_BUDGET.saturating_sub(notice_reserve);
    let mut out = Vec::new();
    let mut used = 0usize;
    for line in lines {
        let line_units = line.encode_utf16().count();
        let separator = if out.is_empty() { 0 } else { 1 };
        if used + separator + line_units > budget {
            break;
        }
        used += separator + line_units;
        out.push(line.clone());
    }
    out.join("\n")
}

fn clip_units(text: &str, units: usize) -> String {
    let mut end = 0usize;
    for (count, (index, ch)) in text.char_indices().enumerate() {
        if count >= units {
            break;
        }
        end = index + ch.len_utf8();
    }
    if end < text.len() {
        format!("{}…", text[..end].trim_end())
    } else {
        text.to_string()
    }
}

pub(crate) async fn maybe_emit_member_rejoin(
    app: &Arc<BotState>,
    ctx: &serenity::client::Context,
    guild_id: GuildId,
    user: &User,
) -> bool {
    logging::emit_member_event(
        app,
        ctx,
        guild_id,
        "member_rejoin",
        user,
        BTreeMap::new(),
        None,
    )
    .await
    .0 == logging::Delivery::Sent
}

#[cfg(test)]
mod tests;
