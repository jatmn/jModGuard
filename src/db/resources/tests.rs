use super::*;
use crate::db::{Db, params};

struct Fixture {
    db: Db,
    path: std::path::PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_resources_{label}_{:032x}.db",
            rand::random::<u128>()
        ));
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }

    fn rows(&self) -> Vec<(String, i64, i64)> {
        self.db
            .with_conn_sync(|conn| {
                let mut statement = conn.prepare(
                    "SELECT process, span_secs, start_secs FROM resource_rollups \
                     ORDER BY process, span_secs, start_secs",
                )?;
                let rows = statement.query_map((), |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<i64>(1)?,
                        row.get::<i64>(2)?,
                    ))
                })?;
                rows.collect()
            })
            .unwrap()
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

const HOUR: i64 = 1_800_000_000 - 1_800_000_000 % HOUR_SECS;

fn minute(start_secs: i64, cpu: u32, requests: u64) -> Rollup {
    let mut rollup = Rollup::empty(start_secs, MINUTE_SECS);
    rollup.cpu_avg_tenths = Some(cpu);
    rollup.cpu_max_tenths = Some(cpu);
    rollup.data.cpu_samples = 12;
    rollup.data.minutes = 1;
    rollup.data.starts = vec![1_000];
    rollup.data.counters.insert("api_requests".into(), requests);
    rollup
}

fn status(sampled_at_ms: i64, guilds: i64) -> ProcessStatus {
    let mut status = ProcessStatus {
        sampled_at_ms: Some(sampled_at_ms),
        interval_ms: 60_000,
        ..ProcessStatus::default()
    };
    status.gauges.insert("guilds".into(), guilds);
    status
}

#[tokio::test]
async fn writer_keeps_each_minute_once_and_merges_it_into_its_hour() {
    let f = Fixture::new("writer");
    let now = HOUR + 600;
    let added = record_resource_rollups(
        &f.db,
        vec![
            (ResourceProcess::Api, minute(HOUR, 100, 3)),
            (ResourceProcess::Api, minute(HOUR + 60, 300, 5)),
            (ResourceProcess::Bot, minute(HOUR, 50, 0)),
        ],
        Some((ResourceProcess::Bot, status(now * 1000, 4))),
        now,
    )
    .await
    .unwrap();
    assert_eq!(added.minute_rows_inserted, 3);
    assert_eq!(added.bot_minute_starts, vec![HOUR]);
    assert_eq!(added.api_minute_starts, vec![HOUR, HOUR + 60]);
    // The API keeps recent minutes; collecting them again changes nothing.
    let again = record_resource_rollups(
        &f.db,
        vec![
            (ResourceProcess::Api, minute(HOUR, 999, 999)),
            (ResourceProcess::Api, minute(HOUR + 60, 999, 999)),
            (ResourceProcess::Bot, minute(HOUR, 999, 999)),
        ],
        Some((ResourceProcess::Bot, status(now * 1000 + 60_000, 5))),
        now + 60,
    )
    .await
    .unwrap();
    assert_eq!(again.minute_rows_inserted, 0);
    assert_eq!(again.api_minute_starts, [HOUR, HOUR + 60]);
    assert_eq!(again.bot_minute_starts, [HOUR]);

    let minutes = resource_records(&f.db, MINUTE_SECS, HOUR, HOUR + HOUR_SECS)
        .await
        .unwrap();
    assert_eq!(minutes.api.len(), 2);
    assert_eq!(minutes.api[1].cpu_avg_tenths, Some(300), "first write wins");
    assert_eq!(minutes.bot.len(), 1);
    assert_eq!(minutes.statuses.len(), 1);
    let (process, updated_at, bot) = &minutes.statuses[0];
    assert_eq!(process, "bot");
    assert_eq!(*updated_at, now * 1000 + 60_000, "status is replaced");
    assert_eq!(bot.gauges["guilds"], 5);

    let hours = resource_records(&f.db, HOUR_SECS, HOUR, HOUR + HOUR_SECS)
        .await
        .unwrap();
    let hour = &hours.api[0];
    assert_eq!((hour.start_secs, hour.span_secs), (HOUR, HOUR_SECS));
    assert_eq!(hour.data.minutes, 2);
    assert_eq!(hour.cpu_avg_tenths, Some(200));
    assert_eq!(hour.cpu_max_tenths, Some(300));
    assert_eq!(hour.data.counters["api_requests"], 8);
    assert_eq!(hours.bot[0].data.minutes, 1);
}

#[tokio::test]
async fn writes_prune_expired_rows_and_refuse_out_of_window_minutes() {
    let f = Fixture::new("prune");
    let first = HOUR;
    record_resource_rollups(
        &f.db,
        vec![(ResourceProcess::Bot, minute(first, 1, 1))],
        None,
        first + 60,
    )
    .await
    .unwrap();
    assert_eq!(
        f.rows(),
        vec![
            ("bot".into(), MINUTE_SECS, first),
            ("bot".into(), HOUR_SECS, first)
        ]
    );

    // Two days later the minute has expired; its hour is kept for 31 days.
    let later = first + RESOURCE_MINUTE_RETENTION_SECS + 2 * HOUR_SECS;
    let added = record_resource_rollups(
        &f.db,
        vec![
            (ResourceProcess::Bot, minute(later - 60, 1, 1)),
            (ResourceProcess::Bot, minute(first - 60, 1, 1)),
            (ResourceProcess::Bot, minute(later + 600, 1, 1)),
            (ResourceProcess::Bot, Rollup::empty(later - 30, MINUTE_SECS)),
        ],
        None,
        later,
    )
    .await
    .unwrap();
    assert_eq!(
        added.minute_rows_inserted, 1,
        "only the in-window, aligned minute is stored"
    );
    assert_eq!(added.bot_minute_starts, vec![later - 60]);
    assert!(added.api_minute_starts.is_empty());
    let later_hour = (later - 60) - (later - 60).rem_euclid(HOUR_SECS);
    assert_eq!(
        f.rows(),
        vec![
            ("bot".into(), MINUTE_SECS, later - 60),
            ("bot".into(), HOUR_SECS, first),
            ("bot".into(), HOUR_SECS, later_hour)
        ]
    );

    let month_later = first + RESOURCE_HOUR_RETENTION_SECS + 2 * HOUR_SECS;
    record_resource_rollups(&f.db, Vec::new(), None, month_later)
        .await
        .unwrap();
    assert!(
        !f.rows().contains(&("bot".into(), HOUR_SECS, first)),
        "hours older than the retention are pruned"
    );
}

#[tokio::test]
async fn rejected_minutes_do_not_appear_in_write_result() {
    let f = Fixture::new("rejected");
    let now = HOUR + 600;
    let future = now + 10 * MINUTE_SECS;
    let write = record_resource_rollups(
        &f.db,
        vec![(ResourceProcess::Api, minute(future, 1, 1))],
        None,
        now,
    )
    .await
    .unwrap();
    assert_eq!(write.minute_rows_inserted, 0);
    assert!(write.api_minute_starts.is_empty());
}

#[tokio::test]
async fn oversized_label_breakdowns_are_dropped_before_totals() {
    let f = Fixture::new("bounded");
    let mut big = minute(HOUR, 1, 9);
    for family in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        big.data.labelled.insert(
            family.into(),
            (0..64)
                .map(|i| (format!("{}{i}", "x".repeat(70)), 1))
                .collect(),
        );
    }
    record_resource_rollups(&f.db, vec![(ResourceProcess::Api, big)], None, HOUR + 120)
        .await
        .unwrap();
    let stored = resource_records(&f.db, MINUTE_SECS, HOUR, HOUR + 60)
        .await
        .unwrap();
    assert!(stored.api[0].data.labelled.is_empty());
    assert_eq!(stored.api[0].data.counters["api_requests"], 9);
}

#[test]
fn prune_and_history_reads_use_the_primary_key_range() {
    let f = Fixture::new("plans");
    f.db.with_conn_sync(|conn| {
        crate::db::tests::assert_index_plan(conn, PRUNE_SQL, params![MINUTE_SECS, 0], false);
        for process in [ResourceProcess::Bot, ResourceProcess::Api] {
            crate::db::tests::assert_index_plan(
                conn,
                &history_sql(process),
                params![MINUTE_SECS, 0, 1, 10],
                true,
            );
        }
        Ok(())
    })
    .unwrap();
}
