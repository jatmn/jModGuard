//! Discord-only activity reports. No database write and no message text.
//!
//! Member diffs require the previous member this process already cached.
//! A missing previous member is not a change.

use std::collections::{BTreeMap, BTreeSet};

use serenity::model::channel::{ChannelType, GuildChannel, PartialGuildChannel};
use serenity::model::guild::{Member, Role};
use serenity::model::permissions::Permissions;
use serenity::model::voice::VoiceState;

use crate::templates;

const NAME_UNITS: usize = 80;
const LIST_UNITS: usize = 700;
const DETAIL_UNITS: usize = 500;

pub(crate) struct ActivityReport {
    pub event: &'static str,
    pub vars: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemberView {
    pub user_id: u64,
    pub username: String,
    pub global_name: Option<String>,
    pub nick: Option<String>,
    pub roles: BTreeSet<u64>,
}

impl MemberView {
    pub(crate) fn from_member(member: &Member) -> Self {
        Self {
            user_id: member.user.id.get(),
            username: member.user.name.clone(),
            global_name: member.user.global_name.clone(),
            nick: member.nick.clone(),
            roles: member.roles.iter().map(|id| id.get()).collect(),
        }
    }

    pub(crate) fn from_voice(state: &VoiceState) -> Self {
        if let Some(member) = &state.member {
            return Self::from_member(member);
        }
        let id = state.user_id.get();
        let label = id.to_string();
        Self {
            user_id: id,
            username: label,
            global_name: None,
            nick: None,
            roles: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoleView {
    pub id: u64,
    pub guild_id: u64,
    pub name: String,
    pub permissions: Permissions,
    pub colour: u32,
    pub secondary_colour: Option<u32>,
    pub tertiary_colour: Option<u32>,
    pub hoist: bool,
    pub mentionable: bool,
    pub position: u16,
    pub unicode_emoji: Option<String>,
    pub has_icon: bool,
}

impl RoleView {
    pub(crate) fn from_role(role: &Role) -> Self {
        Self {
            id: role.id.get(),
            guild_id: role.guild_id.get(),
            name: role.name.clone(),
            permissions: role.permissions,
            colour: role.colour.0,
            secondary_colour: role.colours.secondary_colour.map(|colour| colour.0),
            tertiary_colour: role.colours.tertiary_colour.map(|colour| colour.0),
            hoist: role.hoist,
            mentionable: role.mentionable,
            position: role.position,
            unicode_emoji: role.unicode_emoji.clone(),
            has_icon: role.icon.is_some(),
        }
    }

    pub(crate) fn unknown(id: u64, guild_id: u64) -> Self {
        Self {
            id,
            guild_id,
            name: String::new(),
            permissions: Permissions::empty(),
            colour: 0,
            secondary_colour: None,
            tertiary_colour: None,
            hoist: false,
            mentionable: false,
            position: 0,
            unicode_emoji: None,
            has_icon: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoleAction {
    Created,
    Updated,
    Deleted,
}

impl RoleAction {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadView {
    pub id: u64,
    pub guild_id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub owner_id: Option<u64>,
    pub archived: Option<bool>,
    pub locked: Option<bool>,
    pub auto_archive_minutes: Option<u16>,
    pub invitable: Option<bool>,
}

impl ThreadView {
    pub(crate) fn from_guild(channel: &GuildChannel) -> Option<Self> {
        if !is_thread(channel.kind) {
            return None;
        }
        let meta = channel.thread_metadata;
        Some(Self {
            id: channel.id.get(),
            guild_id: channel.guild_id.get(),
            parent_id: channel.parent_id.map(|id| id.get()),
            name: channel.name.clone(),
            owner_id: channel.owner_id.map(|id| id.get()),
            archived: meta.map(|meta| meta.archived),
            locked: meta.map(|meta| meta.locked),
            auto_archive_minutes: meta.map(|meta| u16::from(meta.auto_archive_duration)),
            invitable: meta.map(|meta| meta.invitable),
        })
    }

    pub(crate) fn from_deleted(
        partial: &PartialGuildChannel,
        full: Option<&GuildChannel>,
    ) -> Option<Self> {
        if let Some(full) = full {
            return Self::from_guild(full);
        }
        if !is_thread(partial.kind) {
            return None;
        }
        Some(Self {
            id: partial.id.get(),
            guild_id: partial.guild_id.get(),
            parent_id: Some(partial.parent_id.get()),
            name: String::new(),
            owner_id: None,
            archived: None,
            locked: None,
            auto_archive_minutes: None,
            invitable: None,
        })
    }
}

fn is_thread(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    )
}

pub(crate) fn member_activity_reports(old: &MemberView, new: &MemberView) -> Vec<ActivityReport> {
    if old.user_id != new.user_id {
        return Vec::new();
    }
    let mut reports = Vec::new();
    let added: Vec<u64> = new.roles.difference(&old.roles).copied().collect();
    let removed: Vec<u64> = old.roles.difference(&new.roles).copied().collect();
    if !added.is_empty() || !removed.is_empty() {
        let mut vars = member_vars(new);
        vars.insert("added".into(), role_list(&added));
        vars.insert("removed".into(), role_list(&removed));
        reports.push(ActivityReport {
            event: "member_role_update",
            vars,
        });
    }
    if old.nick != new.nick {
        let mut vars = member_vars(new);
        vars.insert("before".into(), optional_label(&old.nick));
        vars.insert("after".into(), optional_label(&new.nick));
        reports.push(ActivityReport {
            event: "nickname_change",
            vars,
        });
    }
    if old.username != new.username || old.global_name != new.global_name {
        let mut vars = member_vars(new);
        vars.insert("before".into(), bound_text(&old.username, NAME_UNITS));
        vars.insert("after".into(), bound_text(&new.username, NAME_UNITS));
        vars.insert("before_global".into(), optional_label(&old.global_name));
        vars.insert("after_global".into(), optional_label(&new.global_name));
        reports.push(ActivityReport {
            event: "username_change",
            vars,
        });
    }
    reports
}

pub(crate) fn voice_report(
    member: &MemberView,
    old_channel: Option<u64>,
    new_channel: Option<u64>,
) -> Option<ActivityReport> {
    let (event, channel, before) = match (old_channel, new_channel) {
        (None, Some(channel)) => ("voice_join", channel, None),
        (Some(channel), None) => ("voice_leave", channel, None),
        (Some(before), Some(channel)) if before != channel => ("voice_move", channel, Some(before)),
        _ => return None,
    };
    let mut vars = member_vars(member);
    vars.insert("channel".into(), channel_mention(channel));
    vars.insert("channel_id".into(), channel.to_string());
    vars.insert("source_channel_id".into(), channel.to_string());
    if let Some(before) = before {
        vars.insert("before".into(), channel_mention(before));
        vars.insert("before_channel_id".into(), before.to_string());
    }
    Some(ActivityReport { event, vars })
}

pub(crate) fn role_change_report(
    action: RoleAction,
    old: Option<&RoleView>,
    new: Option<&RoleView>,
) -> Option<ActivityReport> {
    let current = new.or(old)?;
    if current.guild_id == 0 || current.id == 0 {
        return None;
    }
    if action == RoleAction::Updated && old.is_some_and(|old| old == current) {
        return None;
    }
    let mut vars = BTreeMap::new();
    vars.insert("action".into(), action.label().into());
    vars.insert("role".into(), role_mention(current.id));
    vars.insert("role_id".into(), current.id.to_string());
    vars.insert("role_name".into(), name_or_unavailable(&current.name));
    vars.insert("detail".into(), role_detail(action, old, new));
    Some(ActivityReport {
        event: "role_change",
        vars,
    })
}

pub(crate) fn thread_created(thread: &ThreadView) -> Option<ActivityReport> {
    thread_report("thread_create", thread, None)
}

pub(crate) fn thread_deleted(thread: &ThreadView) -> Option<ActivityReport> {
    thread_report("thread_delete", thread, None)
}

pub(crate) fn thread_updated(old: Option<&ThreadView>, new: &ThreadView) -> Option<ActivityReport> {
    if let Some(old) = old
        && old.name == new.name
        && old.archived == new.archived
        && old.locked == new.locked
        && old.auto_archive_minutes == new.auto_archive_minutes
        && old.invitable == new.invitable
    {
        return None;
    }
    thread_report("thread_update", new, old)
}

fn thread_report(
    event: &'static str,
    thread: &ThreadView,
    old: Option<&ThreadView>,
) -> Option<ActivityReport> {
    if thread.guild_id == 0 || thread.id == 0 {
        return None;
    }
    let mut vars = BTreeMap::new();
    vars.insert("thread".into(), channel_mention(thread.id));
    vars.insert("thread_id".into(), thread.id.to_string());
    vars.insert("thread_name".into(), name_or_unavailable(&thread.name));
    match thread.parent_id {
        Some(parent) if parent != 0 => {
            vars.insert("channel".into(), channel_mention(parent));
            vars.insert("source_channel_id".into(), parent.to_string());
        }
        _ => {
            vars.insert("channel".into(), "unavailable".into());
        }
    }
    if let Some(owner) = thread.owner_id.filter(|id| *id != 0) {
        let label = owner.to_string();
        vars.extend(templates::user_vars(owner, &label, &label));
    } else {
        vars.insert("mention".into(), "unavailable".into());
    }
    if event == "thread_update" {
        vars.insert(
            "before".into(),
            old.map(|old| name_or_unavailable(&old.name))
                .unwrap_or_else(|| "unavailable".into()),
        );
        vars.insert("archived".into(), flag_label(thread.archived));
        vars.insert("locked".into(), flag_label(thread.locked));
        vars.insert(
            "auto_archive".into(),
            archive_label(thread.auto_archive_minutes),
        );
        vars.insert("invitable".into(), flag_label(thread.invitable));
    }
    Some(ActivityReport { event, vars })
}

fn member_vars(member: &MemberView) -> BTreeMap<String, String> {
    let display = member
        .nick
        .as_deref()
        .map(str::trim)
        .filter(|nick| !nick.is_empty())
        .or_else(|| {
            member
                .global_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
        })
        .unwrap_or(member.username.as_str());
    templates::user_vars(
        member.user_id,
        &bound_text(&member.username, NAME_UNITS),
        &bound_text(display, NAME_UNITS),
    )
}

fn role_detail(action: RoleAction, old: Option<&RoleView>, new: Option<&RoleView>) -> String {
    let text = match action {
        RoleAction::Created => format!(
            "created {}",
            name_or_unavailable(new.map(|role| role.name.as_str()).unwrap_or(""))
        ),
        RoleAction::Deleted => format!(
            "deleted {}",
            name_or_unavailable(old.or(new).map(|role| role.name.as_str()).unwrap_or(""))
        ),
        RoleAction::Updated => match (old, new) {
            (Some(old), Some(new)) => role_changes(old, new),
            _ => "updated; previous role state unavailable".into(),
        },
    };
    bound_text(&text, DETAIL_UNITS)
}

fn role_changes(old: &RoleView, new: &RoleView) -> String {
    let mut parts = Vec::new();
    if old.name != new.name {
        parts.push(format!(
            "name {} → {}",
            name_or_unavailable(&old.name),
            name_or_unavailable(&new.name)
        ));
    }
    if old.colour != new.colour
        || old.secondary_colour != new.secondary_colour
        || old.tertiary_colour != new.tertiary_colour
    {
        parts.push("colour changed".into());
    }
    if old.permissions != new.permissions {
        parts.push("permissions changed".into());
    }
    if old.hoist != new.hoist {
        parts.push(format!("hoist {}", on_off(new.hoist)));
    }
    if old.mentionable != new.mentionable {
        parts.push(format!("mentionable {}", on_off(new.mentionable)));
    }
    if old.position != new.position {
        parts.push(format!("position {} → {}", old.position, new.position));
    }
    if old.unicode_emoji != new.unicode_emoji {
        parts.push("emoji changed".into());
    }
    if old.has_icon != new.has_icon {
        parts.push("icon changed".into());
    }
    if parts.is_empty() {
        "updated".into()
    } else {
        parts.join("; ")
    }
}

fn role_list(ids: &[u64]) -> String {
    if ids.is_empty() {
        return "none".into();
    }
    let text = ids
        .iter()
        .map(|id| role_mention(*id))
        .collect::<Vec<_>>()
        .join(" ");
    bound_text(&text, LIST_UNITS)
}

fn role_mention(id: u64) -> String {
    format!("<@&{id}>")
}

fn channel_mention(id: u64) -> String {
    format!("<#{id}>")
}

fn optional_label(value: &Option<String>) -> String {
    match value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) => bound_text(text, NAME_UNITS),
        None => "none".into(),
    }
}

fn name_or_unavailable(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "unavailable".into()
    } else {
        bound_text(trimmed, NAME_UNITS)
    }
}

fn archive_label(minutes: Option<u16>) -> String {
    match minutes {
        Some(0) => "off".into(),
        Some(mins) => format!("{mins} minutes"),
        None => "unavailable".into(),
    }
}

fn flag_label(value: Option<bool>) -> String {
    match value {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "unavailable".into(),
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn bound_text(text: &str, units: usize) -> String {
    if text.encode_utf16().count() <= units {
        return text.to_string();
    }
    let marker = " [display clipped]";
    let keep = units.saturating_sub(marker.encode_utf16().count());
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let next = ch.len_utf16();
        if used + next > keep {
            break;
        }
        out.push(ch);
        used += next;
    }
    out.push_str(marker);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BotProcessConfig;
    use crate::db::{self, Db, SqlExecutor};
    use crate::logging::{Delivery, LogDelivery, emit_to};
    use crate::state::BotState;
    use crate::types::ACTIVITY_LOG_EVENTS;
    use serenity::async_trait;
    use serenity::builder::CreateMessage;
    use serenity::model::id::{ChannelId, GuildId};
    use std::sync::{Arc, Mutex};

    fn member(
        id: u64,
        username: &str,
        global: Option<&str>,
        nick: Option<&str>,
        roles: &[u64],
    ) -> MemberView {
        MemberView {
            user_id: id,
            username: username.into(),
            global_name: global.map(str::to_string),
            nick: nick.map(str::to_string),
            roles: roles.iter().copied().collect(),
        }
    }

    fn thread(name: &str, archived: bool) -> ThreadView {
        ThreadView {
            id: 77,
            guild_id: 789,
            parent_id: Some(321),
            name: name.into(),
            owner_id: Some(42),
            archived: Some(archived),
            locked: Some(false),
            auto_archive_minutes: Some(1440),
            invitable: Some(false),
        }
    }

    fn role(name: &str, position: u16) -> RoleView {
        RoleView {
            id: 55,
            guild_id: 789,
            name: name.into(),
            permissions: Permissions::empty(),
            colour: 0,
            secondary_colour: None,
            tertiary_colour: None,
            hoist: false,
            mentionable: false,
            position,
            unicode_emoji: None,
            has_icon: false,
        }
    }

    #[test]
    fn member_diffs_cover_roles_nickname_and_username_only() {
        let old = member(
            42,
            "oldname",
            Some("Old Display"),
            Some("Old Nick"),
            &[1, 2],
        );
        assert!(member_activity_reports(&old, &old).is_empty());

        let nick = member(
            42,
            "oldname",
            Some("Old Display"),
            Some("New Nick"),
            &[1, 2],
        );
        let reports = member_activity_reports(&old, &nick);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].event, "nickname_change");
        assert_eq!(reports[0].vars["before"], "Old Nick");
        assert_eq!(reports[0].vars["after"], "New Nick");

        let cleared = member(42, "oldname", Some("Old Display"), None, &[1, 2]);
        let reports = member_activity_reports(&old, &cleared);
        assert_eq!(reports[0].vars["after"], "none");

        let renamed = member(
            42,
            "newname",
            Some("New Display"),
            Some("Old Nick"),
            &[1, 2],
        );
        let reports = member_activity_reports(&old, &renamed);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].event, "username_change");
        assert_eq!(reports[0].vars["before"], "oldname");
        assert_eq!(reports[0].vars["after"], "newname");
        assert_eq!(reports[0].vars["before_global"], "Old Display");
        assert_eq!(reports[0].vars["after_global"], "New Display");

        let display_only = member(
            42,
            "oldname",
            Some("New Display"),
            Some("Old Nick"),
            &[1, 2],
        );
        assert_eq!(
            member_activity_reports(&old, &display_only)[0].event,
            "username_change"
        );

        let roles = member(
            42,
            "oldname",
            Some("Old Display"),
            Some("Old Nick"),
            &[2, 3],
        );
        let reports = member_activity_reports(&old, &roles);
        assert_eq!(reports[0].event, "member_role_update");
        assert_eq!(reports[0].vars["added"], "<@&3>");
        assert_eq!(reports[0].vars["removed"], "<@&1>");
        assert!(!reports[0].vars.contains_key("content"));
    }

    #[test]
    fn long_role_lists_stay_inside_the_discord_limit() {
        let old = member(42, "oldname", None, None, &[]);
        let ids: Vec<u64> = (0..80).map(|n| 1_000_000_000_000_000_000u64 + n).collect();
        let new = member(42, "oldname", None, None, &ids);
        let report = member_activity_reports(&old, &new).pop().unwrap();
        assert!(report.vars["added"].contains("[display clipped]"));
        let template = crate::defaults::activity_log_templates()
            .into_iter()
            .find(|(event, _)| *event == "member_role_update")
            .unwrap()
            .1;
        let rendered = templates::render_bounded(template, &report.vars, 2000);
        assert!(rendered.encode_utf16().count() <= 2000);
        assert!(rendered.contains("roles updated"));
        assert!(!rendered.contains("message body"));
    }

    #[test]
    fn voice_classifies_join_leave_and_move_only() {
        let member = member(42, "ada", None, None, &[]);
        assert!(voice_report(&member, None, None).is_none());
        assert!(voice_report(&member, Some(321), Some(321)).is_none());
        let join = voice_report(&member, None, Some(321)).unwrap();
        assert_eq!(join.event, "voice_join");
        assert_eq!(join.vars["channel"], "<#321>");
        assert_eq!(join.vars["source_channel_id"], "321");
        let leave = voice_report(&member, Some(321), None).unwrap();
        assert_eq!(leave.event, "voice_leave");
        let moved = voice_report(&member, Some(321), Some(654)).unwrap();
        assert_eq!(moved.event, "voice_move");
        assert_eq!(moved.vars["before"], "<#321>");
        assert_eq!(moved.vars["before_channel_id"], "321");
        assert_eq!(moved.vars["source_channel_id"], "654");
        assert!(!moved.vars.contains_key("content"));
    }

    #[test]
    fn role_and_thread_reports_skip_unchanged_state_and_message_text() {
        let created = role("Mods", 1);
        let report = role_change_report(RoleAction::Created, None, Some(&created)).unwrap();
        assert_eq!(report.event, "role_change");
        assert_eq!(report.vars["action"], "created");
        assert!(report.vars["detail"].contains("Mods"));
        assert!(role_change_report(RoleAction::Updated, Some(&created), Some(&created)).is_none());
        let renamed = role("Staff", 2);
        let updated =
            role_change_report(RoleAction::Updated, Some(&created), Some(&renamed)).unwrap();
        assert!(updated.vars["detail"].contains("name"));
        assert!(updated.vars["detail"].contains("position 1 → 2"));
        assert!(!updated.vars["detail"].contains("permissions"));
        let mut emoji = created.clone();
        emoji.unicode_emoji = Some("star".into());
        emoji.has_icon = true;
        let appearance =
            role_change_report(RoleAction::Updated, Some(&created), Some(&emoji)).unwrap();
        assert!(appearance.vars["detail"].contains("emoji changed"));
        assert!(appearance.vars["detail"].contains("icon changed"));
        assert!(!appearance.vars["detail"].contains("star"));

        let open = thread("topic title", false);
        assert!(thread_updated(Some(&open), &open).is_none());
        let archived = thread("topic title", true);
        let update = thread_updated(Some(&open), &archived).unwrap();
        assert_eq!(update.vars["archived"], "yes");
        assert_eq!(update.vars["before"], "topic title");
        assert_eq!(update.vars["auto_archive"], "1440 minutes");
        assert_eq!(update.vars["invitable"], "no");
        let mut longer = open.clone();
        longer.auto_archive_minutes = Some(10080);
        let timer = thread_updated(Some(&open), &longer).unwrap();
        assert_eq!(timer.vars["auto_archive"], "10080 minutes");
        let created = thread_created(&open).unwrap();
        assert_eq!(created.event, "thread_create");
        assert_eq!(created.vars["thread_name"], "topic title");
        assert_eq!(created.vars["source_channel_id"], "321");
        assert_eq!(created.vars["mention"], "<@42>");
        assert!(!created.vars.contains_key("content"));
        let unknown = thread_updated(None, &open).unwrap();
        assert_eq!(unknown.vars["before"], "unavailable");
        let deleted = thread_deleted(&ThreadView {
            name: String::new(),
            owner_id: None,
            ..open
        })
        .unwrap();
        assert_eq!(deleted.vars["thread_name"], "unavailable");
        assert_eq!(deleted.vars["mention"], "unavailable");
    }

    fn sample(event: &str) -> ActivityReport {
        let old = member(42, "oldname", Some("Old Display"), Some("Old Nick"), &[1]);
        let new = member(42, "newname", Some("New Display"), Some("New Nick"), &[2]);
        match event {
            "member_role_update" | "nickname_change" | "username_change" => {
                member_activity_reports(&old, &new)
                    .into_iter()
                    .find(|report| report.event == event)
                    .unwrap()
            }
            "voice_join" => voice_report(&new, None, Some(321)).unwrap(),
            "voice_leave" => voice_report(&new, Some(321), None).unwrap(),
            "voice_move" => voice_report(&new, Some(321), Some(654)).unwrap(),
            "role_change" => {
                role_change_report(RoleAction::Created, None, Some(&role("Mods", 1))).unwrap()
            }
            "thread_create" => thread_created(&thread("topic title", false)).unwrap(),
            "thread_update" => thread_updated(None, &thread("topic title", false)).unwrap(),
            "thread_delete" => thread_deleted(&thread("topic title", false)).unwrap(),
            other => panic!("unexpected event {other}"),
        }
    }

    #[test]
    fn each_activity_template_formats_inside_the_discord_limit() {
        for event in ACTIVITY_LOG_EVENTS {
            let report = sample(event);
            let template = crate::defaults::activity_log_templates()
                .into_iter()
                .find(|(name, _)| *name == event)
                .unwrap()
                .1;
            let rendered = templates::render_bounded(template, &report.vars, 2000);
            assert!(rendered.encode_utf16().count() <= 2000, "{event}");
            assert!(!rendered.contains("{user}"), "{event}: {rendered}");
            assert!(!rendered.contains("message body"), "{event}");
            assert!(
                !report.vars.contains_key("content"),
                "{event} must not carry message text"
            );
        }
    }

    struct Fixture {
        app: Arc<BotState>,
        path: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "jmodguard_activity_{:032x}.db",
                rand::random::<u128>()
            ));
            let db = Db::open(&path, 2).unwrap();
            let mut cfg = BotProcessConfig::default();
            cfg.log.discord_concurrency = 1;
            cfg.log.discord_timeout_secs = 1;
            Self {
                app: BotState::new(cfg, db).unwrap(),
                path,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
            crate::tenant::remove_sidecars(&self.path);
        }
    }

    #[derive(Default)]
    struct Sink {
        messages: Mutex<Vec<(ChannelId, serde_json::Value)>>,
    }

    #[async_trait]
    impl LogDelivery for Sink {
        async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
            self.messages
                .lock()
                .unwrap()
                .push((channel, serde_json::to_value(message)?));
            Ok(())
        }
    }

    async fn set_filter(app: &BotState, event: &str, data: Option<&str>) {
        let event = event.to_string();
        let data = data.map(str::to_string);
        app.db
            .with_conn(move |conn| {
                conn.execute_bounded(
                    "DELETE FROM log_filters WHERE guild_id=789 AND event_type=?1",
                    db::params![event.clone()],
                    65_536,
                )?;
                if let Some(data) = data {
                    conn.execute_bounded(
                        "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,?1,?2)",
                        db::params![event, data],
                        65_536,
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        app.invalidate_configs();
    }

    #[tokio::test]
    async fn each_activity_event_follows_channel_and_ignore_toggles() {
        let f = Fixture::new();
        let sink = Sink::default();
        let guild = GuildId::new(789);
        for event in ACTIVITY_LOG_EVENTS {
            let report = sample(event);
            db::delete_log_channel(&f.app.db, 789, event).await.unwrap();
            set_filter(&f.app, event, None).await;
            assert_eq!(
                emit_to(&f.app, &sink, guild, report.event, report.vars.clone()).await,
                Delivery::Unconfigured,
                "{event} is opt-in"
            );
            assert!(sink.messages.lock().unwrap().is_empty(), "{event}");

            db::upsert_log_channel(&f.app.db, 789, event, 456)
                .await
                .unwrap();
            f.app.invalidate_configs();
            assert_eq!(
                emit_to(&f.app, &sink, guild, report.event, report.vars.clone()).await,
                Delivery::Sent,
                "{event}"
            );
            {
                let messages = sink.messages.lock().unwrap();
                assert_eq!(messages.len(), 1, "{event}");
                assert_eq!(messages[0].0, ChannelId::new(456));
                let text = messages[0].1["content"].as_str().unwrap();
                assert!(text.encode_utf16().count() <= 2000, "{event}");
                assert!(!text.contains("message body"), "{event}");
            }
            sink.messages.lock().unwrap().clear();

            if report.vars.contains_key("user_id") {
                set_filter(&f.app, event, Some(r#"{"users":["42"],"channels":[]}"#)).await;
                assert_eq!(
                    emit_to(&f.app, &sink, guild, report.event, report.vars.clone()).await,
                    Delivery::Unconfigured,
                    "{event} user ignore"
                );
                assert!(sink.messages.lock().unwrap().is_empty(), "{event}");
            }
            if let Some(source) = report.vars.get("source_channel_id").cloned() {
                let data = format!(r#"{{"users":[],"channels":["{source}"]}}"#);
                set_filter(&f.app, event, Some(&data)).await;
                assert_eq!(
                    emit_to(&f.app, &sink, guild, report.event, report.vars.clone()).await,
                    Delivery::Unconfigured,
                    "{event} channel ignore"
                );
            }
            if event == "voice_move" {
                set_filter(&f.app, event, Some(r#"{"users":[],"channels":["321"]}"#)).await;
                assert_eq!(
                    emit_to(&f.app, &sink, guild, report.event, report.vars.clone()).await,
                    Delivery::Unconfigured,
                    "voice move ignore matches the channel left"
                );
            }
            set_filter(&f.app, event, None).await;
            db::delete_log_channel(&f.app.db, 789, event).await.unwrap();
            f.app.invalidate_configs();
            sink.messages.lock().unwrap().clear();
        }
    }
}
