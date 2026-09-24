use super::*;

fn at(offset_ms: i64) -> i64 {
    static START: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let start = START.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            - 60_000
    });
    start + offset_ms
}

async fn ready() -> (Fixture, StoreRegistration, GuildDataStore, GuildDataScope) {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let placement = f.placement(&reg, 10).await;
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    let scope = GuildDataScope::new(
        placement,
        GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    store.bind_initial_membership(scope.clone()).await.unwrap();
    (f, reg, store, scope)
}

#[tokio::test]
async fn snapshot_accounting_tracks_edits_consumption_expiry_and_eviction() {
    let (_fixture, _registration, store, scope) = ready().await;
    for id in [101, 102, 103, 104] {
        assert_eq!(
            store
                .observe_snapshot(scope.clone(), observation(id, Some("A"), None, 1))
                .await
                .unwrap(),
            SnapshotOutcome::Stored
        );
    }
    store
        .apply_snapshot_edit(
            scope.clone(),
            observation(101, Some("longer edit"), Some(at(100)), 1),
        )
        .await
        .unwrap();
    let (count, logical, sum): (i64, i64, i64) = store.db.with_conn_sync(|conn| {
        let tx = conn.transaction_primary_read()?;
        let health = tx.query_row("SELECT snapshot_count,snapshot_logical_bytes FROM guild_snapshot_health WHERE singleton=1", (), |r| Ok((r.get::<i64>(0)?, r.get::<i64>(1)?)))?;
        let sum = tx.query_row("SELECT COALESCE(SUM(encoded_bytes),0) FROM guild_message_snapshots", (), |r| Ok(r.get::<i64>(0)?))?;
        tx.finish_read()?;
        Ok((health.0, health.1, sum))
    }).unwrap();
    assert_eq!(count, 4);
    assert_eq!(logical, sum);
    assert!(
        store
            .take_snapshot(scope.clone(), 20, 101, at(200))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .take_snapshot(scope.clone(), 20, 104, at(86_400_001))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.evict_snapshots(1, at(300)).await.unwrap(), 1);
    assert_eq!(
        store.expire_snapshots(100, at(86_400_001)).await.unwrap(),
        1
    );
    let counters: (i64, i64, i64, i64, i64) = store.db.with_conn_sync(|conn| {
        let tx = conn.transaction_primary_read()?;
        let row = tx.query_row("SELECT snapshot_count,snapshot_logical_bytes,estimated_snapshot_bytes,expired_count,evicted_count FROM guild_snapshot_health WHERE singleton=1", (), |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        tx.finish_read()?;
        Ok(row)
    }).unwrap();
    assert_eq!(counters, (0, 0, 0, 2, 1));
    store
        .update_snapshot_health(SnapshotHealthUpdate {
            epoch: 2,
            state: SnapshotHealthState::Active,
            reason: None,
            observed_at_ms: Some(at(500)),
            lost_delta: 0,
            now_ms: at(500),
        })
        .await
        .unwrap();
    let reset: (i64, i64, Option<i64>) = store.db.with_conn_sync(|conn| conn.query_row("SELECT expired_count,evicted_count,counters_reset_at_ms FROM guild_snapshot_health WHERE singleton=1", (), |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))).unwrap();
    assert_eq!(reset, (0, 0, Some(at(500))));
}

fn observation(
    message_id: i64,
    text: Option<&str>,
    edit_at_ms: Option<i64>,
    epoch: u64,
) -> SnapshotObservation {
    SnapshotObservation {
        channel_id: 20,
        message_id,
        created_at_ms: at(0),
        observed_at_ms: edit_at_ms.unwrap_or(at(1)),
        edit_at_ms,
        coverage_epoch: epoch,
        fields: SnapshotFields {
            content: text.map(str::to_owned),
            author_id: Some(30),
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn latest_edit_partial_duplicate_restart_and_reopen_delete() {
    let (f, reg, store, scope) = ready().await;
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation(40, Some("A"), None, 1))
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation(40, Some("late create"), None, 1))
            .await
            .unwrap(),
        SnapshotOutcome::Duplicate
    );
    let first = store
        .apply_snapshot_edit(scope.clone(), observation(40, Some("B"), Some(at(100)), 1))
        .await
        .unwrap();
    assert_eq!(first.outcome, SnapshotOutcome::Stored);
    assert_eq!(first.before.unwrap().fields.content.as_deref(), Some("A"));
    assert_eq!(first.after.unwrap().fields.content.as_deref(), Some("B"));

    let mut partial = observation(40, None, Some(at(100)), 1);
    partial.fields.attachments = Some(vec![]);
    let same = store
        .apply_snapshot_edit(scope.clone(), partial)
        .await
        .unwrap();
    assert_eq!(same.outcome, SnapshotOutcome::Duplicate);
    assert!(same.before.is_none());
    let merged = same.after.unwrap();
    assert_eq!(merged.fields.content.as_deref(), Some("B"));
    assert_eq!(merged.fields.attachments, Some(vec![]));
    assert!(merged.continuous);
    assert_eq!(
        store
            .apply_snapshot_edit(
                scope.clone(),
                observation(40, Some("stale"), Some(at(50)), 1)
            )
            .await
            .unwrap()
            .outcome,
        SnapshotOutcome::Stale
    );

    let restart = store
        .apply_snapshot_edit(scope.clone(), observation(40, Some("C"), Some(at(200)), 2))
        .await
        .unwrap();
    assert_eq!(restart.before.unwrap().fields.content.as_deref(), Some("B"));
    assert!(!restart.after.unwrap().continuous);
    drop(store);
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    let taken = reopened
        .take_snapshot(scope.clone(), 20, 40, at(300))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.fields.content.as_deref(), Some("C"));
    assert_eq!(taken.expires_at_ms, at(86_400_000));
    assert!(
        reopened
            .take_snapshot(scope, 20, 40, at(301))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn expiry_scope_and_erasure_keep_other_guild_rows() {
    let (f, reg, store, scope) = ready().await;
    let other_placement = f.placement(&reg, 11).await;
    store
        .bind_initial_placement(other_placement.clone())
        .await
        .unwrap();
    let other = GuildDataScope::new(
        other_placement,
        GuildEnrollment {
            guild_id: 11,
            id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            joined_at_ns: 200,
        },
    )
    .unwrap();
    store.bind_initial_membership(other.clone()).await.unwrap();
    store
        .observe_snapshot(scope.clone(), observation(41, Some("private"), None, 1))
        .await
        .unwrap();
    store
        .observe_snapshot(other.clone(), observation(41, Some("other"), None, 1))
        .await
        .unwrap();
    assert!(
        store
            .take_snapshot(other.clone(), 20, 41, at(5))
            .await
            .unwrap()
            .is_some()
    );
    let mut other_later = observation(42, Some("other"), None, 1);
    other_later.created_at_ms = at(1000);
    store
        .observe_snapshot(other.clone(), other_later)
        .await
        .unwrap();
    store
        .observe_snapshot(scope.clone(), observation(45, Some("expire"), None, 1))
        .await
        .unwrap();
    assert!(
        store
            .take_snapshot(scope.clone(), 20, 41, at(86_400_000))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.expire_snapshots(100, at(86_400_000)).await.unwrap(),
        1
    );
    store
        .observe_snapshot(scope.clone(), observation(43, Some("erase"), None, 1))
        .await
        .unwrap();
    store.close_membership(scope.clone()).await.unwrap();
    assert!(!store.erase_closed_scope(scope.clone()).await.unwrap());
    assert!(store.erase_closed_scope(scope.clone()).await.unwrap());
    assert!(
        store
            .observe_snapshot(scope, observation(44, Some("stale"), None, 1))
            .await
            .is_err()
    );
    assert!(
        store
            .take_snapshot(other, 20, 42, at(5))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn author_omitted_edit_requires_persisted_author() {
    let (_, _, store, scope) = ready().await;
    let mut edit = observation(90, Some("edited"), Some(at(100)), 1);
    edit.fields.author_id = None;
    let mut unknown_create = observation(92, Some("unknown"), None, 1);
    unknown_create.fields.author_id = None;
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), unknown_create)
            .await
            .unwrap(),
        SnapshotOutcome::Missing
    );
    assert_eq!(
        store
            .apply_snapshot_edit_existing(scope.clone(), edit.clone())
            .await
            .unwrap()
            .outcome,
        SnapshotOutcome::Missing
    );
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation(90, Some("original"), None, 1))
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    let changed = store
        .apply_snapshot_edit_existing(scope.clone(), edit)
        .await
        .unwrap();
    assert_eq!(changed.outcome, SnapshotOutcome::Stored);
    assert_eq!(
        changed.before.unwrap().fields.content.as_deref(),
        Some("original")
    );
    assert_eq!(
        changed.after.unwrap().fields.content.as_deref(),
        Some("edited")
    );
    assert!(
        store
            .take_snapshot(scope, 20, 90, at(101))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn queued_observation_cannot_write_after_original_expiry() {
    let (_, _, store, scope) = ready().await;
    let mut delayed = observation(91, Some("old"), None, 1);
    delayed.created_at_ms = at(-86_400_001);
    delayed.observed_at_ms = delayed.created_at_ms + 1;
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), delayed)
            .await
            .unwrap(),
        SnapshotOutcome::Expired
    );
    assert!(
        store
            .take_snapshot(scope, 20, 91, at(1))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stale_delete_clock_cannot_report_expired_snapshot() {
    let (_, _, store, scope) = ready().await;
    let created = at(-86_400_001);
    let expires = created + 86_400_000;
    let guild = scope.guild_id();
    let enrollment = scope.enrollment.id.clone();
    store.db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded(
            "INSERT INTO guild_message_snapshots(guild_id,enrollment_id,channel_id,message_id,author_id,content,created_at_ms,first_observed_at_ms,last_observed_at_ms,expires_at_ms,coverage_epoch,encoded_bytes,reservation_bytes) VALUES(?1,?2,20,93,30,'expired',?3,?4,?4,?5,1,7,0)",
            params![guild, enrollment, created, created + 1, expires],
            1_000_000,
        )?;
        tx.commit()
    }).await.unwrap();
    assert!(
        store
            .take_snapshot(scope.clone(), 20, 93, created + 2)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .take_snapshot(scope, 20, 93, at(1))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn bulk_delete_repeated_id_returns_one_before_state() {
    let (_, _, store, scope) = ready().await;
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation(94, Some("once"), None, 1))
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    let rows = store
        .take_snapshots_at_epoch(scope.clone(), 20, vec![95, 94, 94], at(10), 1)
        .await
        .unwrap();
    assert!(rows[0].is_none());
    assert_eq!(
        rows[1].as_ref().unwrap().fields.content.as_deref(),
        Some("once")
    );
    assert!(rows[2].is_none());
    assert!(
        store
            .take_snapshot(scope, 20, 94, at(11))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn conservative_debt_cannot_exceed_fresh_whole_file_bound() {
    let (_fixture, _registration, store, scope) = ready().await;
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation(96, Some("first"), None, 1))
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    store.db.with_conn(|conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded(
            "UPDATE guild_snapshot_health SET physical_debt_bytes=?1,physical_sample_at_ms=?2 WHERE singleton=1",
            params![crate::capacity::SNAPSHOT_BUDGET_BYTES as i64 + 1, at(1)],
            1_000_000,
        )?;
        tx.commit()
    }).await.unwrap();
    assert!(
        store.measure_usage(false).await.unwrap().allocated_bytes
            < crate::capacity::SNAPSHOT_BUDGET_BYTES
    );
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), observation(97, Some("second"), None, 1))
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    let rows = store
        .take_snapshots_at_epoch(scope, 20, vec![96, 97], at(2), 1)
        .await
        .unwrap();
    assert_eq!(
        rows[0].as_ref().unwrap().fields.content.as_deref(),
        Some("first")
    );
    assert_eq!(
        rows[1].as_ref().unwrap().fields.content.as_deref(),
        Some("second")
    );
}

#[tokio::test]
async fn attachment_metadata_clipping_marks_truncation_without_omitting_entries() {
    let (_fixture, _registration, store, scope) = ready().await;
    let mut item = observation(98, Some("before"), None, 1);
    item.fields.attachments = Some(vec![SnapshotAttachment {
        id: 1,
        filename: "€".repeat(100),
        size: 1,
        content_type: Some("€".repeat(50)),
        url: "€".repeat(350),
    }]);
    assert_eq!(
        store.observe_snapshot(scope.clone(), item).await.unwrap(),
        SnapshotOutcome::Stored
    );
    let edited = store
        .apply_snapshot_edit(
            scope.clone(),
            observation(98, Some("after"), Some(at(100)), 1),
        )
        .await
        .unwrap()
        .after
        .unwrap();
    assert!(edited.fields.attachments_truncated);
    assert_eq!(edited.fields.attachments_omitted, Some(0));
    let kept = &edited.fields.attachments.as_ref().unwrap()[0];
    assert_eq!(kept.filename.len(), 255);
    assert_eq!(kept.url.len(), 1023);
    assert_eq!(kept.content_type.as_ref().unwrap().len(), 126);
    assert!(kept.filename.chars().all(|c| c == '€'));
    assert!(kept.url.chars().all(|c| c == '€'));
    assert!(
        kept.content_type
            .as_ref()
            .unwrap()
            .chars()
            .all(|c| c == '€')
    );

    for (id, omitted) in [(99, 0), (100, 2)] {
        let mut preclipped = observation(id, Some("before"), None, 1);
        preclipped.fields.attachments = Some(vec![SnapshotAttachment {
            id: 2,
            filename: "short".into(),
            size: 1,
            content_type: None,
            url: "https://example.invalid/file".into(),
        }]);
        preclipped.fields.attachments_truncated = true;
        preclipped.fields.attachments_omitted = Some(omitted);
        assert_eq!(
            store
                .observe_snapshot(scope.clone(), preclipped)
                .await
                .unwrap(),
            SnapshotOutcome::Stored
        );
        let edited = store
            .apply_snapshot_edit(
                scope.clone(),
                observation(id, Some("after"), Some(at(100)), 1),
            )
            .await
            .unwrap()
            .after
            .unwrap();
        assert!(edited.fields.attachments_truncated);
        assert_eq!(edited.fields.attachments_omitted, Some(omitted));
        assert_eq!(edited.fields.attachments.as_ref().unwrap().len(), 1);
    }
}

/// Manual local-storage measurement. This exercises the real scoped snapshot
/// store, not Serenity ingress, Discord delivery, or a remote provider.
/// Run with `--ignored --nocapture`; no message bodies enter the output.
#[tokio::test]
#[ignore = "manual 10/100 events-per-second local snapshot workload measurement"]
async fn measure_sustained_snapshot_workload_and_cleanup_reuse() {
    fn p(values: &mut [u128], percentile: usize) -> u128 {
        if values.is_empty() {
            return 0;
        }
        values.sort_unstable();
        values[(values.len() - 1) * percentile / 100]
    }
    fn rss_kib() -> Option<u64> {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }
    fn cpu_ticks() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let (_, rest) = stat.rsplit_once(") ")?;
        let mut fields = rest.split_whitespace();
        let user: u64 = fields.nth(11)?.parse().ok()?;
        let system: u64 = fields.next()?.parse().ok()?;
        user.checked_add(system)
    }
    #[allow(clippy::too_many_arguments)]
    async fn report(
        store: &GuildDataStore,
        scope_label: &str,
        phase: &str,
        elapsed_ms: u128,
        attempted: usize,
        creates: usize,
        edits: usize,
        deletes: usize,
        refused: usize,
        unavailable: usize,
        pressure_evicted: usize,
        timings: &mut [u128],
        retention_ages_ms: &mut [u128],
        cleanup_rows: usize,
        rss_start: Option<u64>,
        cpu_start: Option<u64>,
    ) {
        let measured_at = std::time::Instant::now();
        let usage = store.measure_usage(true).await.unwrap();
        let overview_ms = measured_at.elapsed().as_millis();
        eprintln!(
            "SNAPSHOT_MEASUREMENT {}",
            serde_json::json!({
                "scope": scope_label, "phase": phase,
                "elapsed_ms": elapsed_ms, "attempted_events": attempted,
                "accepted_creates": creates, "accepted_edits": edits, "available_deletes": deletes,
                "capacity_refused": refused, "unavailable": unavailable,
                "pressure_evicted_rows": pressure_evicted,
                "observed_events_per_sec": if elapsed_ms == 0 { 0.0 } else { attempted as f64 * 1000.0 / elapsed_ms as f64 },
                "event_path_latency_ms_p50": p(timings, 50) as f64 / 1000.0,
                "event_path_latency_ms_p95": p(timings, 95) as f64 / 1000.0,
                "event_path_latency_ms_p99": p(timings, 99) as f64 / 1000.0,
                "store_call_samples": timings.len(), "cleanup_rows": cleanup_rows,
                "deleted_message_age_ms_p50": p(retention_ages_ms, 50),
                "deleted_message_age_ms_p95": p(retention_ages_ms, 95),
                "deleted_message_age_samples": retention_ages_ms.len(),
                "allocated_bytes": usage.allocated_bytes, "snapshot_count": usage.snapshot_count,
                "snapshot_logical_bytes": usage.snapshot_logical_bytes,
                "page_count": usage.page_count, "page_size": usage.page_size,
                "freelist_count": usage.freelist_count, "reusable_bytes": usage.reusable_bytes,
                "snapshot_data_bytes": usage.categories.snapshot_data_bytes,
                "index_bytes": usage.categories.index_bytes,
                "detail_query_ms": overview_ms,
                "client_process_rss_kib": rss_kib(), "client_process_rss_start_kib": rss_start,
                "client_process_cpu_ticks": cpu_ticks(), "client_process_cpu_start_ticks": cpu_start,
            })
        );
    }

    let seconds: usize = std::env::var("JMODGUARD_SNAPSHOT_MEASURE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
        .clamp(10, 120);
    let remote_url = std::env::var("JMODGUARD_SNAPSHOT_MEASURE_URL");
    let remote_token = std::env::var("JMODGUARD_SNAPSHOT_MEASURE_TOKEN");
    let (_fixture, _registration, store, scope, scope_label) = match (remote_url, remote_token) {
        (Ok(url), Ok(token)) => {
            let parsed = url::Url::parse(&url).expect("valid remote measurement URL");
            assert_eq!(
                parsed.scheme(),
                "http",
                "measurement remote must be loopback HTTP"
            );
            assert!(
                matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
                    && parsed.port().is_some(),
                "measurement remote must be a localhost container with an explicit port"
            );
            assert!(!token.is_empty(), "remote measurement token is required");
            let fixture = Fixture::new();
            let registration = fixture
                .registration(StoreId::new(), StorePolicy::Shared)
                .await;
            let options = GuildStoreOptions {
                path: fixture.store_path.clone(),
                url,
                token,
                pool_size: 2,
            };
            let store = GuildDataStore::initialize(options, registration.clone()).unwrap();
            let placement = fixture.placement(&registration, 10).await;
            store
                .bind_initial_placement(placement.clone())
                .await
                .unwrap();
            let scope = GuildDataScope::new(
                placement,
                GuildEnrollment {
                    guild_id: 10,
                    id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    joined_at_ns: 100,
                },
            )
            .unwrap();
            store.bind_initial_membership(scope.clone()).await.unwrap();
            (
                fixture,
                registration,
                store,
                scope,
                "localhost_hrana_storage_client",
            )
        }
        (Err(_), Err(_)) => {
            let (fixture, registration, store, scope) = ready().await;
            (
                fixture,
                registration,
                store,
                scope,
                "local_sqlite_storage_only",
            )
        }
        _ => panic!("set both remote measurement URL and TOKEN, or neither"),
    };
    let mut live = std::collections::VecDeque::new();
    let mut next_id = 1_000_000i64;
    let mut last_event_ms = 0i64;
    let rss_start = rss_kib();
    let cpu_start = cpu_ticks();
    for rate in [10usize, 100] {
        let started = std::time::Instant::now();
        let pace_start = tokio::time::Instant::now();
        let mut timings = Vec::with_capacity(rate * seconds);
        let mut retention_ages_ms = Vec::new();
        let (mut creates, mut edits, mut deletes) = (0, 0, 0);
        let (mut refused, mut unavailable, mut pressure_evicted) = (0, 0, 0);
        for tick in 0..rate * seconds {
            let due = pace_start
                + std::time::Duration::from_nanos(
                    (tick as u64).saturating_mul(1_000_000_000 / rate as u64),
                );
            tokio::time::sleep_until(due).await;
            let event_ms = chrono::Utc::now().timestamp_millis().max(last_event_ms + 1);
            last_event_ms = event_ms;
            let op_started = std::time::Instant::now();
            if tick % 10 == 0
                && let Some(id) = live.pop_front()
            {
                let taken = store
                    .take_snapshot_at_epoch(scope.clone(), 20, id, event_ms, 1)
                    .await
                    .unwrap();
                if let Some(row) = taken {
                    retention_ages_ms.push((event_ms - row.created_at_ms).max(0) as u128);
                    deletes += 1;
                } else {
                    unavailable += 1;
                }
            } else if tick % 5 == 0
                && let Some(&id) = live.back()
            {
                let edit = observation(id, Some("edited synthetic text"), Some(event_ms), 1);
                match store
                    .apply_snapshot_edit(scope.clone(), edit)
                    .await
                    .unwrap()
                    .outcome
                {
                    SnapshotOutcome::Stored => edits += 1,
                    SnapshotOutcome::CapacityRefused => {
                        refused += 1;
                        pressure_evicted += store.evict_snapshots(100, event_ms).await.unwrap();
                    }
                    SnapshotOutcome::Missing | SnapshotOutcome::Expired => unavailable += 1,
                    other => panic!("unexpected edit outcome: {other:?}"),
                }
            } else {
                let id = next_id;
                next_id += 1;
                let mut item = observation(id, Some("synthetic text"), None, 1);
                item.observed_at_ms = event_ms;
                match store.observe_snapshot(scope.clone(), item).await.unwrap() {
                    SnapshotOutcome::Stored => {
                        live.push_back(id);
                        creates += 1;
                    }
                    SnapshotOutcome::CapacityRefused => {
                        refused += 1;
                        pressure_evicted += store.evict_snapshots(100, event_ms).await.unwrap();
                    }
                    other => panic!("unexpected create outcome: {other:?}"),
                }
            }
            timings.push(op_started.elapsed().as_micros());
        }
        let elapsed_ms = started.elapsed().as_millis();
        report(
            &store,
            scope_label,
            &format!("paced_{rate}_events_per_sec"),
            elapsed_ms,
            rate * seconds,
            creates,
            edits,
            deletes,
            refused,
            unavailable,
            pressure_evicted,
            &mut timings,
            &mut retention_ages_ms,
            0,
            rss_start,
            cpu_start,
        )
        .await;
    }

    // Large records use the same 32 KiB text limit and bounded attachment metadata.
    let large = "🦀".repeat(8192);
    let burst_start = std::time::Instant::now();
    let mut burst_timings = Vec::new();
    let mut no_retention_ages = Vec::new();
    let (mut burst_accepted, mut burst_refused, mut burst_evicted) = (0, 0, 0);
    for _ in 0..64 {
        let id = next_id;
        next_id += 1;
        let mut item = observation(id, Some(&large), None, 1);
        let event_ms = chrono::Utc::now().timestamp_millis().max(last_event_ms + 1);
        last_event_ms = event_ms;
        item.observed_at_ms = event_ms;
        item.fields.attachments = Some(
            (0..8)
                .map(|index| SnapshotAttachment {
                    id: index + 1,
                    filename: format!("synthetic-{index}.bin"),
                    size: 1,
                    content_type: Some("application/octet-stream".into()),
                    url: format!("https://example.invalid/{index}/{}", "x".repeat(900)),
                })
                .collect(),
        );
        let op_started = std::time::Instant::now();
        match store.observe_snapshot(scope.clone(), item).await.unwrap() {
            SnapshotOutcome::Stored => {
                live.push_back(id);
                burst_accepted += 1;
            }
            SnapshotOutcome::CapacityRefused => {
                burst_refused += 1;
                burst_evicted += store.evict_snapshots(100, event_ms).await.unwrap();
            }
            other => panic!("unexpected burst outcome: {other:?}"),
        }
        burst_timings.push(op_started.elapsed().as_micros());
    }
    report(
        &store,
        scope_label,
        "large_record_burst",
        burst_start.elapsed().as_millis(),
        64,
        burst_accepted,
        0,
        0,
        burst_refused,
        0,
        burst_evicted,
        &mut burst_timings,
        &mut no_retention_ages,
        0,
        rss_start,
        cpu_start,
    )
    .await;

    // Repeated oldest-first eviction followed by replacement observes page reuse.
    let reuse_start = std::time::Instant::now();
    let mut reuse_timings = Vec::new();
    let (mut evicted, mut reuse_accepted, mut reuse_refused, mut reuse_pressure_evicted) =
        (0, 0, 0, 0);
    for _ in 0..10 {
        let op_started = std::time::Instant::now();
        let event_ms = chrono::Utc::now().timestamp_millis().max(last_event_ms + 1);
        last_event_ms = event_ms;
        let n = store.evict_snapshots(100, event_ms).await.unwrap();
        reuse_timings.push(op_started.elapsed().as_micros());
        assert!(n <= 100);
        evicted += n;
        for _ in 0..n {
            let id = next_id;
            next_id += 1;
            let op_started = std::time::Instant::now();
            let mut item = observation(id, Some("replacement synthetic text"), None, 1);
            let event_ms = chrono::Utc::now().timestamp_millis().max(last_event_ms + 1);
            last_event_ms = event_ms;
            item.observed_at_ms = event_ms;
            match store.observe_snapshot(scope.clone(), item).await.unwrap() {
                SnapshotOutcome::Stored => reuse_accepted += 1,
                SnapshotOutcome::CapacityRefused => {
                    reuse_refused += 1;
                    reuse_pressure_evicted += store.evict_snapshots(100, event_ms).await.unwrap();
                }
                other => panic!("unexpected replacement outcome: {other:?}"),
            }
            reuse_timings.push(op_started.elapsed().as_micros());
        }
    }
    report(
        &store,
        scope_label,
        "evict_and_reuse_10x100",
        reuse_start.elapsed().as_millis(),
        evicted * 2,
        reuse_accepted,
        0,
        evicted,
        reuse_refused,
        0,
        reuse_pressure_evicted,
        &mut reuse_timings,
        &mut no_retention_ages,
        evicted,
        rss_start,
        cpu_start,
    )
    .await;

    // Fast-forward the cleanup API clock across original-creation + 24 hours.
    // Writes still consult actual wall time; this is cleanup-clock simulation only.
    let expiry_start = std::time::Instant::now();
    let mut expiry_timings = Vec::new();
    let mut expired = 0;
    for _ in 0..200 {
        let op_started = std::time::Instant::now();
        let n = store.expire_snapshots(100, at(86_400_001)).await.unwrap();
        expiry_timings.push(op_started.elapsed().as_micros());
        expired += n;
        if n == 0 {
            break;
        }
    }
    let remaining = store.measure_usage(true).await.unwrap().snapshot_count;
    assert_eq!(
        remaining,
        Some(0),
        "bounded cleanup should catch up in fixture"
    );
    report(
        &store,
        scope_label,
        "expiry_clock_plus_24h",
        expiry_start.elapsed().as_millis(),
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        &mut expiry_timings,
        &mut no_retention_ages,
        expired,
        rss_start,
        cpu_start,
    )
    .await;
}

#[tokio::test]
async fn equal_version_partials_merge_supplied_fields_and_keep_conflicts() {
    let (f, reg, store, scope) = ready().await;
    let mut created = observation(70, Some("A"), None, 1);
    created.fields.attachments = Some(vec![SnapshotAttachment {
        id: 1,
        filename: "x.txt".into(),
        size: 4,
        content_type: None,
        url: "https://cdn.example/x".into(),
    }]);
    assert_eq!(
        store
            .observe_snapshot(scope.clone(), created)
            .await
            .unwrap(),
        SnapshotOutcome::Stored
    );
    let mut text = observation(70, Some("B"), Some(at(100)), 1);
    text.fields.content_truncated = true;
    text.fields.content_original_bytes = Some(99);
    text.fields.attachments = None;
    let first = store
        .apply_snapshot_edit(scope.clone(), text)
        .await
        .unwrap();
    assert_eq!(first.outcome, SnapshotOutcome::Stored);
    assert_eq!(
        first.after.as_ref().unwrap().fields.content.as_deref(),
        Some("B")
    );
    assert_eq!(
        first
            .after
            .as_ref()
            .unwrap()
            .fields
            .attachments
            .as_ref()
            .unwrap()
            .len(),
        1
    );

    let mut attachments = observation(70, None, Some(at(100)), 1);
    attachments.fields.attachments = Some(vec![]);
    attachments.fields.attachments_omitted = Some(2);
    let second = store
        .apply_snapshot_edit_existing(scope.clone(), attachments)
        .await
        .unwrap();
    assert_eq!(second.outcome, SnapshotOutcome::Duplicate);
    assert!(second.before.is_none());
    let merged = second.after.unwrap();
    assert_eq!(merged.fields.content.as_deref(), Some("B"));
    assert!(merged.fields.content_truncated);
    assert_eq!(merged.fields.content_original_bytes, Some(99));
    assert_eq!(merged.fields.attachments, Some(vec![]));
    assert_eq!(merged.fields.attachments_omitted, Some(2));

    let mut conflict = observation(70, Some("C"), Some(at(100)), 1);
    conflict.fields.attachments = None;
    let kept = store
        .apply_snapshot_edit(scope.clone(), conflict)
        .await
        .unwrap();
    assert_eq!(kept.outcome, SnapshotOutcome::Duplicate);
    assert_eq!(kept.after.unwrap().fields.content.as_deref(), Some("B"));

    let mut reversed = observation(71, Some("A"), None, 1);
    reversed.fields.attachments = Some(vec![SnapshotAttachment {
        id: 2,
        filename: "y.txt".into(),
        size: 1,
        content_type: None,
        url: "https://cdn.example/y".into(),
    }]);
    store
        .observe_snapshot(scope.clone(), reversed)
        .await
        .unwrap();
    let mut clear = observation(71, None, Some(at(100)), 1);
    clear.fields.attachments = Some(vec![]);
    store
        .apply_snapshot_edit_existing(scope.clone(), clear)
        .await
        .unwrap();
    let restored = store
        .apply_snapshot_edit(scope.clone(), observation(71, Some("B"), Some(at(100)), 1))
        .await
        .unwrap();
    assert_eq!(restored.outcome, SnapshotOutcome::Duplicate);
    assert_eq!(restored.after.unwrap().fields.content.as_deref(), Some("B"));
    assert_eq!(
        store
            .take_snapshot(scope.clone(), 20, 71, at(150))
            .await
            .unwrap()
            .unwrap()
            .fields
            .attachments,
        Some(vec![])
    );

    drop(store);
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    let next = reopened
        .apply_snapshot_edit(scope.clone(), observation(70, Some("D"), Some(at(200)), 1))
        .await
        .unwrap();
    let before = next.before.unwrap();
    assert_eq!(before.fields.content.as_deref(), Some("B"));
    assert_eq!(before.fields.attachments, Some(vec![]));
    let single = reopened
        .take_snapshot(scope.clone(), 20, 70, at(300))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(single.fields.content.as_deref(), Some("D"));
    assert_eq!(single.fields.attachments, Some(vec![]));
}

#[tokio::test]
async fn closed_scope_erasure_deletes_snapshots_in_batches() {
    let (f, reg, store, scope) = ready().await;
    let other_placement = f.placement(&reg, 11).await;
    store
        .bind_initial_placement(other_placement.clone())
        .await
        .unwrap();
    let other = GuildDataScope::new(
        other_placement,
        GuildEnrollment {
            guild_id: 11,
            id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            joined_at_ns: 200,
        },
    )
    .unwrap();
    store.bind_initial_membership(other.clone()).await.unwrap();
    store
        .observe_snapshot(other.clone(), observation(42, Some("neighbor"), None, 1))
        .await
        .unwrap();
    for id in 1..=101 {
        store
            .observe_snapshot(scope.clone(), observation(1_000 + id, Some("row"), None, 1))
            .await
            .unwrap();
    }
    assert!(store.erase_closed_scope(scope.clone()).await.is_err());
    store.close_membership(scope.clone()).await.unwrap();
    assert!(!store.erase_closed_scope(scope.clone()).await.unwrap());
    let left: i64 = store
        .db
        .with_conn_sync(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=10",
                (),
                |row| Ok(row.get(0)?),
            )
        })
        .unwrap();
    assert_eq!(left, 1);
    assert!(!store.erase_closed_scope(scope.clone()).await.unwrap());
    assert!(store.erase_closed_scope(scope).await.unwrap());
    assert!(
        store
            .take_snapshot(other, 20, 42, at(5))
            .await
            .unwrap()
            .is_some()
    );
}
