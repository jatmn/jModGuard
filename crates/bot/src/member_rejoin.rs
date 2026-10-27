//! Optional prior-case note when a member joins and the guild store already has cases.

use crate::db::guild_data::{MemberHistoryEntry, MemberHistoryPage};
use crate::logging;
use crate::state::BotState;
use crate::templates;
use serenity::model::id::GuildId;
use serenity::model::user::User;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const PRIOR_CASE_LIMIT: u32 = 8;
const REASON_UNITS: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RejoinNote {
    Cases { lines: Vec<String>, truncated: bool },
    IndexNotReady,
}

pub(crate) fn rejoin_note_from_history(
    history: Result<MemberHistoryPage, anyhow::Error>,
    ever_banned: bool,
) -> Option<RejoinNote> {
    let page = match history {
        Ok(page) => page,
        Err(_) => return None,
    };
    let cases: Vec<MemberHistoryEntry> = page
        .entries
        .into_iter()
        .filter(|entry| entry.case_id.is_some())
        .collect();
    if cases.is_empty() {
        return ever_banned.then_some(RejoinNote::IndexNotReady);
    }
    let truncated = page.next_cursor.is_some();
    let lines = cases.iter().map(format_case_line).collect();
    Some(RejoinNote::Cases { lines, truncated })
}

pub(crate) fn member_rejoin_vars(user: &User, note: &RejoinNote) -> BTreeMap<String, String> {
    let display = user
        .global_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(user.name.as_str());
    let mut vars = templates::user_vars(user.id.get(), &user.name, display);
    match note {
        RejoinNote::IndexNotReady => {
            vars.insert("cases".into(), String::new());
            vars.insert(
                "notice".into(),
                "Ban history is recorded; indexed case lines are not ready yet.".into(),
            );
        }
        RejoinNote::Cases { lines, truncated } => {
            vars.insert("cases".into(), lines.join("\n"));
            vars.insert(
                "notice".into(),
                if *truncated {
                    "Older cases are omitted from this note.".into()
                } else {
                    String::new()
                },
            );
        }
    }
    vars
}

fn format_case_line(entry: &MemberHistoryEntry) -> String {
    let action = &entry.kind;
    let outcome = entry.outcome.as_deref().unwrap_or("unknown");
    let time = format_case_time(entry.occurred_at_ms);
    let mut line = format!("- `{action}` · {outcome} · `{time}`");
    if let Some(reason) = entry.reason.as_deref().filter(|value| !value.is_empty()) {
        let clean = jmodguard::ban_reason::without_tag(reason);
        let clipped = clip_units(clean, REASON_UNITS);
        if !clipped.is_empty() {
            line.push_str(&format!(" — {clipped}"));
        }
    }
    line
}

fn format_case_time(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| "unavailable".into())
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
) {
    let Some(enrollment) = crate::cases::capture(app, guild_id.get()).await else {
        return;
    };
    let Ok((store, scope)) = app.tenant.live(enrollment).await else {
        return;
    };
    let user_id = user.id.get() as i64;
    let ever_banned = match store.get_member(scope.clone(), user_id).await {
        Ok(Some(profile)) => profile.ever_banned,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                user_id,
                "member rejoin profile unavailable: {error:#}"
            );
            false
        }
    };
    let history = store
        .member_history(scope, user_id, None, PRIOR_CASE_LIMIT)
        .await;
    if let Err(error) = &history {
        tracing::warn!(
            guild_id = guild_id.get(),
            user_id,
            "member rejoin case history unavailable: {error:#}"
        );
    }
    let Some(note) = rejoin_note_from_history(history, ever_banned) else {
        return;
    };
    let vars = member_rejoin_vars(user, &note);
    logging::emit_member_event(app, ctx, guild_id, "member_rejoin", user, vars, None).await;
}

#[cfg(test)]
mod tests;
