use super::*;
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
    assert!(parts[0].contains("Attachment unavailable"));
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
    let edit_text = messages[0].1["content"].as_str().unwrap();
    assert!(edit_text.contains("before text") && edit_text.contains("after text"));
    let second_text = messages[1].1["content"].as_str().unwrap();
    assert!(second_text.contains("after text") && second_text.contains("final text"));
    let delete_text = messages[2].1["content"].as_str().unwrap();
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

fn edit_report(event: &MessageUpdateEvent, known_author_id: Option<u64>) -> Option<MessageReport> {
    message_update_report(event, known_author_id, None)
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
    assert!(!dedup.should_report(&gif));
    assert!(!dedup.should_report(&update(serde_json::json!({"content":"new text"}))));
    let real = edited(serde_json::json!({"content":"actually edited"}));
    assert_eq!(
        edit_report(&real, None).unwrap().vars["after"],
        "actually edited"
    );
    assert!(dedup.should_report(&real));
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
    assert!(!dedup.should_report(&reply));
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
    assert!(dedup.should_report(&edited_reply));
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
    assert!(message_update_report(&log_update, None, Some(99)).is_none());
    let reported = message_update_report(&log_update, None, Some(1)).unwrap();
    assert_eq!(reported.vars["after"], nested);
    let mut anonymous = edited(serde_json::json!({"content": nested}));
    anonymous.author = None;
    assert!(message_update_report(&anonymous, Some(99), Some(99)).is_none());
    assert_eq!(
        message_update_report(&anonymous, Some(99), Some(1))
            .unwrap()
            .vars["user_id"],
        "99"
    );
}

#[test]
fn repeated_edit_observations_emit_once_but_changed_content_emits_again() {
    let dedup = EditReportDedup::default();
    let first = edited(serde_json::json!({"content":"first"}));
    assert!(dedup.should_report(&first));
    assert!(!dedup.should_report(&first));
    assert!(!dedup.should_report(&first));
    let changed = edited_at(
        serde_json::json!({"content":"second"}),
        "2026-09-21T00:00:02.000Z",
    );
    assert!(dedup.should_report(&changed));
    assert!(!dedup.should_report(&changed));
    assert!(
        !dedup.should_report(&first),
        "a replay of an earlier timestamp is still the same reported edit"
    );
    let mut other_channel = first.clone();
    other_channel.channel_id = ChannelId::new(999);
    assert!(dedup.should_report(&other_channel));
}

#[test]
fn partial_fields_for_one_edit_are_one_observation() {
    let dedup = EditReportDedup::default();
    let content = edited(serde_json::json!({"content":"edited"}));
    let attachments = edited(serde_json::json!({"attachments":[]}));
    let combined = edited(serde_json::json!({"content":"edited","attachments":[]}));
    assert!(dedup.should_report(&content));
    assert!(!dedup.should_report(&attachments));
    assert!(!dedup.should_report(&combined));
    assert!(dedup.should_report(&edited_at(
        serde_json::json!({"content":"edited again"}),
        "2026-09-21T00:00:02.000Z"
    )));

    let reverse = EditReportDedup::default();
    assert!(reverse.should_report(&attachments));
    assert!(!reverse.should_report(&content));
    assert!(!reverse.should_report(&combined));
    assert!(reverse.should_report(&edited_at(
        serde_json::json!({"content":"edited again"}),
        "2026-09-21T00:00:02.000Z"
    )));
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
    assert!(dedup.should_report(&content));
    assert!(!dedup.should_report(&same_edit));
    assert!(!dedup.should_report(&update(serde_json::json!({
        "content":"edited field supplied again",
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }))));
    assert!(dedup.should_report(&next_edit));
    assert!(!dedup.should_report(&next_edit));
    assert!(
        !dedup.should_report(&content),
        "a replay of T1 after T2 is still the same reported edit"
    );
    assert!(dedup.should_report(&update(serde_json::json!({
        "content":"third edit",
        "edited_timestamp":"2026-09-21T00:00:03.000Z"
    }))));

    let two_fields = EditReportDedup::default();
    assert!(two_fields.should_report(&update(serde_json::json!({
        "content":"before", "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }))));
    assert!(two_fields.should_report(&update(serde_json::json!({
        "content":"after",
        "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));
    assert!(!two_fields.should_report(&update(serde_json::json!({
        "attachments":[{"id":"555", "filename":"image.png", "description":"new",
            "proxy_url":"https://cdn.example/proxy", "url":"https://cdn.example/image",
            "size":100}],
        "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));

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
    assert!(metadata.should_report(&attachment("before", EDITED)));
    assert!(!metadata.should_report(&attachment("before", EDITED)));
    assert!(metadata.should_report(&attachment("after", "2026-09-21T00:00:02.000Z")));
}

#[test]
fn untimestamped_refresh_does_not_hide_a_later_timestamped_edit() {
    let dedup = EditReportDedup::default();
    assert!(!dedup.should_report(&update(serde_json::json!({"content":"after"}))));
    assert!(dedup.should_report(&edited_at(
        serde_json::json!({"content":"after"}),
        "2026-09-21T00:00:02.000Z"
    )));
    assert!(!dedup.should_report(&edited_at(
        serde_json::json!({"attachments":[]}),
        "2026-09-21T00:00:02.000Z"
    )));
    assert!(dedup.should_report(&edited_at(
        serde_json::json!({"content":"third"}),
        "2026-09-21T00:00:03.000Z"
    )));
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
fn delivery_disables_mentions_and_embeds_and_bounds_unicode() {
    let payload = serde_json::to_value(log_message(&format!(
        "@everyone <@123> <@&456> {}",
        "🦀".repeat(3000)
    )))
    .unwrap();
    assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
    assert_eq!(payload["allowed_mentions"]["replied_user"], false);
    assert_eq!(
        payload["flags"].as_u64().unwrap() & MessageFlags::SUPPRESS_EMBEDS.bits(),
        MessageFlags::SUPPRESS_EMBEDS.bits()
    );
    let text = payload["content"].as_str().unwrap();
    assert!(text.encode_utf16().count() <= 2000);
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
    let rendered = messages[0].1["content"].as_str().unwrap();
    assert!(rendered.starts_with("**:gear: System notice**\n\n**Time:** `"));
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
    assert!(
        messages[0].1["content"]
            .as_str()
            .unwrap()
            .contains(&format!(
                "Bot shutting down (v{}). Uptime: 1m 1s",
                env!("CARGO_PKG_VERSION")
            ))
    );
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
    assert!(
        messages[0].1["content"]
            .as_str()
            .unwrap()
            .contains("Bot shutting down")
    );
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
async fn disabled_and_overloaded_delivery_do_not_queue_reports() {
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
        let text = message["content"].as_str().unwrap();
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
    assert!(
        sink.sink.messages.lock().unwrap()[0].1["content"]
            .as_str()
            .unwrap()
            .contains("moderator (<@777>)")
    );
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
        let text = message["content"].as_str().unwrap();
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
fn received_message_join_context_is_scoped_consumed_expiring_and_bounded() {
    let cache = MemberJoinContext::default();
    let joined = serenity::model::Timestamp::from_unix_timestamp(1_760_000_000).unwrap();
    cache.remember(1, 2, Some(joined));
    cache.remember(1, 2, None);
    assert_eq!(cache.take(2, 2), None);
    assert_eq!(cache.take(1, 2), Some(joined));
    assert_eq!(cache.take(1, 2), None);
    cache.remember(1, 2, Some(joined));
    cache.inner.lock().get_mut(&(1, 2)).unwrap().1 = Instant::now() - MEMBER_JOINS_KEEP;
    assert_eq!(cache.take(1, 2), None);
    for user in 0..=MEMBER_JOINS_LIMIT {
        cache.remember(1, user as u64, Some(joined));
    }
    assert_eq!(cache.inner.lock().len(), MEMBER_JOINS_LIMIT);
    cache.clear_guild(1);
    assert!(cache.inner.lock().is_empty());
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
    assert!(
        sink.sink.messages.lock().unwrap()[0].1["content"]
            .as_str()
            .unwrap()
            .contains("**Reason:** spam")
    );
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
            let last = messages.last().unwrap().1["content"].as_str().unwrap();
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
            let delivery = emit_to(
                &f.app,
                &UnavailableAuditSink {
                    mode,
                    observed: prior.timestamp_millis(),
                },
                GuildId::new(789),
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
                emit_to(&f.app, &sink, GuildId::new(789), event, vars).await,
                Delivery::Sent
            );
            let messages = sink.sink.messages.lock().unwrap();
            let text = messages[0].1["content"].as_str().unwrap();
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
        messages[0].1["content"].as_str().unwrap(),
        "**:outbox_tray: elgringo1092 left the server**\n\n**User ID:** `553144871846805504`\n**Time in server:** `Unavailable`\n**Joined at:** `Unavailable`\n**Left at:** `Sep 22, 2026 • 18:02:22 UTC`"
    );
    assert!(
        messages[1].1["content"]
            .as_str()
            .unwrap()
            .contains("**Time:** `Sep 22, 2026 • 18:02:22 UTC`")
    );
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
