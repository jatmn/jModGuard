//! Gateway access changes for the WebUI (#653).
//!
//! The API reuses a user's OAuth guild list for minutes. Role, member and
//! owner updates seen here are queued in memory and written about once a
//! second as `meta` access marks, so the API drops an affected snapshot on
//! the next request instead of waiting for it to expire. Only the writer
//! records; pending marks wait while this process cannot.

use crate::state::BotState;
use jmodguard::db;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
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

/// Epochs written, by commit time, until they are old enough to prune.
#[derive(Default)]
struct Written(VecDeque<(Instant, i64)>);

impl Written {
    fn record(&mut self, epoch: i64) {
        self.0.push_back((Instant::now(), epoch));
    }

    /// How many leading epochs no unexpired API snapshot can predate, and
    /// the newest of them.
    fn prunable(&self) -> Option<(usize, i64)> {
        let now = Instant::now();
        let count = self
            .0
            .iter()
            .take_while(|(at, _)| now.saturating_duration_since(*at) >= db::ACCESS_MARK_RETENTION)
            .count();
        count.checked_sub(1).map(|last| (count, self.0[last].1))
    }
}

/// Writes pending marks until the process exits.
pub async fn run(app: Arc<BotState>) {
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut written = Written::default();
    let mut failing = false;
    loop {
        tick.tick().await;
        if !app.activation.may_record() {
            continue;
        }
        match flush(&app, &mut written).await {
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

/// Writes every pending batch, pruning marks past their retention. A failed
/// batch goes back to the queue for the next tick.
async fn flush(app: &BotState, written: &mut Written) -> anyhow::Result<()> {
    while let Some(batch) = app.access_marks.take_batch() {
        let prune = written.prunable();
        let through = prune.map(|(_, epoch)| epoch);
        match db::record_access_marks(&app.db, batch.0.clone(), batch.1.clone(), through).await {
            Ok(epoch) => {
                if let Some((count, _)) = prune {
                    written.0.drain(..count);
                }
                written.record(epoch);
            }
            Err(error) => {
                app.access_marks.restore(batch);
                return Err(error);
            }
        }
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
    async fn flush_writes_queued_marks_then_prunes_them_after_the_retention() {
        let directory =
            std::env::temp_dir().join(format!("access_marks_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            db::Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let mut written = Written::default();
        app.access_marks.member(10, 42);
        app.access_marks.guild(20);
        flush(&app, &mut written).await.unwrap();
        assert!(
            app.access_marks.take_batch().is_none(),
            "the queue is drained"
        );
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 1);
        assert_eq!(db::access_mark(&app.db, 20, 7).await.unwrap(), 1);
        assert_eq!(db::access_mark(&app.db, 10, 7).await.unwrap(), 0);

        tokio::time::advance(db::ACCESS_MARK_RETENTION).await;
        app.access_marks.member(30, 42);
        flush(&app, &mut written).await.unwrap();
        assert_eq!(db::access_epoch(&app.db).await.unwrap(), 2);
        assert_eq!(db::access_mark(&app.db, 10, 42).await.unwrap(), 0, "pruned");
        assert_eq!(db::access_mark(&app.db, 20, 7).await.unwrap(), 0, "pruned");
        assert_eq!(db::access_mark(&app.db, 30, 42).await.unwrap(), 2);
        drop(app);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test(start_paused = true)]
    async fn only_epochs_older_than_the_retention_are_pruned() {
        let mut written = Written::default();
        written.record(3);
        tokio::time::advance(Duration::from_secs(2)).await;
        written.record(5);
        assert_eq!(written.prunable(), None);
        tokio::time::advance(db::ACCESS_MARK_RETENTION).await;
        written.record(6);
        assert_eq!(written.prunable(), Some((2, 5)));
    }
}
