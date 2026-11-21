//! Gateway access changes for the WebUI (#653).
//!
//! The API reuses a user's OAuth guild list for minutes. Role, member and
//! owner updates seen here are queued in memory and written about once a
//! second as `meta` access marks, so the API drops an affected snapshot on
//! the next request instead of waiting for it to expire. Only the writer
//! records; pending marks wait while this process cannot.

use crate::state::BotState;
use jmodguard::db;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);
/// Users kept per guild before they become one guild mark.
const MEMBERS_PER_GUILD: usize = db::ACCESS_MARK_BATCH_MEMBERS;

#[derive(Default)]
struct Pending {
    guilds: BTreeSet<i64>,
    members: BTreeMap<i64, BTreeSet<i64>>,
}

type Batch = (Vec<i64>, Vec<(i64, i64)>);

#[derive(Default)]
pub struct AccessMarks {
    pending: Mutex<Pending>,
}

impl AccessMarks {
    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every member's access in `guild` may have changed (roles or owner).
    pub fn guild(&self, guild: u64) {
        let Ok(guild) = i64::try_from(guild) else {
            return;
        };
        if guild <= 0 {
            return;
        }
        let mut pending = self.pending();
        pending.members.remove(&guild);
        pending.guilds.insert(guild);
    }

    /// One member's access in `guild` may have changed (roles, or they left).
    pub fn member(&self, guild: u64, user: u64) {
        let (Ok(guild), Ok(user)) = (i64::try_from(guild), i64::try_from(user)) else {
            return;
        };
        if guild <= 0 || user <= 0 {
            return;
        }
        let mut pending = self.pending();
        if pending.guilds.contains(&guild) {
            return;
        }
        let users = pending.members.entry(guild).or_default();
        users.insert(user);
        if users.len() > MEMBERS_PER_GUILD {
            pending.members.remove(&guild);
            pending.guilds.insert(guild);
        }
    }

    fn take_batch(&self) -> Option<Batch> {
        let mut pending = self.pending();
        let mut guilds = Vec::new();
        while guilds.len() < db::ACCESS_MARK_BATCH_GUILDS {
            let Some(guild) = pending.guilds.pop_first() else {
                break;
            };
            guilds.push(guild);
        }
        let mut members = Vec::new();
        while members.len() < db::ACCESS_MARK_BATCH_MEMBERS {
            let Some(mut entry) = pending.members.first_entry() else {
                break;
            };
            let guild = *entry.key();
            let user = entry.get_mut().pop_first();
            if entry.get().is_empty() {
                entry.remove();
            }
            if let Some(user) = user {
                members.push((guild, user));
            }
        }
        (!guilds.is_empty() || !members.is_empty()).then_some((guilds, members))
    }

    fn restore(&self, (guilds, members): Batch) {
        for guild in guilds {
            self.guild(guild as u64);
        }
        for (guild, user) in members {
            self.member(guild as u64, user as u64);
        }
    }
}

/// Writes pending marks until the process exits.
pub async fn run(app: Arc<BotState>) {
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_maintenance = Instant::now();
    let mut failing = false;
    loop {
        tick.tick().await;
        if !app.activation.may_record() {
            continue;
        }
        match flush(&app, &mut next_maintenance).await {
            Ok(()) => failing = false,
            Err(error) => {
                if !failing {
                    tracing::warn!("webui access marks deferred: {error:#}");
                }
                failing = true;
            }
        }
    }
}

/// Writes pending batches and runs retention maintenance even when idle.
async fn flush(app: &BotState, next_maintenance: &mut Instant) -> anyhow::Result<()> {
    while let Some(batch) = app.access_marks.take_batch() {
        if let Err(error) = db::record_access_marks(&app.db, batch.0.clone(), batch.1.clone()).await
        {
            app.access_marks.restore(batch);
            return Err(error);
        }
    }
    if Instant::now() >= *next_maintenance && !db::maintain_access_marks(&app.db).await? {
        *next_maintenance = Instant::now() + MAINTENANCE_INTERVAL;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_marks_fold_into_a_guild_mark_and_batches_stay_bounded() {
        let marks = AccessMarks::default();
        for user in 1..=MEMBERS_PER_GUILD as u64 {
            marks.member(7, user);
        }
        marks.member(8, 1);
        let (guilds, members) = marks.take_batch().unwrap();
        assert!(guilds.is_empty());
        assert_eq!(members.len(), db::ACCESS_MARK_BATCH_MEMBERS);
        marks.restore((guilds, members));
        marks.member(7, 999);
        let (guilds, members) = marks.take_batch().unwrap();
        assert_eq!(
            guilds,
            vec![7],
            "an overflowing guild becomes one guild mark"
        );
        assert_eq!(members, vec![(8, 1)]);
        assert!(marks.take_batch().is_none());
        marks.guild(9);
        marks.member(9, 1);
        assert_eq!(marks.take_batch(), Some((vec![9], vec![])));
    }

    #[tokio::test(start_paused = true)]
    async fn both_owner_transfers_mark_the_guild_with_an_unchanged_serenity_cache() {
        use serenity::client::EventHandler;
        use serenity::model::guild::{Guild, PartialGuild};
        use serenity::model::id::{GuildId, UserId};

        let directory =
            std::env::temp_dir().join(format!("access_marks_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            db::Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let handler = crate::handlers::Handler {
            app: Arc::clone(&app),
        };
        let ctx = crate::commands::mod_reason_dm::fixture_context_for_dm_order_test(
            Arc::new(serenity::http::Http::new("fixture")),
            Arc::new(serenity::cache::Cache::new()),
        )
        .await;
        let mut cached = Guild::default();
        cached.id = GuildId::new(10);
        cached.owner_id = UserId::new(41);
        let mut incoming = PartialGuild::from(cached.clone());
        incoming.owner_id = UserId::new(42);
        let mut next_maintenance = Instant::now();

        handler
            .guild_update(ctx.clone(), Some(cached.clone()), incoming.clone())
            .await;
        flush(&app, &mut next_maintenance).await.unwrap();
        let snapshot_epoch = db::access_epoch(&app.db).await.unwrap();
        assert_eq!(snapshot_epoch, 1);

        // Serenity still supplies owner 41 as the cached owner on the return transfer.
        incoming.owner_id = cached.owner_id;
        handler.guild_update(ctx, Some(cached), incoming).await;
        flush(&app, &mut next_maintenance).await.unwrap();
        assert!(db::access_mark(&app.db, 10, 42).await.unwrap() > snapshot_epoch);
        assert_eq!(db::access_mark(&app.db, 20, 42).await.unwrap(), 0);
        drop(handler);
        drop(app);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_flush_restores_both_access_marks_for_retry() {
        let directory =
            std::env::temp_dir().join(format!("access_marks_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            db::Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        app.access_marks.member(10, 42);
        app.access_marks.guild(20);
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "CREATE TRIGGER fail_member_access_mark BEFORE INSERT ON meta
                     WHEN NEW.key = 'access_mark:10:42'
                     BEGIN SELECT RAISE(ABORT, 'access mark write failed'); END",
                    65_536,
                )
            })
            .await
            .unwrap();
        let mut next_maintenance = Instant::now();
        let error = flush(&app, &mut next_maintenance).await.unwrap_err();
        assert!(format!("{error:#}").contains("access mark write failed"));
        assert_eq!(db::access_epoch(&app.db).await.unwrap(), 0);
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 0);
        assert_eq!(db::access_mark(&app.db, 20, 7).await.unwrap(), 0);

        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TRIGGER fail_member_access_mark", 65_536)
            })
            .await
            .unwrap();
        flush(&app, &mut next_maintenance).await.unwrap();
        assert_eq!(db::access_epoch(&app.db).await.unwrap(), 1);
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 1);
        assert_eq!(db::access_mark(&app.db, 20, 7).await.unwrap(), 1);
        drop(app);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_flush_prunes_expired_marks_after_reopening_the_database() {
        let directory =
            std::env::temp_dir().join(format!("access_marks_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("core.db");
        let app = BotState::new(Default::default(), db::Db::open(&path, 2).unwrap()).unwrap();
        let mut next_maintenance = Instant::now();
        app.access_marks.member(10, 42);
        app.access_marks.guild(20);
        flush(&app, &mut next_maintenance).await.unwrap();
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 1);
        assert_eq!(db::access_mark(&app.db, 20, 7).await.unwrap(), 1);
        assert_eq!(db::access_mark(&app.db, 10, 7).await.unwrap(), 0);
        drop(app);

        let app = BotState::new(Default::default(), db::Db::open(&path, 2).unwrap()).unwrap();
        let mut next_maintenance = Instant::now();
        flush(&app, &mut next_maintenance).await.unwrap();
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 1);

        // SQLite has its own clock. Age one persisted row without sleeping
        // and leave the other mark fresh.
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "UPDATE meta SET value = '1:1' WHERE key = 'access_mark:10:42'",
                    64 * 1024 * 1024,
                )
            })
            .await
            .unwrap();
        tokio::time::advance(MAINTENANCE_INTERVAL).await;
        flush(&app, &mut next_maintenance).await.unwrap();
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 0, "pruned");
        assert_eq!(
            db::access_mark(&app.db, 20, 7).await.unwrap(),
            1,
            "still live"
        );
        assert_eq!(db::access_epoch(&app.db).await.unwrap(), 1);
        drop(app);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
