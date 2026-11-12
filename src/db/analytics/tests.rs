use super::*;
use crate::db::{Db, GuildEnrollment, enroll_guild, retire_guild_enrollment};
use libsql::params::IntoParams;

struct Fixture {
    db: Db,
    path: std::path::PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_analytics_{label}_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }

    fn insert_raw(&self, sql: &str, params: impl IntoParams + Send + 'static) {
        self.db
            .with_conn_sync(move |conn| {
                let tx = conn.transaction_immediate()?;
                tx.execute_bounded(sql, params, 64 * 1024 * 1024)?;
                tx.commit()
            })
            .unwrap();
    }

    fn insert_warn(&self, guild_id: i64, day_offset: i64) {
        self.insert_raw(
            "INSERT INTO warns (guild_id, user_id, moderator_id, reason, created_at) \
             VALUES (?1, 7, 8, NULL, ?2)",
            crate::db::params![guild_id, rfc3339_day(day_offset)],
        );
    }

    fn insert_ban(&self, guild_id: i64, day_offset: i64) {
        self.insert_raw(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active) \
             VALUES (?1, 7, 8, NULL, ?2, 1)",
            crate::db::params![guild_id, rfc3339_day(day_offset)],
        );
    }

    fn insert_unattributed_ban(&self, guild_id: i64, user_id: i64, reason: &str) {
        self.insert_raw(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active) \
             VALUES (?1, ?2, NULL, ?3, ?4, 1)",
            crate::db::params![guild_id, user_id, reason, rfc3339_day(0)],
        );
    }

    fn insert_event(&self, guild_id: i64, event_type: &str, data: &str, day_offset: i64) {
        self.insert_raw(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at) \
             VALUES (?1, ?2, 7, NULL, ?3, ?4)",
            crate::db::params![guild_id, event_type, data, rfc3339_day(day_offset)],
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

fn member_daily_rows(db: &Db, guild_id: i64) -> i64 {
    db.with_conn_sync(move |conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM analytics_member_daily WHERE guild_id=?1",
            crate::db::params![guild_id],
            |row| Ok(row.get(0)?),
        )
    })
    .unwrap()
}

fn rfc3339_day(day_offset: i64) -> String {
    (chrono::Utc::now().date_naive() - chrono::Duration::days(day_offset))
        .format("%Y-%m-%dT12:00:00+00:00")
        .to_string()
}

async fn bump(db: &Db, enrollment: &GuildEnrollment, day_offset: i64, joins: i64, leaves: i64) {
    bump_member_daily(
        db,
        enrollment.clone(),
        (chrono::Utc::now() - chrono::Duration::days(day_offset)).timestamp_millis(),
        joins,
        leaves,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn guild_series_buckets_counts_and_zero_fills_the_window() {
    let f = Fixture::new("series");
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    f.insert_warn(10, 0);
    f.insert_warn(10, 0);
    f.insert_warn(10, 1);
    f.insert_ban(10, 1);
    f.insert_event(10, "automod", r#"{"action":"ban"}"#, 0);
    // Dry-run automod and non-punishing actions match the lifetime definition:
    // they are routine observations, not counted enforcement.
    f.insert_event(10, "automod", r#"{"action":"delete"}"#, 0);
    f.insert_event(10, "automod", r#"{"action":"warn","dry_run":1}"#, 0);
    f.insert_event(10, "mod_action", r#"{"action":"kick"}"#, 1);
    f.insert_event(10, "timeout_add", "{}", 2);
    // Not a counted bucket for the series.
    f.insert_event(10, "plan_executed", r#"{"moderation":true}"#, 0);
    bump(&f.db, &enrollment, 0, 3, 1).await;
    bump(&f.db, &enrollment, 0, 1, 0).await;
    bump(&f.db, &enrollment, 2, 2, 2).await;

    let analytics = enrolled_guild_analytics(&f.db, enrollment, 4)
        .await
        .unwrap();
    assert_eq!(analytics.window_days, 4);
    assert_eq!(analytics.series.len(), 4);
    let today = &analytics.series[3];
    assert_eq!(today.warns, 2, "two warns today");
    assert_eq!(today.bans, 0);
    assert_eq!(
        today.automod, 1,
        "only recorded non-dry-run automod bans count"
    );
    assert_eq!(today.mod_actions, 0);
    assert_eq!(today.joins, 4, "3 + 1 accumulated on the same day");
    assert_eq!(today.leaves, 1);
    let yesterday = &analytics.series[2];
    assert_eq!(yesterday.warns, 1);
    assert_eq!(yesterday.bans, 1);
    assert_eq!(yesterday.mod_actions, 1);
    assert_eq!(yesterday.joins, 0, "unobserved days zero-fill");
    assert_eq!(yesterday.leaves, 0);
    let three_days_ago = &analytics.series[1];
    assert_eq!(three_days_ago.timeouts, 1);
    assert_eq!(three_days_ago.joins, 2);
    assert_eq!(three_days_ago.leaves, 2);
    assert_eq!(analytics.series[0].warns, 0);
    for point in &analytics.series {
        assert_eq!(point.day.len(), 10);
    }
}

#[tokio::test]
async fn guild_series_stays_isolated_per_guild() {
    let f = Fixture::new("isolation");
    let ten = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let twenty = enroll_guild(&f.db, 20, 100).await.unwrap().unwrap();
    f.insert_warn(10, 0);
    f.insert_warn(20, 0);
    f.insert_warn(20, 0);
    f.insert_event(20, "mod_action", "{}", 0);
    bump(&f.db, &ten, 0, 1, 0).await;
    bump(&f.db, &twenty, 0, 0, 5).await;
    let ten_series = enrolled_guild_analytics(&f.db, ten, 2).await.unwrap();
    assert_eq!(ten_series.series[1].warns, 1);
    assert_eq!(ten_series.series[1].mod_actions, 0);
    assert_eq!(ten_series.series[1].joins, 1);
    assert_eq!(ten_series.series[1].leaves, 0);
    let twenty_series = enrolled_guild_analytics(&f.db, twenty, 2).await.unwrap();
    assert_eq!(twenty_series.series[1].warns, 2);
    assert_eq!(twenty_series.series[1].mod_actions, 1);
    assert_eq!(twenty_series.series[1].joins, 0);
    assert_eq!(twenty_series.series[1].leaves, 5);
}

#[tokio::test]
async fn analytics_reads_and_writes_refuse_retired_enrollments() {
    let f = Fixture::new("retired");
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(
        enrolled_guild_analytics(&f.db, enrollment.clone(), 30)
            .await
            .unwrap_err()
            .is::<crate::db::EnrollmentChanged>()
    );
    assert!(
        bump_member_daily(
            &f.db,
            enrollment,
            chrono::Utc::now().timestamp_millis(),
            1,
            0
        )
        .await
        .unwrap_err()
        .is::<crate::db::EnrollmentChanged>()
    );
}

#[tokio::test]
async fn member_daily_counters_prune_beyond_the_horizon() {
    let f = Fixture::new("prune");
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let old_day = (chrono::Utc::now().date_naive()
        - chrono::Duration::days(ANALYTICS_MEMBER_DAILY_HORIZON_DAYS + 5))
    .format("%Y-%m-%d")
    .to_string();
    f.insert_raw(
        "INSERT INTO analytics_member_daily (guild_id, day, joins, leaves) VALUES (?1, ?2, 9, 9)",
        crate::db::params![10, old_day],
    );
    bump(&f.db, &enrollment, 0, 1, 1).await;
    assert_eq!(
        member_daily_rows(&f.db, 10),
        1,
        "the horizon row is pruned by the next write"
    );
    let series = enrolled_guild_analytics(&f.db, enrollment, 3)
        .await
        .unwrap();
    assert_eq!(series.series[2].joins, 1);
    assert_eq!(series.series[2].leaves, 1);
}

#[tokio::test]
async fn window_requests_clamp_to_the_supported_maximum() {
    let f = Fixture::new("window");
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let analytics = enrolled_guild_analytics(&f.db, enrollment, 5000)
        .await
        .unwrap();
    assert_eq!(analytics.window_days, ANALYTICS_MAX_WINDOW_DAYS);
    assert_eq!(analytics.series.len() as i64, ANALYTICS_MAX_WINDOW_DAYS);
}

#[tokio::test]
async fn platform_analytics_aggregates_guilds_and_counts_active_servers() {
    let f = Fixture::new("platform");
    let ten = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let twenty = enroll_guild(&f.db, 20, 100).await.unwrap().unwrap();
    enroll_guild(&f.db, 30, 100).await.unwrap().unwrap();
    retire_guild_enrollment(&f.db, 30, 100).await.unwrap();

    f.insert_warn(10, 0);
    f.insert_warn(20, 0);
    f.insert_warn(20, 0);
    f.insert_ban(20, 0);
    f.insert_event(10, "automod", r#"{"action":"ban"}"#, 0);
    f.insert_event(20, "mod_action", "{}", 0);
    bump(&f.db, &ten, 0, 5, 1).await;
    bump(&f.db, &twenty, 0, 2, 3).await;

    let platform = platform_analytics(&f.db, 7).await.unwrap();
    assert_eq!(platform.window_days, 7);
    assert_eq!(platform.series.len(), 7);
    let today = &platform.series[6];
    assert_eq!(today.warns, 3, "guild aggregates sum across servers");
    assert_eq!(today.bans, 1);
    assert_eq!(today.automod, 1);
    assert_eq!(today.mod_actions, 1);
    assert_eq!(today.joins, 7);
    assert_eq!(today.leaves, 4);
    assert_eq!(platform.servers, 2, "retired enrollments are not servers");
    let guild_ids: Vec<String> = platform
        .guilds
        .iter()
        .map(|totals| totals.guild_id.to_string())
        .collect();
    assert_eq!(guild_ids, vec!["10".to_string(), "20".to_string()]);
    let guild_ten = &platform.guilds[0];
    assert_eq!(guild_ten.warns, 1);
    assert_eq!(guild_ten.automod, 1);
    assert_eq!(guild_ten.joins, 5);
    assert_eq!(guild_ten.leaves, 1);
    let guild_twenty = &platform.guilds[1];
    assert_eq!(guild_twenty.warns, 2);
    assert_eq!(guild_twenty.bans, 1);
    assert_eq!(guild_twenty.mod_actions, 1);
    assert_eq!(guild_twenty.joins, 2);
    assert_eq!(guild_twenty.leaves, 3);
}

#[tokio::test]
async fn departure_erasure_removes_member_counters_before_re_enrollment() {
    let f = Fixture::new("erasure");
    // Inline retirement: an unplaced guild settles directory data in the
    // retirement transaction.
    let first = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let kept = enroll_guild(&f.db, 20, 100).await.unwrap().unwrap();
    bump(&f.db, &first, 0, 4, 2).await;
    bump(&f.db, &kept, 0, 1, 0).await;
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert_eq!(member_daily_rows(&f.db, 10), 0);
    assert_eq!(
        member_daily_rows(&f.db, 20),
        1,
        "other guilds keep counters"
    );
    let rejoined = enroll_guild(&f.db, 10, 200).await.unwrap().unwrap();
    let series = enrolled_guild_analytics(&f.db, rejoined, 1).await.unwrap();
    assert_eq!(series.series[0].joins, 0, "a new installation starts empty");
    assert_eq!(series.series[0].leaves, 0);

    // Deferred erasure: the active-enrollment guard still refuses, and a
    // counter-only cleanup changes no configuration version.
    f.insert_raw(
        "INSERT INTO analytics_member_daily (guild_id, day, joins, leaves) VALUES (30, ?1, 3, 1)",
        crate::db::params![chrono::Utc::now().format("%Y-%m-%d").to_string()],
    );
    assert!(
        !crate::db::wipe_departed_directory_guild_data(&f.db, 20)
            .await
            .unwrap()
    );
    assert_eq!(member_daily_rows(&f.db, 20), 1);
    let before = crate::db::get_primary_versions_sync(&f.db).unwrap();
    assert!(
        crate::db::wipe_departed_directory_guild_data(&f.db, 30)
            .await
            .unwrap()
    );
    assert_eq!(member_daily_rows(&f.db, 30), 0);
    let after = crate::db::get_primary_versions_sync(&f.db).unwrap();
    assert_eq!(before.config_revision, after.config_revision);
    assert_eq!(before.slash_revision, after.slash_revision);
}

#[tokio::test]
async fn spam_hits_include_dedicated_spam_bans_once() {
    let f = Fixture::new("spam");
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    // Dedicated exact-image and repeated-text enforcement: ban mirror only.
    f.insert_unattributed_ban(10, 21, "[automod:attachment_flood] identical image");
    f.insert_unattributed_ban(10, 22, "[automod:repeat_text] repeated text");
    // Generic executor ban: its automod event is the spam record.
    f.insert_unattributed_ban(10, 23, "[automod:invite] posted a link");
    f.insert_event(10, "automod", r#"{"action":"ban"}"#, 0);
    // A moderator's ban is not automod enforcement, whatever its reason text.
    f.insert_raw(
        "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active) \
         VALUES (10, 24, 8, '[automod:repeat_text] typed by a moderator', ?1, 1)",
        crate::db::params![rfc3339_day(0)],
    );
    f.insert_unattributed_ban(20, 25, "[automod:repeat_text] other server");

    let guild = enrolled_guild_analytics(&f.db, enrollment, 1)
        .await
        .unwrap();
    assert_eq!(guild.series[0].bans, 4);
    assert_eq!(guild.series[0].automod, 3);

    let platform = platform_analytics(&f.db, 1).await.unwrap();
    assert_eq!(platform.series[0].automod, 4);
    assert_eq!(platform.guilds[0].guild_id, 10);
    assert_eq!(platform.guilds[0].automod, 3);
    assert_eq!(platform.guilds[1].automod, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn analytics_reads_fit_the_remote_snapshot_budget() {
    use crate::db::tests::remote_stream::HranaFixture;
    use std::time::Duration;
    let f = Fixture::new("remote");
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    enroll_guild(&f.db, 20, 100).await.unwrap().unwrap();
    f.insert_warn(10, 0);
    f.insert_ban(20, 0);
    f.insert_unattributed_ban(10, 21, "[automod:repeat_text] repeated text");
    f.insert_event(10, "automod", r#"{"action":"ban"}"#, 0);
    f.insert_event(20, "timeout_add", "{}", 0);
    bump(&f.db, &enrollment, 0, 2, 1).await;
    // Real libSQL HTTP against this SQLite file with the reported ~350 ms per
    // request and the provider's five-second interactive lifetime; the
    // production four-second snapshot budget is unchanged.
    let (_fixture, remote) =
        HranaFixture::delayed_database(&f.path, Duration::from_millis(350), Duration::from_secs(5))
            .await;
    let guild = enrolled_guild_analytics(&remote, enrollment, 2)
        .await
        .expect("guild analytics must finish inside the snapshot budget");
    let today = &guild.series[1];
    assert_eq!(
        (
            today.warns,
            today.bans,
            today.automod,
            today.joins,
            today.leaves
        ),
        (1, 1, 2, 2, 1)
    );
    let platform = platform_analytics(&remote, 2)
        .await
        .expect("platform analytics must finish inside the snapshot budget");
    let today = &platform.series[1];
    assert_eq!(
        (today.warns, today.bans, today.automod, today.timeouts),
        (1, 2, 2, 1)
    );
    assert_eq!(platform.servers, 2);
    assert_eq!(platform.guilds.len(), 2);
    assert_eq!(platform.guilds[0].automod, 2);
    assert_eq!(platform.guilds[1].timeouts, 1);
}
