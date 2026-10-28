//! `!userinfo` / `/userinfo` — cached member snapshot plus warns and case history.

use crate::db::guild_data::{CaseLimits, GuildDataScope, GuildDataStore, MemberHistoryEntry};
use anyhow::{Result, anyhow};
use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};
use std::collections::BTreeMap;

use super::{Args, BehaviorOutcome, CmdEnv};

pub(crate) const CASE_LINE_LIMIT: u32 = 8;

pub(crate) const MISSING: &str = "missing";
pub(crate) const VOICE_NOT_IN: &str = "not in a voice channel";
pub(crate) const VOICE_NOT_CACHED: &str = "not cached";
/// Voice channel, or a channel mention, whose view cannot be shown.
pub(crate) const VOICE_UNAVAILABLE: &str = "unavailable";
pub(crate) const CASES_UNAVAILABLE: &str = "unavailable";
pub(crate) const CASES_NONE: &str = "none";
pub(crate) const CLIPPED_SUFFIX: &str = "\n… clipped";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberSnapshot {
    pub user_id: u64,
    pub username: Option<String>,
    pub global_name: Option<String>,
    pub nick: Option<String>,
    pub joined_at: Option<String>,
    pub roles: Vec<String>,
    pub roles_cached: bool,
    pub voice: VoiceSnapshot,
    pub member_cached: bool,
    pub guild_cached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceSnapshot {
    NotCached,
    NotInChannel,
    /// The shared voice lookup could not show a channel name or id.
    Unavailable,
    Channel {
        name: String,
        id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WarnsField {
    Count(usize),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CasesField {
    Lines(Vec<String>),
    None,
    Unavailable,
}

pub(crate) fn capture_member_snapshot(
    guild_id: GuildId,
    target: UserId,
    actor_id: UserId,
    actor_roles: &[RoleId],
    cache: &serenity::cache::Cache,
) -> MemberSnapshot {
    let user_id = target.get();
    let global_user = cache.user(target);
    let username = global_user.as_ref().map(|u| u.name.clone());
    let global_name = global_user.as_ref().and_then(|u| u.global_name.clone());

    let Some(guild) = cache.guild(guild_id) else {
        return MemberSnapshot {
            user_id,
            username,
            global_name,
            nick: None,
            joined_at: None,
            roles: Vec::new(),
            roles_cached: false,
            voice: VoiceSnapshot::NotCached,
            member_cached: false,
            guild_cached: false,
        };
    };

    let member = guild.members.get(&target);
    let member_cached = member.is_some();
    let nick = member.and_then(|m| m.nick.clone());
    let joined_at = member.and_then(|m| m.joined_at).map(|ts| ts.to_string());
    let username = username.or_else(|| member.map(|m| m.user.name.clone()));
    let global_name = global_name.or_else(|| member.and_then(|m| m.user.global_name.clone()));

    MemberSnapshot {
        user_id,
        username,
        global_name,
        nick,
        joined_at,
        roles: role_labels(&guild.roles, member.map(|m| &m.roles)),
        roles_cached: true,
        voice: voice_from_lookup(cache, guild_id, target, actor_id, actor_roles),
        member_cached,
        guild_cached: true,
    }
}

fn role_labels(
    guild_roles: &std::collections::HashMap<RoleId, serenity::model::guild::Role>,
    member_roles: Option<&Vec<RoleId>>,
) -> Vec<String> {
    let Some(member_roles) = member_roles else {
        return Vec::new();
    };
    member_roles
        .iter()
        .map(|role_id| {
            guild_roles
                .get(role_id)
                .map(|r| r.name.clone())
                .unwrap_or_else(|| role_id.get().to_string())
        })
        .collect()
}

fn voice_from_lookup(
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    target: UserId,
    actor_id: UserId,
    actor_roles: &[RoleId],
) -> VoiceSnapshot {
    use super::voice_lookup::{VoiceLookupSnapshot, snapshot_cached_voice};
    match snapshot_cached_voice(cache, guild_id, actor_id, actor_roles, target) {
        VoiceLookupSnapshot::InChannel {
            channel_id,
            channel_name,
        } => VoiceSnapshot::Channel {
            name: channel_name,
            id: channel_id,
        },
        VoiceLookupSnapshot::NotInChannel => VoiceSnapshot::NotInChannel,
        VoiceLookupSnapshot::GuildNotCached => VoiceSnapshot::NotCached,
        VoiceLookupSnapshot::ChannelUnavailable => VoiceSnapshot::Unavailable,
    }
}

#[derive(Clone, Copy)]
enum MentionKind {
    Channel,
    User,
    Role,
}

/// User, role, and channel mentions in a case reason. An id the caller cannot
/// view, or that is not a snowflake, is replaced without leaving the id behind.
fn redact_unviewable_channel_mentions(text: &str, viewer: Option<ChannelView<'_>>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some((kind, header_len)) = mention_header(tail) else {
            out.push('<');
            rest = &tail[1..];
            continue;
        };
        let after = &tail[header_len..];
        let Some(end) = after.find('>') else {
            out.push_str(tail);
            return out;
        };
        let id_text = &after[..end];
        if id_text.is_empty() || !id_text.bytes().all(|byte| byte.is_ascii_digit()) {
            out.push_str(&tail[..header_len]);
            rest = &tail[header_len..];
            continue;
        }
        let raw_len = header_len + end + 1;
        if mention_is_reportable(kind, id_text, viewer) {
            out.push_str(&tail[..raw_len]);
        } else {
            out.push_str(VOICE_UNAVAILABLE);
        }
        rest = &tail[raw_len..];
    }
    out.push_str(rest);
    out
}

fn mention_header(tail: &str) -> Option<(MentionKind, usize)> {
    if tail.starts_with("<#") {
        Some((MentionKind::Channel, 2))
    } else if tail.starts_with("<@&") {
        Some((MentionKind::Role, 3))
    } else if tail.starts_with("<@!") {
        Some((MentionKind::User, 3))
    } else if tail.starts_with("<@") {
        Some((MentionKind::User, 2))
    } else {
        None
    }
}

/// `false` for zero, for a value that does not fit in a snowflake, and for a
/// channel the caller cannot view. A private thread also needs Manage Threads
/// or known membership; uncached membership stays hidden. Parsing is fallible
/// and never builds a zero id.
fn mention_is_reportable(kind: MentionKind, digits: &str, viewer: Option<ChannelView<'_>>) -> bool {
    match kind {
        MentionKind::Channel => digits.parse::<ChannelId>().ok().is_some_and(|channel_id| {
            viewer.is_some_and(|view| {
                super::member_can_view_channel(
                    view.guild,
                    view.actor_id,
                    view.actor_roles,
                    channel_id,
                ) && super::private_thread_visible_to(
                    view.guild,
                    view.actor_id,
                    view.actor_roles,
                    channel_id,
                )
            })
        }),
        MentionKind::User => digits.parse::<UserId>().is_ok(),
        MentionKind::Role => digits.parse::<RoleId>().is_ok(),
    }
}

pub(crate) fn snapshot_has_cached_user(snapshot: &MemberSnapshot) -> bool {
    snapshot.member_cached || snapshot.username.is_some()
}

pub(crate) fn apply_rest_user(snapshot: &mut MemberSnapshot, name: String, global: Option<String>) {
    if snapshot.username.is_none() {
        snapshot.username = Some(name);
    }
    if snapshot.global_name.is_none() {
        snapshot.global_name = global;
    }
}

pub(crate) fn apply_missing_user_identity(snapshot: &mut MemberSnapshot) {
    if snapshot.username.is_none() {
        snapshot.username = Some(MISSING.to_string());
    }
    if snapshot.global_name.is_none() {
        snapshot.global_name = Some(MISSING.to_string());
    }
}

pub(crate) fn case_lines_from_entries(
    entries: &[MemberHistoryEntry],
    viewer: Option<ChannelView<'_>>,
) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            let reason = entry
                .reason
                .as_deref()
                .map(|reason| redact_unviewable_channel_mentions(reason, viewer));
            format_case_line(
                &entry.kind,
                entry.outcome.as_deref(),
                entry.occurred_at_ms,
                reason.as_deref(),
            )
        })
        .collect()
}

/// Caller identity used to decide whether a cached channel may be named.
#[derive(Clone, Copy)]
pub(crate) struct ChannelView<'a> {
    pub guild: &'a serenity::model::guild::Guild,
    pub actor_id: UserId,
    pub actor_roles: &'a [RoleId],
}

pub(crate) fn format_voice(voice: &VoiceSnapshot) -> String {
    match voice {
        VoiceSnapshot::NotCached => VOICE_NOT_CACHED.to_string(),
        VoiceSnapshot::NotInChannel => VOICE_NOT_IN.to_string(),
        VoiceSnapshot::Unavailable => VOICE_UNAVAILABLE.to_string(),
        VoiceSnapshot::Channel { name, id } => format!("{} (`{id}`)", report_text(name)),
    }
}

pub(crate) fn format_roles(roles: &[String], roles_cached: bool, member_cached: bool) -> String {
    if !roles_cached || !member_cached {
        return MISSING.to_string();
    }
    if roles.is_empty() {
        return MISSING.to_string();
    }
    roles.join(", ")
}

pub(crate) fn format_warns(warns: &WarnsField) -> String {
    match warns {
        WarnsField::Count(n) => n.to_string(),
        WarnsField::Unavailable => "unavailable".to_string(),
    }
}

pub(crate) fn format_cases(cases: &CasesField) -> String {
    match cases {
        CasesField::Lines(lines) => lines.join("\n"),
        CasesField::None => CASES_NONE.to_string(),
        CasesField::Unavailable => CASES_UNAVAILABLE.to_string(),
    }
}

pub(crate) fn format_case_line(
    action: &str,
    outcome: Option<&str>,
    occurred_at_ms: Option<i64>,
    reason: Option<&str>,
) -> String {
    let time = match occurred_at_ms {
        Some(ms) => {
            let ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| ms.to_string());
            crate::db::time_ago(&Some(ts))
        }
        None => MISSING.to_string(),
    };
    let outcome = outcome.unwrap_or(MISSING);
    let mut line = format!("`{action}` ({outcome}) — {time}");
    if let Some(reason) = reason {
        let stripped = jmodguard::ban_reason::without_tag(reason);
        if !stripped.is_empty() {
            let clipped = clip_reason(&report_text(stripped), 120);
            line.push_str(&format!(" — {clipped}"));
        }
    }
    line
}

fn clip_reason(reason: &str, max_chars: usize) -> String {
    if reason.chars().count() <= max_chars {
        return reason.to_string();
    }
    let mut out: String = reason.chars().take(max_chars).collect();
    out.push('…');
    out
}

pub(crate) fn userinfo_template_vars(
    snapshot: &MemberSnapshot,
    warns: &WarnsField,
    cases: &CasesField,
    roles: &[String],
    case_lines: &[String],
    clipped: &str,
) -> BTreeMap<String, String> {
    let account_created = UserId::new(snapshot.user_id).created_at();
    let account_age_seconds =
        (chrono::Utc::now().timestamp() - account_created.unix_timestamp()).max(0) as u64;
    let fallback_id = snapshot.user_id.to_string();
    let display = snapshot
        .nick
        .as_deref()
        .or(snapshot.global_name.as_deref())
        .or(snapshot.username.as_deref())
        .unwrap_or(&fallback_id);
    let mut vars = BTreeMap::new();
    vars.insert("user_id".into(), snapshot.user_id.to_string());
    vars.insert("display".into(), report_text(display));
    vars.insert(
        "username".into(),
        snapshot
            .username
            .as_deref()
            .map(report_text)
            .unwrap_or_else(|| MISSING.to_string()),
    );
    vars.insert(
        "global_name".into(),
        snapshot
            .global_name
            .as_deref()
            .map(report_text)
            .unwrap_or_else(|| MISSING.to_string()),
    );
    vars.insert(
        "nick".into(),
        snapshot
            .nick
            .as_deref()
            .map(report_text)
            .unwrap_or_else(|| MISSING.to_string()),
    );
    vars.insert("account_created".into(), account_created.to_string());
    vars.insert(
        "account_age".into(),
        jmodguard::templates::format_duration(account_age_seconds),
    );
    vars.insert(
        "joined_at".into(),
        if snapshot.member_cached {
            snapshot
                .joined_at
                .clone()
                .unwrap_or_else(|| MISSING.to_string())
        } else {
            MISSING.to_string()
        },
    );
    let safe_roles: Vec<String> = roles.iter().map(|role| report_text(role)).collect();
    vars.insert(
        "roles".into(),
        format_roles(&safe_roles, snapshot.roles_cached, snapshot.member_cached),
    );
    vars.insert("voice".into(), format_voice(&snapshot.voice));
    vars.insert("warns".into(), format_warns(warns));
    vars.insert("cases".into(), format_cases(cases));
    vars.insert("clipped".into(), clipped.to_string());
    let _ = case_lines;
    vars
}

pub(crate) fn render_userinfo_message(
    template: &str,
    snapshot: &MemberSnapshot,
    warns: &WarnsField,
    case_lines: Vec<String>,
) -> String {
    let cases_field = if case_lines.is_empty() {
        CasesField::None
    } else {
        CasesField::Lines(case_lines.clone())
    };
    render_userinfo_with_cases(template, snapshot, warns, cases_field, case_lines)
}

pub(crate) fn render_userinfo_with_cases(
    template: &str,
    snapshot: &MemberSnapshot,
    warns: &WarnsField,
    cases: CasesField,
    case_lines: Vec<String>,
) -> String {
    let mut roles = snapshot.roles.clone();
    let mut lines = match &cases {
        CasesField::Lines(values) => values.clone(),
        CasesField::None | CasesField::Unavailable => case_lines,
    };
    let mut clipped = String::new();
    let cases_for_pass = cases.clone();
    if fits(
        template,
        snapshot,
        warns,
        &cases_for_pass,
        &roles,
        &lines,
        &clipped,
    ) {
        return render_pass(
            template,
            snapshot,
            warns,
            &cases_for_pass,
            &roles,
            &lines,
            &clipped,
        );
    }
    roles.clear();
    clipped = CLIPPED_SUFFIX.to_string();
    let cases_for_pass = cases.clone();
    if fits(
        template,
        snapshot,
        warns,
        &cases_for_pass,
        &roles,
        &lines,
        &clipped,
    ) {
        return render_pass(
            template,
            snapshot,
            warns,
            &cases_for_pass,
            &roles,
            &lines,
            &clipped,
        );
    }
    while lines.len() > 1 {
        lines.pop();
        let trimmed = CasesField::Lines(lines.clone());
        if fits(
            template, snapshot, warns, &trimmed, &roles, &lines, &clipped,
        ) {
            return render_pass(
                template, snapshot, warns, &trimmed, &roles, &lines, &clipped,
            );
        }
    }
    jmodguard::templates::render_bounded(
        template,
        &userinfo_template_vars(
            snapshot,
            warns,
            &CasesField::Unavailable,
            &[],
            &[],
            CLIPPED_SUFFIX,
        ),
        2000,
    )
}

fn fits(
    template: &str,
    snapshot: &MemberSnapshot,
    warns: &WarnsField,
    cases: &CasesField,
    roles: &[String],
    case_lines: &[String],
    clipped: &str,
) -> bool {
    let vars = userinfo_template_vars(snapshot, warns, cases, roles, case_lines, clipped);
    utf16_len(&jmodguard::templates::render(template, &vars)) <= 2000
}

fn render_pass(
    template: &str,
    snapshot: &MemberSnapshot,
    warns: &WarnsField,
    cases: &CasesField,
    roles: &[String],
    case_lines: &[String],
    clipped: &str,
) -> String {
    let display_cases = match cases {
        CasesField::Lines(_) => CasesField::Lines(case_lines.to_vec()),
        other => other.clone(),
    };
    let vars = userinfo_template_vars(snapshot, warns, &display_cases, roles, case_lines, clipped);
    jmodguard::templates::render_bounded(template, &vars, 2000)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Member-fed and case-fed report text. Same escaping as a rejoin case reason:
/// one line, with mention and markdown characters treated as data.
fn report_text(text: &str) -> String {
    crate::member_rejoin::sanitize_reason_for_report(text)
}

pub(crate) async fn resolve_userinfo_target(env: &CmdEnv<'_>, args: &mut Args) -> Result<UserId> {
    match args.next_arg() {
        Some(token) => super::resolve_user(env.ctx, env.guild_id, &token)
            .await
            .ok_or_else(|| anyhow!("could not resolve user `{token}`")),
        None => Ok(env.actor),
    }
}

pub(crate) fn discord_lookup_error(err: &serenity::Error) -> Option<String> {
    let code = match err {
        serenity::Error::Http(http_err) => http_err.status_code().map(|s| s.as_u16()).unwrap_or(0),
        _ => return None,
    };
    if code == 429 || (500..=599).contains(&code) {
        Some(format!("Discord API error ({code}): {err}"))
    } else {
        None
    }
}

pub(crate) async fn rest_user_lookup(
    env: &CmdEnv<'_>,
    target: UserId,
) -> Result<(String, Option<String>)> {
    let call =
        crate::discord::race_discord_call(&env.app.activation, env.ctx.http.get_user(target)).await;
    match call {
        crate::discord::DiscordCall::Cancelled => Err(anyhow!("writer lease is not held")),
        crate::discord::DiscordCall::Finished(Ok(user)) => {
            Ok((user.name.clone(), user.global_name.clone()))
        }
        crate::discord::DiscordCall::Finished(Err(err)) => {
            if let Some(message) = discord_lookup_error(&err) {
                Err(anyhow!(message))
            } else {
                Err(err.into())
            }
        }
    }
}

async fn load_case_lines(
    env: &CmdEnv<'_>,
    enrollment: Option<crate::db::GuildEnrollment>,
    target: UserId,
) -> CasesField {
    let Some(enrollment) = enrollment else {
        return CasesField::Unavailable;
    };
    let live = env.app.tenant.live(enrollment).await;
    let Ok((store, scope)) = live else {
        return CasesField::Unavailable;
    };
    load_case_lines_from_store(&store, scope, target, Some(env)).await
}

fn cases_from_loaded(
    loaded: Result<(Vec<MemberHistoryEntry>, bool)>,
    viewer: Option<ChannelView<'_>>,
) -> CasesField {
    match loaded {
        Ok((entries, _truncated)) => {
            let lines = case_lines_from_entries(&entries, viewer);
            if lines.is_empty() {
                CasesField::None
            } else {
                CasesField::Lines(lines)
            }
        }
        Err(_) => CasesField::Unavailable,
    }
}

async fn load_case_lines_from_store(
    store: &GuildDataStore,
    scope: GuildDataScope,
    target: UserId,
    env: Option<&CmdEnv<'_>>,
) -> CasesField {
    let loaded = store
        .member_case_entries(
            scope,
            target.get() as i64,
            CASE_LINE_LIMIT,
            CaseLimits::default(),
        )
        .await;
    let guild = env.and_then(|env| env.ctx.cache.guild(env.guild_id));
    let viewer = match (env, guild.as_ref()) {
        (Some(env), Some(guild)) => Some(ChannelView {
            guild,
            actor_id: env.actor,
            actor_roles: &env.actor_member.roles,
        }),
        _ => None,
    };
    cases_from_loaded(loaded, viewer)
}

pub(crate) async fn run(
    env: &CmdEnv<'_>,
    enrollment: Option<crate::db::GuildEnrollment>,
    vars: &mut BTreeMap<String, String>,
) -> Result<BehaviorOutcome> {
    let mut args = env.args.clone();
    let target = resolve_userinfo_target(env, &mut args).await?;
    let mut snapshot = capture_member_snapshot(
        env.guild_id,
        target,
        env.actor,
        &env.actor_member.roles,
        &env.ctx.cache,
    );

    if !snapshot_has_cached_user(&snapshot) {
        match rest_user_lookup(env, target).await {
            Ok((name, global)) => apply_rest_user(&mut snapshot, name, global),
            Err(error) if error.to_string() == "writer lease is not held" => return Err(error),
            Err(_) => apply_missing_user_identity(&mut snapshot),
        }
    }

    let warns =
        match crate::db::count_warns(&env.app.db, env.guild_id.get() as i64, target.get() as i64)
            .await
        {
            Ok(count) => WarnsField::Count(count),
            Err(_) => WarnsField::Unavailable,
        };

    let cases = load_case_lines(env, enrollment, target).await;
    let template = jmodguard::defaults::USER_INFO_TEMPLATE;
    let response = match cases {
        CasesField::Unavailable => render_userinfo_with_cases(
            template,
            &snapshot,
            &warns,
            CasesField::Unavailable,
            Vec::new(),
        ),
        CasesField::None => render_userinfo_message(template, &snapshot, &warns, Vec::new()),
        CasesField::Lines(lines) => render_userinfo_message(template, &snapshot, &warns, lines),
    };
    vars.insert("target_id".into(), target.to_string());
    Ok(BehaviorOutcome {
        response: Some(response),
        vars: vars.clone(),
        suppress_response: false,
    })
}

#[cfg(test)]
mod tests;
