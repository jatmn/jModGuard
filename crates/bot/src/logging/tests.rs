use super::*;

// Inspect the real builder payload while retaining the existing evidence assertions.
pub(crate) fn log_text(message: &serde_json::Value) -> String {
    assert!(
        message.get("content").is_none(),
        "log content must be an embed"
    );
    assert!(
        message.get("flags").is_none(),
        "the log embed must remain visible"
    );
    assert_eq!(message["embeds"].as_array().unwrap().len(), 1);
    let embed = &message["embeds"][0];
    assert!(embed["color"].as_u64().is_some());
    let fields = embed["fields"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let title = embed["title"].as_str().unwrap();
    // Shipped layouts carry one map emoji per title and label; custom text stays plain.
    let mut text = if fields.is_empty() {
        assert_eq!(map_emoji_prefix(title), None, "custom title: {title}");
        title.to_string()
    } else {
        let emoji = map_emoji_prefix(title).expect(title);
        assert!(
            jmodguard::emoji::EVENTS
                .iter()
                .chain(jmodguard::emoji::ACTIONS)
                .any(|(_, known)| *known == emoji),
            "{title}"
        );
        title[emoji.len() + 1..].to_string()
    };
    if let Some(description) = embed["description"].as_str() {
        text.push_str("\n\n");
        text.push_str(description);
    }
    for (index, field) in fields.iter().enumerate() {
        let name = field["name"].as_str().unwrap();
        let emoji = map_emoji_prefix(name).expect(name);
        let label = &name[emoji.len() + 1..];
        assert_eq!(jmodguard::emoji::label(label), Some(emoji), "{name}");
        text.push_str(if index == 0 { "\n\n" } else { "\n" });
        text.push_str(&format!(
            "**{}:** {}",
            label,
            field["value"].as_str().unwrap()
        ));
    }
    text
}

/// The map emoji (followed by one space) that starts `text`, if any.
pub(crate) fn map_emoji_prefix(text: &str) -> Option<&'static str> {
    jmodguard::emoji::all().find(|emoji| {
        text.strip_prefix(emoji)
            .is_some_and(|rest| rest.starts_with(' '))
    })
}

use crate::config::BotProcessConfig;
use crate::db::Db;
use crate::db::SqlExecutor;
use crate::state::BotState;
use std::sync::Mutex;

#[test]
fn bulk_transcript_reports_omissions_within_attachment_limit() {
    let row = MessageSnapshot {
        guild_id: 789,
        channel_id: 456,
        message_id: 123,
        fields: crate::db::guild_data::SnapshotFields {
            content: Some("message text ".repeat(300)),
            ..Default::default()
        },
        created_at_ms: 1,
        first_observed_at_ms: 1,
        last_observed_at_ms: 2,
        edit_at_ms: None,
        expires_at_ms: 86_400_001,
        generation: 1,
        coverage_epoch: 1,
        continuous: false,
        content_supplied: false,
        attachments_supplied: false,
    };
    let mut transcript = BulkTranscript::new(1000);
    for id in 1..=1000 {
        transcript.push(MessageId::new(id), Some(&row));
    }
    assert_eq!(transcript.retained, 1000);
    assert!(transcript.omitted > 0);
    let bytes = transcript.finish();
    assert!(bytes.len() <= 256 * 1024);
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("entries omitted")
    );
}

#[test]
fn bulk_transcript_counts_unique_missing_unknown_and_filtered_entries() {
    let ids = crate::snapshot_capture::unique_ids(&[
        MessageId::new(123),
        MessageId::new(123),
        MessageId::new(456),
        MessageId::new(789),
    ]);
    let mut transcript = BulkTranscript::new(ids.len());
    transcript.push(ids[0], None);
    transcript.push_filtered();
    transcript.push_unknown(ids[2]);
    assert_eq!(transcript.total, 3);
    assert_eq!(transcript.unavailable, 1);
    assert_eq!(transcript.unknown, 1);
    assert_eq!(transcript.filtered, 1);
    assert_eq!(transcript.retained, 0);
    let output = String::from_utf8(transcript.finish()).unwrap();
    assert_eq!(
        output
            .matches("Message 123 | retained content unavailable")
            .count(),
        1
    );
    assert!(output.contains("Message 789 | snapshot storage unavailable"));
}

#[test]
fn plain_bulk_fallback_accounts_for_complete_entries_and_does_not_claim_attachment() {
    let mut transcript = BulkTranscript::new(9);
    for id in 1..=9 {
        transcript.push(MessageId::new(id), None);
    }
    let display = BTreeMap::from([("details".into(), "transcript attached".into())]);
    let parts = fallback_bulk_parts("{details}", &display, &transcript);
    assert!(parts.len() <= 3);
    assert!(parts.iter().all(|part| part.encode_utf16().count() <= 2000));
    assert!(parts[0].contains("Discord rejected the transcript attachment (HTTP 403)"));
    assert!(!parts[0].contains("transcript attached"));
    let shown: usize = parts
        .iter()
        .map(|part| part.matches(" | retained content unavailable").count())
        .sum();
    assert_eq!(shown, 9);
    assert!(parts[0].contains("entries omitted: 0"));

    let mut long = BulkTranscript::new(200);
    for id in 1..=200 {
        long.push(MessageId::new(id), None);
    }
    let bounded = fallback_bulk_parts("{details}", &display, &long);
    assert_eq!(bounded.len(), 3, "large transcripts stop after three parts");
    let shown: usize = bounded
        .iter()
        .map(|part| part.matches(" | retained content unavailable").count())
        .sum();
    assert!(shown < 200);
    assert!(bounded[0].contains(&format!("entries omitted: {}", 200 - shown)));
    assert!(
        bounded
            .iter()
            .all(|part| part.encode_utf16().count() <= 2000)
    );
}

#[test]
fn bulk_fallback_copy_names_the_attachment_rejection_trigger() {
    let mut transcript = BulkTranscript::new(1);
    transcript.push(MessageId::new(123), None);
    let parts = fallback_bulk_parts("{details}", &BTreeMap::new(), &transcript);
    assert!(parts[0].contains("Discord rejected the transcript attachment (HTTP 403)"));
    assert!(parts[0].contains("bounded text fallback"));
    assert!(!parts[0].contains("plain-message fallback"));
}

#[test]
fn retained_edit_and_delete_reports_distinguish_empty_missing_and_gap() {
    let mut before = MessageSnapshot {
        guild_id: 789,
        channel_id: 456,
        message_id: 123,
        fields: crate::db::guild_data::SnapshotFields {
            content: Some("A".into()),
            author_id: Some(42),
            ..Default::default()
        },
        created_at_ms: 1,
        first_observed_at_ms: 1,
        last_observed_at_ms: 2,
        edit_at_ms: None,
        expires_at_ms: 86_400_001,
        generation: 1,
        coverage_epoch: 1,
        continuous: false,
        content_supplied: false,
        attachments_supplied: false,
    };
    let mut after = before.clone();
    after.fields.content = Some("B".into());
    let mut edit = edit_report(&edited(serde_json::json!({"content":"B"})), Some(42)).unwrap();
    apply_retained_edit(&mut edit, Some(&before), Some(&after));
    assert_eq!(edit.vars["before"], "A");
    assert_eq!(edit.vars["after"], "B");
    assert!(edit.vars["coverage"].contains("gap"));

    after.fields.content = Some(String::new());
    let mut deletion = MessageReport {
        retained_expiries: Default::default(),
        guild_id: GuildId::new(789),
        event: "message_delete",
        vars: message_vars(GuildId::new(789), ChannelId::new(456), MessageId::new(123)),
    };
    apply_retained_delete(&mut deletion, Some(&after));
    assert_eq!(deletion.vars["content"], "empty");
    before.fields.content = None;
    apply_retained_delete(&mut deletion, Some(&before));
    assert_eq!(deletion.vars["content"], "not supplied to capture");
}

#[test]
fn refused_edit_uses_retained_author_for_existing_user_filter() {
    let prior = MessageSnapshot {
        guild_id: 789,
        channel_id: 456,
        message_id: 123,
        fields: crate::db::guild_data::SnapshotFields {
            content: Some("A".into()),
            author_id: Some(42),
            ..Default::default()
        },
        created_at_ms: 1,
        first_observed_at_ms: 1,
        last_observed_at_ms: 2,
        edit_at_ms: None,
        expires_at_ms: 86_400_001,
        generation: 1,
        coverage_epoch: 1,
        continuous: false,
        content_supplied: false,
        attachments_supplied: false,
    };
    let mut report = MessageReport {
        retained_expiries: Default::default(),
        guild_id: GuildId::new(789),
        event: "message_edit",
        vars: message_vars(GuildId::new(789), ChannelId::new(456), MessageId::new(123)),
    };
    report.vars.insert("after".into(), "B".into());
    apply_retained_edit(&mut report, Some(&prior), None);
    assert_eq!(report.vars["after"], "B");
    assert_eq!(report.vars["user_id"], "42");
    assert_eq!(report.vars["mention"], "<@42>");
    let filter = db::LogFilter {
        users: vec!["42".into()],
        channels: Vec::new(),
    };
    assert!(excluded(&filter, &report.vars));
}

#[test]
fn shipped_edit_template_keeps_both_long_unicode_sides_and_coverage() {
    let before = MessageSnapshot {
        guild_id: 789,
        channel_id: 456,
        message_id: 123,
        fields: crate::db::guild_data::SnapshotFields {
            content: Some("🦀".repeat(700)),
            content_truncated: true,
            author_id: Some(42),
            ..Default::default()
        },
        created_at_ms: 1,
        first_observed_at_ms: 1,
        last_observed_at_ms: 2,
        edit_at_ms: None,
        expires_at_ms: 86_400_001,
        generation: 1,
        coverage_epoch: 1,
        continuous: false,
        content_supplied: false,
        attachments_supplied: false,
    };
    let mut after = before.clone();
    after.fields.content = Some("🌕".repeat(700));
    after.fields.attachments = Some(vec![crate::db::guild_data::SnapshotAttachment {
        description: None,
        id: 1,
        filename: "f".repeat(256),
        size: 5,
        content_type: Some("image/jpeg".into()),
        url: "https://cdn.discordapp.com/attachments/".to_string() + &"x".repeat(1000),
    }]);
    after.fields.attachments_truncated = true;
    after.fields.attachments_omitted = Some(0);
    let mut report = edit_report(&edited(serde_json::json!({"content":"new"})), Some(42)).unwrap();
    apply_retained_edit(&mut report, Some(&before), Some(&after));
    let template = default_template("message_edit");
    bound_default_message_vars("message_edit", &mut report.vars);
    let rendered = templates::render_bounded(template, &report.vars, 2000);
    assert!(rendered.contains("**Before (last observed):**"));
    assert!(rendered.contains("**After:**"));
    assert!(rendered.contains("**Coverage:** last observed; observation gap"));
    assert!(rendered.contains("display clipped; retained text truncated"));
    assert!(rendered.contains("**Attachments:**"));
    assert!(rendered.contains("attachment metadata truncated during capture"));
    assert!(!rendered.contains("0 attachment(s) omitted"));
    assert!(rendered.encode_utf16().count() <= 2000);
}

fn render_shipped_delete(event: &'static str, row: &MessageSnapshot) -> String {
    let mut report = MessageReport {
        retained_expiries: Default::default(),
        guild_id: GuildId::new(789),
        event,
        vars: message_vars(GuildId::new(789), ChannelId::new(456), MessageId::new(123)),
    };
    if event == "automod_delete" {
        report.vars.insert("rule".into(), "Bad words".into());
        report.vars.insert("rule_id".into(), "badword".into());
    }
    report
        .vars
        .insert("time".into(), "2026-10-27T00:00:00Z".into());
    apply_retained_delete(&mut report, Some(row));
    let template = default_template(event);
    bound_default_message_vars(event, &mut report.vars);
    let rendered = templates::render_bounded(template, &report.vars, 2000);
    assert!(
        rendered.encode_utf16().count() <= 2000,
        "{event}: {rendered}"
    );
    assert!(
        rendered.contains("**Last observed text:** old retained text"),
        "{rendered}"
    );
    assert!(!rendered.contains("{coverage}"), "{rendered}");
    rendered
}

#[test]
fn shipped_automod_delete_template_renders_gap_and_partial_coverage() {
    let mut row = MessageSnapshot {
        guild_id: 789,
        channel_id: 456,
        message_id: 123,
        fields: crate::db::guild_data::SnapshotFields {
            content: Some("old retained text".into()),
            author_id: Some(42),
            ..Default::default()
        },
        created_at_ms: 1,
        first_observed_at_ms: 1,
        last_observed_at_ms: 2,
        edit_at_ms: None,
        expires_at_ms: 86_400_001,
        generation: 1,
        coverage_epoch: 1,
        continuous: false,
        content_supplied: true,
        attachments_supplied: false,
    };
    let automod_gap = render_shipped_delete("automod_delete", &row);
    let plain_gap = render_shipped_delete("message_delete", &row);
    let gap = "**Coverage:** last observed; observation gap or restart";
    assert!(automod_gap.contains(gap), "{automod_gap}");
    assert!(plain_gap.contains(gap), "{plain_gap}");
    assert!(!automod_gap.contains("**Text:**"), "{automod_gap}");

    row.continuous = true;
    let automod_partial = render_shipped_delete("automod_delete", &row);
    let plain_partial = render_shipped_delete("message_delete", &row);
    let partial = "**Coverage:** last observed; later edits may be missing";
    assert!(automod_partial.contains(partial), "{automod_partial}");
    assert!(plain_partial.contains(partial), "{plain_partial}");
}

#[tokio::test]
async fn committed_snapshot_transition_reaches_edit_and_delete_delivery() {
    use crate::db::guild_data::{SnapshotFields, SnapshotObservation, SnapshotOutcome};
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, 789, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = f.app.tenant.live(enrollment).await.unwrap();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 456)
        .await
        .unwrap();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let observation = SnapshotObservation {
        channel_id: 456,
        message_id: 123,
        created_at_ms: now - 1000,
        observed_at_ms: now,
        edit_at_ms: None,
        coverage_epoch: 7,
        fields: SnapshotFields {
            content: Some("before text".into()),
            attachments: Some(Vec::new()),
            author_id: Some(42),
            author_name: Some("human".into()),
            ..Default::default()
        },
    };
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation.clone())
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    let mut edit_observation = observation;
    edit_observation.observed_at_ms = now + 1;
    edit_observation.edit_at_ms = Some(now + 1);
    edit_observation.fields.content = Some("after text".into());
    edit_observation.fields.author_id = None;
    edit_observation.fields.author_name = None;
    let transition = store
        .apply_snapshot_edit_existing(scope.clone(), edit_observation.clone())
        .await
        .unwrap();
    assert_eq!(transition.outcome, SnapshotOutcome::Stored);
    let mut edit = edit_report(
        &edited(serde_json::json!({"content":"after text"})),
        Some(42),
    )
    .unwrap();
    apply_retained_edit(
        &mut edit,
        transition.before.as_ref(),
        transition.after.as_ref(),
    );
    let sink = Sink::default();
    assert_eq!(
        emit_to(&f.app, &sink, edit.guild_id, edit.event, edit.vars).await,
        Delivery::Sent
    );
    edit_observation.observed_at_ms = now + 2;
    edit_observation.edit_at_ms = Some(now + 2);
    edit_observation.fields.content = Some("final text".into());
    let next = store
        .apply_snapshot_edit_existing(scope.clone(), edit_observation)
        .await
        .unwrap();
    assert_eq!(next.outcome, SnapshotOutcome::Stored);
    assert_eq!(
        next.before.as_ref().unwrap().fields.content.as_deref(),
        Some("after text")
    );
    assert_eq!(
        next.after.as_ref().unwrap().fields.content.as_deref(),
        Some("final text")
    );
    let mut second_edit = edit_report(
        &edited_at(
            serde_json::json!({"content":"final text"}),
            "2026-09-21T00:00:02.000Z",
        ),
        Some(42),
    )
    .unwrap();
    apply_retained_edit(&mut second_edit, next.before.as_ref(), next.after.as_ref());
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            second_edit.guild_id,
            second_edit.event,
            second_edit.vars
        )
        .await,
        Delivery::Sent
    );
    // Ordinary handler callbacks may finish in reverse order. They update only
    // bounded identity hints; the retained transition has already committed.
    remember_message_update(
        &f.app,
        &edited_at(
            serde_json::json!({"content":"final text"}),
            "2026-09-21T00:00:02.000Z",
        ),
        Some(42),
    );
    remember_message_update(
        &f.app,
        &edited_at(
            serde_json::json!({"content":"after text"}),
            "2026-09-21T00:00:01.000Z",
        ),
        Some(42),
    );
    let deleted = store
        .take_snapshot_at_epoch(scope, 456, 123, now + 3, 7)
        .await
        .unwrap()
        .unwrap();
    let mut deletion = MessageReport {
        retained_expiries: Default::default(),
        guild_id: GuildId::new(789),
        event: "message_delete",
        vars: message_vars(GuildId::new(789), ChannelId::new(456), MessageId::new(123)),
    };
    apply_retained_delete(&mut deletion, Some(&deleted));
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            deletion.guild_id,
            deletion.event,
            deletion.vars
        )
        .await,
        Delivery::Sent
    );
    let messages = sink.messages.lock().unwrap();
    let edit_text = log_text(&messages[0].1);
    assert!(edit_text.contains("before text") && edit_text.contains("after text"));
    let second_text = log_text(&messages[1].1);
    assert!(second_text.contains("after text") && second_text.contains("final text"));
    let delete_text = log_text(&messages[2].1);
    assert!(delete_text.contains("final text") && delete_text.contains("<@42>"));
}

const EDITED: &str = "2026-09-21T00:00:01.000Z";

fn update(fields: serde_json::Value) -> MessageUpdateEvent {
    let mut value = serde_json::json!({"id":"123", "channel_id":"456", "guild_id":"789"});
    value
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    serde_json::from_value(value).unwrap()
}

fn edited(fields: serde_json::Value) -> MessageUpdateEvent {
    edited_at(fields, EDITED)
}

fn edited_at(mut fields: serde_json::Value, timestamp: &str) -> MessageUpdateEvent {
    fields.as_object_mut().unwrap().insert(
        "edited_timestamp".into(),
        serde_json::Value::String(timestamp.to_string()),
    );
    update(fields)
}

fn dedup_report(dedup: &EditReportDedup, event: &MessageUpdateEvent) -> bool {
    let Some((guild, at)) = event.guild_id.zip(event.edited_timestamp) else {
        return false;
    };
    dedup
        .window((guild, event.channel_id, event.id))
        .is_some_and(|window| window.claim(at.timestamp_millis()))
}

fn edit_report(event: &MessageUpdateEvent, known_author_id: Option<u64>) -> Option<MessageReport> {
    message_update_report(event, known_author_id, None, None)
}

#[test]
fn basic_updates_use_only_supplied_fields_and_never_invent_history() {
    let report = edit_report(&edited(serde_json::json!({"content":"new text"})), None).unwrap();
    assert_eq!(report.vars["after"], "new text");
    assert_eq!(report.vars["before"], "unavailable (not captured)");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(
        report.vars["message_url"],
        "https://discord.com/channels/789/456/123"
    );
    let attachment_only =
        edit_report(&edited(serde_json::json!({"attachments":[]})), None).unwrap();
    assert_eq!(attachment_only.vars["attachments"], "0");
    assert_eq!(attachment_only.vars["after"], "not supplied in this update");
    let emptied = edit_report(&edited(serde_json::json!({"content":""})), None).unwrap();
    assert_eq!(emptied.vars["after"], "");
    assert!(
        edit_report(
            &update(serde_json::json!({"embeds":[],"pinned":true})),
            None
        )
        .is_none()
    );
    let mut dm = edited(serde_json::json!({"content":"private"}));
    dm.guild_id = None;
    assert!(edit_report(&dm, None).is_none());
}

#[test]
fn untimestamped_unfurls_and_empty_attachments_are_not_edits() {
    let gif = update(serde_json::json!({
        "content":"https://tenor.com/view/wtf-is-this-thomasvanhaecht-vubdiscord-walking-in-on-the-wrong-thing-walking-in-gif-21572821",
        "attachments":[],
        "embeds":[{"url":"https://tenor.com/view/example.gif"}]
    }));
    assert!(edit_report(&gif, None).is_none());
    assert!(edit_report(&update(serde_json::json!({"content":"new text"})), None).is_none());
    assert!(edit_report(&update(serde_json::json!({"attachments":[]})), None).is_none());
    let dedup = EditReportDedup::default();
    assert!(!dedup_report(&dedup, &gif));
    assert!(!dedup_report(
        &dedup,
        &update(serde_json::json!({"content":"new text"}))
    ));
    let real = edited(serde_json::json!({"content":"actually edited"}));
    assert_eq!(
        edit_report(&real, None).unwrap().vars["after"],
        "actually edited"
    );
    assert!(dedup_report(&dedup, &real));
}

#[test]
fn untimestamped_reply_refreshes_are_not_edits() {
    let reply = update(serde_json::json!({
        "content":"replying to that gif",
        "attachments":[],
        "type": 19,
        "timestamp":"2026-09-22T14:52:00.000Z",
        "message_reference": {
            "message_id":"111",
            "channel_id":"456",
            "guild_id":"789"
        }
    }));
    assert!(edit_report(&reply, None).is_none());
    let dedup = EditReportDedup::default();
    assert!(!dedup_report(&dedup, &reply));
    let edited_reply = edited(serde_json::json!({
        "content":"edited the reply",
        "type": 19,
        "message_reference": {
            "message_id":"111",
            "channel_id":"456",
            "guild_id":"789"
        }
    }));
    assert_eq!(
        edit_report(&edited_reply, None).unwrap().vars["after"],
        "edited the reply"
    );
    assert!(dedup_report(&dedup, &edited_reply));
}

#[test]
fn bot_own_log_updates_do_not_stack_prior_reports() {
    let nested =
        "**:pencil2: Message updated in <#456>**\nCurrent text: https://tenor.com/view/example";
    let mut log_update = edited(serde_json::json!({"content": nested}));
    let mut author = serenity::model::user::User::default();
    author.id = serenity::model::id::UserId::new(99);
    author.name = "jModGuard".into();
    log_update.author = Some(author);
    assert!(message_update_report(&log_update, None, Some(99), None).is_none());
    let reported = message_update_report(&log_update, None, Some(1), None).unwrap();
    assert_eq!(reported.vars["after"], nested);
    let mut anonymous = edited(serde_json::json!({"content": nested}));
    anonymous.author = None;
    assert!(message_update_report(&anonymous, Some(99), Some(99), None).is_none());
    assert_eq!(
        message_update_report(&anonymous, Some(99), Some(1), None)
            .unwrap()
            .vars["user_id"],
        "99"
    );
}

#[test]
fn repeated_edit_observations_emit_once_but_changed_content_emits_again() {
    let dedup = EditReportDedup::default();
    let first = edited(serde_json::json!({"content":"first"}));
    assert!(dedup_report(&dedup, &first));
    assert!(!dedup_report(&dedup, &first));
    assert!(!dedup_report(&dedup, &first));
    let changed = edited_at(
        serde_json::json!({"content":"second"}),
        "2026-09-21T00:00:02.000Z",
    );
    assert!(dedup_report(&dedup, &changed));
    assert!(!dedup_report(&dedup, &changed));
    assert!(
        !dedup_report(&dedup, &first),
        "a replay of an earlier timestamp is still the same reported edit"
    );
    let mut other_channel = first.clone();
    other_channel.channel_id = ChannelId::new(999);
    assert!(dedup_report(&dedup, &other_channel));
}

#[tokio::test(start_paused = true)]
async fn edit_window_capacity_preserves_pending_claims_and_reuses_idle_slots() {
    let dedup = EditReportDedup::default();
    let key = |id| (GuildId::new(7), ChannelId::new(8), MessageId::new(id));
    let mut pending: Vec<_> = (1..=EDIT_REPORT_LIMIT as u64)
        .map(|id| dedup.window(key(id)).unwrap())
        .collect();
    assert!(pending[0].claim(42));
    let new_key = key(EDIT_REPORT_LIMIT as u64 + 1);
    assert!(dedup.window(new_key).is_none());
    pending.pop();
    assert!(dedup.window(new_key).unwrap().claim(42));
    assert!(!dedup.window(key(1)).unwrap().claim(42));

    drop(pending);
    tokio::time::advance(EDIT_REPORT_KEEP).await;
    assert!(dedup.window(key(1)).unwrap().claim(42));
}

#[test]
fn partial_fields_for_one_edit_are_one_observation() {
    let dedup = EditReportDedup::default();
    let content = edited(serde_json::json!({"content":"edited"}));
    let attachments = edited(serde_json::json!({"attachments":[]}));
    let combined = edited(serde_json::json!({"content":"edited","attachments":[]}));
    assert!(dedup_report(&dedup, &content));
    assert!(!dedup_report(&dedup, &attachments));
    assert!(!dedup_report(&dedup, &combined));
    assert!(dedup_report(
        &dedup,
        &edited_at(
            serde_json::json!({"content":"edited again"}),
            "2026-09-21T00:00:02.000Z"
        )
    ));

    let reverse = EditReportDedup::default();
    assert!(dedup_report(&reverse, &attachments));
    assert!(!dedup_report(&reverse, &content));
    assert!(!dedup_report(&reverse, &combined));
    assert!(dedup_report(
        &reverse,
        &edited_at(
            serde_json::json!({"content":"edited again"}),
            "2026-09-21T00:00:02.000Z"
        )
    ));
}

#[test]
fn distinct_edit_timestamps_and_attachment_descriptions_are_reported() {
    let dedup = EditReportDedup::default();
    let content = update(serde_json::json!({
        "content":"edited",
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }));
    let same_edit = update(serde_json::json!({
        "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }));
    let next_edit = update(serde_json::json!({
        "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }));
    assert!(dedup_report(&dedup, &content));
    assert!(!dedup_report(&dedup, &same_edit));
    assert!(!dedup_report(
        &dedup,
        &update(serde_json::json!({
            "content":"edited field supplied again",
            "edited_timestamp":"2026-09-21T00:00:01.000Z"
        }))
    ));
    assert!(dedup_report(&dedup, &next_edit));
    assert!(!dedup_report(&dedup, &next_edit));
    assert!(
        !dedup_report(&dedup, &content),
        "a replay of T1 after T2 is still the same reported edit"
    );
    assert!(dedup_report(
        &dedup,
        &update(serde_json::json!({
            "content":"third edit",
            "edited_timestamp":"2026-09-21T00:00:03.000Z"
        }))
    ));

    let two_fields = EditReportDedup::default();
    assert!(dedup_report(
        &two_fields,
        &update(serde_json::json!({
            "content":"before", "attachments":[],
            "edited_timestamp":"2026-09-21T00:00:01.000Z"
        }))
    ));
    assert!(dedup_report(
        &two_fields,
        &update(serde_json::json!({
            "content":"after",
            "edited_timestamp":"2026-09-21T00:00:02.000Z"
        }))
    ));
    assert!(!dedup_report(
        &two_fields,
        &update(serde_json::json!({
            "attachments":[{"id":"555", "filename":"image.png", "description":"new",
                "proxy_url":"https://cdn.example/proxy", "url":"https://cdn.example/image",
                "size":100}],
            "edited_timestamp":"2026-09-21T00:00:02.000Z"
        }))
    ));

    let attachment = |description: &str, timestamp: &str| {
        edited_at(
            serde_json::json!({"attachments":[{
                "id":"555", "filename":"image.png", "description":description,
                "proxy_url":"https://cdn.example/proxy", "url":"https://cdn.example/image",
                "size":100
            }]}),
            timestamp,
        )
    };
    let metadata = EditReportDedup::default();
    assert!(dedup_report(&metadata, &attachment("before", EDITED)));
    assert!(!dedup_report(&metadata, &attachment("before", EDITED)));
    assert!(dedup_report(
        &metadata,
        &attachment("after", "2026-09-21T00:00:02.000Z")
    ));
}

#[test]
fn untimestamped_refresh_does_not_hide_a_later_timestamped_edit() {
    let dedup = EditReportDedup::default();
    assert!(!dedup_report(
        &dedup,
        &update(serde_json::json!({"content":"after"}))
    ));
    assert!(dedup_report(
        &dedup,
        &edited_at(
            serde_json::json!({"content":"after"}),
            "2026-09-21T00:00:02.000Z"
        )
    ));
    assert!(!dedup_report(
        &dedup,
        &edited_at(
            serde_json::json!({"attachments":[]}),
            "2026-09-21T00:00:02.000Z"
        )
    ));
    assert!(dedup_report(
        &dedup,
        &edited_at(
            serde_json::json!({"content":"third"}),
            "2026-09-21T00:00:03.000Z"
        )
    ));
}

#[test]
fn supplied_author_id_reaches_message_edit_filter() {
    let mut event = edited(serde_json::json!({"content":"edited"}));
    let mut author = serenity::model::user::User::default();
    author.id = serenity::model::id::UserId::new(42);
    author.name = "otherbot".into();
    event.author = Some(author);
    let report = edit_report(&event, None).unwrap();
    assert_eq!(report.vars["user_id"], "42");
    assert!(excluded(
        &db::LogFilter {
            users: vec!["42".into()],
            channels: vec![]
        },
        &report.vars
    ));
    assert!(!excluded(
        &db::LogFilter {
            users: vec!["otherbot".into()],
            channels: vec![]
        },
        &report.vars
    ));
}

#[test]
fn authorless_edit_uses_an_observed_bot_id_for_exclusion_only() {
    let f = Fixture::new();
    remember_message(
        &f.app,
        789,
        456,
        123,
        Some(42),
        Some("the other bot's report"),
        None,
    );
    let known = known_message_author(&f.app, 789, 456, 123);
    assert_eq!(known, Some(42));
    let event = edited(serde_json::json!({"content":"edited report"}));
    let report = edit_report(&event, known).unwrap();
    assert_eq!(report.vars["user_id"], "42");
    assert_eq!(report.vars["user"], "unavailable");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(report.vars["after"], "edited report");
    assert!(excluded(
        &db::LogFilter {
            users: vec!["42".into()],
            channels: vec![],
        },
        &report.vars,
    ));
    assert_eq!(known_message_author(&f.app, 789, 456, 999), None);
    let cold = edit_report(&event, None).unwrap();
    assert_eq!(cold.vars["user_id"], "unavailable");
}

#[test]
fn log_filter_matches_known_ids_and_source_channels_only() {
    let filter = db::LogFilter {
        users: vec!["123".into(), "999".into()],
        channels: vec!["456".into()],
    };
    assert!(excluded(
        &filter,
        &BTreeMap::from([("user_id".into(), "123".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([("actor_id".into(), "999".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([("target_id".into(), "123".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([("channel_id".into(), "456".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([
            ("source_channel_id".into(), "456".into()),
            ("channel".into(), "<#789>".into()),
        ])
    ));
    assert!(!excluded(
        &filter,
        &BTreeMap::from([
            ("source_channel_id".into(), "789".into()),
            ("channel".into(), "<#456>".into()),
        ])
    ));
    assert!(!excluded(
        &filter,
        &BTreeMap::from([
            ("user_id".into(), "unavailable".into()),
            ("username".into(), "123".into()),
            ("user".into(), "999".into()),
        ])
    ));
    // Render labels are not user identities, even if another user's ID is ignored.
    assert!(!excluded(
        &db::LogFilter {
            users: vec!["555".into()],
            channels: vec![]
        },
        &BTreeMap::from([
            ("user_id".into(), "999".into()),
            ("actor_id".into(), "999".into()),
            ("user".into(), "messages".into()),
        ])
    ));
}

#[test]
fn basic_delete_reports_ids_and_unavailable_content() {
    let path = std::env::temp_dir().join(format!("log_del_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["message_id"], "123");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(report.vars["content"], "unavailable (not captured)");
    let rendered = templates::render(default_template(report.event), &report.vars);
    assert!(rendered.contains("123"));
    assert!(rendered.contains("unavailable (not captured)"));
    assert!(!rendered.contains("{mention}"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn observed_delete_reports_keep_only_recent_snippet_identity() {
    let path = std::env::temp_dir().join(format!("log_snip_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    remember_message(&app, 789, 456, 123, Some(1), Some("hello there"), None);
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["content"], "unavailable (not captured)");
    assert_eq!(report.vars["user_id"], "1");
    let missing = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(missing.vars["content"], "unavailable (not captured)");
    let _ = std::fs::remove_file(path);
}

#[test]
fn message_identity_snippets_expire_and_evict_only_within_their_guild() {
    let f = Fixture::new();
    let author = |guild, message| known_message_author(&f.app, guild, 456, message);
    remember_message(&f.app, 789, 456, 1, Some(42), None, None);
    remember_message_create(&f.app, 790, 456, 1, Some(43), true);
    {
        let mut snippets = f.app.log_snippets.inner.lock();
        let first = snippets.get_mut((789, 456, 1)).unwrap();
        first.at -= Duration::from_secs(1);
    }
    for id in 2..=64 {
        remember_message(&f.app, 789, 456, id, Some(42), None, None);
    }
    assert_eq!(author(789, 1), Some(42));
    remember_message(&f.app, 789, 456, 65, Some(42), None, None);
    assert_eq!(author(789, 1), None);
    for id in 2..=65 {
        assert_eq!(author(789, id), Some(42));
    }
    assert_eq!(author(790, 1), Some(43));

    let stored_at = f
        .app
        .log_snippets
        .inner
        .lock()
        .get((790, 456, 1))
        .unwrap()
        .at;
    assert_eq!(
        known_message_identity_at(&f.app, 790, 456, 1, || stored_at + Duration::from_secs(599)),
        Some((43, true))
    );
    assert_eq!(
        known_message_identity_at(&f.app, 790, 456, 1, || stored_at + Duration::from_secs(600)),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn snippet_expiry_order_stays_bounded_across_guilds() {
    let start = tokio::time::Instant::now().into_std();
    let mut cache = SnippetEntries::default();
    let refreshed = (1, 10, 1);
    let inserted = (1, 10, 65);
    for message in 1..=64 {
        for guild in 1..=100 {
            cache.remember(
                (guild, 10, message),
                Some(guild),
                (guild, 10, message) == refreshed,
                start + Duration::from_millis(message),
            );
        }
    }

    let refresh_time = start + Duration::from_millis(100);
    for _ in 0..1024 {
        cache.remember(refreshed, None, false, refresh_time);
    }
    let preserved = cache.get(refreshed).unwrap();
    assert_eq!(preserved.author, 1);
    assert!(preserved.origin == MessageOrigin::Webhook);
    assert_eq!(preserved.at, refresh_time);
    cache.remember(
        inserted,
        Some(99),
        false,
        refresh_time + Duration::from_millis(1),
    );
    assert!(cache.get((1, 10, 2)).is_none());
    assert!(cache.get((2, 10, 2)).is_some());
    for entries in cache.by_guild.values() {
        assert_eq!(entries.len(), SNIPPETS_PER_GUILD);
    }

    let mut previous = None;
    let mut current = cache.oldest;
    for _ in 0..6400 {
        let entry = cache.get(current.unwrap()).unwrap();
        assert_eq!(entry.older, previous);
        previous = current;
        current = entry.newer;
    }
    assert_eq!(previous, Some(inserted));
    assert_eq!(previous, cache.newest);
    assert!(current.is_none());

    cache.expire(start + SNIPPET_KEEP + Duration::from_millis(1));
    assert!(cache.get((2, 10, 1)).is_none());
    assert!(cache.get((2, 10, 2)).is_some());
    assert!(cache.get(refreshed).is_some());
    cache.expire(start + SNIPPET_KEEP + Duration::from_millis(64));
    assert_eq!(cache.by_guild.len(), 1);
    assert_eq!(cache.by_guild[&1].len(), 2);
    assert_eq!(cache.oldest, Some(refreshed));
    assert_eq!(cache.newest, Some(inserted));

    cache.remember(refreshed, Some(0), false, refresh_time + SNIPPET_KEEP);
    assert!(cache.get(refreshed).is_none());
    assert_eq!(cache.remove(inserted).unwrap().author, 99);
    assert!(cache.by_guild.is_empty());
    assert!(cache.oldest.is_none());
    assert!(cache.newest.is_none());
}

#[test]
fn snippet_preserves_author_for_filtering_without_exposing_unretained_payload() {
    let path =
        std::env::temp_dir().join(format!("log_identity_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    let urls = vec!["https://cdn.discordapp.com/attachments/456/1/secret.jpg".into()];
    remember_message(
        &app,
        789,
        456,
        123,
        Some(42),
        Some("not retained"),
        Some(&urls),
    );
    remember_message_update(
        &app,
        &edited(serde_json::json!({"content":"also not retained"})),
        Some(42),
    );
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["user_id"], "42");
    assert_eq!(report.vars["content"], "unavailable (not captured)");
    assert_eq!(report.vars["attachments"], "unavailable (not captured)");
    assert!(
        !report
            .vars
            .values()
            .any(|value| value.contains("secret.jpg"))
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn partial_update_without_author_preserves_create_snippet_author() {
    let path = std::env::temp_dir().join(format!("log_snip_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    remember_message(&app, 789, 456, 123, Some(42), Some("hello there"), None);
    remember_message(&app, 789, 456, 123, None, Some("edited text"), None);
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["content"], "unavailable (not captured)");
    assert_eq!(report.vars["user_id"], "42");
    let _ = std::fs::remove_file(path);
}

#[test]
fn unattributed_update_does_not_create_a_snippet() {
    let path = std::env::temp_dir().join(format!("log_snip_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    remember_message(&app, 789, 456, 123, None, Some("orphan edit"), None);
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["content"], "unavailable (not captured)");
    let _ = std::fs::remove_file(path);
}

#[test]
fn delivery_uses_visible_embed_suppresses_mentions_and_bounds_unicode() {
    let payload = serde_json::to_value(log_message(
        "system",
        "{content}",
        &BTreeMap::from([(
            "content".into(),
            format!("@everyone <@123> <@&456> {}", "🦀".repeat(3000)),
        )]),
    ))
    .unwrap();
    assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
    assert_eq!(payload["allowed_mentions"]["replied_user"], false);
    assert!(payload.get("flags").is_none());
    assert!(payload.get("content").is_none());
    assert_eq!(payload["embeds"][0]["title"], "System notice");
    assert_eq!(payload["embeds"][0]["color"], jmodguard::log_embeds::YELLOW);
    let text = payload["embeds"][0]["description"].as_str().unwrap();
    assert!(text.encode_utf16().count() <= 4096);
    assert!(text.starts_with("@everyone <@123> <@&456>"));
    assert!(text.ends_with('…'));
}

struct Fixture {
    app: Arc<BotState>,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::with_timeout(1)
    }
    fn with_timeout(timeout_secs: u64) -> Self {
        let path =
            std::env::temp_dir().join(format!("jmodguard_logs_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = 1;
        cfg.log.discord_timeout_secs = timeout_secs;
        let app = BotState::new(cfg, db).unwrap();
        app.activation.mark_lock_held(true);
        Self { app, path }
    }
}
#[tokio::test]
async fn cutoff_drops_configured_log_delivery() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 456)
        .await
        .unwrap();
    f.app.activation.cut_off();
    let sink = Sink::default();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_edit",
            BTreeMap::new()
        )
        .await,
        Delivery::Dropped
    );
    assert!(sink.messages.lock().unwrap().is_empty());
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

#[tokio::test]
async fn persisted_event_filter_blocks_matching_reports_only() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 456)
        .await
        .unwrap();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_bounded(
                "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'message_edit',?1)",
                db::params![r#"{"users":["123"],"channels":["999"]}"#],
                65_536,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let sink = Sink::default();
    let guild = GuildId::new(789);
    for vars in [
        BTreeMap::from([("user_id".into(), "123".into())]),
        BTreeMap::from([("channel_id".into(), "999".into())]),
    ] {
        assert_eq!(
            emit_to(&f.app, &sink, guild, "message_edit", vars).await,
            Delivery::Unconfigured
        );
    }
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            guild,
            "message_edit",
            BTreeMap::from([("user_id".into(), "124".into())])
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            guild,
            "message_delete",
            BTreeMap::from([("user_id".into(), "123".into())])
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 2);
}
#[serenity::async_trait]
impl LogDelivery for Sink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.messages
            .lock()
            .unwrap()
            .push((channel, serde_json::to_value(message)?));
        Ok(())
    }
}

#[tokio::test]
async fn member_rejoin_delivers_when_indexed_case_exceeds_read_budget() {
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseError, CaseEvidence, CaseId, CaseLimits, CaseOutcome, CaseSource,
    };
    const GUILD: i64 = 10;
    const SUBJECT: i64 = 91;
    assert_eq!(CaseLimits::default().max_record_bytes, 65_536);

    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = f.app.tenant.live(enrollment).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 100, 1_000)
        .await
        .unwrap();
    while !store
        .backfill_member_case_index(scope.clone(), 100, 100)
        .await
        .unwrap()
    {}
    let write_limits = CaseLimits {
        max_record_bytes: 1_048_576,
        ..CaseLimits::default()
    };
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            CaseData {
                ban_attempt_tag: None,
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Ban,
                outcome: CaseOutcome::Succeeded,
                subject_id: SUBJECT,
                actor_id: Some(7),
                occurred_at_ms: 3_000,
                reason: Some("z".repeat(262_144)),
                evidence: CaseEvidence::Unavailable,
            },
            write_limits,
        )
        .await
        .unwrap();
    let read_error = store
        .member_case_entries(scope, SUBJECT, 8, CaseLimits::default())
        .await
        .unwrap_err();
    assert!(
        read_error
            .downcast_ref::<CaseError>()
            .is_some_and(|error| matches!(error, CaseError::Limit)),
        "default read budget must still refuse the oversize case: {read_error:#}"
    );

    db::upsert_log_channel(&f.app.db, GUILD, "member_rejoin", 456)
        .await
        .unwrap();
    let mut user = serenity::model::user::User::default();
    user.id = serenity::model::id::UserId::new(SUBJECT as u64);
    user.name = "member".into();
    let vars = crate::templates::user_vars(user.id.get(), &user.name, &user.name);
    let sink = Sink::default();
    let (delivery, _) = super::emit_to_observed_with_audit(
        &f.app,
        &sink,
        GuildId::new(GUILD as u64),
        "member_rejoin",
        vars,
        None,
        Admission::Immediate,
        || true,
        false,
        Some(&user),
        None,
    )
    .await;
    assert_eq!(delivery, Delivery::Sent);
    let messages = sink.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    let content = log_text(&messages[0].1);
    assert!(
        content.contains("read budget"),
        "rejoin note omitted the budget notice: {content}"
    );
    assert!(
        !content.contains(&"z".repeat(32)),
        "rejoin note included the oversize reason"
    );
    assert!(content.contains("91"), "{content}");
}

#[tokio::test]
async fn graceful_shutdown_notice_uses_system_route_and_compact_uptime() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "system", 456)
        .await
        .unwrap();
    let sink = Sink::default();
    let content = shutdown_notice_content(3_661);
    assert_eq!(
        content,
        format!(
            "Bot shutting down (v{}). Uptime: 1h 1m 1s",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(
        emit_system_notice_to(
            &f.app,
            &sink,
            GuildId::new(789),
            content,
            Admission::Immediate
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(
        emit_system_notice_to(
            &f.app,
            &sink,
            GuildId::new(790),
            "not configured".into(),
            Admission::Immediate,
        )
        .await,
        Delivery::Unconfigured
    );
    let messages = sink.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, ChannelId::new(456));
    let rendered = log_text(&messages[0].1);
    assert!(rendered.starts_with("System notice\n\n**Time:** `"));
    assert!(rendered.ends_with(&format!(
        "Bot shutting down (v{}). Uptime: 1h 1m 1s",
        env!("CARGO_PKG_VERSION")
    )));
}

#[tokio::test]
async fn graceful_shutdown_emits_notices_before_cutoff() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "system", 456)
        .await
        .unwrap();
    let sink = Arc::new(Sink::default());
    let app = Arc::clone(&f.app);
    let delivery = Arc::clone(&sink);
    assert!(f.app.activation.may_record());
    crate::activation::drain_for_shutdown_notices(&f.app.activation, async move {
        assert!(app.activation.may_record());
        let sent = emit_shutdown_notices_to(
            &app,
            delivery,
            vec![GuildId::new(789)],
            shutdown_notice_content(1),
            SHUTDOWN_NOTICE_PHASE,
        )
        .await;
        assert_eq!(sent, 1);
    })
    .await;
    assert!(!f.app.activation.may_record());
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lease_loss_cutoff_emits_no_shutdown_notices() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "system", 456)
        .await
        .unwrap();
    f.app.activation.note_lease_lost();
    let sink = Arc::new(Sink::default());
    let sent = emit_shutdown_notices_to(
        &f.app,
        sink.clone(),
        vec![GuildId::new(789)],
        shutdown_notice_content(1),
        SHUTDOWN_NOTICE_PHASE,
    )
    .await;
    assert_eq!(sent, 0);
    assert!(sink.messages.lock().unwrap().is_empty());
    assert!(!f.app.activation.may_record());
}

#[tokio::test]
async fn shutdown_notice_waits_for_busy_log_delivery_capacity() {
    use std::future::Future;

    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "system", 456)
        .await
        .unwrap();
    let sink = Sink::default();
    let permit = f.app.log_delivery_slots.acquire().await.unwrap();
    let mut notice = Box::pin(emit_system_notice_to(
        &f.app,
        &sink,
        GuildId::new(789),
        shutdown_notice_content(61),
        Admission::WaitForShutdown,
    ));
    let first_poll =
        std::future::poll_fn(|cx| std::task::Poll::Ready(notice.as_mut().poll(cx))).await;
    assert!(first_poll.is_pending());
    assert!(sink.messages.lock().unwrap().is_empty());
    drop(permit);
    assert_eq!(notice.await, Delivery::Sent);
    let messages = sink.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, ChannelId::new(456));
    assert!(log_text(&messages[0].1).contains(&format!(
        "Bot shutting down (v{}). Uptime: 1m 1s",
        env!("CARGO_PKG_VERSION")
    )));
}

#[tokio::test]
async fn shutdown_phase_attempts_all_guilds_with_shared_send_capacity() {
    let f = Fixture::new();
    for guild in 789..793 {
        db::upsert_log_channel(&f.app.db, guild, "system", guild + 100)
            .await
            .unwrap();
    }
    let sink = Arc::new(Sink::default());
    let sent = emit_shutdown_notices_to(
        &f.app,
        Arc::clone(&sink),
        (789..793).map(GuildId::new).collect(),
        shutdown_notice_content(61),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(sent, 4);
    let messages = sink.messages.lock().unwrap();
    assert_eq!(messages.len(), 4);
    for channel in 889..893 {
        assert!(
            messages
                .iter()
                .any(|(id, _)| *id == ChannelId::new(channel))
        );
    }
}

#[tokio::test]
async fn shutdown_phase_expires_even_when_all_delivery_slots_stay_busy() {
    let f = Fixture::new();
    let permit = f.app.log_delivery_slots.acquire().await.unwrap();
    let sent = tokio::time::timeout(
        Duration::from_millis(500),
        emit_shutdown_notices_to(
            &f.app,
            Arc::new(Sink::default()),
            (789..793).map(GuildId::new).collect(),
            shutdown_notice_content(61),
            Duration::from_millis(20),
        ),
    )
    .await
    .expect("whole shutdown phase must expire before a stuck per-notice deadline");
    assert_eq!(sent, 0);
    drop(permit);
}

#[tokio::test]
async fn shutdown_notices_spend_at_most_three_seconds_waiting_for_capacity() {
    let f = Fixture::with_timeout(60);
    let _busy = f.app.log_delivery_slots.acquire().await.unwrap();
    let http = Arc::new(serenity::http::HttpBuilder::new("fixture").build());
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    let sent = emit_shutdown_notices(
        &f.app,
        http,
        vec![GuildId::new(789)],
        shutdown_notice_content(1),
    )
    .await;
    assert_eq!(sent, 0);
    let elapsed = start.elapsed();
    assert!(
        (Duration::from_secs(3)..Duration::from_millis(3010)).contains(&elapsed),
        "phase budget elapsed after {elapsed:?}"
    );
}

#[tokio::test]
async fn shutdown_stops_pending_start_notices_before_sending_stop() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "system", 456)
        .await
        .unwrap();
    let sink = Arc::new(Sink::default());
    let release_sync = Arc::new(tokio::sync::Notify::new());
    let (sync_started, sync_started_rx) = tokio::sync::oneshot::channel();
    let app = Arc::clone(&f.app);
    let startup_sink = Arc::clone(&sink);
    let startup_release = Arc::clone(&release_sync);
    f.app
        .spawn_startup_notice_task(async move {
            let _ = sync_started.send(());
            startup_release.notified().await;
            emit_system_notice_to(
                &app,
                startup_sink.as_ref(),
                GuildId::new(789),
                "Bot started".into(),
                Admission::Immediate,
            )
            .await;
        })
        .await;
    sync_started_rx.await.unwrap();

    f.app.stop_startup_notice_task().await;
    release_sync.notify_waiters();
    assert_eq!(
        emit_shutdown_notices_to(
            &f.app,
            Arc::clone(&sink),
            vec![GuildId::new(789)],
            shutdown_notice_content(61),
            Duration::from_secs(1),
        )
        .await,
        1
    );
    let late_task_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran = Arc::clone(&late_task_ran);
    f.app
        .spawn_startup_notice_task(async move {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
    tokio::task::yield_now().await;
    assert!(!late_task_ran.load(std::sync::atomic::Ordering::SeqCst));

    let messages = sink.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(log_text(&messages[0].1).contains("Bot shutting down"));
}

#[tokio::test]
async fn configured_routine_delivery_needs_no_history_tables_and_keeps_guild_routes_separate() {
    let f = Fixture::new();
    for (guild, channel) in [(789, 456), (790, 457)] {
        db::upsert_log_channel(&f.app.db, guild, "message_edit", channel)
            .await
            .unwrap();
    }
    // Removing these tables makes any accidental history access fail immediately.
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS members; DROP TABLE IF EXISTS users; DROP TABLE events;",
                65_536,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let sink = Sink::default();
    for guild in [789, 790] {
        let mut event = edited(serde_json::json!({"content":"hello"}));
        event.guild_id = Some(GuildId::new(guild));
        let report = edit_report(&event, None).unwrap();
        assert_eq!(
            emit_to(&f.app, &sink, report.guild_id, report.event, report.vars).await,
            Delivery::Sent
        );
    }
    // A warm valid config is sufficient during DB failure; do not query history.
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded("DROP TABLE meta;", 65_536)?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_edit",
            BTreeMap::new()
        )
        .await,
        Delivery::Sent
    );
    let sent = sink.messages.lock().unwrap();
    assert_eq!(
        sent.iter().map(|(ch, _)| ch.get()).collect::<Vec<_>>(),
        [456, 457, 456]
    );
}

#[tokio::test]
async fn disabled_reports_and_expired_admission_do_not_send() {
    let f = Fixture::new();
    let sink = Sink::default();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Unconfigured
    );
    let permit = f.app.log_delivery_slots.acquire().await.unwrap();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Dropped
    );
    drop(permit);
    assert!(sink.messages.lock().unwrap().is_empty());
    assert_eq!(
        f.app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn full_log_delivery_wait_queue_drops_and_counts_the_next_report() {
    use std::future::Future;
    use std::task::Poll;

    let f = Fixture::new();
    let sink = Sink::default();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    let held_slot = reserve_log_delivery(&f.app).unwrap();
    let waiters: Vec<_> = (0..128)
        .map(|_| {
            let reservation = reserve_log_delivery(&f.app).expect("128 reports may wait");
            assert!(matches!(
                reservation,
                LogDeliveryReservation::Waiting { .. }
            ));
            reservation
        })
        .collect();
    let mut report = std::pin::pin!(emit_to(
        &f.app,
        &sink,
        GuildId::new(789),
        "message_delete",
        BTreeMap::new(),
    ));
    let delivery = std::future::poll_fn(|cx| match report.as_mut().poll(cx) {
        Poll::Ready(delivery) => Poll::Ready(delivery),
        Poll::Pending => panic!("a full queue must drop immediately"),
    })
    .await;
    assert_eq!(delivery, Delivery::Dropped);
    assert!(sink.messages.lock().unwrap().is_empty());
    assert_eq!(
        f.app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    drop((held_slot, waiters));
}

struct StalledSink;
#[serenity::async_trait]
impl LogDelivery for StalledSink {
    async fn send(&self, _: ChannelId, _: CreateMessage) -> anyhow::Result<()> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn slow_delivery_expires_and_releases_capacity_without_retry() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    assert_eq!(
        emit_to(
            &f.app,
            &StalledSink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(f.app.log_delivery_slots.available_permits(), 1);
    let sink = Sink::default();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
}

#[test]
fn invalid_delivery_budgets_fail_before_building_runtime_state() {
    let f = Fixture::new();
    for (concurrency, timeout) in [(0, 1), (1, 0), (1, u64::MAX)] {
        let db = Db::open(&f.path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = concurrency;
        cfg.log.discord_timeout_secs = timeout;
        assert!(BotState::new(cfg, db).is_err());
    }
}

fn audit_fixture(action: Action, target: u64, observed_ms: i64, reason: Option<&str>) -> AuditLogs {
    let id = (((observed_ms as u64 - 1_420_070_400_000) << 22) | action.num() as u64).to_string();
    serde_json::from_value(serde_json::json!({
        "audit_log_entries": [{"id":id,"target_id":target.to_string(),
            "user_id":"777", "action_type":action.num(), "reason":reason}],
        "users":[{"id":"777","username":"moderator","discriminator":"0","avatar":null}],
        "application_commands":[], "auto_moderation_rules":[], "guild_scheduled_events":[],
        "integrations":[], "threads":[], "webhooks":[]
    }))
    .unwrap()
}

type AuditRequest = (hyper::Uri, tokio::sync::oneshot::Sender<AuditLogs>);

async fn held_audit_proxy() -> (
    serenity::http::Http,
    tokio::sync::mpsc::UnboundedReceiver<AuditRequest>,
    tokio::task::JoinHandle<Result<(), hyper::Error>>,
) {
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Method, Response, Server, StatusCode};
    use std::convert::Infallible;
    use tokio::sync::{mpsc, oneshot};

    let (requests, received) = mpsc::unbounded_channel();
    let server = Server::bind(&"127.0.0.1:0".parse().unwrap());
    let address = server.local_addr();
    let server = tokio::spawn(server.serve(make_service_fn(move |_| {
        let requests = requests.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |request: hyper::Request<Body>| {
                let (reply, page) = oneshot::channel();
                assert_eq!(request.method(), Method::GET);
                let _ = requests.send((request.uri().clone(), reply));
                async move {
                    let response = match page.await {
                        Ok(page) => Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::to_vec(&page).unwrap()))
                            .unwrap(),
                        Err(_) => Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::empty())
                            .unwrap(),
                    };
                    Ok::<_, Infallible>(response)
                }
            }))
        }
    })));
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .build();
    (http, received, server)
}

fn assert_audit_request(uri: &hyper::Uri, guild: GuildId, action: Action) {
    assert!(
        uri.path().ends_with(&format!("/guilds/{guild}/audit-logs")),
        "{uri}"
    );
    let query = uri.query().unwrap();
    assert!(query.split('&').any(|part| part == "limit=100"), "{uri}");
    assert!(
        query
            .split('&')
            .any(|part| part == format!("action_type={}", action.num())),
        "{uri}"
    );
}

async fn overlap_audit_requests(late: bool) {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let f = Fixture::with_timeout(6);
    let (http, mut requests, server) = held_audit_proxy().await;
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let one = claims.observe(guild.get(), action, 123, observed);
    let two = claims.observe(guild.get(), action, 124, observed);
    let mut one = Box::pin(lookup_member_audit(&f.app, &http, guild, &one));
    let (uri, reply) = tokio::select! {
        request = requests.recv() => request.unwrap(),
        _ = &mut one => panic!("first lookup ended before the HTTP request"),
    };
    assert_audit_request(&uri, guild, action);
    let mut two = Box::pin(lookup_member_audit(&f.app, &http, guild, &two));
    assert!(
        poll_fn(|cx| Poll::Ready(two.as_mut().poll(cx)))
            .await
            .is_pending()
    );

    let second_page = || audit_fixture(action, 124, observed + 1, Some("second target"));
    let mut first_page = audit_fixture(action, 123, observed, Some("first target"));
    if !late {
        first_page.entries.extend(second_page().entries);
    }
    reply.send(first_page).unwrap();

    let mut lookups = std::pin::pin!(async { tokio::join!(one, two) });
    let mut additional_reads = 0;
    let (one, two) = loop {
        tokio::select! {
            result = &mut lookups => break result,
            request = requests.recv() => {
                let (uri, reply) = request.unwrap();
                assert_audit_request(&uri, guild, action);
                additional_reads += 1;
                reply.send(second_page()).unwrap();
            }
        }
    };
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert_eq!(one.unwrap()["reason"], "first target");
    assert_eq!(two.unwrap()["reason"], "second target");
    assert_eq!(additional_reads, usize::from(late), "extra GET requests");
}

#[tokio::test]
async fn member_audit_overlapping_targets_share_one_get() {
    overlap_audit_requests(false).await;
}

#[tokio::test]
async fn member_audit_late_target_uses_a_new_page() {
    overlap_audit_requests(true).await;
}

#[tokio::test]
async fn member_audit_does_not_claim_from_a_page_started_before_the_observation() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let f = Fixture::with_timeout(6);
    let (http, mut requests, server) = held_audit_proxy().await;
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let first = claims.observe(guild.get(), action, 123, observed);
    let mut first = Box::pin(lookup_member_audit(&f.app, &http, guild, &first));
    let (uri, reply) = tokio::select! {
        request = requests.recv() => request.unwrap(),
        _ = &mut first => panic!("first lookup ended before the HTTP request"),
    };
    assert_audit_request(&uri, guild, action);

    let later = claims.observe(guild.get(), action, 124, observed + 10);
    let mut later = Box::pin(lookup_member_audit(&f.app, &http, guild, &later));
    assert!(
        poll_fn(|cx| Poll::Ready(later.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let older_entry = || audit_fixture(action, 124, observed + 1, Some("previous action"));
    let mut first_page = audit_fixture(action, 123, observed, Some("first target"));
    first_page.entries.extend(older_entry().entries);
    reply.send(first_page).unwrap();

    let mut lookups = std::pin::pin!(async { tokio::join!(first, later) });
    let mut fresh_reads = 0;
    let (first, later) = loop {
        tokio::select! {
            result = &mut lookups => break result,
            request = requests.recv() => {
                let (uri, reply) = request.unwrap();
                assert_audit_request(&uri, guild, action);
                fresh_reads += 1;
                let mut fresh = audit_fixture(action, 124, observed + 10, Some("current action"));
                fresh.entries.extend(older_entry().entries);
                reply.send(fresh).unwrap();
            }
        }
    };
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert_eq!(first.unwrap()["reason"], "first target");
    assert!(later.is_none(), "fresh audit page is ambiguous: {later:?}");
    assert_eq!(fresh_reads, 3);
}

#[tokio::test]
async fn member_audit_cancelled_waiter_and_other_keys_leave_the_page_in_flight() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let f = Fixture::with_timeout(6);
    let (http, mut requests, server) = held_audit_proxy().await;
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let other_guild = GuildId::new(790);
    let ban = member_audit_action("member_ban").unwrap();
    let unban = member_audit_action("member_unban").unwrap();
    let observed = 1_790_086_832_000;
    let first = claims.observe(guild.get(), ban, 123, observed);
    let cancelled = claims.observe(guild.get(), ban, 124, observed);
    let elsewhere = claims.observe(other_guild.get(), ban, 123, observed);
    let other_action = claims.observe(guild.get(), unban, 123, observed);
    let mut first = Box::pin(lookup_member_audit(&f.app, &http, guild, &first));
    let (uri, held) = tokio::select! {
        request = requests.recv() => request.unwrap(),
        _ = &mut first => panic!("first lookup ended before the HTTP request"),
    };
    assert_audit_request(&uri, guild, ban);
    let mut cancelled = Box::pin(lookup_member_audit(&f.app, &http, guild, &cancelled));
    assert!(
        poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(cancelled);

    let (elsewhere, other_action, ()) = tokio::join!(
        lookup_member_audit(&f.app, &http, other_guild, &elsewhere),
        lookup_member_audit(&f.app, &http, guild, &other_action),
        async {
            let mut distinct = HashSet::new();
            tokio::time::timeout(Duration::from_secs(3), async {
                for _ in 0..2 {
                    let (uri, reply) = requests.recv().await.unwrap();
                    let (guild, action, reason) = if uri.path().contains("/guilds/790/") {
                        (other_guild, ban, "other guild")
                    } else {
                        (guild, unban, "other action")
                    };
                    assert_audit_request(&uri, guild, action);
                    assert!(distinct.insert((guild, action.num())));
                    reply
                        .send(audit_fixture(action, 123, observed, Some(reason)))
                        .unwrap();
                }
            })
            .await
            .expect("separate keys must make their own requests while the first is held");
        },
    );
    assert_eq!(elsewhere.unwrap()["reason"], "other guild");
    assert_eq!(other_action.unwrap()["reason"], "other action");
    held.send(audit_fixture(ban, 123, observed, Some("held page")))
        .unwrap();
    assert_eq!(first.await.unwrap()["reason"], "held page");
    assert!(requests.try_recv().is_err(), "cancelled waiter sent a GET");
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

struct HeldAuditSink {
    observed: i64,
    calls: std::sync::atomic::AtomicUsize,
    miss_first: bool,
    page_delay: Duration,
}

#[serenity::async_trait]
impl LogDelivery for HeldAuditSink {
    async fn send(&self, _: ChannelId, _: CreateMessage) -> anyhow::Result<()> {
        unreachable!()
    }

    async fn member_audit(&self, _: GuildId, action: Action) -> anyhow::Result<AuditLogs> {
        let mut page = audit_fixture(action, 123, self.observed, Some("fresh retry"));
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match (self.miss_first, call) {
            (true, 0) => page.entries.clear(),
            (true, 1) | (false, 0) => std::future::pending().await,
            _ if !self.page_delay.is_zero() => tokio::time::sleep(self.page_delay).await,
            _ => {}
        }
        Ok(page)
    }
}

#[tokio::test]
async fn member_audit_retry_uses_a_page_started_after_its_wait() {
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let f = Fixture::with_timeout(6);
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let target = claims.observe(guild.get(), action, 123, observed);
    let other = claims.observe(guild.get(), action, 124, observed);
    let sink = HeldAuditSink {
        observed,
        calls: Default::default(),
        miss_first: true,
        page_delay: Duration::ZERO,
    };

    tokio::time::pause();
    let mut target = Box::pin(lookup_member_audit(&f.app, &sink, guild, &target));
    assert!(
        poll_fn(|cx| Poll::Ready(target.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    tokio::time::advance(Duration::from_millis(250)).await;
    let mut other = Box::pin(lookup_member_audit(&f.app, &sink, guild, &other));
    assert!(
        poll_fn(|cx| Poll::Ready(other.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(sink.calls.load(Ordering::Relaxed), 2);
    assert_eq!(target.await.unwrap()["reason"], "fresh retry");
    assert_eq!(sink.calls.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn member_audit_waiter_keeps_its_deadline_after_the_owner_expires() {
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let f = Fixture::with_timeout(6);
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let owner = claims.observe(guild.get(), action, 124, observed);
    let waiter = claims.observe(guild.get(), action, 123, observed);
    let sink = HeldAuditSink {
        observed,
        calls: Default::default(),
        miss_first: false,
        page_delay: Duration::ZERO,
    };

    tokio::time::pause();
    let mut owner = Box::pin(lookup_member_audit(&f.app, &sink, guild, &owner));
    assert!(
        poll_fn(|cx| Poll::Ready(owner.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    tokio::time::advance(Duration::from_millis(100)).await;
    let mut waiter = Box::pin(lookup_member_audit(&f.app, &sink, guild, &waiter));
    assert!(
        poll_fn(|cx| Poll::Ready(waiter.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(
        sink.calls.load(Ordering::Relaxed),
        1,
        "waiter shares the held GET"
    );
    assert!(owner.await.is_none());
    assert_eq!(waiter.await.unwrap()["reason"], "fresh retry");
    assert_eq!(sink.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn member_audit_waiter_has_time_for_a_fresh_request_when_the_owner_stalls() {
    let f = Fixture::with_timeout(6);
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let owner = claims.observe(guild.get(), action, 124, observed);
    let waiter = claims.observe(guild.get(), action, 123, observed);
    let sink = HeldAuditSink {
        observed,
        calls: Default::default(),
        miss_first: false,
        page_delay: Duration::from_millis(300),
    };
    let owner_finished = std::cell::Cell::new(false);

    tokio::time::pause();
    let (owner, (waiter, finished_before_owner)) = tokio::join!(
        async {
            let result = lookup_member_audit(&f.app, &sink, guild, &owner).await;
            owner_finished.set(true);
            result
        },
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let result = lookup_member_audit(&f.app, &sink, guild, &waiter).await;
            (result, !owner_finished.get())
        },
    );
    assert!(owner.is_none());
    assert_eq!(
        waiter.expect("the waiter has time to receive its own page")["reason"],
        "fresh retry"
    );
    assert!(finished_before_owner, "the held request is still in flight");
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn member_audit_requires_unique_recent_target_and_action() {
    let observed = 1_790_086_832_000;
    for event in ["member_ban", "member_unban"] {
        let action = member_audit_action(event).unwrap();
        let mut logs = audit_fixture(
            action,
            123,
            observed,
            Some("[jmg:0123456789abcdef0123] actual reason"),
        );
        let vars = unclaimed_audit_vars(&logs, action, 123, observed).unwrap();
        assert_eq!(vars["actor"], "moderator (<@777>)");
        assert_eq!(vars["actor_id"], "777");
        assert_eq!(vars["reason"], "actual reason");
        assert!(unclaimed_audit_vars(&logs, action, 999, observed).is_none());
        assert!(
            unclaimed_audit_vars(&logs, Action::Member(MemberAction::Kick), 123, observed)
                .is_none()
        );
        assert!(unclaimed_audit_vars(&logs, action, 123, observed + 10_001).is_none());
        assert!(unclaimed_audit_vars(&logs, action, 123, observed - 3_001).is_none());
        logs.entries.push(logs.entries[0].clone());
        assert!(unclaimed_audit_vars(&logs, action, 123, observed).is_none());
    }
}

#[test]
fn audit_without_reason_or_user_name_keeps_known_identity() {
    let observed = 1_790_086_832_000;
    let action = member_audit_action("member_ban").unwrap();
    let mut logs = audit_fixture(action, 123, observed, None);
    logs.users.clear();
    let vars = unclaimed_audit_vars(&logs, action, 123, observed).unwrap();
    assert_eq!(vars["actor"], "<@777>");
    assert_eq!(vars["reason"], "No reason provided");
}

struct AuditSink {
    sink: Sink,
    calls: std::sync::atomic::AtomicUsize,
    missing_first: bool,
    stalled: bool,
}
#[serenity::async_trait]
impl LogDelivery for AuditSink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.sink.send(channel, message).await
    }
    async fn member_audit(&self, guild: GuildId, action: Action) -> anyhow::Result<AuditLogs> {
        assert_eq!(guild.get(), 789);
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.stalled {
            std::future::pending::<()>().await;
        }
        let mut logs = audit_fixture(
            action,
            123,
            chrono::Utc::now().timestamp_millis(),
            Some("spam"),
        );
        // Distinct calls in this fixture represent distinct mocked actions.
        // Repeated audit responses are modeled explicitly by RepeatedAuditSink.
        logs.entries[0].id = serenity::model::id::AuditLogEntryId::new(
            logs.entries[0].id.get() + ((call as u64 + 1) << 8),
        );
        if self.missing_first && call == 0 {
            logs.entries.clear();
        }
        Ok(logs)
    }
}

#[tokio::test]
async fn member_audit_skips_permanently_ineligible_observations() {
    use std::sync::atomic::Ordering;

    let f = Fixture::with_timeout(4);
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let superseded = claims.observe(guild.get(), action, 123, observed);
    let current = claims.observe(guild.get(), action, 123, observed + 6_000);
    let logs = audit_fixture(action, 123, observed + 6_000, Some("spam"));
    assert!(claims.claim(&current, &logs).is_some());
    let untracked = MemberAuditObservation {
        guild: guild.get(),
        target: 456,
        action,
        observed_ms: observed,
        observed_at: superseded.observed_at,
        generation: None,
    };
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: false,
    };

    tokio::time::pause();
    for (label, observation) in [
        ("superseded", superseded),
        ("claimed", current),
        ("untracked", untracked),
    ] {
        assert!(
            lookup_member_audit(&f.app, &sink, guild, &observation)
                .await
                .is_none()
        );
        assert_eq!(sink.calls.load(Ordering::Relaxed), 0, "{label}");
    }
}

#[tokio::test]
async fn member_audit_rechecks_eligibility_after_the_retry_delay() {
    use std::future::Future;
    use std::sync::atomic::Ordering;

    let f = Fixture::with_timeout(4);
    let claims = &f.app.member_audits;
    let guild = GuildId::new(789);
    let action = member_audit_action("member_ban").unwrap();
    let observed = 1_790_086_832_000;
    let original = claims.observe(guild.get(), action, 123, observed);
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: true,
        stalled: false,
    };

    tokio::time::pause();
    let mut lookup = Box::pin(lookup_member_audit(&f.app, &sink, guild, &original));
    let first_poll =
        std::future::poll_fn(|cx| std::task::Poll::Ready(lookup.as_mut().poll(cx))).await;
    assert!(first_poll.is_pending());
    assert_eq!(sink.calls.load(Ordering::Relaxed), 1);

    claims.observe(guild.get(), action, 123, observed + 6_000);
    assert!(lookup.await.is_none());
    assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn member_reports_render_audit_context_and_apply_actor_filters() {
    let f = Fixture::new();
    for event in ["member_ban", "member_unban"] {
        db::upsert_log_channel(&f.app.db, 789, event, 456)
            .await
            .unwrap();
    }
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: false,
    };
    for event in ["member_ban", "member_unban"] {
        let vars = templates::user_vars(123, "username", "Display");
        assert_eq!(
            emit_to(&f.app, &sink, GuildId::new(789), event, vars).await,
            Delivery::Sent
        );
    }
    for (_, message) in sink.sink.messages.lock().unwrap().iter() {
        let text = log_text(message);
        assert!(text.contains("moderator (<@777>)"));
        assert!(text.contains("**Reason:** spam"));
        assert!(!text.contains("{actor}"));
    }
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_bounded(
                "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'member_ban',?1)",
                db::params![r#"{"users":["777"],"channels":[]}"#],
                65_536,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    f.app.invalidate_configs();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_ban",
            templates::user_vars(123, "username", "Display")
        )
        .await,
        Delivery::Unconfigured
    );
    assert_eq!(sink.sink.messages.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn gateway_ban_reuses_claimed_audit_for_report() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: false,
    };
    let now = chrono::Utc::now().timestamp_millis();
    let observation = f
        .app
        .member_audits
        .observe_event(789, 123, "member_ban", now)
        .unwrap();
    let context = lookup_member_audit(&f.app, &sink, GuildId::new(789), &observation)
        .await
        .unwrap();
    assert_eq!(context["actor_id"], "777");
    assert_eq!(context["actor_name"], "moderator");
    assert_eq!(context["reason"], "spam");
    let mut vars = templates::user_vars(123, "username", "Display");
    vars.extend(context);
    assert_eq!(
        emit_to_observed(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_ban",
            vars,
            Some(observation),
            Admission::Immediate,
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(log_text(&sink.sink.messages.lock().unwrap()[0].1).contains("moderator (<@777>)"));
}

#[tokio::test]
async fn ban_report_returns_late_audit_for_its_stored_row() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: false,
    };
    let observation = f
        .app
        .member_audits
        .observe_event(
            789,
            123,
            "member_ban",
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
    let (delivery, audit) = emit_to_observed_with_audit(
        &f.app,
        &sink,
        GuildId::new(789),
        "member_ban",
        templates::user_vars(123, "username", "Display"),
        Some(observation),
        Admission::Immediate,
        || true,
        false,
        None,
        None,
    )
    .await;
    assert_eq!(delivery, Delivery::Sent);
    let audit = audit.unwrap();
    assert_eq!(audit["actor_id"], "777");
    assert_eq!(audit["reason"], "spam");
}

#[tokio::test]
async fn inaccessible_audit_renders_explicit_fallback_for_both_events() {
    let f = Fixture::new();
    let sink = Sink::default();
    for event in ["member_ban", "member_unban"] {
        db::upsert_log_channel(&f.app.db, 789, event, 456)
            .await
            .unwrap();
        f.app.invalidate_configs();
        assert_eq!(
            emit_to(
                &f.app,
                &sink,
                GuildId::new(789),
                event,
                templates::user_vars(123, "username", "Display")
            )
            .await,
            Delivery::Sent
        );
    }
    for (_, message) in sink.messages.lock().unwrap().iter() {
        let text = log_text(message);
        assert!(text.contains("**Moderator:** unavailable"));
        assert!(text.contains("**Reason:** unavailable"));
        assert!(!text.contains("{actor}"));
    }
}

#[tokio::test]
async fn stalled_audit_still_delivers_fallback_within_report_budget() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: true,
    };
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_ban",
            templates::user_vars(123, "username", "Display")
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(f.app.log_delivery_slots.available_permits(), 1);
}

#[test]
fn leave_uses_supplied_join_time_and_explicit_missing_context() {
    let observed = chrono::DateTime::from_timestamp(1_790_086_832, 0).unwrap();
    let joined =
        serenity::model::Timestamp::from_unix_timestamp(observed.timestamp() - 90_060).unwrap();
    let vars = member_join_vars(Some(joined), observed);
    assert_eq!(vars["duration"], "1d 1h 1m");
    assert_eq!(vars["joined"], joined.to_string());
    let text = templates::render(default_template("member_leave"), &vars);
    assert!(text.contains("**Time in server:** `1d 1h 1m`"));
    assert_eq!(member_join_vars(None, observed)["duration"], "Unavailable");
    assert_eq!(member_join_vars(None, observed)["joined"], "Unavailable");
}

#[test]
fn deferred_verified_leave_uses_captured_stay_and_original_event_time() {
    let joined_at_ms = 1_790_000_000_000;
    let departed_at_ms = joined_at_ms + 3_600_000;
    let departure = db::guild_data::MemberDeparture {
        user_id: 9007199254740993,
        joined_at_ms: Some(joined_at_ms),
        departed_at_ms,
        completed_stay_ms: Some(3_600_000),
    };
    let profile = db::guild_data::MemberProfile {
        user_id: departure.user_id.to_string(),
        display_name: "Saved nickname".into(),
        username: Some("saved_user".into()),
        global_name: None,
        nickname: Some("Saved nickname".into()),
        is_current: false,
        is_banned: false,
        ever_banned: false,
        current_ban_reason: None,
        joined_at_ms: Some(joined_at_ms),
        departed_at_ms: Some(departed_at_ms),
        observed_at_ms: departed_at_ms,
        revision: 2,
    };
    let vars = verified_member_departure_vars(&departure, Some(&profile)).unwrap();
    assert_eq!(vars["user"], "Saved nickname");
    assert_eq!(vars["username"], "saved_user");
    assert_eq!(vars["duration"], "1h");
    assert_eq!(vars["joined"], "2026-09-21T14:13:20+00:00");
    assert_eq!(vars["time"], "2026-09-21T15:13:20+00:00");
    let fallback = verified_member_departure_vars(&departure, None).unwrap();
    assert_eq!(fallback["user"], departure.user_id.to_string());
    assert_eq!(fallback["duration"], "1h");
}

#[test]
fn received_message_join_context_is_scoped_consumed_and_expiring() {
    let cache = MemberJoinContext::default();
    let joined = serenity::model::Timestamp::from_unix_timestamp(1_760_000_000).unwrap();
    cache.remember(1, 2, Some(joined));
    let seen = cache.inner.lock().by_member[&(1, 2)].1;
    cache.remember(1, 2, None);
    assert_eq!(cache.inner.lock().by_member[&(1, 2)].1, seen);
    assert_eq!(cache.take(2, 2), None);
    assert_eq!(cache.take(1, 2), Some(joined));
    assert_eq!(cache.take(1, 2), None);
    {
        let mut entries = cache.inner.lock();
        let expired = seen - MEMBER_JOINS_KEEP;
        entries.remember((1, 2), joined, expired);
        entries.remember((1, 3), joined, expired);
    }
    assert_eq!(cache.take(1, 2), None);
    cache.remember(2, 2, Some(joined));
    let entries = cache.inner.lock();
    assert_eq!(entries.by_member.len(), 1);
    assert_eq!(entries.by_age.len(), 1);
    assert!(!entries.by_member.contains_key(&(1, 3)));
}

#[test]
fn received_message_join_context_refresh_preserves_oldest_eviction_and_index_bound() {
    let cache = MemberJoinContext::default();
    let joined = serenity::model::Timestamp::from_unix_timestamp(1_760_000_000).unwrap();
    let refreshed = serenity::model::Timestamp::from_unix_timestamp(1_759_999_940).unwrap();
    let per_guild = MEMBER_JOINS_LIMIT / 2;
    cache.remember(1, 0, Some(joined));
    {
        let mut entries = cache.inner.lock();
        let start = entries.by_member[&(1, 0)].1 - Duration::from_secs(1);
        for user in 0..MEMBER_JOINS_LIMIT {
            let guild = if user < per_guild { 1 } else { 2 };
            let seen = start + Duration::from_micros(user as u64);
            entries.remember((guild, user as u64), joined, seen);
        }
    }
    for _ in 0..MEMBER_JOINS_LIMIT {
        cache.remember(1, 0, Some(refreshed));
    }
    cache.remember(1, MEMBER_JOINS_LIMIT as u64, Some(joined));
    {
        let entries = cache.inner.lock();
        assert_eq!(entries.by_member.len(), MEMBER_JOINS_LIMIT);
        assert_eq!(entries.by_age.len(), MEMBER_JOINS_LIMIT);
    }
    assert_eq!(cache.take(1, 1), None);
    assert_eq!(cache.take(1, 0), Some(refreshed));
    cache.clear_guild(1);
    assert_eq!(cache.take(1, 2), None);
    assert_eq!(cache.take(2, per_guild as u64), Some(joined));
    let entries = cache.inner.lock();
    assert_eq!(entries.by_member.len(), per_guild - 1);
    assert_eq!(entries.by_age.len(), per_guild - 1);
}

#[tokio::test]
async fn late_audit_is_retried_but_disabled_reports_do_not_lookup() {
    let f = Fixture::with_timeout(4);
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: true,
        stalled: false,
    };
    let now = chrono::Utc::now();
    let mut vars = templates::user_vars(123, "username", "Display");
    // The disabled predecessor is a distinct observation, outside the new
    // action's audit-time ambiguity window.
    vars.insert(
        "time".into(),
        (now - chrono::Duration::seconds(6)).to_rfc3339(),
    );
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_ban", vars.clone()).await,
        Delivery::Unconfigured
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    f.app.invalidate_configs();
    vars.insert("time".into(), now.to_rfc3339());
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_ban", vars).await,
        Delivery::Sent
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(log_text(&sink.sink.messages.lock().unwrap()[0].1).contains("**Reason:** spam"));
}

#[tokio::test]
async fn audit_lookup_stops_after_three_empty_reads_and_sends_fallback() {
    #[derive(Default)]
    struct EmptyAudit {
        sink: Sink,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[serenity::async_trait]
    impl LogDelivery for EmptyAudit {
        async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
            self.sink.send(channel, message).await
        }

        async fn member_audit(&self, _: GuildId, action: Action) -> anyhow::Result<AuditLogs> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut logs = audit_fixture(action, 123, 1_700_000_000_000, None);
            logs.entries.clear();
            Ok(logs)
        }
    }

    let f = Fixture::with_timeout(10);
    let sink = EmptyAudit::default();
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    f.app.configs.get(&f.app.db, 789).await.unwrap();
    tokio::time::pause();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_ban",
            templates::user_vars(123, "user", "User")
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 3);
    let sent = sink.sink.messages.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let report = log_text(&sent[0].1);
    assert!(report.contains("**Moderator:** unavailable"), "{report}");
    assert!(report.contains("**Reason:** unavailable"), "{report}");
}

fn unclaimed_audit_vars(
    logs: &AuditLogs,
    action: Action,
    target: u64,
    observed_ms: i64,
) -> Option<BTreeMap<String, String>> {
    super::member_audit_vars(logs, action, target, observed_ms, |_| false).map(|(_, vars)| vars)
}

struct RepeatedAuditSink {
    sink: Sink,
    responses: Mutex<std::collections::VecDeque<AuditLogs>>,
    fail_first_send: bool,
    sends: std::sync::atomic::AtomicUsize,
}
#[serenity::async_trait]
impl LogDelivery for RepeatedAuditSink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        if self
            .sends
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            == 0
            && self.fail_first_send
        {
            anyhow::bail!("fixture delivery failed");
        }
        self.sink.send(channel, message).await
    }
    async fn member_audit(&self, _: GuildId, _: Action) -> anyhow::Result<AuditLogs> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no response"))
    }
}

#[tokio::test]
async fn repeated_actions_never_reuse_audit_after_sent_filtered_or_failed_report() {
    for event in ["member_ban", "member_unban"] {
        for previous in [Delivery::Sent, Delivery::Unconfigured, Delivery::Failed] {
            let f = Fixture::with_timeout(4);
            db::upsert_log_channel(&f.app.db, 789, event, 456)
                .await
                .unwrap();
            if previous == Delivery::Unconfigured {
                f.app
                    .db
                    .with_conn(move |conn| {
                        conn.execute_bounded(
                            "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,?1,?2)",
                            db::params![event, r#"{"users":["777"],"channels":[]}"#],
                            65_536,
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let action = member_audit_action(event).unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            let old = audit_fixture(action, 123, now - 4_000, Some("previous action"));
            let old_again = audit_fixture(action, 123, now - 4_000, Some("previous action"));
            let mut current = audit_fixture(action, 123, now, Some("current action"));
            current.entries[0].user_id = serenity::model::id::UserId::new(888);
            current.entries.push(old.entries[0].clone());
            let sink = RepeatedAuditSink {
                sink: Sink::default(),
                responses: Mutex::new([old, old_again, current].into()),
                fail_first_send: previous == Delivery::Failed,
                sends: Default::default(),
            };
            let vars = templates::user_vars(123, "username", "Display");
            assert_eq!(
                emit_to(&f.app, &sink, GuildId::new(789), event, vars.clone()).await,
                previous
            );
            assert_eq!(
                emit_to(&f.app, &sink, GuildId::new(789), event, vars).await,
                Delivery::Sent
            );
            let messages = sink.sink.messages.lock().unwrap();
            let last = log_text(&messages.last().unwrap().1);
            assert!(last.contains("**Moderator:** <@888>"), "{last}");
            assert!(last.contains("**Reason:** current action"), "{last}");
            assert!(!last.contains("previous action"));
            assert!(sink.responses.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn audit_claims_are_scoped_atomic_and_bounded_without_live_eviction() {
    let claims = MemberAuditClaims::default();
    let action = member_audit_action("member_ban").unwrap();
    let observed = chrono::Utc::now().timestamp_millis();
    let logs = audit_fixture(action, 123, observed, None);
    let first = claims.observe(1, action, 123, observed);
    assert!(claims.claim(&first, &logs).is_some());
    assert!(claims.claim(&first, &logs).is_none());
    let other_guild = claims.observe(2, action, 123, observed);
    assert!(claims.claim(&other_guild, &logs).is_some());
    {
        let mut state = claims.inner.lock();
        for id in 0..MEMBER_AUDIT_LIMIT as u64 - 2 {
            state.claimed.insert((3, id), Instant::now());
        }
        assert_eq!(state.claimed.len(), MEMBER_AUDIT_LIMIT);
    }
    let full = claims.observe(4, action, 123, observed);
    assert!(claims.claim(&full, &logs).is_none());
    assert!(claims.claim(&first, &logs).is_none());
    for at in claims.inner.lock().claimed.values_mut() {
        *at = Instant::now() - MEMBER_AUDIT_KEEP;
    }
    assert!(claims.claim(&full, &logs).is_some());
    assert_eq!(claims.inner.lock().claimed.len(), 1);
}

struct UnavailableAuditSink {
    mode: &'static str,
    observed: i64,
}
#[serenity::async_trait]
impl LogDelivery for UnavailableAuditSink {
    async fn send(&self, _: ChannelId, _: CreateMessage) -> anyhow::Result<()> {
        Ok(())
    }
    async fn member_audit(&self, _: GuildId, action: Action) -> anyhow::Result<AuditLogs> {
        match self.mode {
            "error" => anyhow::bail!("audit unavailable"),
            "stalled" => std::future::pending::<()>().await,
            "disabled" | "filtered" | "dropped" => panic!("skipped report queried audit"),
            _ => {}
        }
        let mut logs = audit_fixture(action, 123, self.observed, Some("previous action"));
        if self.mode == "ambiguous" {
            logs.entries.push(logs.entries[0].clone());
        } else {
            logs.entries.clear();
        }
        Ok(logs)
    }
}

#[tokio::test]
async fn delayed_audit_after_unknown_or_skipped_report_cannot_explain_repeat_action() {
    async fn emit_with_paused_clock(
        f: &Fixture,
        sink: &impl LogDelivery,
        event: &str,
        vars: BTreeMap<String, String>,
    ) -> Delivery {
        let guild = GuildId::new(789);
        // Load configuration on real time because the DB runs on a separate runtime.
        f.app.configs.get(&f.app.db, guild.get()).await.unwrap();
        tokio::time::pause();
        let delivery = emit_to(&f.app, sink, guild, event, vars).await;
        tokio::time::resume();
        delivery
    }

    for event in ["member_ban", "member_unban"] {
        for mode in [
            "empty",
            "error",
            "stalled",
            "ambiguous",
            "disabled",
            "filtered",
            "dropped",
        ] {
            let f = Fixture::with_timeout(4);
            if mode != "disabled" {
                db::upsert_log_channel(&f.app.db, 789, event, 456)
                    .await
                    .unwrap();
            }
            if mode == "filtered" {
                f.app
                    .db
                    .with_conn(move |conn| {
                        conn.execute_bounded(
                            "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,?1,?2)",
                            db::params![event, r#"{"users":["123"],"channels":[]}"#],
                            65_536,
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let now = chrono::Utc::now();
            let prior = now - chrono::Duration::seconds(6);
            let mut vars = templates::user_vars(123, "username", "Display");
            vars.insert("time".into(), prior.to_rfc3339());
            let permit = if mode == "dropped" {
                Some(f.app.log_delivery_slots.acquire().await.unwrap())
            } else {
                None
            };
            let delivery = emit_with_paused_clock(
                &f,
                &UnavailableAuditSink {
                    mode,
                    observed: prior.timestamp_millis(),
                },
                event,
                vars.clone(),
            )
            .await;
            drop(permit);
            assert_eq!(
                delivery,
                match mode {
                    "disabled" | "filtered" => Delivery::Unconfigured,
                    "dropped" => Delivery::Dropped,
                    _ => Delivery::Sent,
                },
                "{event}/{mode}"
            );
            db::upsert_log_channel(&f.app.db, 789, event, 456)
                .await
                .unwrap();
            f.app
                .db
                .with_conn(|conn| {
                    conn.execute_bounded("DELETE FROM log_filters", db::params![], 65_536)?;
                    Ok(())
                })
                .await
                .unwrap();
            f.app.invalidate_configs();
            let action = member_audit_action(event).unwrap();
            let old = audit_fixture(
                action,
                123,
                prior.timestamp_millis(),
                Some("previous action"),
            );
            let mut current =
                audit_fixture(action, 123, now.timestamp_millis(), Some("current action"));
            current.entries[0].user_id = serenity::model::id::UserId::new(888);
            current.entries.push(old.entries[0].clone());
            let sink = RepeatedAuditSink {
                sink: Sink::default(),
                responses: Mutex::new([old, current].into()),
                fail_first_send: false,
                sends: Default::default(),
            };
            vars.insert("time".into(), now.to_rfc3339());
            assert_eq!(
                emit_with_paused_clock(&f, &sink, event, vars).await,
                Delivery::Sent
            );
            let messages = sink.sink.messages.lock().unwrap();
            let text = log_text(&messages[0].1);
            assert!(
                text.contains("**Moderator:** <@888>"),
                "{event}/{mode}: {text}"
            );
            assert!(
                text.contains("**Reason:** current action"),
                "{event}/{mode}: {text}"
            );
            assert!(!text.contains("previous action"));
            assert!(sink.responses.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn unresolved_observations_fence_old_handlers_and_overlapping_audit_windows() {
    for event in ["member_ban", "member_unban"] {
        let claims = MemberAuditClaims::default();
        let action = member_audit_action(event).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let previous = claims.observe(1, action, 123, now - 6_000);
        let current = claims.observe(1, action, 123, now);
        let old_logs = audit_fixture(action, 123, now - 6_000, None);
        assert!(claims.claim(&previous, &old_logs).is_none());
        assert!(claims.claim(&current, &old_logs).is_none());
        // A candidate at the end of the unresolved predecessor's window is unsafe.
        let overlap = audit_fixture(action, 123, now - 3_000, None);
        assert!(claims.claim(&current, &overlap).is_none());
        let logs = audit_fixture(action, 123, now, None);
        assert!(claims.claim(&current, &logs).is_some());
        // A successful intervening action does not erase earlier uncertainty.
        let next = claims.observe(1, action, 123, now + 1_000);
        assert!(claims.claim(&next, &overlap).is_none());
        let other_target = claims.observe(1, action, 456, now);
        assert!(
            claims
                .claim(
                    &other_target,
                    &audit_fixture(action, 456, now - 3_000, None)
                )
                .is_some()
        );
        let other_guild = claims.observe(2, action, 123, now);
        assert!(claims.claim(&other_guild, &overlap).is_some());
        let other_action = member_audit_action(if event == "member_ban" {
            "member_unban"
        } else {
            "member_ban"
        })
        .unwrap();
        let other = claims.observe(1, other_action, 123, now);
        assert!(
            claims
                .claim(&other, &audit_fixture(other_action, 123, now - 3_000, None))
                .is_some()
        );
    }
}

#[test]
fn untracked_observation_temporarily_disables_attribution_without_evicting_uncertainty() {
    let claims = MemberAuditClaims::default();
    let action = member_audit_action("member_ban").unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let first = claims.observe(1, action, 1, now);
    for target in 2..=MEMBER_AUDIT_LIMIT as u64 {
        claims.observe(1, action, target, now);
    }
    let overflow = claims.observe(1, action, MEMBER_AUDIT_LIMIT as u64 + 1, now);
    assert!(overflow.generation.is_none());
    let logs = audit_fixture(action, 1, now, None);
    assert!(claims.claim(&first, &logs).is_none());
    assert_eq!(claims.inner.lock().subjects.len(), MEMBER_AUDIT_LIMIT);
    {
        let mut state = claims.inner.lock();
        for subject in state.subjects.values_mut() {
            subject.at = Instant::now() - MEMBER_AUDIT_KEEP;
        }
        state.saturated_until = Some(Instant::now() - Duration::from_secs(1));
    }
    assert!(claims.claim(&first, &logs).is_none());
    let fresh = claims.observe(1, action, 1, now);
    assert!(claims.claim(&fresh, &logs).is_some());
    assert_eq!(claims.inner.lock().subjects.len(), 1);
}

#[tokio::test]
async fn readable_leave_preview_and_supplied_edit_timestamp_reach_discord() {
    let f = Fixture::new();
    for event in ["member_leave", "message_edit"] {
        db::upsert_log_channel(&f.app.db, 789, event, 456)
            .await
            .unwrap();
    }
    let sink = Sink::default();
    let observed = chrono::DateTime::parse_from_rfc3339("2026-09-22T18:02:22.758612736+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut vars = templates::user_vars(553144871846805504, "elgringo1092", "elgringo1092");
    vars.extend(member_join_vars(None, observed));
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_leave", vars).await,
        Delivery::Sent
    );
    let event = edited_at(
        serde_json::json!({"content":"updated text"}),
        "2026-09-22T18:02:22.758Z",
    );
    let report = edit_report(&event, None).unwrap();
    assert_eq!(
        emit_to(&f.app, &sink, report.guild_id, report.event, report.vars).await,
        Delivery::Sent
    );
    let cfg = f.app.configs.get(&f.app.db, 789).await.unwrap();
    let messages = sink.messages.lock().unwrap();
    assert_eq!(
        log_text(&messages[0].1),
        "Member left\n\n**User:** elgringo1092 · <@553144871846805504> (`553144871846805504`)\n**User ID:** `553144871846805504`\n**Time in server:** `Unavailable`\n**Joined at:** `Unavailable`\n**Left at:** `Sep 22, 2026 • 18:02:22 UTC`"
    );
    assert!(log_text(&messages[1].1).contains("**Time:** `Sep 22, 2026 • 18:02:22 UTC`"));
    assert_eq!(cfg.find_command("status").unwrap().name, "stats");
    assert!(std::ptr::eq(
        cfg.find_command("stats").unwrap(),
        cfg.find_command("STATUS").unwrap()
    ));
}

#[test]
fn automod_default_template_preserves_proving_hashes_under_discord_budget() {
    use crate::image_spam::image_notice_evidence;
    let snowflake = |n: u64| 1_000_000_000_000_000_000u64 + n;
    let messages: Vec<_> = (0..16)
        .map(|i| (snowflake(i * 2), snowflake(i * 2 + 1)))
        .collect();
    let proving = vec![[0xcd_u8; 32]];
    let hash_hex = proving[0]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let evidence = image_notice_evidence(&messages, &proving);
    let delete_notes = messages
        .iter()
        .map(|(channel, message)| format!("{channel}/{message} Succeeded"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut vars = BTreeMap::from([
        ("rule".into(), "Exact image spam".into()),
        ("mention".into(), format!("<@{}>", snowflake(99))),
        ("user_id".into(), snowflake(99).to_string()),
        ("source_channel_id".into(), snowflake(0).to_string()),
        ("channel".into(), format!("<#{}>", snowflake(0))),
        (
            "action".into(),
            "delete matched messages; ban Succeeded".into(),
        ),
        (
            "reason".into(),
            "Verified identical image bytes: at least 2 common distinct images across 3 channels in 1m".into(),
        ),
        ("content".into(), format!("{evidence}; {delete_notes}")),
        ("time".into(), "2026-10-09T00:00:00+00:00".into()),
    ]);
    let template = default_template("automod");
    let rendered = render_log_template("automod", template, &mut vars);
    assert!(
        rendered.contains(&hash_hex),
        "expected full proving digest: {rendered}"
    );
    assert!(rendered.encode_utf16().count() <= 2000);
}

fn proving_digests(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("{n:064x}")).collect()
}

/// Independent of `bound_proving_hash_list`: same phrase, measured in UTF-16 units.
fn whole_hash_notice(digests: &[String], budget: usize) -> String {
    let total = digests.len();
    for shown in (0..total).rev() {
        let omitted = total - shown;
        let text = if shown == 0 {
            format!("and {omitted} more")
        } else {
            format!("sha256 {} and {omitted} more", digests[..shown].join(","))
        };
        if text.encode_utf16().count() <= budget {
            return text;
        }
    }
    String::new()
}

#[test]
fn oversized_hash_set_renders_whole_hashes_and_omitted_count() {
    let digests = proving_digests(40);
    let expected = format!("sha256 {} and 10 more", digests[..30].join(","));
    assert_eq!(expected.encode_utf16().count(), 1968);
    assert!(expected.encode_utf16().count() <= 2000);
    assert!(
        format!("sha256 {} and 9 more", digests[..31].join(","))
            .encode_utf16()
            .count()
            > 2000
    );
    assert_eq!(whole_hash_notice(&digests, 2000), expected);
    for content in [
        format!("sha256 {}", digests.join(",")),
        format!("sha256 {}; channel 9, message 8", digests.join(",")),
    ] {
        let mut vars = BTreeMap::from([("content".into(), content)]);
        let rendered = render_log_template("automod", "{content}", &mut vars);
        assert_eq!(rendered, expected);
        assert!(rendered.encode_utf16().count() <= 2000);
        assert!(!rendered.contains('…'));
        assert!(!rendered.contains("channel"));
        let shown = rendered
            .strip_prefix("sha256 ")
            .unwrap()
            .split(" and ")
            .next()
            .unwrap();
        assert!(shown.split(',').all(|digest| digest.len() == 64));
    }
}

#[test]
fn custom_automod_template_keeps_large_match_hashes_within_budget() {
    let template = "**Rule:** {rule}\n**Evidence:** {content}\n**Action:** {action}";
    assert_ne!(template, default_template("automod"));
    let digests = proving_digests(40);
    let locations = (0..16)
        .map(|i| format!("channel {i}, message {}", i + 100))
        .collect::<Vec<_>>()
        .join("; ");
    let content = format!("sha256 {}; {locations}", digests.join(","));
    let mut vars = BTreeMap::from([
        ("rule".into(), "Exact image spam".into()),
        ("action".into(), "log only".into()),
        ("content".into(), content),
    ]);
    let budget = templates::substitution_budget(template, &vars, "content", 2000);
    let expected_content = whole_hash_notice(&digests, budget);
    assert!(expected_content.starts_with("sha256 "));
    assert!(expected_content.contains(" and "));
    assert!(!expected_content.contains("channel"));
    let mut expected_vars = vars.clone();
    expected_vars.insert("content".into(), expected_content);
    let expected = templates::render(template, &expected_vars);
    let rendered = render_log_template("automod", template, &mut vars);
    assert_eq!(rendered, expected);
    assert!(rendered.encode_utf16().count() <= 2000);
    assert!(!rendered.contains('…'));
    assert!(rendered.contains("**Action:** log only"));
    assert!(rendered.contains(&format!("{:064x}", 0)));
    assert!(!rendered.contains("channel 0"));
}

#[test]
fn repeated_content_token_keeps_each_hash_copy_whole() {
    let template = "{content}\n{content}";
    let digests = proving_digests(40);
    let content = format!("sha256 {}", digests.join(","));
    let mut vars = BTreeMap::from([("content".into(), content)]);
    let budget = templates::substitution_budget(template, &vars, "content", 2000) / 2;
    let expected_content = whole_hash_notice(&digests, budget);
    let expected = format!("{expected_content}\n{expected_content}");
    let rendered = render_log_template("automod", template, &mut vars);
    assert_eq!(rendered, expected);
    assert!(rendered.encode_utf16().count() <= 2000);
    assert!(!rendered.contains('…'));
    let mut lines = rendered.lines();
    assert_eq!(lines.next(), lines.next());
    assert!(lines.next().is_none());
}

fn assert_whole_digests(text: &str) {
    let Some(rest) = text.strip_prefix("sha256 ") else {
        return;
    };
    let hash_section = rest
        .split(" and ")
        .next()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .trim();
    if hash_section.is_empty() {
        return;
    }
    for token in hash_section.split(',') {
        assert_eq!(token.len(), 64, "partial digest in {text}");
        assert!(
            token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "partial digest in {text}"
        );
    }
}

#[test]
fn repeated_content_tail_at_zero_tail_budget_keeps_whole_digests() {
    let digests = proving_digests(4);
    let fourth = digests[3].clone();
    let hashes = format!("sha256 {}", digests.join(","));
    let sep = "; ";
    let marker = " [display clipped]";
    let hash_units = hashes.encode_utf16().count();
    assert_eq!(hash_units, 266);
    // Room for the hashes and separator, but not the clipped-tail marker.
    let per_copy = hash_units + sep.encode_utf16().count() + 7;
    assert_eq!(per_copy, 275);
    assert!(hash_units + sep.encode_utf16().count() + marker.encode_utf16().count() > per_copy);
    let chrome = "x".repeat(2000 - per_copy * 2 - 1);
    let template = format!("{chrome}{{content}}\n{{content}}");
    let content = format!("{hashes}{sep}channel 9, message 8");
    let mut vars = BTreeMap::from([("content".into(), content)]);
    let total = templates::substitution_budget(&template, &vars, "content", 2000);
    assert_eq!(total / 2, per_copy);
    let rendered = render_log_template("automod", &template, &mut vars);
    assert!(rendered.encode_utf16().count() <= 2000);
    assert!(vars["content"].encode_utf16().count() <= per_copy);
    assert_eq!(rendered.matches(fourth.as_str()).count(), 2, "{rendered}");
    assert_whole_digests(&vars["content"]);
    assert!(!rendered.contains('…'));
}

#[test]
fn automod_content_bound_stays_within_budget_across_sizes() {
    for budget in [
        0, 1, 8, 18, 19, 70, 265, 266, 268, 274, 275, 286, 287, 400, 2000,
    ] {
        for count in [0, 1, 2, 4, 8] {
            for tail_len in [0, 1, 8, 40] {
                let content = if count == 0 {
                    "y".repeat(tail_len.max(1))
                } else {
                    let digests = proving_digests(count);
                    let hashes = format!("sha256 {}", digests.join(","));
                    if tail_len == 0 {
                        hashes
                    } else {
                        format!("{hashes}; {}", "y".repeat(tail_len))
                    }
                };
                let mut vars = BTreeMap::from([("content".into(), content)]);
                bound_automod_content_var("{content}", &mut vars, budget);
                let bounded = &vars["content"];
                assert!(
                    bounded.encode_utf16().count() <= budget,
                    "budget {budget} hashes {count} tail {tail_len} got {}",
                    bounded.encode_utf16().count()
                );
                assert_whole_digests(bounded);
            }
        }
    }
}

async fn discord_status_proxy(
    status: u16,
    code: isize,
) -> (
    serenity::http::Http,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            loop {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let body = format!(r#"{{"code":{code},"message":"fixture"}}"#);
            let response = format!(
                "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{address}"))
        .application_id(serenity::model::id::ApplicationId::new(1))
        .ratelimiter_disabled(true)
        .build();
    (http, calls, server)
}

struct ProxyLogSink {
    http: serenity::http::Http,
}

#[serenity::async_trait]
impl LogDelivery for ProxyLogSink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        channel.send_message(&self.http, message).await?;
        Ok(())
    }
}

#[tokio::test]
async fn permanent_log_channel_failure_skips_second_delivery_attempt() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    let (http, calls, server) = discord_status_proxy(404, 10003).await;
    let sink = ProxyLogSink { http };
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new(),
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new(),
        )
        .await,
        Delivery::Blocked
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.app.invalidate_configs();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new(),
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn transient_log_channel_failure_is_retried_on_next_event() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    let (http, calls, server) = discord_status_proxy(503, 0).await;
    let sink = ProxyLogSink { http };
    for _ in 0..2 {
        assert_eq!(
            emit_to(
                &f.app,
                &sink,
                GuildId::new(789),
                "message_delete",
                BTreeMap::new(),
            )
            .await,
            Delivery::Failed
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

async fn fixture_discord_http_error(status: u16, code: isize) -> serenity::Error {
    let (http, _, server) = discord_status_proxy(status, code).await;
    let err = ChannelId::new(1)
        .send_message(&http, serenity::builder::CreateMessage::new().content("x"))
        .await
        .unwrap_err();
    server.abort();
    err
}

#[test]
fn incomplete_bulk_fallback_without_rest_error_counts_delivery_failure() {
    let f = Fixture::new();
    let target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: 1,
        channel: 2,
    };
    let recovery = f
        .app
        .remembered_discord_targets
        .capture_recovery_epoch(target);
    assert_eq!(
        f.app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    settle_bulk_channel_delivery(&f.app, recovery, target, false, None);
    assert_eq!(
        f.app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn bulk_channel_delivery_forget_after_successful_fallback_clears_remembered() {
    let path = std::env::temp_dir().join(format!(
        "jmg-bulk-forget-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    let target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: 1,
        channel: 2,
    };
    let now = std::time::Instant::now();
    app.remembered_discord_targets
        .remember_permanent_for_test(target, now);
    assert!(app.remembered_discord_targets.blocked(target, now));
    let recovery = app
        .remembered_discord_targets
        .capture_recovery_epoch(target);
    settle_bulk_channel_delivery(&app, recovery, target, true, None);
    assert!(!app.remembered_discord_targets.blocked(target, now));
    drop(app);
    let _ = std::fs::remove_dir_all(&path);
}

#[tokio::test]
async fn bulk_channel_delivery_terminal_fallback_failure_is_remembered() {
    let path = std::env::temp_dir().join(format!(
        "jmg-bulk-remember-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    let target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: 3,
        channel: 4,
    };
    let now = std::time::Instant::now();
    let error = fixture_discord_http_error(403, 50013).await;
    let recovery = app
        .remembered_discord_targets
        .capture_recovery_epoch(target);
    settle_bulk_channel_delivery(&app, recovery, target, false, Some(&error));
    assert!(app.remembered_discord_targets.blocked(target, now));
    drop(app);
    let _ = std::fs::remove_dir_all(&path);
}

#[tokio::test]
async fn bulk_channel_delivery_clear_guild_unblocks_before_next_attempt() {
    let path = std::env::temp_dir().join(format!(
        "jmg-bulk-clear-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    let target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: 7,
        channel: 8,
    };
    let now = std::time::Instant::now();
    let error = fixture_discord_http_error(403, 50013).await;
    let recovery = app
        .remembered_discord_targets
        .capture_recovery_epoch(target);
    settle_bulk_channel_delivery(&app, recovery, target, false, Some(&error));
    assert!(app.remembered_discord_targets.blocked(target, now));
    app.remembered_discord_targets.clear_guild(7);
    assert!(!app.remembered_discord_targets.blocked(target, now));
    drop(app);
    let _ = std::fs::remove_dir_all(&path);
}

const CREATED_MESSAGE_JSON: &str = r#"{"id":"1","channel_id":"456","author":{"id":"1","username":"fixture","discriminator":"0"},"content":"","timestamp":"2016-01-01T00:00:00.000000+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"mention_channels":[],"attachments":[],"embeds":[],"pinned":false,"type":0}"#;

async fn discord_bulk_attachment_forbidden_proxy() -> (
    serenity::http::Http,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            loop {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let attachment = request
                .windows("deleted-messages.txt".len())
                .any(|w| w == b"deleted-messages.txt");
            let (status, body) = if attachment {
                ("403 Forbidden", r#"{"code":50013,"message":"fixture"}"#)
            } else {
                ("200 OK", CREATED_MESSAGE_JSON)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{address}"))
        .application_id(serenity::model::id::ApplicationId::new(1))
        .ratelimiter_disabled(true)
        .build();
    (http, calls, server)
}

#[tokio::test]
async fn bulk_text_fallback_delivers_every_part_after_attachment_forbidden() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let f = Fixture::with_timeout(30);
    let mut transcript = BulkTranscript::new(200);
    for id in 1..=200 {
        transcript.push(MessageId::new(id), None);
    }
    let display = BTreeMap::from([
        ("channel".into(), "<#456>".into()),
        ("details".into(), "transcript attached".into()),
    ]);
    let parts = fallback_bulk_parts("{details}", &display, &transcript);
    assert!(parts.len() > 1);
    let (http, calls, server) = discord_bulk_attachment_forbidden_proxy().await;
    let destination = ChannelId::new(456);
    let rest_target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: 789,
        channel: 456,
    };
    let recovery = f
        .app
        .remembered_discord_targets
        .capture_recovery_epoch(rest_target);
    let started = Instant::now();
    let fallback = deliver_bulk_text_fallback_parts(
        &f.app,
        &http,
        destination,
        rest_target,
        "{details}",
        &display,
        &transcript,
        Duration::from_secs(30),
        started,
        recovery,
        None,
    )
    .await;
    assert!(
        fallback.terminal_error.is_none(),
        "unexpected discord error: {:?}",
        fallback.terminal_error
    );
    assert_eq!(
        fallback.outcome,
        BulkDeliveryOutcome::Delivered,
        "expected every fallback part to succeed"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        parts.len(),
        "every text fallback part must reach Discord"
    );
    server.abort();
}

#[derive(Clone, Copy)]
pub(super) enum ReplayAttachmentProxy {
    /// 403 when the body names the transcript file; 200 for plain text.
    Forbidden,
    /// Read each request, then leave the connection open.
    Hang,
    /// Definite non-permission failure. No plain-text retry.
    ServerError,
}

pub(super) async fn read_proxied_request(stream: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    let header_end = loop {
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            return None;
        }
        request.extend_from_slice(&buf[..n]);
        if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
        if request.len() > 64 * 1024 {
            return None;
        }
    };
    let header = String::from_utf8_lossy(&request[..header_end]);
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse::<usize>().ok()
        })
        .unwrap_or(0);
    while request.len() < header_end + length && request.len() < 2 * 1024 * 1024 {
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    Some(request)
}

async fn write_proxy_reply(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

pub(super) fn request_log_payload(request: &str) -> serde_json::Value {
    let body = request.split_once("\r\n\r\n").unwrap().1;
    if let Some((_, json_part)) = body.split_once("name=\"payload_json\"") {
        let json = json_part
            .split_once("\r\n\r\n")
            .unwrap()
            .1
            .split("\r\n--")
            .next()
            .unwrap();
        serde_json::from_str(json).unwrap()
    } else {
        serde_json::from_str(body).unwrap()
    }
}

fn request_names_transcript(request: &[u8]) -> bool {
    request
        .windows("deleted-messages.txt".len())
        .any(|window| window == b"deleted-messages.txt")
}

pub(super) async fn replay_attachment_proxy(
    mode: ReplayAttachmentProxy,
) -> (
    serenity::http::Http,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    let (http, requests, server, _) = replay_attachment_proxy_signalling(mode).await;
    (http, requests, server)
}

/// Also returns a signal that stores a permit for each received request.
async fn replay_attachment_proxy_signalling(
    mode: ReplayAttachmentProxy,
) -> (
    serenity::http::Http,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
    Arc<tokio::sync::Notify>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let saved = Arc::clone(&requests);
    let received = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&received);
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let saved = Arc::clone(&saved);
            let signal = Arc::clone(&signal);
            tokio::spawn(async move {
                let Some(request) = read_proxied_request(&mut stream).await else {
                    return;
                };
                let attachment = request_names_transcript(&request);
                saved.lock().unwrap().push(request);
                signal.notify_one();
                match mode {
                    ReplayAttachmentProxy::Hang => std::future::pending::<()>().await,
                    ReplayAttachmentProxy::ServerError => {
                        write_proxy_reply(
                            &mut stream,
                            500,
                            "Error",
                            r#"{"code":0,"message":"fixture"}"#,
                        )
                        .await;
                    }
                    ReplayAttachmentProxy::Forbidden if attachment => {
                        write_proxy_reply(
                            &mut stream,
                            403,
                            "Forbidden",
                            r#"{"code":50013,"message":"fixture"}"#,
                        )
                        .await;
                    }
                    ReplayAttachmentProxy::Forbidden => {
                        write_proxy_reply(&mut stream, 200, "OK", CREATED_MESSAGE_JSON).await;
                    }
                }
            });
        }
    });
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{address}"))
        .application_id(serenity::model::id::ApplicationId::new(1))
        .ratelimiter_disabled(true)
        .build();
    (http, requests, server, received)
}

pub(super) fn replay_snapshot(content: &str) -> MessageSnapshot {
    MessageSnapshot {
        guild_id: 789,
        channel_id: 456,
        message_id: 123,
        fields: crate::db::guild_data::SnapshotFields {
            content: Some(content.into()),
            author_id: Some(42),
            ..Default::default()
        },
        created_at_ms: 1,
        first_observed_at_ms: 1,
        last_observed_at_ms: 1,
        edit_at_ms: None,
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 86_400_000,
        generation: 1,
        coverage_epoch: 1,
        continuous: false,
        content_supplied: true,
        attachments_supplied: false,
    }
}

async fn prepare_replayed_automod_bulk(f: &Fixture, filter: Option<&str>) {
    db::upsert_log_channel(&f.app.db, 789, "automod_delete", 901)
        .await
        .unwrap();
    db::upsert_log_channel(&f.app.db, 789, "message_bulk_delete", 903)
        .await
        .unwrap();
    if let Some(data) = filter {
        let data = data.to_string();
        f.app
            .db
            .with_conn(move |conn| {
                conn.execute_bounded(
                    "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'message_bulk_delete',?1)",
                    db::params![data],
                    65_536,
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    f.app.invalidate_configs();
}

async fn replay_failed_automod_bulk(f: &Fixture, sink: &impl LogDelivery) {
    let attempt = f
        .app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    let retained = replay_snapshot("bulk fallback body");
    assert!(
        observed_delete_report(
            &f.app,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            Some(&retained),
        )
        .is_none()
    );
    let removal = f
        .app
        .automod_delete_marks
        .remove_unconfirmed(789, 456, 123, attempt);
    assert_eq!(
        removal.action,
        crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
    );
    replay_unconfirmed_plain_delete(
        &f.app,
        sink,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        &removal,
    )
    .await;
}

fn request_text(requests: &[Vec<u8>]) -> Vec<String> {
    requests
        .iter()
        .map(|request| String::from_utf8_lossy(request).into_owned())
        .collect()
}

#[tokio::test]
async fn replayed_bulk_attachment_forbidden_delivers_plain_text() {
    use std::sync::atomic::Ordering;
    let f = Fixture::with_timeout(30);
    prepare_replayed_automod_bulk(&f, None).await;
    let (http, requests, server) = replay_attachment_proxy(ReplayAttachmentProxy::Forbidden).await;
    let before = f.app.log_delivery_failures.load(Ordering::Relaxed);
    replay_failed_automod_bulk(&f, &http).await;
    let seen = request_text(&requests.lock().unwrap());
    assert_eq!(
        seen.len(),
        2,
        "attachment attempt plus one text part: {seen:?}"
    );
    assert!(
        request_names_transcript(seen[0].as_bytes()),
        "first send must attach the transcript: {}",
        seen[0]
    );
    assert!(
        seen[0].contains("/channels/903/"),
        "bulk destination: {}",
        seen[0]
    );
    assert!(
        !request_names_transcript(seen[1].as_bytes()),
        "text fallback must not attach again: {}",
        seen[1]
    );
    assert!(
        seen[1].contains("Discord rejected the transcript attachment (HTTP 403)")
            && seen[1].contains("bulk fallback body"),
        "plain delivery: {}",
        seen[1]
    );
    assert!(
        seen[1].contains("/channels/903/"),
        "fallback stays on the bulk channel: {}",
        seen[1]
    );
    assert_eq!(
        f.app.log_delivery_failures.load(Ordering::Relaxed),
        before,
        "a delivered text fallback is not a failed log"
    );
    for (index, request) in seen.iter().enumerate() {
        let payload = request_log_payload(request);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["color"], jmodguard::log_embeds::RED);
        assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
        assert!(payload.get("content").is_none());
        assert!(payload.get("flags").is_none());
        if index == 0 {
            assert_eq!(embed["title"], "🗑️ Messages deleted");
            assert_eq!(embed["fields"][0]["name"], "📍 Channel");
            assert_eq!(embed["fields"][0]["value"], "<#456>");
            assert!(
                embed["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["name"] == "ℹ️ Details"
                        && f["value"].as_str().unwrap() == "last observed: bulk fallback body")
            );
        } else {
            // Transcript chunks are description text, not the shipped field layout.
            assert_eq!(embed["title"], "Messages deleted");
            assert!(
                embed["description"]
                    .as_str()
                    .unwrap()
                    .contains("bulk fallback body")
            );
            assert!(
                embed["description"]
                    .as_str()
                    .unwrap()
                    .encode_utf16()
                    .count()
                    <= 4096
            );
        }
    }
    server.abort();
}

#[tokio::test]
async fn replayed_filtered_bulk_attachment_fallback_omits_retained_text() {
    let f = Fixture::with_timeout(30);
    prepare_replayed_automod_bulk(&f, Some(r#"{"users":["42"],"channels":[]}"#)).await;
    let (http, requests, server) = replay_attachment_proxy(ReplayAttachmentProxy::Forbidden).await;
    replay_failed_automod_bulk(&f, &http).await;
    let seen = request_text(&requests.lock().unwrap());
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen.iter()
            .all(|request| !request.contains("bulk fallback body")),
        "filtered fallback exposed retained text: {seen:?}"
    );
    assert!(
        seen[1].contains("excluded by user filter")
            || seen[1].contains("Discord rejected the transcript attachment (HTTP 403)"),
        "{seen:?}"
    );
    server.abort();
}

#[tokio::test]
async fn replayed_bulk_attachment_timeout_or_cancel_or_server_error_is_not_resent() {
    use std::sync::atomic::Ordering;
    let f = Fixture::with_timeout(1);
    prepare_replayed_automod_bulk(&f, None).await;
    let (http, requests, server) = replay_attachment_proxy(ReplayAttachmentProxy::Hang).await;
    let before = f.app.log_delivery_failures.load(Ordering::Relaxed);
    replay_failed_automod_bulk(&f, &http).await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(request_names_transcript(&requests.lock().unwrap()[0]));
    assert!(f.app.log_delivery_failures.load(Ordering::Relaxed) > before);
    server.abort();

    let f = Fixture::with_timeout(30);
    prepare_replayed_automod_bulk(&f, None).await;
    let (http, requests, server, received) =
        replay_attachment_proxy_signalling(ReplayAttachmentProxy::Hang).await;
    let app = Arc::clone(&f.app);
    let watcher = tokio::spawn(async move {
        received.notified().await;
        app.activation.cut_off();
    });
    replay_failed_automod_bulk(&f, &http).await;
    watcher.abort();
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "cancellation must not send the plain-text fallback"
    );
    server.abort();

    let f = Fixture::with_timeout(30);
    prepare_replayed_automod_bulk(&f, None).await;
    let (http, requests, server) =
        replay_attachment_proxy(ReplayAttachmentProxy::ServerError).await;
    replay_failed_automod_bulk(&f, &http).await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn ambiguous_bulk_timeout_does_not_release_a_deferred_replay() {
    async fn prepare_bulk_only(f: &Fixture) -> crate::automod_delete_marks::AutomodDeleteAttempt {
        db::upsert_log_channel(&f.app.db, 789, "message_bulk_delete", 903)
            .await
            .unwrap();
        f.app.invalidate_configs();
        let attempt = f
            .app
            .automod_delete_marks
            .insert(789, 456, 123, "badword", "Bad words");
        let retained = replay_snapshot("bulk fallback body");
        let placement = place_bulk_delete(
            &f.app,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            Some(&retained),
            true,
        );
        assert!(placement.reserved_pending);
        attempt
    }

    let f = Fixture::with_timeout(1);
    let attempt = prepare_bulk_only(&f).await;
    let (http, requests, server) = replay_attachment_proxy(ReplayAttachmentProxy::Hang).await;
    let sink = Sink::default();
    let removal = f
        .app
        .automod_delete_marks
        .remove_unconfirmed(789, 456, 123, attempt);
    assert!(removal.bulk_inflight);
    replay_unconfirmed_plain_delete(
        &f.app,
        &sink,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        &removal,
    )
    .await;
    assert!(sink.messages.lock().unwrap().is_empty());
    let retained = replay_snapshot("bulk fallback body");
    let mut transcript = BulkTranscript::new(1);
    transcript.keep_pending_automod(MessageId::new(123));
    transcript.push(MessageId::new(123), Some(&retained));
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        transcript,
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Ambiguous);
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "the timed-out attempt still handed the transcript to Discord"
    );
    settle_bulk_delivery(
        &f.app,
        &sink,
        GuildId::new(789),
        ChannelId::new(456),
        &[MessageId::new(123)],
        outcome,
    )
    .await;
    assert!(
        sink.messages.lock().unwrap().is_empty(),
        "ambiguous timeout must not resend the deferred replay: {:?}",
        sink.messages.lock().unwrap()
    );
    server.abort();

    let f = Fixture::with_timeout(30);
    let attempt = prepare_bulk_only(&f).await;
    let (http, requests, server) =
        replay_attachment_proxy(ReplayAttachmentProxy::ServerError).await;
    let sink = Sink::default();
    let removal = f
        .app
        .automod_delete_marks
        .remove_unconfirmed(789, 456, 123, attempt);
    replay_unconfirmed_plain_delete(
        &f.app,
        &sink,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        &removal,
    )
    .await;
    assert!(sink.messages.lock().unwrap().is_empty());
    let retained = replay_snapshot("bulk fallback body");
    let mut transcript = BulkTranscript::new(1);
    transcript.keep_pending_automod(MessageId::new(123));
    transcript.push(MessageId::new(123), Some(&retained));
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        transcript,
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Failed);
    assert_eq!(requests.lock().unwrap().len(), 1);
    settle_bulk_delivery(
        &f.app,
        &sink,
        GuildId::new(789),
        ChannelId::new(456),
        &[MessageId::new(123)],
        outcome,
    )
    .await;
    let bodies: Vec<String> = sink
        .messages
        .lock()
        .unwrap()
        .iter()
        .map(|(channel, body)| format!("{channel} {body}"))
        .collect();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(
        bodies[0].contains("last observed: bulk fallback body"),
        "a definite bulk failure still releases one replay: {bodies:?}"
    );
    server.abort();
}

enum SequencedProxyReply {
    Status {
        status: u16,
        body: &'static str,
    },
    /// HTTP 200 with a declared length the body does not satisfy.
    TruncatedSuccess,
}

async fn sequenced_bulk_proxy(
    replies: Vec<SequencedProxyReply>,
) -> (
    serenity::http::Http,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let saved = Arc::clone(&requests);
    let replies = Arc::new(Mutex::new(replies));
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let replies = Arc::clone(&replies);
            let counter = Arc::clone(&counter);
            let saved = Arc::clone(&saved);
            tokio::spawn(async move {
                let Some(request) = read_proxied_request(&mut stream).await else {
                    return;
                };
                saved.lock().unwrap().push(request);
                counter.fetch_add(1, Ordering::SeqCst);
                let reply = {
                    let mut pending = replies.lock().unwrap();
                    if pending.is_empty() {
                        SequencedProxyReply::Status {
                            status: 500,
                            body: r#"{"code":0,"message":"unexpected"}"#,
                        }
                    } else {
                        pending.remove(0)
                    }
                };
                let bytes = match reply {
                    SequencedProxyReply::Status { status, body } => format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .into_bytes(),
                    SequencedProxyReply::TruncatedSuccess => {
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n{"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&bytes).await;
            });
        }
    });
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{address}"))
        .application_id(serenity::model::id::ApplicationId::new(1))
        .ratelimiter_disabled(true)
        .build();
    (http, requests, calls, server)
}

async fn defer_inflight_bulk_only(f: &Fixture) -> Sink {
    db::upsert_log_channel(&f.app.db, 789, "message_bulk_delete", 903)
        .await
        .unwrap();
    f.app.invalidate_configs();
    let attempt = f
        .app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    let retained = replay_snapshot("bulk fallback body");
    let placement = place_bulk_delete(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        Some(&retained),
        true,
    );
    assert!(placement.reserved_pending);
    let removal = f
        .app
        .automod_delete_marks
        .remove_unconfirmed(789, 456, 123, attempt);
    assert!(removal.bulk_inflight);
    let sink = Sink::default();
    replay_unconfirmed_plain_delete(
        &f.app,
        &sink,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        &removal,
    )
    .await;
    assert!(sink.messages.lock().unwrap().is_empty());
    sink
}

fn pending_bulk_transcript(messages: u64) -> BulkTranscript {
    let mut transcript = BulkTranscript::new(messages as usize);
    let retained = replay_snapshot("bulk fallback body");
    transcript.keep_pending_automod(MessageId::new(123));
    transcript.push(MessageId::new(123), Some(&retained));
    for id in 2..=messages {
        transcript.push(MessageId::new(id), None);
    }
    transcript
}

async fn settle_pending_bulk(f: &Fixture, sink: &Sink, outcome: BulkDeliveryOutcome) {
    settle_bulk_delivery(
        &f.app,
        sink,
        GuildId::new(789),
        ChannelId::new(456),
        &[MessageId::new(123)],
        outcome,
    )
    .await;
}

#[tokio::test]
async fn truncated_success_body_stays_ambiguous_for_attachment_fallback_and_count() {
    let rejection = anyhow::Error::from(serenity::Error::Json(
        serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
    ));
    assert!(!bulk_send_definitely_rejected(&rejection));
    assert!(!bulk_send_definitely_rejected(&anyhow::anyhow!(
        "transport"
    )));
    let http_error = fixture_discord_http_error(500, 0).await;
    assert!(bulk_send_definitely_rejected(&anyhow::Error::from(
        http_error
    )));

    let f = Fixture::with_timeout(30);
    let sink = defer_inflight_bulk_only(&f).await;
    let (http, requests, calls, server) =
        sequenced_bulk_proxy(vec![SequencedProxyReply::TruncatedSuccess]).await;
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        pending_bulk_transcript(1),
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Ambiguous);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(request_names_transcript(&requests.lock().unwrap()[0]));
    settle_pending_bulk(&f, &sink, outcome).await;
    assert!(
        sink.messages.lock().unwrap().is_empty(),
        "a truncated success released the deferred replay: {:?}",
        sink.messages.lock().unwrap()
    );
    server.abort();

    let f = Fixture::with_timeout(30);
    let sink = defer_inflight_bulk_only(&f).await;
    let parts = fallback_bulk_parts(
        "{details}",
        &BTreeMap::from([
            ("channel".into(), "<#456>".into()),
            ("details".into(), "transcript attached".into()),
        ]),
        &pending_bulk_transcript(200),
    );
    assert!(parts.len() > 1, "the fixture must have a second text part");
    let (http, _requests, calls, server) = sequenced_bulk_proxy(vec![
        SequencedProxyReply::Status {
            status: 403,
            body: r#"{"code":50013,"message":"fixture"}"#,
        },
        SequencedProxyReply::TruncatedSuccess,
    ])
    .await;
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        pending_bulk_transcript(200),
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Ambiguous);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the first text part's read error must not send another part"
    );
    settle_pending_bulk(&f, &sink, outcome).await;
    assert!(sink.messages.lock().unwrap().is_empty());
    server.abort();

    let f = Fixture::with_timeout(30);
    let data = r#"{"users":["42"],"channels":[]}"#.to_string();
    f.app
        .db
        .with_conn(move |conn| {
            conn.execute_bounded(
                "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'message_bulk_delete',?1)",
                db::params![data],
                65_536,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let sink = defer_inflight_bulk_only(&f).await;
    let (http, requests, calls, server) =
        sequenced_bulk_proxy(vec![SequencedProxyReply::TruncatedSuccess]).await;
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        pending_bulk_transcript(1),
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Ambiguous);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        !request_names_transcript(&requests.lock().unwrap()[0]),
        "the filtered send attached the transcript"
    );
    settle_pending_bulk(&f, &sink, outcome).await;
    assert!(sink.messages.lock().unwrap().is_empty());
    server.abort();

    let f = Fixture::with_timeout(30);
    let sink = defer_inflight_bulk_only(&f).await;
    let (http, _requests, calls, server) = sequenced_bulk_proxy(vec![
        SequencedProxyReply::Status {
            status: 403,
            body: r#"{"code":50013,"message":"fixture"}"#,
        },
        SequencedProxyReply::Status {
            status: 500,
            body: r#"{"code":0,"message":"fixture"}"#,
        },
    ])
    .await;
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        pending_bulk_transcript(200),
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Failed);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    settle_pending_bulk(&f, &sink, outcome).await;
    let bodies: Vec<String> = sink
        .messages
        .lock()
        .unwrap()
        .iter()
        .map(|(channel, body)| format!("{channel} {body}"))
        .collect();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(bodies[0].contains("last observed: bulk fallback body"));
    server.abort();
}

#[tokio::test]
async fn partly_sent_bulk_text_fallback_stays_ambiguous_after_rejection() {
    let f = Fixture::with_timeout(30);
    let sink = defer_inflight_bulk_only(&f).await;
    let (http, requests, calls, server) = sequenced_bulk_proxy(vec![
        SequencedProxyReply::Status {
            status: 403,
            body: r#"{"code":50013,"message":"fixture"}"#,
        },
        SequencedProxyReply::Status {
            status: 200,
            body: CREATED_MESSAGE_JSON,
        },
        SequencedProxyReply::Status {
            status: 500,
            body: r#"{"code":0,"message":"fixture"}"#,
        },
    ])
    .await;
    let outcome = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        pending_bulk_transcript(200),
    )
    .await;
    assert_eq!(outcome, BulkDeliveryOutcome::Ambiguous);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    {
        let requests = requests.lock().unwrap();
        assert!(request_names_transcript(&requests[0]));
        assert!(
            requests[1..]
                .iter()
                .all(|request| !request_names_transcript(request))
        );
    }
    settle_pending_bulk(&f, &sink, outcome).await;
    assert!(sink.messages.lock().unwrap().is_empty());
    server.abort();
}

pub(super) struct ScriptedHttpReply {
    pub(super) status: u16,
    pub(super) headers: &'static str,
    pub(super) body: &'static str,
}

pub(super) async fn scripted_discord_proxy(
    replies: Vec<ScriptedHttpReply>,
) -> (
    serenity::http::Http,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let replies = Arc::new(Mutex::new(replies));
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut header_end = None;
            loop {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    break;
                }
            }
            let Some(header_end) = header_end else {
                break;
            };
            let header = String::from_utf8_lossy(&request[..header_end]);
            let length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if !name.eq_ignore_ascii_case("content-length") {
                        return None;
                    }
                    value.trim().parse::<usize>().ok()
                })
                .unwrap_or(0);
            while request.len() < header_end + length {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = {
                let mut pending = replies.lock().unwrap();
                if pending.is_empty() {
                    ScriptedHttpReply {
                        status: 500,
                        headers: "",
                        body: "unexpected",
                    }
                } else {
                    pending.remove(0)
                }
            };
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n{headers}\r\n{body}",
                status = reply.status,
                len = reply.body.len(),
                headers = reply.headers,
                body = reply.body,
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{address}"))
        .application_id(serenity::model::id::ApplicationId::new(1))
        .build();
    (http, calls, server)
}

#[tokio::test]
async fn discord_rest_counts_logical_requests_across_delivery_paths() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Use the process subscriber and exact counter deltas without parallel test traffic.
    const CHILD: &str = "JMODGUARD_TEST_REST_COUNTS";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logging::tests::discord_rest_counts_logical_requests_across_delivery_paths",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    jmodguard::init_tracing(
        &jmodguard::LogConfig {
            directory: Default::default(),
            ..Default::default()
        },
        "bot",
    );
    let limited = || ScriptedHttpReply {
        status: 429,
        headers: "retry-after: 0\r\n",
        body: "{}",
    };
    let ok = || ScriptedHttpReply {
        status: 200,
        headers: "",
        body: "{}",
    };
    let (mut http, calls, server) =
        scripted_discord_proxy(vec![limited(), ok(), limited(), ok()]).await;
    let channel = ChannelId::new(456);
    let send = |current| {
        crate::discord::send_discord_log_after_rate_limit(
            &http,
            channel,
            CreateMessage::new().content("fixture"),
            None,
            current,
            &crate::discord::ignore_global_rate_limit,
            None,
        )
    };
    let counter = || jmodguard::resources::peek_counter("discord_rest");
    assert!(send(&|| true).await.unwrap());
    assert_eq!((calls.load(Ordering::SeqCst), counter()), (2, 1));

    let current = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&current);
    let hook = Arc::new(move || flag.store(false, Ordering::SeqCst));
    let still_current = || current.load(Ordering::SeqCst);
    assert!(
        !crate::discord::with_rate_limit_wait_hook(hook, send(&still_current))
            .await
            .unwrap()
    );
    assert_eq!((calls.load(Ordering::SeqCst), counter()), (3, 2));

    // A normal Serenity request is counted by its span.
    http.ratelimiter = None;
    http.request(serenity::http::Request::new(
        serenity::http::Route::Channel {
            channel_id: channel,
        },
        serenity::http::LightMethod::Get,
    ))
    .await
    .unwrap();
    assert_eq!((calls.load(Ordering::SeqCst), counter()), (4, 3));
    server.abort();
}

#[tokio::test]
async fn stale_incomplete_notice_is_dropped_after_preemptive_rate_limit_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::with_timeout(5);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 200,
            headers: "x-ratelimit-limit: 1\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset-after: 30\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "x-ratelimit-limit: 1\r\nx-ratelimit-remaining: 1\r\nx-ratelimit-reset-after: 30\r\n",
            body: "{}",
        },
    ])
    .await;
    let ignore_rl = crate::discord::ignore_global_rate_limit;
    assert!(
        crate::discord::send_discord_log_after_rate_limit(
            &http,
            ChannelId::new(456),
            CreateMessage::new().content("seed"),
            None,
            &|| true,
            &ignore_rl,
            None,
        )
        .await
        .unwrap()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let superseded = Arc::new(AtomicBool::new(false));
    let hook_flag = Arc::clone(&superseded);
    let commit_flag = Arc::clone(&superseded);
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let hook: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || hook_flag.store(true, Ordering::SeqCst));
    let sent = crate::discord::with_rate_limit_wait_hook(hook, async {
        crate::discord::send_discord_log_after_rate_limit(
            &http,
            ChannelId::new(456),
            CreateMessage::new().content("stale"),
            None,
            &move || !commit_flag.load(Ordering::SeqCst),
            &ignore_rl,
            None,
        )
        .await
    })
    .await;
    let elapsed = started.elapsed();
    assert!(matches!(sent, Ok(false)));
    assert!(
        superseded.load(Ordering::SeqCst),
        "the reservation must change during the rate-limit wait"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a stale notice must not be written after the wait"
    );
    assert!(
        elapsed >= Duration::from_secs(25),
        "preemptive wait was too short: {elapsed:?}"
    );
    assert_eq!(
        f.app.member_sync_rate_limit_until_ms.load(Ordering::SeqCst),
        0
    );
    server.abort();
}

#[tokio::test]
async fn stale_incomplete_notice_is_not_retried_after_a_429_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::with_timeout(5);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "retry-after: 2\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    let superseded = Arc::new(AtomicBool::new(false));
    let hook_flag = Arc::clone(&superseded);
    let commit_flag = Arc::clone(&superseded);
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let hook: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || hook_flag.store(true, Ordering::SeqCst));
    let ignore_rl = crate::discord::ignore_global_rate_limit;
    let sent = crate::discord::with_rate_limit_wait_hook(hook, async {
        crate::discord::send_discord_log_after_rate_limit(
            &http,
            ChannelId::new(456),
            CreateMessage::new().content("stale"),
            None,
            &move || !commit_flag.load(Ordering::SeqCst),
            &ignore_rl,
            None,
        )
        .await
    })
    .await;
    let elapsed = started.elapsed();
    assert!(matches!(sent, Ok(false)));
    assert!(superseded.load(Ordering::SeqCst));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the 429 request already left; the retry must not"
    );
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(3),
        "429 wait duration unexpected: {elapsed:?}"
    );
    assert_eq!(
        f.app.member_sync_rate_limit_until_ms.load(Ordering::SeqCst),
        0
    );
    server.abort();
}

#[tokio::test]
async fn conditional_permanent_403_50013_suppresses_the_second_http_request() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 403,
            headers: "",
            body: r#"{"code":50013,"message":"fixture"}"#,
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Blocked
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn conditional_permanent_404_unknown_channel_suppresses_the_second_http_request() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 404,
            headers: "",
            body: r#"{"code":10003,"message":"Unknown Channel"}"#,
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Blocked
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn conditional_403_without_destination_code_is_not_remembered() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 403,
            headers: "",
            body: r#"{"code":0,"message":"fixture"}"#,
        },
        ScriptedHttpReply {
            status: 403,
            headers: "",
            body: r#"{"code":0,"message":"fixture"}"#,
        },
    ])
    .await;
    for _ in 0..2 {
        assert_eq!(
            emit_to_if_after_rate_limit(
                &f.app,
                &http,
                GuildId::new(789),
                "automod",
                BTreeMap::new(),
                || true,
            )
            .await,
            Delivery::Failed
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn conditional_transient_5xx_and_429_are_not_remembered() {
    use std::sync::atomic::Ordering;
    let target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: 789,
        channel: 456,
    };
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 503,
            headers: "",
            body: r#"{"code":0,"message":"fixture"}"#,
        },
        ScriptedHttpReply {
            status: 503,
            headers: "",
            body: r#"{"code":0,"message":"fixture"}"#,
        },
    ])
    .await;
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Failed
    );
    assert!(
        !f.app
            .remembered_discord_targets
            .blocked(target, std::time::Instant::now())
    );
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();

    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "",
            body: r#"{"code":0,"message":"fixture"}"#,
        },
        ScriptedHttpReply {
            status: 429,
            headers: "",
            body: r#"{"code":0,"message":"fixture"}"#,
        },
    ])
    .await;
    for _ in 0..2 {
        assert_eq!(
            emit_to_if_after_rate_limit(
                &f.app,
                &http,
                GuildId::new(789),
                "automod",
                BTreeMap::new(),
                || true,
            )
            .await,
            Delivery::Failed
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        !f.app
            .remembered_discord_targets
            .blocked(target, std::time::Instant::now())
    );
    server.abort();
}

async fn wait_member_sync_rate_limit_persisted(app: &BotState, timeout: Duration) {
    use std::sync::atomic::Ordering;
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
            == app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("member-sync pacing epoch did not persist within {timeout:?}");
}

fn hold_sqlite_writer_lock(
    path: &std::path::Path,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let release = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let db_path = path.to_path_buf();
    let release_flag = Arc::clone(&release);
    let ready_flag = Arc::clone(&ready);
    let handle = std::thread::spawn(move || {
        let db = Db::open(&db_path, 2).expect("open pacing lock database");
        db.with_conn_sync(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            ready_flag.store(true, Ordering::SeqCst);
            while !release_flag.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            conn.execute_batch("COMMIT")?;
            Ok(())
        })
        .expect("hold pacing writer lock");
    });
    let wait_until = std::time::Instant::now() + Duration::from_secs(5);
    while !ready.load(Ordering::SeqCst) {
        if std::time::Instant::now() > wait_until {
            panic!("writer lock thread did not acquire BEGIN IMMEDIATE");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    (handle, release)
}

async fn member_sync_pacing_deadline(app: &BotState, bot_id: i64) -> Option<i64> {
    app.db
        .with_conn(move |conn| {
            let present: i64 = conn.query_row(
                "SELECT COUNT(*) FROM member_sync_pacing WHERE bot_id=?1",
                [bot_id],
                |row| Ok(row.get(0)?),
            )?;
            if present == 0 {
                return Ok(None);
            }
            conn.query_row(
                "SELECT request_after_ms FROM member_sync_pacing WHERE bot_id=?1",
                [bot_id],
                |row| Ok(row.get(0)?),
            )
            .map(Some)
        })
        .await
        .unwrap()
}

fn rate_limit_wait_log_lines(text: &str) -> usize {
    text.lines()
        .filter(|line| line.contains("rate-limit wait"))
        .count()
}

#[tokio::test]
async fn global_429_notifies_member_sync_and_retries_when_current() {
    use crate::logging::persistence_log_tests::CapturedLog;
    use std::sync::atomic::Ordering;
    use tracing::instrument::WithSubscriber;
    let f = Fixture::new();
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 0\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    let log = CapturedLog::default();
    let before = chrono::Utc::now().timestamp_millis();
    let delivery = emit_to_if_after_rate_limit(
        &f.app,
        &http,
        GuildId::new(789),
        "automod",
        BTreeMap::new(),
        || true,
    )
    .with_subscriber(log.dispatch())
    .await;
    let after = chrono::Utc::now().timestamp_millis();
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(stored >= before && stored <= after + 1);
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(stored));
    let text = log.text();
    assert_eq!(rate_limit_wait_log_lines(&text), 1);
    assert!(text.contains("Post"));
    assert!(text.contains("/channels/456/messages"));
    assert!(text.contains("global=true"));
    assert!(!text.contains("127.0.0.1"));
    server.abort();
}

#[tokio::test]
async fn global_429_notifies_member_sync_and_drops_the_retry_when_stale() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new();
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 0\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    let superseded = Arc::new(AtomicBool::new(false));
    let hook_flag = Arc::clone(&superseded);
    let commit_flag = Arc::clone(&superseded);
    let hook: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || hook_flag.store(true, Ordering::SeqCst));
    let before = chrono::Utc::now().timestamp_millis();
    let delivery = crate::discord::with_rate_limit_wait_hook(hook, async {
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            move || !commit_flag.load(Ordering::SeqCst),
        )
        .await
    })
    .await;
    let after = chrono::Utc::now().timestamp_millis();
    assert_eq!(delivery, Delivery::Dropped);
    assert!(superseded.load(Ordering::SeqCst));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(stored >= before && stored <= after + 1);
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(stored));
    server.abort();
}

#[tokio::test]
async fn global_429_malformed_retry_after_does_not_notify() {
    use crate::logging::persistence_log_tests::CapturedLog;
    use std::sync::atomic::Ordering;
    use tracing::instrument::WithSubscriber;

    async fn run_case(headers: &'static str) {
        let f = Fixture::new();
        f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
        db::upsert_log_channel(&f.app.db, 789, "automod", 456)
            .await
            .unwrap();
        let (http, calls, server) = scripted_discord_proxy(vec![ScriptedHttpReply {
            status: 429,
            headers,
            body: "{}",
        }])
        .await;
        let log = CapturedLog::default();
        let delivery = emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .with_subscriber(log.dispatch())
        .await;
        assert_eq!(delivery, Delivery::Failed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.app
                .member_sync_rate_limit_until_ms
                .load(Ordering::Relaxed),
            0
        );
        assert!(member_sync_pacing_deadline(&f.app, 42).await.is_none());
        assert_eq!(rate_limit_wait_log_lines(&log.text()), 0);
        server.abort();
    }

    run_case("x-ratelimit-global: true\r\n").await;
    run_case("x-ratelimit-global: true\r\nretry-after: nope\r\n").await;
}

#[tokio::test]
async fn route_429_does_not_arm_member_sync() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "retry-after: 0\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        f.app
            .member_sync_rate_limit_until_ms
            .load(Ordering::Relaxed),
        0
    );
    assert!(member_sync_pacing_deadline(&f.app, 42).await.is_none());
    server.abort();
}

#[tokio::test]
async fn global_429_virtual_wait_blocks_an_earlier_member_sync_claim() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 10\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 30\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    tokio::time::pause();
    let member_sync_hook = crate::discord::member_sync_global_rate_limit_hook(Arc::clone(&f.app));
    let before = chrono::Utc::now().timestamp_millis();
    let started = tokio::time::Instant::now();
    assert!(
        crate::discord::send_discord_log_after_rate_limit(
            &http,
            ChannelId::new(456),
            CreateMessage::new().content("notice"),
            None,
            &|| true,
            &member_sync_hook,
            None,
        )
        .await
        .unwrap()
    );
    let elapsed = started.elapsed();
    let after = chrono::Utc::now().timestamp_millis();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(
        elapsed >= Duration::from_secs(40) && elapsed < Duration::from_secs(41),
        "virtual wait unexpected: {elapsed:?}"
    );
    let deadline = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(deadline >= before + 30_000 && deadline <= after + 30_000);
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert!(
        crate::db::claim_member_sync_request(
            &f.app.db,
            42,
            "fixture".into(),
            deadline - 1,
            15_000,
            90_000,
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        crate::db::claim_member_sync_request(
            &f.app.db,
            42,
            "fixture".into(),
            deadline,
            15_000,
            90_000,
        )
        .await
        .unwrap()
        .is_some()
    );
    server.abort();
}

#[tokio::test]
async fn global_429_without_published_bot_id_still_retries() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 0\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        f.app
            .member_sync_rate_limit_until_ms
            .load(Ordering::Relaxed)
            > 0
    );
    assert!(member_sync_pacing_deadline(&f.app, 42).await.is_none());
    // On paused time each poll waits until the persist task is idle.
    tokio::time::pause();
    for _ in 0..50 {
        if !f
            .app
            .member_sync_rate_limit_persist_running
            .load(Ordering::SeqCst)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !f.app
            .member_sync_rate_limit_persist_running
            .load(Ordering::SeqCst)
    );
    server.abort();
}

#[tokio::test]
async fn global_429_slow_pacing_write_does_not_spend_the_log_deadline() {
    use std::sync::atomic::Ordering;
    let f = Fixture::with_timeout(1);
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (lock_thread, release) = hold_sqlite_writer_lock(&f.path);
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 0\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    let started = tokio::time::Instant::now();
    let delivery = emit_to_if_after_rate_limit(
        &f.app,
        &http,
        GuildId::new(789),
        "automod",
        BTreeMap::new(),
        || true,
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        elapsed < Duration::from_secs(2),
        "log delivery waited on the pacing write: {elapsed:?}"
    );
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(stored > 0);
    release.store(true, Ordering::SeqCst);
    lock_thread.join().expect("writer lock thread");
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(stored));
    server.abort();
}

#[tokio::test]
async fn global_429_past_the_log_deadline_still_persists_pacing() {
    use std::sync::atomic::Ordering;
    let f = Fixture::with_timeout(1);
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (lock_thread, release) = hold_sqlite_writer_lock(&f.path);
    let (http, calls, server) = scripted_discord_proxy(vec![ScriptedHttpReply {
        status: 429,
        headers: "x-ratelimit-global: true\r\nretry-after: 60\r\n",
        body: "{}",
    }])
    .await;
    tokio::time::pause();
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(stored > 0);
    assert!(
        f.app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
            > f.app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire),
        "pacing must stay dirty while the writer lock is held"
    );
    tokio::time::resume();
    release.store(true, Ordering::SeqCst);
    lock_thread.join().expect("writer lock thread");
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(stored));
    server.abort();
}

#[tokio::test]
async fn global_429_cutoff_during_retry_sleep_still_persists_pacing() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    let (lock_thread, release) = hold_sqlite_writer_lock(&f.path);
    let (http, calls, server) = scripted_discord_proxy(vec![
        ScriptedHttpReply {
            status: 429,
            headers: "x-ratelimit-global: true\r\nretry-after: 0\r\n",
            body: "{}",
        },
        ScriptedHttpReply {
            status: 200,
            headers: "",
            body: "{}",
        },
    ])
    .await;
    let app = Arc::clone(&f.app);
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || app.activation.cut_off());
    let delivery = crate::discord::with_rate_limit_wait_hook(hook, async {
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await
    })
    .await;
    assert_ne!(delivery, Delivery::Sent);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(stored > 0);
    assert!(member_sync_pacing_deadline(&f.app, 42).await.is_none());
    release.store(true, Ordering::SeqCst);
    lock_thread.join().expect("writer lock thread");
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(stored));
    server.abort();
}

#[tokio::test]
async fn global_429_stale_notice_and_failed_pacing_write_still_commits_after_retry() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::with_timeout(60);
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    f.app.publish_member_sync_bot_id(42).await;
    crate::handlers::apply_rate_limit_wait(
        &f.app,
        crate::handlers::RateLimitWait {
            timeout: Duration::from_secs(5),
            limit: 50,
            method: serenity::http::LightMethod::Get,
            path: "/guilds/1/members".to_string(),
            global: false,
        },
    )
    .await;
    let kept = member_sync_pacing_deadline(&f.app, 42).await.unwrap();
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "CREATE TRIGGER reject_pacing_extend
BEFORE UPDATE ON member_sync_pacing
BEGIN SELECT RAISE(ABORT, 'fixture pacing failure'); END;",
                65_536,
            )
        })
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![ScriptedHttpReply {
        status: 429,
        headers: "x-ratelimit-global: true\r\nretry-after: 30\r\n",
        body: "{}",
    }])
    .await;
    let superseded = Arc::new(AtomicBool::new(false));
    let hook_flag = Arc::clone(&superseded);
    let commit_flag = Arc::clone(&superseded);
    tokio::time::pause();
    let hook: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || hook_flag.store(true, Ordering::SeqCst));
    let delivery = crate::discord::with_rate_limit_wait_hook(hook, async {
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            move || !commit_flag.load(Ordering::SeqCst),
        )
        .await
    })
    .await;
    assert_eq!(delivery, Delivery::Dropped);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    assert!(stored > kept);
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(kept));
    for _ in 0..50 {
        if f.app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
            > f.app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        f.app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
            > f.app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
    );
    let stored = f
        .app
        .member_sync_rate_limit_until_ms
        .load(Ordering::Relaxed);
    f.app
        .db
        .with_conn(|conn| conn.execute_batch_bounded("DROP TRIGGER reject_pacing_extend;", 65_536))
        .await
        .unwrap();
    wait_member_sync_rate_limit_persisted(&f.app, Duration::from_secs(5)).await;
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, Some(stored));
    assert!(
        crate::db::claim_member_sync_request(
            &f.app.db,
            42,
            "fixture".into(),
            stored - 1,
            15_000,
            90_000,
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        crate::db::claim_member_sync_request(
            &f.app.db,
            42,
            "fixture".into(),
            stored,
            15_000,
            90_000,
        )
        .await
        .unwrap()
        .is_some()
    );
    server.abort();
}

#[tokio::test]
async fn global_429_failed_pacing_write_stops_after_the_deadline() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    f.app.member_sync_bot_id.store(42, Ordering::Relaxed);
    db::upsert_log_channel(&f.app.db, 789, "automod", 456)
        .await
        .unwrap();
    f.app.publish_member_sync_bot_id(42).await;
    let kept = member_sync_pacing_deadline(&f.app, 42).await;
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "CREATE TRIGGER reject_pacing_insert
BEFORE INSERT ON member_sync_pacing
BEGIN SELECT RAISE(ABORT, 'fixture pacing failure'); END;
CREATE TRIGGER reject_pacing_extend
BEFORE UPDATE ON member_sync_pacing
BEGIN SELECT RAISE(ABORT, 'fixture pacing failure'); END;",
                65_536,
            )
        })
        .await
        .unwrap();
    let (http, calls, server) = scripted_discord_proxy(vec![ScriptedHttpReply {
        status: 429,
        headers: "x-ratelimit-global: true\r\nretry-after: 0\r\n",
        body: "{}",
    }])
    .await;
    assert_eq!(
        emit_to_if_after_rate_limit(
            &f.app,
            &http,
            GuildId::new(789),
            "automod",
            BTreeMap::new(),
            || true,
        )
        .await,
        Delivery::Failed
    );
    assert!(calls.load(Ordering::SeqCst) >= 1);
    // On paused time each poll waits until the persist task is idle.
    tokio::time::pause();
    for _ in 0..50 {
        if f.app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
            > f.app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        f.app.member_sync_rate_limit_epoch.load(Ordering::Acquire)
            > f.app
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
    );
    for _ in 0..50 {
        if !f
            .app
            .member_sync_rate_limit_persist_running
            .load(Ordering::SeqCst)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !f.app
            .member_sync_rate_limit_persist_running
            .load(Ordering::SeqCst)
    );
    assert_eq!(member_sync_pacing_deadline(&f.app, 42).await, kept);
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "DROP TRIGGER reject_pacing_insert; DROP TRIGGER reject_pacing_extend;",
                65_536,
            )
        })
        .await
        .unwrap();
    server.abort();
}

#[test]
fn rejoin_note_canceled_in_flight_allows_redelivery() {
    let deliveries = RejoinNoteDeliveries::default();
    let guard = deliveries
        .begin_in_flight(9, 8, "stay-7")
        .expect("first delivery attempt must reserve the stay");
    drop(guard);
    assert!(
        deliveries.begin_attempt(9, 8, "stay-7"),
        "canceled handler must release the in-flight stay"
    );
}

#[test]
fn rejoin_note_successful_delivery_blocks_redelivery() {
    let deliveries = RejoinNoteDeliveries::default();
    let guard = deliveries
        .begin_in_flight(9, 8, "stay-7")
        .expect("delivery attempt must start");
    guard.complete(true);
    assert!(
        !deliveries.begin_attempt(9, 8, "stay-7"),
        "delivered stay must be deduped"
    );
}

#[test]
fn rejoin_note_failed_delivery_allows_redelivery() {
    let deliveries = RejoinNoteDeliveries::default();
    let guard = deliveries
        .begin_in_flight(9, 8, "stay-7")
        .expect("delivery attempt must start");
    guard.complete(false);
    assert!(
        deliveries.begin_attempt(9, 8, "stay-7"),
        "failed delivery must not mark the stay as delivered"
    );
}

#[test]
fn rejoin_note_undelivered_retry_expires() {
    use std::time::Duration;
    let deliveries = RejoinNoteDeliveries::default();
    deliveries.finish_attempt(9, 8, "stay-expire", false);
    assert!(deliveries.pending_redelivery(9, 8, "stay-expire"));
    deliveries.test_age_undelivered(
        9,
        8,
        "stay-expire",
        MEMBER_JOINS_KEEP + Duration::from_secs(1),
    );
    assert!(
        !deliveries.pending_redelivery(9, 8, "stay-expire"),
        "stale undelivered state must not retry forever"
    );
}

#[test]
fn retain_current_stay_preserves_other_members_delivery_state() {
    let deliveries = RejoinNoteDeliveries::default();
    let guard = deliveries
        .begin_in_flight(10, 77, "stay-a")
        .expect("first member reserves delivery");
    deliveries.retain_current_stay(10, 88, "stay-b");
    assert!(
        !deliveries.begin_attempt(10, 77, "stay-a"),
        "another member's refresh must not release an in-flight stay"
    );
    drop(guard);
}

#[test]
fn rejoin_delivery_burst_respects_settled_entry_cap() {
    let deliveries = RejoinNoteDeliveries::default();
    let overflow = REJOIN_NOTE_DELIVERY_LIMIT + 256;
    for index in 0..overflow {
        let stay = format!("stay-{index}");
        deliveries.finish_attempt(1, index as u64, &stay, true);
    }
    assert!(
        deliveries.test_settled_entry_count() <= REJOIN_NOTE_DELIVERY_LIMIT,
        "delivered stays must stay within the hard cap"
    );
    deliveries.finish_attempt(1, 9_999, "stay-retry", false);
    assert!(
        deliveries.pending_redelivery(1, 9_999, "stay-retry"),
        "fresh undelivered stays must survive eviction of older settled entries"
    );
}

#[test]
fn rejoin_failed_delivery_burst_respects_cap_and_queue() {
    let deliveries = RejoinNoteDeliveries::default();
    let overflow = REJOIN_NOTE_DELIVERY_LIMIT + 256;
    for index in 0..overflow {
        let stay = format!("stay-{index}");
        deliveries.finish_attempt(1, index as u64, &stay, false);
    }
    assert!(
        deliveries.test_settled_entry_count() <= REJOIN_NOTE_DELIVERY_LIMIT,
        "failed stays must stay within the hard cap"
    );
    for _ in 0..512 {
        deliveries.finish_attempt(1, 99_999, "stay-hot", false);
    }
    assert!(
        deliveries.test_settled_entry_count() <= REJOIN_NOTE_DELIVERY_LIMIT,
        "repeated retries must not grow settled entries past the cap"
    );
    assert!(
        deliveries.test_undelivered_queue_len() <= REJOIN_NOTE_DELIVERY_LIMIT,
        "undelivered ordering index must stay within the same cap as live entries"
    );
    assert!(
        deliveries.test_delivered_queue_len() <= REJOIN_NOTE_DELIVERY_LIMIT,
        "delivered ordering index must stay within the same cap as live entries"
    );
    assert!(
        deliveries.pending_redelivery(1, 99_999, "stay-hot"),
        "the newest failed stay must remain retryable in memory"
    );
}

#[test]
fn rejoin_departure_churn_drops_ordering_tokens() {
    let deliveries = RejoinNoteDeliveries::default();
    deliveries.finish_attempt(1, 1, "anchor", false);
    let churn = REJOIN_NOTE_DELIVERY_LIMIT + 256;
    for index in 0..churn {
        let stay = format!("stay-{index}");
        let user = 10_000 + index as u64;
        deliveries.finish_attempt(2, user, &stay, false);
        deliveries.clear_member(2, user);
    }
    assert_eq!(
        deliveries.test_settled_entry_count(),
        1,
        "only the anchor stay should remain"
    );
    assert!(
        deliveries.test_undelivered_queue_len() <= 1,
        "departures must remove ordering tokens, not leave them behind a live front"
    );
    assert!(deliveries.pending_redelivery(1, 1, "anchor"));
    assert_eq!(deliveries.test_delivered_queue_len(), 0);
    for index in 0..churn {
        let stay = format!("sent-{index}");
        let user = 30_000 + index as u64;
        deliveries.finish_attempt(3, user, &stay, true);
        deliveries.clear_member(3, user);
    }
    assert_eq!(
        deliveries.test_delivered_queue_len(),
        0,
        "departures must remove delivered ordering tokens too"
    );
    assert_eq!(deliveries.test_undelivered_queue_len(), 1);
}

#[test]
fn rejoin_stay_replacement_bounds_both_order_indexes() {
    let deliveries = RejoinNoteDeliveries::default();
    let churn = REJOIN_NOTE_DELIVERY_LIMIT + 256;
    for index in 0..churn {
        let failed = format!("failed-{index}");
        deliveries.finish_attempt(7, 7, &failed, false);
        deliveries.retain_current_stay(7, 7, "current-failed");
        let sent = format!("sent-{index}");
        deliveries.finish_attempt(8, 8, &sent, true);
        deliveries.retain_current_stay(8, 8, "current-sent");
    }
    deliveries.finish_attempt(7, 7, "current-failed", false);
    deliveries.finish_attempt(8, 8, "current-sent", true);
    deliveries.retain_current_stay(7, 7, "current-failed");
    deliveries.retain_current_stay(8, 8, "current-sent");
    assert_eq!(deliveries.test_settled_entry_count(), 2);
    assert_eq!(deliveries.test_undelivered_queue_len(), 1);
    assert_eq!(deliveries.test_delivered_queue_len(), 1);
    assert!(deliveries.pending_redelivery(7, 7, "current-failed"));
    assert!(
        !deliveries.begin_attempt(8, 8, "current-sent"),
        "the retained successful stay must still be deduped"
    );
}

#[test]
fn rejoin_note_leave_clears_member_state() {
    let deliveries = RejoinNoteDeliveries::default();
    deliveries.finish_attempt(9, 8, "stay-leave", false);
    deliveries.clear_member(9, 8);
    assert!(!deliveries.pending_redelivery(9, 8, "stay-leave"));
    assert!(deliveries.begin_attempt(9, 8, "stay-leave"));
}

#[tokio::test]
async fn routine_report_retries_slot_admission_and_expires_within_its_deadline() {
    use std::future::Future;
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "system", 456)
        .await
        .unwrap();
    let sink = Sink::default();
    for release in [true, false] {
        let permit = f.app.log_delivery_slots.acquire().await.unwrap();
        let mut report = Box::pin(emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "system",
            BTreeMap::new(),
        ));
        let first =
            std::future::poll_fn(|cx| std::task::Poll::Ready(report.as_mut().poll(cx))).await;
        assert!(
            first.is_pending(),
            "a busy slot should queue a routine report"
        );
        if release {
            drop(permit);
            assert_eq!(report.await, Delivery::Sent);
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(f.app.cfg.log.discord_timeout_secs + 1)).await;
            assert_eq!(report.await, Delivery::Dropped);
            tokio::time::resume();
        }
    }
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn queued_bulk_transcript_expires_before_attachment_and_text_fallback() {
    use std::future::Future;
    let f = Fixture::with_timeout(30);
    db::upsert_log_channel(&f.app.db, 789, "message_bulk_delete", 903)
        .await
        .unwrap();
    f.app.invalidate_configs();
    let mut expired = replay_snapshot("expired-bulk-secret");
    expired.expires_at_ms = chrono::Utc::now().timestamp_millis() + 1_500;
    let valid = replay_snapshot("still-valid-bulk-text");
    let mut transcript = BulkTranscript::new(2);
    transcript.push(MessageId::new(123), Some(&expired));
    transcript.push(MessageId::new(124), Some(&valid));
    let (http, requests, server) = replay_attachment_proxy(ReplayAttachmentProxy::Forbidden).await;
    let held = f.app.log_delivery_slots.acquire().await.unwrap();
    let send = emit_retained_bulk_to(
        &f.app,
        &http,
        GuildId::new(789),
        ChannelId::new(456),
        transcript,
    );
    tokio::pin!(send);
    let first = std::future::poll_fn(|cx| std::task::Poll::Ready(send.as_mut().poll(cx))).await;
    assert!(first.is_pending());
    assert_eq!(f.app.log_delivery_waiters.available_permits(), 127);
    assert!(expired.expires_at_ms > chrono::Utc::now().timestamp_millis());
    tokio::time::sleep(Duration::from_millis(1_600)).await;
    drop(held);
    let outcome = send.await;
    let seen = request_text(&requests.lock().unwrap());
    server.abort();
    let _ = server.await;
    assert_eq!(outcome, BulkDeliveryOutcome::Delivered);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(request_names_transcript(seen[0].as_bytes()));
    for request in seen {
        assert!(request.contains("Message 123"), "{request}");
        assert!(request.contains("snapshot expired"), "{request}");
        assert!(request.contains("still-valid-bulk-text"), "{request}");
        assert!(!request.contains("expired-bulk-secret"), "{request}");
    }
}

#[tokio::test]
async fn delivery_renders_custom_template_as_description_with_event_accent_and_components() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    f.app.db.with_conn(|conn| {
        conn.execute_bounded("INSERT INTO templates(guild_id,event_type,template) VALUES(789,'message_delete','My custom log: {mention} | {content}')", (), 65_536)?;
        Ok(())
    }).await.unwrap();
    f.app.invalidate_configs();
    let sink = Sink::default();
    let vars = BTreeMap::from([
        ("mention".into(), "<@42>".into()),
        ("content".into(), "kept wording".into()),
    ]);
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            vars.clone()
        )
        .await,
        Delivery::Sent
    );
    let messages = sink.messages.lock().unwrap();
    let payload = &messages[0].1;
    assert_eq!(payload["embeds"][0]["title"], "Message deleted");
    assert_eq!(
        payload["embeds"][0]["description"],
        "My custom log: <@42> | kept wording"
    );
    assert!(payload["embeds"][0].get("fields").is_none());
    assert_eq!(payload["embeds"][0]["color"], jmodguard::log_embeds::RED);
    assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
    assert!(payload.get("content").is_none());
}

const EDIT_SOURCE_CHANNEL_JSON: &str = r#"{"id":"456","guild_id":"789","type":0,"name":"general","position":0,"permission_overwrites":[]}"#;

/// A gateway channel-cache miss leaves the source as a mention to resolve.
fn uncached_edit_source_vars() -> BTreeMap<String, String> {
    message_vars(GuildId::new(789), ChannelId::new(456), MessageId::new(123))
}

fn edit_source_channel_gets(requests: &[String]) -> usize {
    requests
        .iter()
        .filter(|request| request.starts_with("GET /api/v10/channels/456 "))
        .count()
}

#[tokio::test]
async fn message_edit_template_without_channel_skips_the_source_channel_lookup() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 901)
        .await
        .unwrap();
    f.app.db.with_conn(|conn| {
        conn.execute_bounded("INSERT INTO templates(guild_id,event_type,template) VALUES(789,'message_edit','Edited: {message_url}')", (), 65_536)?;
        Ok(())
    }).await.unwrap();
    f.app.invalidate_configs();
    let (http, requests, _, server) = sequenced_bulk_proxy(vec![SequencedProxyReply::Status {
        status: 200,
        body: CREATED_MESSAGE_JSON,
    }])
    .await;
    let delivery = emit_to(
        &f.app,
        &http,
        GuildId::new(789),
        "message_edit",
        uncached_edit_source_vars(),
    )
    .await;
    server.abort();
    let seen = request_text(&requests.lock().unwrap());
    assert_eq!(edit_source_channel_gets(&seen), 0, "{seen:?}");
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].starts_with("POST /api/v10/channels/901/messages "),
        "{}",
        seen[0]
    );
    assert_eq!(
        request_log_payload(&seen[0])["embeds"][0]["description"],
        "Edited: https://discord.com/channels/789/456/123"
    );
}

#[tokio::test]
async fn message_edit_blocked_destination_skips_the_source_lookup_even_if_it_recovers() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 901)
        .await
        .unwrap();
    let (http, requests, _, server) = sequenced_bulk_proxy(vec![
        SequencedProxyReply::Status {
            status: 200,
            body: EDIT_SOURCE_CHANNEL_JSON,
        },
        SequencedProxyReply::Status {
            status: 403,
            body: r#"{"code":50013,"message":"fixture"}"#,
        },
        SequencedProxyReply::Status {
            status: 200,
            body: CREATED_MESSAGE_JSON,
        },
    ])
    .await;
    // The shipped template uses the channel name, so only the remembered
    // permanent rejection makes the lookup unnecessary.
    assert_eq!(
        emit_to(
            &f.app,
            &http,
            GuildId::new(789),
            "message_edit",
            uncached_edit_source_vars(),
        )
        .await,
        Delivery::Failed
    );
    let first = request_text(&requests.lock().unwrap());
    assert_eq!(edit_source_channel_gets(&first), 1, "{first:?}");
    assert_eq!(first.len(), 2, "{first:?}");
    let delivery = emit_to(
        &f.app,
        &http,
        GuildId::new(789),
        "message_edit",
        uncached_edit_source_vars(),
    )
    .await;
    let seen = request_text(&requests.lock().unwrap());
    assert_eq!(
        edit_source_channel_gets(&seen[first.len()..]),
        0,
        "{seen:?}"
    );
    assert_eq!(delivery, Delivery::Blocked);
    assert_eq!(seen.len(), first.len(), "{seen:?}");

    let app = Arc::clone(&f.app);
    let delivery = BEFORE_EDIT_DESTINATION_RECHECK
        .scope(
            Box::new(move || app.remembered_discord_targets.clear_guild(789)),
            emit_to(
                &f.app,
                &http,
                GuildId::new(789),
                "message_edit",
                uncached_edit_source_vars(),
            ),
        )
        .await;
    server.abort();
    let seen = request_text(&requests.lock().unwrap());
    let after_recovery = &seen[first.len()..];
    assert_eq!(edit_source_channel_gets(after_recovery), 0, "{seen:?}");
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(after_recovery.len(), 1, "{seen:?}");
    assert!(after_recovery[0].starts_with("POST /api/v10/channels/901/messages "));
    let text = log_text(&request_log_payload(&after_recovery[0]));
    assert!(
        text.starts_with("Message edited\n\n**Channel:** #456\n"),
        "{text}"
    );
}

#[tokio::test]
async fn message_edit_template_with_channel_still_resolves_the_source_name() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 901)
        .await
        .unwrap();
    let (http, requests, _, server) = sequenced_bulk_proxy(vec![
        SequencedProxyReply::Status {
            status: 200,
            body: EDIT_SOURCE_CHANNEL_JSON,
        },
        SequencedProxyReply::Status {
            status: 200,
            body: CREATED_MESSAGE_JSON,
        },
    ])
    .await;
    assert_eq!(
        emit_to(
            &f.app,
            &http,
            GuildId::new(789),
            "message_edit",
            uncached_edit_source_vars(),
        )
        .await,
        Delivery::Sent
    );
    server.abort();
    let seen = request_text(&requests.lock().unwrap());
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(edit_source_channel_gets(&seen[..1]), 1, "{seen:?}");
    assert!(
        seen[1].starts_with("POST /api/v10/channels/901/messages "),
        "{}",
        seen[1]
    );
    let text = log_text(&request_log_payload(&seen[1]));
    assert!(
        text.starts_with("Message edited\n\n**Channel:** #general\n"),
        "{text}"
    );
}

#[test]
fn shipped_automod_embed_keeps_whole_proving_hashes_inside_field_limit() {
    let mut vars = BTreeMap::from([(
        "content".into(),
        format!("sha256 {}", proving_digests(40).join(",")),
    )]);
    let template = default_template("automod");
    let _ = render_log_template("automod", template, &mut vars);
    let payload = serde_json::to_value(log_message("automod", template, &vars)).unwrap();
    let field = payload["embeds"][0]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "🧾 Evidence")
        .unwrap();
    let content = field["value"].as_str().unwrap();
    assert!(content.encode_utf16().count() <= 1024);
    assert_whole_digests(content);
    assert_eq!(content, whole_hash_notice(&proving_digests(40), 1024));
    assert!(content.ends_with("and 25 more"));
    assert!(!content.contains('…'));
}

/// Representative edit, delete and automod reports, each long enough to need
/// the evidence bounds, sent through the real delivery path.
async fn bounded_report_embeds() -> Vec<serde_json::Value> {
    let f = Fixture::new();
    for event in ["message_edit", "message_delete", "automod"] {
        db::upsert_log_channel(&f.app.db, 789, event, 456)
            .await
            .unwrap();
    }
    let sink = Sink::default();
    let guild = GuildId::new(789);
    let long = "é🦀".repeat(600);
    let base = BTreeMap::from([
        ("time".into(), "2026-01-01T00:00:00+00:00".into()),
        ("user".into(), "member".into()),
        ("user_id".into(), "42".into()),
        ("mention".into(), "<@42>".into()),
        ("channel".into(), "general".into()),
        ("channel_id".into(), "456".into()),
    ]);
    let with = |pairs: &[(&str, &str)]| {
        let mut vars = base.clone();
        for (key, value) in pairs {
            vars.insert((*key).into(), (*value).into());
        }
        vars
    };
    let digests = format!("sha256 {}", proving_digests(40).join(","));
    let reports = [
        (
            "message_edit",
            with(&[("before", &long), ("after", &long), ("attachments", &long)]),
        ),
        (
            "message_delete",
            with(&[("content", &long), ("attachments", &long)]),
        ),
        (
            "automod",
            with(&[
                ("rule", "links"),
                ("action", "delete"),
                ("content", &digests),
            ]),
        ),
    ];
    for (event, vars) in reports {
        assert_eq!(
            emit_to(&f.app, &sink, guild, event, vars).await,
            Delivery::Sent
        );
    }
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_bounded(
                "INSERT INTO templates(guild_id,event_type,template) VALUES(789,'automod','Hit: {content} by {mention}')",
                (),
                65_536,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    f.app.invalidate_configs();
    let custom = with(&[("content", &format!("{digests}; {long}{long}"))]);
    assert_eq!(
        emit_to(&f.app, &sink, guild, "automod", custom).await,
        Delivery::Sent
    );
    let messages = sink.messages.lock().unwrap();
    messages
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect()
}

#[tokio::test]
async fn embed_delivery_skips_the_discarded_flat_render() {
    let flat_renders = flat_log_renders();
    let embeds = bounded_report_embeds().await;
    assert_eq!(flat_log_renders(), flat_renders);
    let field = |embed: &serde_json::Value, name: &str| -> String {
        embed["embeds"][0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field["name"] == name)
            .unwrap_or_else(|| panic!("{name}"))["value"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // The shipped layouts keep their evidence bounds, not the generic field limit.
    for (embed, name, units) in [
        (&embeds[0], "⏪ Before (last observed)", 600),
        (&embeds[0], "⏩ After", 600),
        (&embeds[0], "📎 Attachments", 240),
        (&embeds[1], "💬 Last observed text", 900),
        (&embeds[1], "📎 Attachments", 400),
    ] {
        let value = field(embed, name);
        assert!(value.ends_with("… [display clipped]"), "{name}: {value}");
        assert!(
            value.contains('🦀'),
            "{name} must exercise a two-unit character"
        );
        assert!(value.encode_utf16().count() <= units, "{name}");
    }
    assert_eq!(
        field(&embeds[2], "🧾 Evidence"),
        whole_hash_notice(&proving_digests(40), 1024)
    );
    let custom = embeds[3]["embeds"][0]["description"].as_str().unwrap();
    assert!(custom.encode_utf16().count() <= 2000);
    let evidence = custom.strip_prefix("Hit: ").unwrap();
    assert!(evidence.ends_with(" and 10 more by <@42>"), "{custom}");
    assert_whole_digests(evidence);
}
