//! Bounded process-local display data for Discord profiles and guild lists.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::GuildList;
use super::oauth::{DiscordUser, PermissionsField};

// A typical profile uses hundreds of bytes and guild lists vary by orders of
// magnitude. Cap the entry metadata separately from the estimated payload.
const MAX_USERS: usize = 1024;
const PROFILE_PAYLOAD_BUDGET: usize = 512 * 1024;
const GUILD_PAYLOAD_BUDGET: usize = 8 * 1024 * 1024;

struct Entry<T> {
    data: Arc<(Instant, T)>,
    last_used: Instant,
    payload_bytes: usize,
}

struct Entries<T> {
    users: HashMap<u64, Entry<T>>,
    recency: BTreeSet<(Instant, u64)>,
    payload_bytes: usize,
    // A vacant key may have lost a newer entry. Keep a conservative cutoff
    // for new admissions without retaining one timestamp per evicted user.
    evicted_through: Option<Instant>,
}

impl<T> Entries<T> {
    fn remove(&mut self, user: &u64) -> Option<Instant> {
        let entry = self.users.remove(user)?;
        self.recency.remove(&(entry.last_used, *user));
        self.payload_bytes -= entry.payload_bytes;
        Some(entry.data.0)
    }

    fn evict(&mut self, user: &u64) {
        if let Some(observed_at) = self.remove(user) {
            self.evicted_through = self.evicted_through.max(Some(observed_at));
        }
    }
}

/// Freshness stays with the observation. Hits update only the eviction order.
/// A cache miss takes no recency work, and removal walks only the evicted keys.
pub(super) struct DisplayCache<T> {
    entries: Mutex<Entries<T>>,
    max_users: usize,
    max_payload_bytes: usize,
    heap_bytes: fn(&T) -> usize,
}

impl<T> DisplayCache<T> {
    fn new(max_users: usize, max_payload_bytes: usize, heap_bytes: fn(&T) -> usize) -> Self {
        Self {
            entries: Mutex::new(Entries {
                users: HashMap::new(),
                recency: BTreeSet::new(),
                payload_bytes: 0,
                evicted_through: None,
            }),
            max_users,
            max_payload_bytes,
            heap_bytes,
        }
    }

    // Release the lock before a caller clones a list or uses it for display.
    pub(super) fn get(&self, user: &u64) -> Option<Arc<(Instant, T)>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.users.get_mut(user)?;
        let data = Arc::clone(&entry.data);
        let now = Instant::now();
        let last_used = std::mem::replace(&mut entry.last_used, now);
        entries.recency.remove(&(last_used, *user));
        entries.recency.insert((now, *user));
        Some(data)
    }

    /// Observations from concurrent logins can finish in any order. Keep the
    /// newer retained value, and skip weighing older and reused snapshots.
    pub(super) fn insert(&self, user: u64, data: (Instant, T)) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let last_observed_at = entries
            .users
            .get(&user)
            .map(|entry| entry.data.0)
            .or(entries.evicted_through);
        if last_observed_at.is_some_and(|observed_at| observed_at >= data.0) {
            return;
        }
        let payload_bytes = size_of::<(Instant, T)>().saturating_add((self.heap_bytes)(&data.1));
        let _ = entries.remove(&user);
        let Some(available_bytes) = self.max_payload_bytes.checked_sub(payload_bytes) else {
            // Still serve the live response. It cannot fit in the display cache.
            entries.evicted_through = entries.evicted_through.max(Some(data.0));
            return;
        };
        while entries.users.len() >= self.max_users || entries.payload_bytes > available_bytes {
            let Some(&(_, oldest)) = entries.recency.first() else {
                return;
            };
            entries.evict(&oldest);
        }
        let last_used = Instant::now();
        entries.users.insert(
            user,
            Entry {
                data: Arc::new(data),
                last_used,
                payload_bytes,
            },
        );
        entries.recency.insert((last_used, user));
        entries.payload_bytes += payload_bytes;
    }

    #[cfg(test)]
    pub(super) fn set_observed_at_for_test(&self, user: &u64, observed_at: Instant)
    where
        T: Clone,
    {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.users.get_mut(user).expect("seeded display cache");
        Arc::make_mut(&mut entry.data).0 = observed_at;
    }

    #[cfg(test)]
    pub(super) fn remove(&self, user: &u64) {
        self.entries.lock().unwrap().evict(user);
    }

    #[cfg(test)]
    pub(super) fn clear(&self) {
        let mut entries = self.entries.lock().unwrap();
        entries.users.clear();
        entries.recency.clear();
        entries.payload_bytes = 0;
        entries.evicted_through = None;
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().users.is_empty()
    }

    #[cfg(test)]
    pub(super) fn usage(&self) -> (usize, usize) {
        let entries = self.entries.lock().unwrap();
        assert!(entries.users.len() <= self.max_users);
        assert!(entries.payload_bytes <= self.max_payload_bytes);
        (entries.users.len(), entries.payload_bytes)
    }
}

impl DisplayCache<DiscordUser> {
    pub(super) fn profiles() -> Self {
        Self::new(MAX_USERS, PROFILE_PAYLOAD_BUDGET, |user| {
            user.id.capacity()
                + user.username.capacity()
                + user.global_name.as_ref().map_or(0, String::capacity)
                + user.avatar.as_ref().map_or(0, String::capacity)
        })
    }
}

impl DisplayCache<GuildList> {
    pub(super) fn guild_lists() -> Self {
        Self::new(MAX_USERS, GUILD_PAYLOAD_BUDGET, |guilds| {
            std::mem::size_of_val(guilds.as_ref())
                + guilds
                    .iter()
                    .map(|guild| {
                        guild.id.capacity()
                            + guild.name.capacity()
                            + guild.icon.as_ref().map_or(0, String::capacity)
                            + match &guild.permissions {
                                Some(PermissionsField::String(value)) => value.capacity(),
                                _ => 0,
                            }
                    })
                    .sum::<usize>()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn updating_payload_weight_evicts_the_oldest_and_leaves_oversized_payloads_uncached() {
        let overhead = size_of::<(Instant, Vec<u8>)>();
        let cache = DisplayCache::new(3, 2 * overhead + 52, Vec::capacity);
        let now = tokio::time::Instant::now().into_std();
        cache.insert(1, (now, vec![1; 16]));
        cache.insert(2, (now, vec![2; 16]));
        cache.insert(3, (now, vec![3; 16]));
        assert!(cache.get(&1).is_none());
        let held = cache.get(&3).unwrap();

        // A replacement pays only its new weight but can still displace a
        // different key. Existing readers own the previous value.
        let later = now + std::time::Duration::from_secs(1);
        cache.insert(3, (later, vec![3; 37]));
        assert!(cache.get(&2).is_none());
        assert_eq!(cache.get(&3).unwrap().1.len(), 37);
        assert_eq!(held.1.len(), 16);

        cache.insert(2, (later, vec![2; 15]));
        let before = cache.usage();
        cache.insert(4, (later, vec![4; 1024]));
        assert!(cache.get(&4).is_none());
        assert_eq!(cache.usage(), before);

        // An older result cannot discard a newer observation or charge its weight.
        cache.insert(3, (now, vec![3; 1024]));
        assert_eq!(cache.get(&3).unwrap().1.len(), 37);
        assert_eq!(cache.usage(), before);

        let latest = later + std::time::Duration::from_secs(1);
        cache.insert(3, (latest, vec![3; 1024]));
        assert!(cache.get(&3).is_none());
        assert_eq!(cache.usage(), (1, overhead + 15));
        assert!(cache.get(&2).is_some());
    }
}
