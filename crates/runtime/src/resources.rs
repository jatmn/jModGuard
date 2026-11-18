//! Process resource rollups for the owner Bot Resources page.
//!
//! Each process folds its own [`crate::cgroup`] samples and activity counters
//! into one [`Rollup`] per minute. Counters are this process's own work
//! (database calls, Discord calls, requests, log bytes); nothing here reads a
//! host-wide total, writes a database or calls a provider. The bot persists
//! rollups for both processes; the API keeps its recent minutes in memory for
//! the bot to collect over the private read link.

use crate::cgroup::{ContainerSample, CpuLimit, MemoryUse};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds in one persisted minute rollup.
pub const MINUTE_SECS: i64 = 60;
/// Seconds in one persisted hour rollup.
pub const HOUR_SECS: i64 = 3600;
/// Distinct labels kept per labelled family in one rollup; the rest count as `other`.
pub const MAX_LABELS: usize = 24;
const MAX_LABEL_BYTES: usize = 80;
/// Process starts kept in one rollup.
const MAX_STARTS: usize = 32;
/// Completed minutes the API keeps for the bot to collect.
pub const RECENT_MINUTES: usize = 30;
/// Upper bounds (inclusive, milliseconds) of the request latency buckets. One
/// more bucket holds slower requests.
pub const LATENCY_BOUNDS_MS: [u64; 12] =
    [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000];
const LATENCY_SLOTS: usize = LATENCY_BOUNDS_MS.len() + 1;
/// Entries a disk walk may visit before it reports a partial total.
const DISK_WALK_LIMIT: usize = 4096;
const DISK_WALK_DEPTH: usize = 12;
const LABEL_OTHER: &str = "other";

struct Activity {
    counters: BTreeMap<&'static str, u64>,
    labelled: BTreeMap<&'static str, BTreeMap<String, u64>>,
    latency: [u64; LATENCY_SLOTS],
}

static ACTIVITY: Mutex<Activity> = Mutex::new(Activity {
    counters: BTreeMap::new(),
    labelled: BTreeMap::new(),
    latency: [0; LATENCY_SLOTS],
});
static GAUGES: Mutex<BTreeMap<&'static str, i64>> = Mutex::new(BTreeMap::new());
static RECENT: Mutex<VecDeque<Rollup>> = Mutex::new(VecDeque::new());
static DISK: Mutex<Vec<DiskUse>> = Mutex::new(Vec::new());

fn activity() -> std::sync::MutexGuard<'static, Activity> {
    ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Adds `n` to this process's counter for the current minute.
pub fn count(name: &'static str, n: u64) {
    let mut activity = activity();
    let value = activity.counters.entry(name).or_default();
    *value = value.saturating_add(n);
}

/// Adds `n` under `label` in a bounded family (for example gateway events by
/// type). Labels past [`MAX_LABELS`] in one minute count as `other`.
pub fn count_labelled(family: &'static str, label: &str, n: u64) {
    let label = bounded_label(label);
    let mut activity = activity();
    let labels = activity.labelled.entry(family).or_default();
    add_label(labels, label, n);
}

/// The current minute's value of a counter, without taking it.
pub fn peek_counter(name: &str) -> u64 {
    activity().counters.get(name).copied().unwrap_or(0)
}

/// Records one request's latency in the current minute's histogram.
pub fn observe_latency_ms(ms: u64) {
    let slot = LATENCY_BOUNDS_MS
        .iter()
        .position(|bound| ms <= *bound)
        .unwrap_or(LATENCY_BOUNDS_MS.len());
    let mut activity = activity();
    activity.latency[slot] = activity.latency[slot].saturating_add(1);
}

/// Sets a point-in-time value (for example guild count) reported with the
/// next completed minute and the current status.
pub fn set_gauge(name: &'static str, value: i64) {
    GAUGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(name, value);
}

/// Removes a gauge whose value is no longer known; it is then reported as
/// unavailable rather than as its last value.
pub fn clear_gauge(name: &'static str) {
    GAUGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(name);
}

fn gauges() -> BTreeMap<String, i64> {
    GAUGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .map(|(name, value)| ((*name).to_owned(), *value))
        .collect()
}

fn bounded_label(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        return "unknown".into();
    }
    let mut end = label.len().min(MAX_LABEL_BYTES);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label[..end].to_owned()
}

fn add_label(labels: &mut BTreeMap<String, u64>, label: String, n: u64) {
    let key = if labels.contains_key(&label) || labels.len() < MAX_LABELS {
        label
    } else {
        LABEL_OTHER.to_owned()
    };
    let value = labels.entry(key).or_default();
    *value = value.saturating_add(n);
}

/// Counters and histograms one rollup carries. Missing names were not
/// observed by that process; they are unavailable, not zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupData {
    /// Minutes of observation merged into this rollup.
    #[serde(default)]
    pub minutes: u32,
    /// Process start times (Unix ms) observed during this rollup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub starts: Vec<i64>,
    /// Samples with a CPU reading and with a memory reading.
    #[serde(default)]
    pub cpu_samples: u32,
    #[serde(default)]
    pub memory_samples: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counters: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labelled: BTreeMap<String, BTreeMap<String, u64>>,
    /// Request counts per [`LATENCY_BOUNDS_MS`] bucket, plus one slower bucket.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latency: Vec<u64>,
    /// Largest gauge value seen during the rollup.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gauges: BTreeMap<String, i64>,
}

/// One process's resource use over `span_secs` starting at `start_secs`.
/// CPU is in tenths of a percent of one CPU, as in [`ContainerSample`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rollup {
    pub start_secs: i64,
    pub span_secs: i64,
    pub cpu_avg_tenths: Option<u32>,
    pub cpu_max_tenths: Option<u32>,
    pub memory_avg_bytes: Option<u64>,
    pub memory_max_bytes: Option<u64>,
    #[serde(default)]
    pub data: RollupData,
}

impl Rollup {
    /// Adds `other` (a shorter rollup inside this one's span) to this rollup.
    pub fn absorb(&mut self, other: &Rollup) {
        let (cpu_sum, cpu_n) = weighted(self.cpu_avg_tenths.map(u64::from), self.data.cpu_samples);
        let (other_cpu, other_cpu_n) =
            weighted(other.cpu_avg_tenths.map(u64::from), other.data.cpu_samples);
        self.data.cpu_samples = cpu_n.saturating_add(other_cpu_n);
        self.cpu_avg_tenths = average(cpu_sum + other_cpu, self.data.cpu_samples)
            .and_then(|avg| u32::try_from(avg).ok());
        self.cpu_max_tenths = max_option(self.cpu_max_tenths, other.cpu_max_tenths);
        let (memory_sum, memory_n) = weighted(self.memory_avg_bytes, self.data.memory_samples);
        let (other_memory, other_memory_n) =
            weighted(other.memory_avg_bytes, other.data.memory_samples);
        self.data.memory_samples = memory_n.saturating_add(other_memory_n);
        self.memory_avg_bytes = average(memory_sum + other_memory, self.data.memory_samples)
            .and_then(|avg| u64::try_from(avg).ok());
        self.memory_max_bytes = max_option(self.memory_max_bytes, other.memory_max_bytes);
        self.data.minutes = self.data.minutes.saturating_add(other.data.minutes);
        for start in &other.data.starts {
            if !self.data.starts.contains(start) && self.data.starts.len() < MAX_STARTS {
                self.data.starts.push(*start);
            }
        }
        self.data.starts.sort_unstable();
        for (name, value) in &other.data.counters {
            let total = self.data.counters.entry(name.clone()).or_default();
            *total = total.saturating_add(*value);
        }
        for (family, labels) in &other.data.labelled {
            let into = self.data.labelled.entry(family.clone()).or_default();
            for (label, value) in labels {
                add_label(into, label.clone(), *value);
            }
        }
        if !other.data.latency.is_empty() {
            self.data
                .latency
                .resize(LATENCY_SLOTS.max(other.data.latency.len()), 0);
            for (slot, value) in other.data.latency.iter().enumerate() {
                self.data.latency[slot] = self.data.latency[slot].saturating_add(*value);
            }
        }
        for (name, value) in &other.data.gauges {
            self.data
                .gauges
                .entry(name.clone())
                .and_modify(|current| *current = (*current).max(*value))
                .or_insert(*value);
        }
    }

    /// An empty rollup covering `span_secs` from `start_secs`.
    pub fn empty(start_secs: i64, span_secs: i64) -> Self {
        Self {
            start_secs,
            span_secs,
            ..Self::default()
        }
    }
}

fn weighted(avg: Option<u64>, samples: u32) -> (u128, u32) {
    match avg {
        Some(avg) if samples > 0 => (u128::from(avg) * u128::from(samples), samples),
        _ => (0, 0),
    }
}

fn average(sum: u128, samples: u32) -> Option<u128> {
    (samples > 0).then(|| sum / u128::from(samples))
}

fn max_option<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Request latency at quantile `q` (0..=1) as the upper bound of the bucket
/// that reaches it. `None` without requests or when it is past the last bound.
pub fn latency_quantile_ms(buckets: &[u64], q: f64) -> Option<u64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let target = ((total as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0u64;
    for (slot, count) in buckets.iter().enumerate() {
        seen = seen.saturating_add(*count);
        if seen >= target {
            return LATENCY_BOUNDS_MS.get(slot).copied();
        }
    }
    None
}

/// Folds five-second samples into minute rollups.
#[derive(Default)]
pub struct RollupBuilder {
    minute: Option<i64>,
    current: Rollup,
    cpu_sum: u64,
    memory_sum: u128,
}

impl RollupBuilder {
    /// Records one sample taken at `now_secs`. Returns the previous minute
    /// when this sample starts a new one; its counters are the activity
    /// recorded since the previous completed minute.
    pub fn observe(&mut self, now_secs: i64, sample: ContainerSample) -> Option<Rollup> {
        let minute = now_secs - now_secs.rem_euclid(MINUTE_SECS);
        let finished = match self.minute {
            Some(open) if open < minute => Some(self.finish()),
            Some(open) if open > minute => {
                // The wall clock stepped back: drop the open minute rather
                // than record activity under a time it did not happen.
                self.reset();
                None
            }
            _ => None,
        };
        if self.minute.is_none() {
            self.minute = Some(minute);
            self.current = Rollup::empty(minute, MINUTE_SECS);
            self.current.data.minutes = 1;
            self.current.data.starts = vec![process_started_at_ms()];
        }
        if let Some(cpu) = sample.cpu_tenths {
            self.cpu_sum = self.cpu_sum.saturating_add(u64::from(cpu));
            self.current.data.cpu_samples += 1;
            self.current.cpu_max_tenths = max_option(self.current.cpu_max_tenths, Some(cpu));
        }
        if let Some(memory) = sample.memory {
            self.memory_sum = self
                .memory_sum
                .saturating_add(u128::from(memory.program_bytes));
            self.current.data.memory_samples += 1;
            self.current.memory_max_bytes =
                max_option(self.current.memory_max_bytes, Some(memory.program_bytes));
        }
        for (name, value) in gauges() {
            self.current
                .data
                .gauges
                .entry(name)
                .and_modify(|peak| *peak = (*peak).max(value))
                .or_insert(value);
        }
        finished
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn finish(&mut self) -> Rollup {
        let mut rollup = std::mem::take(&mut self.current);
        let cpu_sum = std::mem::take(&mut self.cpu_sum);
        let memory_sum = std::mem::take(&mut self.memory_sum);
        self.minute = None;
        rollup.cpu_avg_tenths = average(u128::from(cpu_sum), rollup.data.cpu_samples)
            .and_then(|avg| u32::try_from(avg).ok());
        rollup.memory_avg_bytes =
            average(memory_sum, rollup.data.memory_samples).and_then(|avg| u64::try_from(avg).ok());
        let taken = std::mem::replace(
            &mut *activity(),
            Activity {
                counters: BTreeMap::new(),
                labelled: BTreeMap::new(),
                latency: [0; LATENCY_SLOTS],
            },
        );
        rollup.data.counters = taken
            .counters
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect();
        rollup.data.labelled = taken
            .labelled
            .into_iter()
            .map(|(family, labels)| (family.to_owned(), labels))
            .collect();
        if taken.latency.iter().any(|count| *count > 0) {
            rollup.data.latency = taken.latency.to_vec();
        }
        rollup
    }
}

/// Keeps a completed minute for the bot to collect. Oldest minutes drop first.
pub fn publish_recent(rollup: Rollup) {
    let mut recent = RECENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    recent.retain(|kept| kept.start_secs != rollup.start_secs);
    recent.push_back(rollup);
    while recent.len() > RECENT_MINUTES {
        recent.pop_front();
    }
}

/// Completed minutes kept by [`publish_recent`], oldest first.
pub fn recent_rollups() -> Vec<Rollup> {
    RECENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Unix milliseconds when this process first asked; call early in `main`.
pub fn process_started_at_ms() -> i64 {
    static STARTED: OnceLock<i64> = OnceLock::new();
    *STARTED.get_or_init(unix_ms_now)
}

pub fn unix_ms_now() -> i64 {
    unix_ms(SystemTime::now())
}

pub fn unix_ms(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Static facts about this process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub version: String,
    /// Source commit the binary was built from, when the build recorded it.
    pub commit: Option<String>,
    pub started_at_ms: i64,
    /// Platform pod (or container host name) and region identifiers.
    pub pod: Option<String>,
    pub region: Option<String>,
    pub ready: bool,
    /// The cgroup CPU quota; `None` when it could not be read.
    pub cpu_limit: Option<CpuLimit>,
    /// CPUs this process may be scheduled on (respects affinity and quota).
    pub schedulable_cpus: Option<u32>,
}

impl ProcessInfo {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            commit: option_env!("JMODGUARD_BUILD_SHA")
                .and_then(|sha| identifier(sha).filter(|sha| sha != "unknown")),
            started_at_ms: process_started_at_ms(),
            pod: env_identifier("BUNNYNET_MC_PODID").or_else(|| env_identifier("HOSTNAME")),
            region: env_identifier("BUNNYNET_MC_REGION"),
            ready: crate::process_ready(),
            cpu_limit: crate::cgroup::read_process_cpu_limit(),
            schedulable_cpus: std::thread::available_parallelism()
                .ok()
                .and_then(|cpus| u32::try_from(cpus.get()).ok()),
        }
    }
}

fn env_identifier(name: &str) -> Option<String> {
    identifier(&std::env::var(name).ok()?)
}

/// Short printable identifiers only; anything else is treated as unknown.
fn identifier(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (!raw.is_empty() && raw.len() <= 64 && raw.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| raw.to_owned())
}

/// Bytes in a directory this process writes. `bytes` is `None` when it could
/// not be measured; `complete` is false when the walk stopped at its bound.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUse {
    /// `tmp`, `logs` or `container_root`.
    pub kind: String,
    pub path: Option<String>,
    pub bytes: Option<u64>,
    pub limit_bytes: Option<u64>,
    pub complete: bool,
    /// Why a value is unavailable.
    pub reason: Option<String>,
}

/// Measures the temporary directory and, when file logging is on, the log
/// directory. The container's root filesystem is reported unavailable: its
/// free/used totals belong to the host, and the writable layer cannot be
/// measured from inside without walking every file.
pub fn measure_disk(log_dir: Option<&Path>) -> Vec<DiskUse> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut disks = vec![DiskUse {
        kind: "container_root".into(),
        path: Some("/".into()),
        bytes: None,
        limit_bytes: None,
        complete: false,
        reason: Some("host_filesystem".into()),
    }];
    let mut measure = |kind: &str, path: PathBuf| {
        let (bytes, complete) = match tree_bytes(&path) {
            Some((bytes, complete)) => (Some(bytes), complete),
            None => (None, false),
        };
        disks.push(DiskUse {
            kind: kind.into(),
            path: Some(path.display().to_string()),
            bytes,
            limit_bytes: tmpfs_limit(&mountinfo, &path),
            complete,
            reason: bytes.is_none().then(|| "unreadable".into()),
        });
    };
    measure("tmp", std::env::temp_dir());
    if let Some(dir) = log_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        measure("logs", dir.to_path_buf());
    }
    disks
}

/// Apparent file bytes under `root`, without following links. `false` when
/// the walk stopped at its entry or depth bound.
fn tree_bytes(root: &Path) -> Option<(u64, bool)> {
    let meta = std::fs::symlink_metadata(root).ok()?;
    if !meta.is_dir() {
        return Some((meta.len(), true));
    }
    let mut total = 0u64;
    let mut visited = 0usize;
    let mut complete = true;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            complete = false;
            continue;
        };
        for entry in entries {
            visited += 1;
            if visited > DISK_WALK_LIMIT {
                return Some((total, false));
            }
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let Ok(meta) = entry.path().symlink_metadata() else {
                complete = false;
                continue;
            };
            if meta.is_dir() {
                if depth + 1 > DISK_WALK_DEPTH {
                    complete = false;
                } else {
                    pending.push((entry.path(), depth + 1));
                }
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Some((total, complete))
}

/// The `size=` of a tmpfs mounted exactly at `path`, in bytes.
fn tmpfs_limit(mountinfo: &str, path: &Path) -> Option<u64> {
    let path = path.to_str()?.trim_end_matches('/');
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let point = fields.get(4)?.trim_end_matches('/');
        if point != path || fields.get(separator + 1) != Some(&"tmpfs") {
            continue;
        }
        let options = fields.get(separator + 3)?;
        return options
            .split(',')
            .find_map(|option| option.strip_prefix("size="))
            .and_then(parse_size);
    }
    None
}

fn parse_size(text: &str) -> Option<u64> {
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((index, _)) => text.split_at(index),
        None => (text, ""),
    };
    let value: u64 = digits.parse().ok()?;
    let scale = match unit.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        _ => return None,
    };
    value.checked_mul(scale)
}

/// Keeps the latest [`measure_disk`] result for [`ProcessStatus::current`].
pub fn set_disk(disk: Vec<DiskUse>) {
    *DISK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = disk;
}

/// Current view of one process: static facts, the latest cgroup sample and
/// gauges. `sampled_at_ms` is when the sample was taken.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub sampled_at_ms: Option<i64>,
    /// How often this status is refreshed; older than two intervals is stale.
    pub interval_ms: u64,
    pub info: ProcessInfo,
    pub cpu_tenths: Option<u32>,
    pub memory: Option<MemoryUse>,
    #[serde(default)]
    pub disk: Vec<DiskUse>,
    #[serde(default)]
    pub gauges: BTreeMap<String, i64>,
}

impl ProcessStatus {
    /// This process's status now, with the last disk measurement kept by
    /// [`set_disk`] (walking directories is left to the minute task).
    pub fn current(interval_ms: u64) -> Self {
        let sample = crate::cgroup::latest_container_sample();
        Self {
            sampled_at_ms: crate::cgroup::latest_container_sample_at().map(unix_ms),
            interval_ms,
            info: ProcessInfo::current(),
            cpu_tenths: sample.cpu_tenths,
            memory: sample.memory,
            disk: DISK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            gauges: gauges(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::{MemoryLimit, MemorySource};

    fn sample(cpu: Option<u32>, memory: Option<u64>) -> ContainerSample {
        ContainerSample {
            memory: memory.map(|program_bytes| MemoryUse {
                program_bytes,
                limit: MemoryLimit::Bytes(1 << 30),
                source: MemorySource::CgroupStat,
            }),
            cpu_tenths: cpu,
        }
    }

    #[test]
    fn builder_folds_one_minute_and_keeps_missing_readings_unavailable() {
        let mut builder = RollupBuilder::default();
        assert!(
            builder
                .observe(120, sample(Some(100), Some(1000)))
                .is_none()
        );
        assert!(builder.observe(125, sample(None, Some(3000))).is_none());
        assert!(builder.observe(130, sample(Some(300), None)).is_none());
        let minute = builder
            .observe(180, sample(None, None))
            .expect("minute closes");
        assert_eq!(minute.start_secs, 120);
        assert_eq!(minute.span_secs, MINUTE_SECS);
        assert_eq!(minute.cpu_avg_tenths, Some(200));
        assert_eq!(minute.cpu_max_tenths, Some(300));
        assert_eq!(minute.memory_avg_bytes, Some(2000));
        assert_eq!(minute.memory_max_bytes, Some(3000));
        assert_eq!(minute.data.cpu_samples, 2);
        assert_eq!(minute.data.minutes, 1);
        assert_eq!(minute.data.starts, vec![process_started_at_ms()]);
        let empty = builder
            .observe(240, sample(None, None))
            .expect("next minute closes");
        assert_eq!(
            empty.cpu_avg_tenths, None,
            "no reading is unavailable, not 0"
        );
        assert_eq!(empty.memory_max_bytes, None);
    }

    #[test]
    fn builder_records_peak_gauges_across_samples_in_one_minute() {
        let mut builder = RollupBuilder::default();
        set_gauge("shard_latency_ms", 900);
        assert!(builder.observe(120, sample(None, None)).is_none());
        set_gauge("shard_latency_ms", 50);
        assert!(builder.observe(140, sample(None, None)).is_none());
        let minute = builder
            .observe(180, sample(None, None))
            .expect("minute closes");
        assert_eq!(minute.data.gauges["shard_latency_ms"], 900);
        clear_gauge("shard_latency_ms");
    }

    #[test]
    fn absorb_weights_averages_and_bounds_labels() {
        let mut hour = Rollup::empty(3600, HOUR_SECS);
        let mut a = Rollup::empty(3600, MINUTE_SECS);
        a.cpu_avg_tenths = Some(100);
        a.cpu_max_tenths = Some(150);
        a.data.cpu_samples = 3;
        a.data.minutes = 1;
        a.data.starts = vec![5];
        a.data.counters.insert("db_ops".into(), 4);
        a.data.latency = vec![1; LATENCY_SLOTS];
        a.data.gauges.insert("guilds".into(), 3);
        let mut b = Rollup::empty(3660, MINUTE_SECS);
        b.cpu_avg_tenths = Some(400);
        b.cpu_max_tenths = Some(500);
        b.data.cpu_samples = 1;
        b.data.minutes = 1;
        b.data.starts = vec![5, 9];
        b.data.counters.insert("db_ops".into(), 6);
        b.data.gauges.insert("guilds".into(), 2);
        b.data.labelled.insert(
            "gateway".into(),
            (0..MAX_LABELS + 5)
                .map(|i| (format!("e{i:02}"), 1))
                .collect(),
        );
        hour.absorb(&a);
        hour.absorb(&b);
        assert_eq!(hour.cpu_avg_tenths, Some(175));
        assert_eq!(hour.cpu_max_tenths, Some(500));
        assert_eq!(hour.memory_avg_bytes, None);
        assert_eq!(hour.data.minutes, 2);
        assert_eq!(hour.data.starts, vec![5, 9]);
        assert_eq!(hour.data.counters["db_ops"], 10);
        assert_eq!(hour.data.gauges["guilds"], 3);
        assert_eq!(hour.data.latency.iter().sum::<u64>(), LATENCY_SLOTS as u64);
        let gateway = &hour.data.labelled["gateway"];
        assert_eq!(gateway.len(), MAX_LABELS + 1);
        assert_eq!(gateway["other"], 5);
    }

    #[test]
    fn latency_quantiles_use_bucket_bounds() {
        let mut buckets = vec![0; LATENCY_SLOTS];
        assert_eq!(latency_quantile_ms(&buckets, 0.5), None);
        buckets[0] = 50;
        buckets[4] = 45;
        buckets[LATENCY_SLOTS - 1] = 5;
        assert_eq!(latency_quantile_ms(&buckets, 0.5), Some(5));
        assert_eq!(latency_quantile_ms(&buckets, 0.95), Some(100));
        assert_eq!(
            latency_quantile_ms(&buckets, 0.99),
            None,
            "past the last bound"
        );
    }

    #[test]
    fn recent_minutes_are_bounded_and_replace_a_republished_minute() {
        for minute in 0..(RECENT_MINUTES as i64 + 5) {
            publish_recent(Rollup::empty(minute * 60, MINUTE_SECS));
        }
        publish_recent(Rollup::empty((RECENT_MINUTES as i64 + 4) * 60, MINUTE_SECS));
        let recent = recent_rollups();
        assert_eq!(recent.len(), RECENT_MINUTES);
        assert_eq!(
            recent.last().unwrap().start_secs,
            (RECENT_MINUTES as i64 + 4) * 60
        );
    }

    #[test]
    fn disk_walk_measures_files_and_reports_tmpfs_size() {
        let root = std::env::temp_dir().join(format!("jmg-disk-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("one"), [0u8; 10]).unwrap();
        std::fs::write(root.join("a/b/two"), [0u8; 32]).unwrap();
        assert_eq!(tree_bytes(&root), Some((42, true)));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(tree_bytes(&root), None);
        let mountinfo = "30 25 0:27 / /tmp rw,nosuid shared:5 - tmpfs tmpfs rw,size=65536k\n\
                         31 25 0:28 / / rw - overlay overlay rw\n";
        assert_eq!(tmpfs_limit(mountinfo, Path::new("/tmp")), Some(64 << 20));
        assert_eq!(
            tmpfs_limit(mountinfo, Path::new("/")),
            None,
            "overlay root has no own limit"
        );
        let disks = measure_disk(None);
        assert_eq!(disks[0].kind, "container_root");
        assert_eq!(
            disks[0].bytes, None,
            "host filesystem totals are never reported"
        );
        assert!(disks.iter().any(|disk| disk.kind == "tmp"));
    }

    #[test]
    fn labels_are_trimmed_and_bounded() {
        assert_eq!(bounded_label("  "), "unknown");
        assert_eq!(bounded_label(&"é".repeat(100)).len(), MAX_LABEL_BYTES);
        assert_eq!(identifier("pod-1"), Some("pod-1".into()));
        assert_eq!(identifier("has space"), None);
    }
}
