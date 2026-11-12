//! This process's own cgroup CPU and memory.
//!
//! Readings come from cgroup v2 (`memory.current`, `memory.stat`,
//! `memory.max`, `cpu.stat`) or the v1 files when those are absent.
//! Membership is `/proc/self/cgroup` resolved with mount roots from
//! `/proc/self/mountinfo`. Host-wide memory and processor totals
//! are not consulted. Nothing here writes a database or calls a provider.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

const MAX_CGROUP_FILE_BYTES: usize = 64 * 1024;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// cgroup v1 reports "no limit" as a page-rounded value at least this large.
const V1_UNLIMITED_BYTES: u64 = 1 << 62;
const TRUNCATED: &str = "…(truncated)";
const KEY_MEMORY_FILES: [&str; 5] = [
    "memory.stat",
    "memory.current",
    "memory.max",
    "memory.usage_in_bytes",
    "memory.limit_in_bytes",
];

static LATEST: Mutex<ContainerSample> = Mutex::new(ContainerSample::unavailable());

/// Memory in use, and which input measured it (see [`MemorySource`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUse {
    pub program_bytes: u64,
    pub limit: MemoryLimit,
    #[serde(default)]
    pub source: MemorySource,
}

/// Where a [`MemoryUse`] came from. Older senders omit it and mean `CgroupStat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemorySource {
    /// cgroup usage minus inactive file cache from `memory.stat`.
    #[default]
    CgroupStat,
    /// `memory.current` or v1 `memory.usage_in_bytes` when `memory.stat` is
    /// missing or unreadable. This includes page cache.
    CgroupUsage,
}

/// `memory.max`. The v2 token `max`, and a v1 limit of at least 2^62 bytes,
/// mean the cgroup has no limit. An absent limit file also means no limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryLimit {
    Bytes(u64),
    Unlimited,
}

/// Cumulative CPU time from `cpu.stat` `usage_usec` or v1 `cpuacct.usage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuCounter {
    pub usage: Duration,
}

/// One directory's memory and CPU counters. `None` means that file set was not readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CgroupReading {
    pub memory: Option<MemoryUse>,
    pub cpu: Option<CpuCounter>,
}

/// Cached container use. CPU is in tenths of a percent (`124` is 12.4%) and
/// stays `None` until two background samples exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSample {
    pub memory: Option<MemoryUse>,
    pub cpu_tenths: Option<u32>,
}

impl ContainerSample {
    pub const fn unavailable() -> Self {
        Self {
            memory: None,
            cpu_tenths: None,
        }
    }
}

/// Read memory and CPU counters from cgroup directories.
///
/// Pass the same directory twice when v2 files, or a v1 memory and cpuacct
/// pair, live together. A missing or unreadable layout yields `None` fields.
pub fn read_cgroup(memory_dir: Option<&Path>, cpu_dir: Option<&Path>) -> CgroupReading {
    CgroupReading {
        memory: memory_dir.and_then(read_memory),
        cpu: cpu_dir.and_then(read_cpu),
    }
}

/// Read this process's cgroup. Unreadable layouts yield an empty reading.
pub fn read_process_cgroup() -> CgroupReading {
    let Some(text) = read_limited(Path::new("/proc/self/cgroup")) else {
        return read_mount_root(Path::new("/sys/fs/cgroup"));
    };
    let Some(mountinfo) = read_limited(Path::new("/proc/self/mountinfo")) else {
        return CgroupReading::default();
    };
    read_mounted_cgroup(&text, &mountinfo)
}

/// Latest sample for this process.
///
/// Copies an in-memory snapshot. Does not open cgroup files and does not wait
/// for the sampler. CPU is `None` until a percentage exists.
pub fn latest_container_sample() -> ContainerSample {
    let guard = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard
}

/// Start the process-local sampler. Later calls do nothing.
///
/// About every five seconds a blocking thread reads this process's cgroup and
/// publishes [`latest_container_sample`]. Call this from a Tokio runtime.
pub fn spawn_container_usage_sampler() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        tokio::spawn(sample_loop());
    });
}

struct Observation {
    reading: CgroupReading,
    at: Instant,
}

#[derive(Default)]
enum CpuRate {
    #[default]
    AwaitingBaseline,
    Tracking {
        usage: Duration,
        at: Instant,
    },
}

enum CpuStep {
    Percent(u32),
    Pending,
    Unchanged,
}

impl CpuRate {
    fn reset(&mut self) {
        *self = Self::AwaitingBaseline;
    }

    fn observe(&mut self, usage: Duration, at: Instant) -> CpuStep {
        let (previous_usage, previous_at) = match *self {
            Self::AwaitingBaseline => {
                *self = Self::Tracking { usage, at };
                return CpuStep::Pending;
            }
            Self::Tracking { usage, at } => (usage, at),
        };
        let wall = at.saturating_duration_since(previous_at);
        if wall.is_zero() {
            return CpuStep::Unchanged;
        }
        if usage < previous_usage {
            *self = Self::Tracking { usage, at };
            return CpuStep::Pending;
        }
        let tenths = usage_tenths(usage.saturating_sub(previous_usage), wall);
        *self = Self::Tracking { usage, at };
        CpuStep::Percent(tenths)
    }
}

fn usage_tenths(usage: Duration, wall: Duration) -> u32 {
    let wall_ns = wall.as_nanos();
    if wall_ns == 0 {
        return 0;
    }
    let tenths = usage.as_nanos().saturating_mul(1_000) / wall_ns;
    u32::try_from(tenths).unwrap_or(u32::MAX)
}

fn fold_sample(
    rate: &mut CpuRate,
    previous_cpu: Option<u32>,
    reading: CgroupReading,
    at: Instant,
) -> ContainerSample {
    let cpu_tenths = match reading.cpu {
        Some(counter) => match rate.observe(counter.usage, at) {
            CpuStep::Percent(tenths) => Some(tenths),
            CpuStep::Pending => None,
            CpuStep::Unchanged => previous_cpu,
        },
        None => {
            rate.reset();
            None
        }
    };
    ContainerSample {
        memory: reading.memory,
        cpu_tenths,
    }
}

fn publish(sample: ContainerSample) {
    let mut guard = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = sample;
}

fn observe_process() -> Observation {
    static LAYOUT: Once = Once::new();
    LAYOUT.call_once(|| {
        let layout = memory_layout(Path::new("/proc"), Path::new("/sys/fs/cgroup"));
        tracing::info!(
            proc_self_cgroup = ?layout.proc_cgroup,
            cgroup_mounts = ?layout.mounts,
            cgroup_dir = ?layout.dir,
            memory_files = ?layout.files,
            key_files = ?layout.key_files,
            "cgroup memory layout"
        );
    });
    let reading = read_process_cgroup();
    report_memory_source(reading.memory);
    Observation {
        reading,
        at: Instant::now(),
    }
}

async fn sample_loop() {
    let mut rate = CpuRate::default();
    let mut cpu_tenths = None;
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = crate::shutdown_signal() => break,
            _ = tick.tick() => {}
        }
        let observed = tokio::task::spawn_blocking(observe_process)
            .await
            .unwrap_or_else(|_| Observation {
                reading: CgroupReading::default(),
                at: Instant::now(),
            });
        let sample = fold_sample(&mut rate, cpu_tenths, observed.reading, observed.at);
        cpu_tenths = sample.cpu_tenths;
        publish(sample);
    }
}

/// Counters at a cgroup mount when `/proc/self/cgroup` itself cannot be read.
fn read_mount_root(mount: &Path) -> CgroupReading {
    CgroupReading {
        memory: read_memory(mount),
        cpu: read_cpu(mount),
    }
}

enum MountKind {
    Unified,
    Controllers {
        memory: bool,
        cpu: bool,
        cpuacct: bool,
    },
}

struct MountedCgroup {
    root: String,
    point: PathBuf,
    kind: MountKind,
}

impl MountedCgroup {
    fn is_unified(&self) -> bool {
        matches!(self.kind, MountKind::Unified)
    }

    fn is_v1_memory(&self) -> bool {
        matches!(self.kind, MountKind::Controllers { memory: true, .. })
    }

    fn is_v1_cpu(&self) -> bool {
        match self.kind {
            MountKind::Controllers { cpu, cpuacct, .. } => cpu || cpuacct,
            MountKind::Unified => false,
        }
    }

    fn has_cpuacct(&self) -> bool {
        matches!(self.kind, MountKind::Controllers { cpuacct: true, .. })
    }
}

struct CgroupEntry {
    unified: bool,
    memory: bool,
    cpu: bool,
    path: String,
}

fn read_mounted_cgroup(cgroup_text: &str, mountinfo: &str) -> CgroupReading {
    let (memory_dirs, cpu_dirs) = cgroup_dirs(cgroup_text, mountinfo);
    CgroupReading {
        memory: memory_dirs.iter().find_map(|dir| read_memory(dir)),
        cpu: cpu_dirs.iter().find_map(|dir| read_cpu(dir)),
    }
}

/// Candidate memory and CPU directories, in reading order.
fn cgroup_dirs(cgroup_text: &str, mountinfo: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mounts = parse_mountinfo(mountinfo);
    let mut memory_dirs = Vec::new();
    let mut cpu_dirs = Vec::new();
    for raw in cgroup_text.lines() {
        let Some(entry) = parse_cgroup_line(raw) else {
            continue;
        };
        if entry.unified {
            let matched = longest_root_mounts(&mounts, &entry.path, MountedCgroup::is_unified);
            let dirs = mount_dirs(&matched, &entry.path);
            memory_dirs.extend_from_slice(&dirs);
            cpu_dirs.extend(dirs);
            continue;
        }
        if entry.memory {
            let matched = longest_root_mounts(&mounts, &entry.path, MountedCgroup::is_v1_memory);
            memory_dirs.extend(mount_dirs(&matched, &entry.path));
        }
        if entry.cpu {
            let matched = longest_root_mounts(&mounts, &entry.path, MountedCgroup::is_v1_cpu);
            let ordered = prefer_cpuacct(&matched);
            cpu_dirs.extend(mount_dirs(&ordered, &entry.path));
        }
    }
    (memory_dirs, cpu_dirs)
}

fn parse_mountinfo(text: &str) -> Vec<MountedCgroup> {
    let mut mounts = Vec::new();
    for raw in text.lines() {
        if let Some(mount) = parse_mount_line(raw) {
            mounts.push(mount);
        }
    }
    mounts
}

// Field 4 is the root and field 5 is the mount point. Optional fields end at "-".
fn parse_mount_line(raw: &str) -> Option<MountedCgroup> {
    let fields: Vec<&str> = raw.split_whitespace().collect();
    let dash = fields.iter().position(|field| *field == "-")?;
    if dash < 6 || dash + 3 >= fields.len() {
        return None;
    }
    let root_raw = unescape_mount_field(fields[3])?;
    let point_raw = unescape_mount_field(fields[4])?;
    let root = normalize_mount_path(&root_raw)?;
    let point = normalize_mount_path(&point_raw)?;
    if !root.starts_with('/') || !point.starts_with('/') {
        return None;
    }
    let kind = match fields[dash + 1] {
        "cgroup2" => MountKind::Unified,
        "cgroup" => {
            let (memory, cpu, cpuacct) = super_controllers(fields[dash + 3]);
            MountKind::Controllers {
                memory,
                cpu,
                cpuacct,
            }
        }
        _ => return None,
    };
    Some(MountedCgroup {
        root,
        point: PathBuf::from(point),
        kind,
    })
}

fn unescape_mount_field(field: &str) -> Option<String> {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let digits = bytes.get(index + 1..index + 4)?;
        let mut value = 0u16;
        for digit in digits {
            if !matches!(*digit, b'0'..=b'7') {
                return None;
            }
            value = value * 8 + u16::from(*digit - b'0');
        }
        decoded.push(u8::try_from(value).ok()?);
        index += 4;
    }
    String::from_utf8(decoded).ok()
}

fn normalize_mount_path(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    if path == "/" {
        return Some("/".to_string());
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        Some("/".to_string())
    } else {
        Some(trimmed.to_string())
    }
}

// Controllers are exact super-option tokens, not mount-point path names.
fn super_controllers(opts: &str) -> (bool, bool, bool) {
    let mut memory = false;
    let mut cpu = false;
    let mut cpuacct = false;
    for token in opts.split(',') {
        match token {
            "memory" => memory = true,
            "cpu" => cpu = true,
            "cpuacct" => cpuacct = true,
            _ => {}
        }
    }
    (memory, cpu, cpuacct)
}

fn parse_cgroup_line(raw: &str) -> Option<CgroupEntry> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    let (id, rest) = line.split_once(':')?;
    let (controllers, path) = rest.split_once(':')?;
    if id == "0" && controllers.is_empty() {
        return Some(CgroupEntry {
            unified: true,
            memory: false,
            cpu: false,
            path: path.to_string(),
        });
    }
    let memory = controller_listed(controllers, "memory");
    let cpu = controller_listed(controllers, "cpu") || controller_listed(controllers, "cpuacct");
    if !(memory || cpu) {
        return None;
    }
    Some(CgroupEntry {
        unified: false,
        memory,
        cpu,
        path: path.to_string(),
    })
}

fn controller_listed(controllers: &str, wanted: &str) -> bool {
    for name in controllers.split(',') {
        if name == wanted {
            return true;
        }
    }
    false
}

fn cgroup_relative<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let path = path.trim();
    if !path.starts_with('/') || !root.starts_with('/') {
        return None;
    }
    if root == "/" {
        return Some(path);
    }
    if path == root {
        return Some("/");
    }
    let rest = path.strip_prefix(root)?;
    rest.starts_with('/').then_some(rest)
}

// Only the longest matching root. A shorter root is not a fallback.
fn longest_root_mounts<'a>(
    mounts: &'a [MountedCgroup],
    cgroup_path: &str,
    mut accept: impl FnMut(&MountedCgroup) -> bool,
) -> Vec<&'a MountedCgroup> {
    let mut best_len = 0usize;
    let mut matched = Vec::new();
    for mount in mounts {
        if !accept(mount) || cgroup_relative(cgroup_path, &mount.root).is_none() {
            continue;
        }
        let len = mount.root.len();
        if len > best_len {
            best_len = len;
            matched.clear();
        }
        if len == best_len {
            matched.push(mount);
        }
    }
    matched
}

// Same-root CPU mounts: cpuacct before a cpu-only mount, in mountinfo order.
fn prefer_cpuacct<'a>(mounts: &[&'a MountedCgroup]) -> Vec<&'a MountedCgroup> {
    let mut preferred = Vec::new();
    let mut rest = Vec::new();
    for mount in mounts {
        if mount.has_cpuacct() {
            preferred.push(*mount);
        } else {
            rest.push(*mount);
        }
    }
    preferred.extend(rest);
    preferred
}

fn mount_dirs(mounts: &[&MountedCgroup], cgroup_path: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for mount in mounts {
        let Some(relative) = cgroup_relative(cgroup_path, &mount.root) else {
            continue;
        };
        if let Some(dir) = join_cgroup(&mount.point, relative) {
            dirs.push(dir);
        }
    }
    dirs
}

fn join_cgroup(mount: &Path, proc_path: &str) -> Option<PathBuf> {
    let relative = proc_path.trim();
    if !relative.starts_with('/') {
        return None;
    }
    let relative = relative.trim_start_matches('/');
    if relative.is_empty() {
        return Some(mount.to_path_buf());
    }
    let mut out = mount.to_path_buf();
    for part in relative.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

fn read_memory(dir: &Path) -> Option<MemoryUse> {
    let v2_path = dir.join("memory.current");
    if let Some(text) = read_limited(&v2_path) {
        return parse_v2_memory(dir, &text);
    }
    if v2_path.exists() {
        return memory_input(None, "memory.current");
    }
    let v1_path = dir.join("memory.usage_in_bytes");
    let name = missing_memory_usage_name(
        v1_path.exists(),
        dir.join("memory.stat").exists(),
        dir.join("cgroup.controllers").exists(),
    );
    let text = memory_input(read_limited(&v1_path), name)?;
    parse_v1_memory(dir, &text)
}

fn missing_memory_usage_name(
    has_v1_usage: bool,
    has_stat: bool,
    has_controllers: bool,
) -> &'static str {
    if has_controllers || (has_stat && !has_v1_usage) {
        "memory.current"
    } else {
        "memory.usage_in_bytes"
    }
}

static FIRST_MISSING: OnceLock<&'static str> = OnceLock::new();

// Remember only a fixed file/key name, never the resolved cgroup path or contents.
fn memory_input<T>(value: Option<T>, name: &'static str) -> Option<T> {
    if value.is_none() {
        let _ = FIRST_MISSING.set(name);
    }
    value
}

// Warn once, the first time memory is unavailable or needed a fallback source.
fn report_memory_source(memory: Option<MemoryUse>) {
    static REPORTED: Once = Once::new();
    let source = memory.map(|memory| memory.source);
    if source != Some(MemorySource::CgroupStat) {
        REPORTED.call_once(|| {
            let name = FIRST_MISSING.get().copied().unwrap_or("none");
            let outcome = source.map_or("unavailable", |_| "fallback");
            tracing::warn!(
                name,
                ?source,
                "container memory {outcome}: cgroup file or key missing, unreadable, or invalid"
            );
        });
    }
}

fn read_memory_limit(dir: &Path, name: &'static str) -> Option<MemoryLimit> {
    let path = dir.join(name);
    if let Some(text) = read_limited(&path) {
        return memory_input(parse_memory_max(&text), name);
    }
    // An unreadable existing file is not evidence of an unlimited cgroup.
    if matches!(path.try_exists(), Ok(false)) {
        return Some(MemoryLimit::Unlimited);
    }
    memory_input(None, name)
}

fn parse_v2_memory(dir: &Path, current_text: &str) -> Option<MemoryUse> {
    let current = memory_input(parse_u64_token(current_text), "memory.current")?;
    let Some(stat) = memory_input(read_limited(&dir.join("memory.stat")), "memory.stat") else {
        return usage_only(current, read_memory_limit(dir, "memory.max")?);
    };
    // Type-based file/shmem counters cannot reconstruct the inactive file list:
    // file-backed unevictable pages are in file but not active/inactive_file,
    // and the combined unevictable counter does not identify their share.
    // Keep memory unavailable when the actual inactive_file counter is absent.
    memory_use(current, &stat, read_memory_limit(dir, "memory.max")?, None)
}

fn parse_v1_memory(dir: &Path, current_text: &str) -> Option<MemoryUse> {
    let current = memory_input(parse_u64_token(current_text), "memory.usage_in_bytes")?;
    let Some(stat) = memory_input(read_limited(&dir.join("memory.stat")), "memory.stat") else {
        return usage_only(current, read_memory_limit(dir, "memory.limit_in_bytes")?);
    };
    let fallback = stat_value(&stat, "total_inactive_file");
    memory_use(
        current,
        &stat,
        read_memory_limit(dir, "memory.limit_in_bytes")?,
        fallback,
    )
}

// Without memory.stat there is no inactive file counter; report labeled raw usage.
fn usage_only(current: u64, limit: MemoryLimit) -> Option<MemoryUse> {
    Some(MemoryUse {
        program_bytes: current,
        limit,
        source: MemorySource::CgroupUsage,
    })
}

fn memory_use(
    current: u64,
    stat_text: &str,
    limit: MemoryLimit,
    fallback: Option<u64>,
) -> Option<MemoryUse> {
    // Keep malformed primary counters unavailable; fallback is for absent keys.
    let inactive = if stat_text
        .lines()
        .any(|line| line.split_whitespace().next() == Some("inactive_file"))
    {
        stat_value(stat_text, "inactive_file")
    } else {
        fallback
    };
    let inactive = memory_input(inactive, "inactive_file")?;
    Some(MemoryUse {
        program_bytes: current.saturating_sub(inactive),
        limit,
        source: MemorySource::CgroupStat,
    })
}

fn read_cpu(dir: &Path) -> Option<CpuCounter> {
    if let Some(text) = read_limited(&dir.join("cpu.stat"))
        && let Some(usec) = stat_value(&text, "usage_usec")
    {
        return Some(CpuCounter {
            usage: Duration::from_micros(usec),
        });
    }
    let nanos = parse_u64_token(&read_limited(&dir.join("cpuacct.usage"))?)?;
    Some(CpuCounter {
        usage: Duration::from_nanos(nanos),
    })
}

fn read_limited(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.by_ref()
        .take(u64::from(u32::try_from(MAX_CGROUP_FILE_BYTES).ok()? + 1))
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > MAX_CGROUP_FILE_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

fn parse_u64_token(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse().ok()
}

fn parse_memory_max(text: &str) -> Option<MemoryLimit> {
    let token = text.split_whitespace().next()?;
    if token == "max" {
        return Some(MemoryLimit::Unlimited);
    }
    let bytes: u64 = token.parse().ok()?;
    if bytes >= V1_UNLIMITED_BYTES {
        Some(MemoryLimit::Unlimited)
    } else {
        Some(MemoryLimit::Bytes(bytes))
    }
}

fn stat_value(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        if name == key {
            return parts.next()?.parse().ok();
        }
    }
    None
}

/// Startup diagnostic fields: one line each, at most 2 KiB together.
struct MemoryLayout {
    proc_cgroup: String,
    mounts: String,
    dir: String,
    files: String,
    key_files: String,
}

fn memory_layout(proc: &Path, mount_root: &Path) -> MemoryLayout {
    let cgroup = read_limited(&proc.join("self/cgroup"));
    let mountinfo = read_limited(&proc.join("self/mountinfo"));
    let dir = match (&cgroup, &mountinfo) {
        (None, _) => Some(mount_root.to_path_buf()),
        (Some(text), Some(info)) => cgroup_dirs(text, info).0.into_iter().next(),
        (Some(_), None) => None,
    };
    let mounts = mountinfo.as_deref().map_or("unreadable".into(), |info| {
        let lines = info.lines().filter(|line| {
            let fstype = line
                .split(" - ")
                .nth(1)
                .unwrap_or("")
                .split_whitespace()
                .next();
            matches!(fstype, Some("cgroup" | "cgroup2"))
        });
        lines.collect::<Vec<_>>().join("\n")
    });
    let (files, key_files) = dir
        .as_deref()
        .map_or(("none".into(), "none".into()), dir_files);
    let dir = dir.map_or("none".into(), |dir| dir.display().to_string());
    MemoryLayout {
        proc_cgroup: one_line(cgroup.as_deref().unwrap_or("unreadable"), 512),
        mounts: one_line(&mounts, 768),
        dir: one_line(&dir, 256),
        files: one_line(&files, 320),
        key_files: one_line(&key_files, 192),
    }
}

// Sorted memory* names, and whether each key file is readable, missing or unreadable.
fn dir_files(dir: &Path) -> (String, String) {
    let names = std::fs::read_dir(dir).map(|entries| {
        let names = entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok());
        let mut names: Vec<String> = names.filter(|name| name.starts_with("memory")).collect();
        names.sort();
        names.join(",")
    });
    let states = KEY_MEMORY_FILES.map(|name| {
        let path = dir.join(name);
        let state = match (read_limited(&path), path.try_exists()) {
            (Some(_), _) => "readable",
            (None, Ok(false)) => "missing",
            (None, _) => "unreadable",
        };
        format!("{name}={state}")
    });
    (
        names.unwrap_or_else(|_| "unreadable".into()),
        states.join(" "),
    )
}

// Joins lines with " | ", blanks other control characters and caps the byte length.
fn one_line(text: &str, cap: usize) -> String {
    let line = text
        .trim()
        .replace('\n', " | ")
        .replace(char::is_control, " ");
    if line.len() <= cap {
        return line;
    }
    let mut end = cap.saturating_sub(TRUNCATED.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATED}", &line[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempTree {
        path: PathBuf,
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn temp_tree() -> TempTree {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("jmg-cgroup-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempTree { path }
    }

    fn write_files(dir: &Path, files: &[(&str, &str)]) {
        for (name, body) in files {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, body).unwrap();
        }
    }

    fn v2_mount_line(root: &str, point: &Path) -> String {
        let point = point.to_str().unwrap();
        format!(
            "21 1 0:21 {root} {point} rw,nosuid,nodev,noexec,relatime shared:1 master:2 - cgroup2 cgroup rw,nsdelegate"
        )
    }

    fn v1_mount_line(root: &str, point: &Path, controllers: &str) -> String {
        let point = point.to_str().unwrap();
        format!(
            "36 35 0:31 {root} {point} rw,nosuid,nodev,noexec,relatime shared:1 master:2 - cgroup cgroup rw,{controllers}"
        )
    }

    #[test]
    fn v2_layout_parses_max_and_subtracts_inactive_file() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "anon 10\ninactive_file 100\nactive_file 5\n"),
                ("memory.max", "  max\n"),
                (
                    "cpu.stat",
                    "usage_usec 5000\nuser_usec 4000\nsystem_usec 1000\n",
                ),
            ],
        );
        let reading = read_cgroup(Some(&tree.path), Some(&tree.path));
        let memory = reading.memory.unwrap();
        assert_eq!(memory.program_bytes, 900);
        assert_eq!(memory.limit, MemoryLimit::Unlimited);
        assert_eq!(memory.source, MemorySource::CgroupStat);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(5_000));
    }

    #[test]
    fn v2_numeric_limit_and_saturated_subtraction() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "100\n"),
                ("memory.stat", "inactive_file 150\n"),
                ("memory.max", "8192\n"),
            ],
        );
        let memory = read_cgroup(Some(&tree.path), None).memory.unwrap();
        assert_eq!(memory.program_bytes, 0);
        assert_eq!(memory.limit, MemoryLimit::Bytes(8192));
    }

    #[test]
    fn v1_layout_parses_max_cpuacct_and_subtracts_inactive_file() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.usage_in_bytes", "3000\n"),
                ("memory.stat", "rss 2000\ninactive_file 250\n"),
                ("memory.limit_in_bytes", "max\n"),
                ("cpuacct.usage", "1500000\n"),
            ],
        );
        let reading = read_cgroup(Some(&tree.path), Some(&tree.path));
        let memory = reading.memory.unwrap();
        assert_eq!(memory.program_bytes, 2_750);
        assert_eq!(memory.limit, MemoryLimit::Unlimited);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_nanos(1_500_000));
    }

    #[test]
    fn v1_page_rounded_limit_is_unlimited_and_the_value_below_it_is_not() {
        let unlimited = temp_tree();
        write_files(
            &unlimited.path,
            &[
                ("memory.usage_in_bytes", "10\n"),
                ("memory.stat", "inactive_file 1\n"),
                ("memory.limit_in_bytes", "9223372036854771712\n"),
            ],
        );
        assert_eq!(
            read_cgroup(Some(&unlimited.path), None)
                .memory
                .unwrap()
                .limit,
            MemoryLimit::Unlimited
        );

        let limited = temp_tree();
        let below = (V1_UNLIMITED_BYTES - 1).to_string();
        write_files(
            &limited.path,
            &[
                ("memory.usage_in_bytes", "10\n"),
                ("memory.stat", "inactive_file 1\n"),
                ("memory.limit_in_bytes", &below),
            ],
        );
        assert_eq!(
            read_cgroup(Some(&limited.path), None).memory.unwrap().limit,
            MemoryLimit::Bytes(V1_UNLIMITED_BYTES - 1)
        );
    }

    #[test]
    fn neither_layout_readable_is_unavailable() {
        let empty = temp_tree();
        let reading = read_cgroup(Some(&empty.path), Some(&empty.path));
        assert_eq!(reading, CgroupReading::default());
        let sample = fold_sample(&mut CpuRate::default(), None, reading, Instant::now());
        assert_eq!(sample, ContainerSample::unavailable());

        let unrelated = temp_tree();
        write_files(
            &unrelated.path,
            &[("meminfo", "MemTotal: 1\n"), ("stat", "cpu 1 2 3\n")],
        );
        assert_eq!(
            read_cgroup(Some(&unrelated.path), Some(&unrelated.path)),
            CgroupReading::default()
        );
        assert_eq!(
            read_cgroup(Some(Path::new("/no/such/jmodguard-cgroup")), None),
            CgroupReading::default()
        );
    }

    #[test]
    fn v2_missing_memory_max_is_unlimited() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
            ],
        );
        assert_eq!(
            read_cgroup(Some(&tree.path), None).memory,
            Some(MemoryUse {
                program_bytes: 900,
                limit: MemoryLimit::Unlimited,
                source: MemorySource::CgroupStat,
            })
        );
    }

    #[test]
    fn v2_missing_inactive_file_with_unevictable_pages_is_unavailable() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                (
                    "memory.stat",
                    "anon 600\nfile 400\nshmem 300\nactive_file 50\nunevictable 50\ninactive_file 0\n",
                ),
                ("memory.max", "2048\n"),
                ("cpu.stat", "usage_usec 5000\n"),
            ],
        );
        assert_eq!(
            read_cgroup(Some(&tree.path), None).memory,
            Some(MemoryUse {
                program_bytes: 1000,
                limit: MemoryLimit::Bytes(2048),
                source: MemorySource::CgroupStat,
            })
        );
        // The 50 file-backed unevictable bytes are in file, but are neither
        // active nor inactive file cache. Removing only the primary counter
        // must not turn them into a cache deduction.
        write_files(
            &tree.path,
            &[(
                "memory.stat",
                "anon 600\nfile 400\nshmem 300\nactive_file 50\nunevictable 50\n",
            )],
        );
        let reading = read_cgroup(Some(&tree.path), Some(&tree.path));
        assert_eq!(reading.memory, None);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(5000));
    }

    #[test]
    fn missing_inactive_file_does_not_pretend_current_is_program_use() {
        let tree = temp_tree();
        // Neither partial nor complete type-based counters establish the list.
        for stat in [
            "anon 1000\n",
            "file 400\n",
            "active_file 150\n",
            "anon 600\nfile 400\nactive_file 50\n",
            "anon 600\nfile 400\nactive_file 50\nshmem 300\n",
            "anon 600\nfile 400\nactive_file 50\nshmem 300\nunevictable 0\n",
        ] {
            write_files(
                &tree.path,
                &[
                    ("memory.current", "1000\n"),
                    ("memory.stat", stat),
                    ("memory.max", "max\n"),
                ],
            );
            assert!(
                read_cgroup(Some(&tree.path), None).memory.is_none(),
                "{stat}"
            );
        }
    }

    #[test]
    fn missing_memory_usage_diagnostic_matches_layout() {
        for (has_v1_usage, has_stat, has_controllers, expected) in [
            (false, true, false, "memory.current"),
            (false, false, true, "memory.current"),
            (false, true, true, "memory.current"),
            (true, true, false, "memory.usage_in_bytes"),
            (true, false, false, "memory.usage_in_bytes"),
            (false, false, false, "memory.usage_in_bytes"),
        ] {
            assert_eq!(
                missing_memory_usage_name(has_v1_usage, has_stat, has_controllers),
                expected
            );
        }
    }

    #[test]
    fn root_with_only_memory_stat_and_cpu_stat_keeps_memory_unavailable() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                (
                    "memory.stat",
                    "anon 600\nfile 400\nactive_file 150\ninactive_file 250\n",
                ),
                ("cpu.stat", "usage_usec 5000\n"),
            ],
        );
        // Root memory.stat alone must not be treated as container usage.
        let reading = read_mount_root(&tree.path);
        assert_eq!(reading.memory, None);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(5000));
    }

    #[test]
    fn v1_missing_limit_is_unlimited() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.usage_in_bytes", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
            ],
        );
        assert_eq!(
            read_cgroup(Some(&tree.path), None).memory,
            Some(MemoryUse {
                program_bytes: 900,
                limit: MemoryLimit::Unlimited,
                source: MemorySource::CgroupStat,
            })
        );
    }

    #[test]
    fn v1_missing_inactive_file_uses_total_inactive_file() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.usage_in_bytes", "1000\n"),
                ("memory.stat", "rss 700\ntotal_inactive_file 250\n"),
                ("memory.limit_in_bytes", "2048\n"),
            ],
        );
        assert_eq!(
            read_cgroup(Some(&tree.path), None).memory,
            Some(MemoryUse {
                program_bytes: 750,
                limit: MemoryLimit::Bytes(2048),
                source: MemorySource::CgroupStat,
            })
        );
    }

    #[test]
    fn v2_inconsistent_file_counters_do_not_invent_memory_usage() {
        let tree = temp_tree();
        for stat in [
            "file 100\nactive_file 150\nshmem 0\n",
            "file 1500\nactive_file 100\nshmem 0\n",
            "file 400\nactive_file 50\nshmem 500\n",
        ] {
            write_files(
                &tree.path,
                &[
                    ("memory.current", "1000\n"),
                    ("memory.stat", stat),
                    ("memory.max", "max\n"),
                ],
            );
            assert_eq!(read_cgroup(Some(&tree.path), None).memory, None, "{stat}");
        }
    }

    #[test]
    fn v2_primary_inactive_file_ignores_shmem() {
        let tree = temp_tree();
        for shmem in ["", "shmem 300\n", "shmem 500\n"] {
            let stat = format!("file 400\nactive_file 50\ninactive_file 100\n{shmem}");
            write_files(
                &tree.path,
                &[("memory.current", "1000\n"), ("memory.stat", &stat)],
            );
            assert_eq!(
                read_cgroup(Some(&tree.path), None)
                    .memory
                    .unwrap()
                    .program_bytes,
                900
            );
        }
    }

    #[test]
    fn present_invalid_or_unreadable_memory_limits_stay_unavailable() {
        for (current, limit) in [
            ("memory.current", "memory.max"),
            ("memory.usage_in_bytes", "memory.limit_in_bytes"),
        ] {
            let tree = temp_tree();
            for text in ["invalid\n", ""] {
                write_files(
                    &tree.path,
                    &[
                        (current, "1000\n"),
                        ("memory.stat", "inactive_file 100\n"),
                        (limit, text),
                    ],
                );
                assert!(read_cgroup(Some(&tree.path), None).memory.is_none());
            }
            std::fs::remove_file(tree.path.join(limit)).unwrap();
            std::fs::create_dir(tree.path.join(limit)).unwrap();
            assert!(read_cgroup(Some(&tree.path), None).memory.is_none());
        }
    }

    #[test]
    fn v2_proc_path_is_used_and_a_different_root_is_not() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("app/memory.current", "1000\n"),
                ("app/memory.stat", "inactive_file 100\n"),
                ("app/memory.max", "max\n"),
                ("app/cpu.stat", "usage_usec 9\n"),
                ("memory.current", "5\n"),
                ("memory.stat", "inactive_file 0\n"),
                ("memory.max", "9\n"),
                ("cpu.stat", "usage_usec 1\n"),
            ],
        );
        let info = format!("{}\n", v2_mount_line("/", &tree.path));
        let reading = read_mounted_cgroup("0::/app\n", &info);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));

        let elsewhere = read_mounted_cgroup("0::/other\n", &info);
        assert_eq!(elsewhere, CgroupReading::default());
    }

    #[test]
    fn v1_proc_controllers_select_their_directories() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory/app/memory.usage_in_bytes", "80\n"),
                ("memory/app/memory.stat", "inactive_file 30\n"),
                ("memory/app/memory.limit_in_bytes", "4096\n"),
                ("cpuacct/app/cpuacct.usage", "2000\n"),
            ],
        );
        let info = format!(
            "{}\n{}\n",
            v1_mount_line("/", &tree.path.join("memory"), "memory"),
            v1_mount_line("/", &tree.path.join("cpuacct"), "cpuacct"),
        );
        let reading = read_mounted_cgroup("2:cpu,cpuacct:/app\n1:memory:/app\n", &info);
        assert_eq!(reading.memory.unwrap().program_bytes, 50);
        assert_eq!(reading.memory.unwrap().limit, MemoryLimit::Bytes(4096));
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_nanos(2_000));
    }

    #[test]
    fn proc_parent_segments_are_rejected() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 1\n"),
                ("memory.max", "max\n"),
            ],
        );
        let info = format!("{}\n", v2_mount_line("/", &tree.path));
        let reading = read_mounted_cgroup("0::/../outside\n", &info);
        assert_eq!(reading, CgroupReading::default());
    }

    #[test]
    fn unreadable_proc_can_use_the_mount_root() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "40\n"),
                ("memory.stat", "inactive_file 10\n"),
                ("memory.max", "max\n"),
            ],
        );
        let reading = read_mount_root(&tree.path);
        assert_eq!(reading.memory.unwrap().program_bytes, 30);
    }

    #[test]
    fn first_cpu_sample_is_not_a_percentage() {
        let mut rate = CpuRate::default();
        let started = Instant::now();
        let baseline = CgroupReading {
            memory: Some(MemoryUse {
                program_bytes: 900,
                limit: MemoryLimit::Unlimited,
                source: MemorySource::CgroupStat,
            }),
            cpu: Some(CpuCounter {
                usage: Duration::from_millis(1_000),
            }),
        };
        let first = fold_sample(&mut rate, None, baseline, started);
        assert_eq!(first.cpu_tenths, None);
        assert_eq!(first.memory.unwrap().program_bytes, 900);
        assert!(started.elapsed() < Duration::from_millis(200));

        let next = CgroupReading {
            memory: baseline.memory,
            cpu: Some(CpuCounter {
                usage: Duration::from_millis(1_250),
            }),
        };
        let second = fold_sample(
            &mut rate,
            first.cpu_tenths,
            next,
            started + Duration::from_secs(1),
        );
        assert_eq!(second.cpu_tenths, Some(250));

        let reset = CgroupReading {
            memory: None,
            cpu: Some(CpuCounter {
                usage: Duration::from_millis(1),
            }),
        };
        let third = fold_sample(
            &mut rate,
            second.cpu_tenths,
            reset,
            started + Duration::from_secs(2),
        );
        assert_eq!(third.cpu_tenths, None);
    }

    #[test]
    fn latest_sample_does_not_wait_and_starts_unavailable() {
        let started = Instant::now();
        let sample = latest_container_sample();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(sample, ContainerSample::unavailable());
    }

    #[test]
    fn reader_source_does_not_name_host_meminfo_or_stat() {
        let source = include_str!("cgroup.rs");
        let meminfo = ["/proc/", "meminfo"].concat();
        let host_stat = ["/proc/", "stat"].concat();
        assert!(
            !source.contains(&meminfo),
            "host memory file must not be read"
        );
        assert!(
            !source.contains(&host_stat),
            "host processor file must not be read"
        );
    }

    #[test]
    fn process_cgroup_read_does_not_panic() {
        let _ = read_process_cgroup();
    }

    #[test]
    fn v2_private_namespace_reads_the_mount_point() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        write_files(
            &tree.path.join("self"),
            &[
                ("memory.current", "4000\n"),
                ("memory.stat", "inactive_file 0\n"),
                ("memory.max", "10\n"),
                ("cpu.stat", "usage_usec 3\n"),
            ],
        );
        let info = format!("{}\n", v2_mount_line("/", &tree.path));
        let reading = read_mounted_cgroup("0::/\n", &info);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));
    }

    #[test]
    fn v1_docker_bind_resolves_to_the_mount_point() {
        let tree = temp_tree();
        let memory = tree.path.join("memory");
        let cpuacct = tree.path.join("cpuacct");
        write_files(
            &memory,
            &[
                ("memory.usage_in_bytes", "3000\n"),
                ("memory.stat", "inactive_file 250\n"),
                ("memory.limit_in_bytes", "max\n"),
            ],
        );
        write_files(&cpuacct, &[("cpuacct.usage", "1500000\n")]);
        write_files(
            &memory.join("docker/0123456789abcdef"),
            &[
                ("memory.usage_in_bytes", "9000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.limit_in_bytes", "4096\n"),
            ],
        );
        write_files(
            &cpuacct.join("docker/0123456789abcdef"),
            &[("cpuacct.usage", "42\n")],
        );
        let info = format!(
            "{}\n{}\n",
            v1_mount_line("/docker/0123456789abcdef", &memory, "memory"),
            v1_mount_line("/docker/0123456789abcdef", &cpuacct, "cpuacct"),
        );
        let reading = read_mounted_cgroup(
            "1:memory:/docker/0123456789abcdef\n2:cpuacct:/docker/0123456789abcdef\n",
            &info,
        );
        let got = reading.memory.unwrap();
        assert_eq!(got.program_bytes, 2_750);
        assert_eq!(got.limit, MemoryLimit::Unlimited);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_nanos(1_500_000));
    }

    #[test]
    fn v2_subtree_strips_the_mount_root() {
        let tree = temp_tree();
        write_files(
            &tree.path.join("bot"),
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        write_files(
            &tree.path,
            &[
                ("memory.current", "5\n"),
                ("memory.stat", "inactive_file 0\n"),
                ("memory.max", "9\n"),
                ("cpu.stat", "usage_usec 1\n"),
            ],
        );
        write_files(
            &tree.path.join("workload/bot"),
            &[
                ("memory.current", "50\n"),
                ("memory.stat", "inactive_file 5\n"),
                ("memory.max", "99\n"),
                ("cpu.stat", "usage_usec 7\n"),
            ],
        );
        let info = format!("{}\n", v2_mount_line("/workload", &tree.path));
        let reading = read_mounted_cgroup("0::/workload/bot\n", &info);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));
    }

    #[test]
    fn v2_path_outside_mount_root_is_unavailable() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        write_files(
            &tree.path.join("other"),
            &[
                ("memory.current", "50\n"),
                ("memory.stat", "inactive_file 5\n"),
                ("memory.max", "99\n"),
                ("cpu.stat", "usage_usec 7\n"),
            ],
        );
        let info = format!("{}\n", v2_mount_line("/workload", &tree.path));
        let reading = read_mounted_cgroup("0::/other\n", &info);
        assert_eq!(reading, CgroupReading::default());
    }

    #[test]
    fn mountinfo_without_cgroup_mount_is_unavailable() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        write_files(
            &tree.path.join("workload/bot"),
            &[
                ("memory.current", "50\n"),
                ("memory.stat", "inactive_file 5\n"),
                ("memory.max", "99\n"),
                ("cpu.stat", "usage_usec 7\n"),
            ],
        );
        let point = tree.path.to_str().unwrap();
        let info =
            format!("21 1 8:1 / {point} rw,relatime shared:1 master:2 - ext4 /dev/sda1 rw\n");
        let reading = read_mounted_cgroup("0::/workload/bot\n", &info);
        assert_eq!(reading, CgroupReading::default());
        assert_eq!(
            read_mounted_cgroup("0::/workload/bot\n", ""),
            CgroupReading::default()
        );
    }

    #[test]
    fn v1_memory_path_outside_mount_root_is_unavailable() {
        let tree = temp_tree();
        let memory = tree.path.join("memory");
        write_files(
            &memory,
            &[
                ("memory.usage_in_bytes", "3000\n"),
                ("memory.stat", "inactive_file 250\n"),
                ("memory.limit_in_bytes", "max\n"),
            ],
        );
        write_files(
            &memory.join("other"),
            &[
                ("memory.usage_in_bytes", "9000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.limit_in_bytes", "4096\n"),
            ],
        );
        let info = format!(
            "{}\n",
            v1_mount_line("/docker/0123456789abcdef", &memory, "memory")
        );
        let reading = read_mounted_cgroup("1:memory:/other\n", &info);
        assert!(reading.memory.is_none());
        assert_eq!(reading, CgroupReading::default());
    }

    #[test]
    fn unreadable_longest_mount_root_does_not_fall_back() {
        let tree = temp_tree();
        let parent = tree.path.join("parent");
        let specific = tree.path.join("specific");
        write_files(
            &parent.join("workload/bot"),
            &[
                ("memory.current", "5000\n"),
                ("memory.stat", "inactive_file 0\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 4\n"),
            ],
        );
        std::fs::create_dir_all(&specific).unwrap();
        let longest_first = format!(
            "{}\n{}\n",
            v2_mount_line("/workload", &specific),
            v2_mount_line("/", &parent),
        );
        let missing = read_mounted_cgroup("0::/workload/bot\n", &longest_first);
        assert_eq!(missing, CgroupReading::default());

        write_files(
            &specific.join("bot"),
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        let longest_second = format!(
            "{}\n{}\n",
            v2_mount_line("/", &parent),
            v2_mount_line("/workload", &specific),
        );
        let reading = read_mounted_cgroup("0::/workload/bot\n", &longest_second);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));
    }

    #[test]
    fn cpuacct_mount_is_preferred_over_an_earlier_cpu_mount() {
        let tree = temp_tree();
        write_files(
            &tree.path.join("cpu/app"),
            &[("cpu.stat", "usage_usec 111\n")],
        );
        write_files(
            &tree.path.join("cpuacct/app"),
            &[("cpuacct.usage", "1500000\n")],
        );
        let preferred = format!(
            "{}\n{}\n",
            v1_mount_line("/", &tree.path.join("cpu"), "cpu"),
            v1_mount_line("/", &tree.path.join("cpuacct"), "cpuacct"),
        );
        let reading = read_mounted_cgroup("2:cpu:/app\n", &preferred);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_nanos(1_500_000));

        std::fs::create_dir_all(tree.path.join("empty-acct")).unwrap();
        let fallback = format!(
            "{}\n{}\n",
            v1_mount_line("/", &tree.path.join("empty-acct"), "cpuacct"),
            v1_mount_line("/", &tree.path.join("cpu"), "cpu"),
        );
        let reading = read_mounted_cgroup("2:cpu:/app\n", &fallback);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(111));
    }

    #[test]
    fn mountinfo_octal_escapes_are_decoded() {
        let tree = temp_tree();
        let spaced = tree.path.join("a b");
        write_files(
            &spaced,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        write_files(
            &tree.path,
            &[
                ("memory.current", "5\n"),
                ("memory.stat", "inactive_file 0\n"),
                ("memory.max", "9\n"),
                ("cpu.stat", "usage_usec 1\n"),
            ],
        );
        let point = tree.path.to_str().unwrap();
        let info = format!(
            "21 1 0:21 /work\\040load/ {point}/a\\040b rw,relatime shared:1 master:2 - cgroup2 cgroup rw\n"
        );
        let reading = read_mounted_cgroup("0::/work load\n", &info);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));
    }

    #[test]
    fn truncated_or_non_octal_mountinfo_escape_skips_the_line() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.max", "max\n"),
                ("cpu.stat", "usage_usec 9\n"),
            ],
        );
        write_files(
            &tree.path.join("decoy"),
            &[
                ("memory.current", "5\n"),
                ("memory.stat", "inactive_file 0\n"),
                ("memory.max", "9\n"),
                ("cpu.stat", "usage_usec 1\n"),
            ],
        );
        let point = tree.path.to_str().unwrap();
        let decoy_path = tree.path.join("decoy");
        let decoy = decoy_path.to_str().unwrap();
        let only_truncated =
            format!("21 1 0:21 /\\04 {point} rw,relatime shared:1 master:2 - cgroup2 cgroup rw\n");
        assert_eq!(
            read_mounted_cgroup("0::/\n", &only_truncated),
            CgroupReading::default()
        );
        let only_non_octal =
            format!("21 1 0:21 /\\018 {point} rw,relatime shared:1 master:2 - cgroup2 cgroup rw\n");
        assert_eq!(
            read_mounted_cgroup("0::/\n", &only_non_octal),
            CgroupReading::default()
        );
        let mixed = format!(
            "20 1 0:20 /\\04 {decoy} rw,relatime shared:1 master:2 - cgroup2 cgroup rw\n\
             21 1 0:21 /\\018 {decoy} rw,relatime shared:1 master:2 - cgroup2 cgroup rw\n\
             22 1 0:22 / {point} rw,relatime shared:1 master:2 - cgroup2 cgroup rw\n"
        );
        let reading = read_mounted_cgroup("0::/\n", &mixed);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));
    }

    #[test]
    fn controller_is_super_option_not_mount_point_name() {
        let tree = temp_tree();
        write_files(
            &tree.path.join("memory/app"),
            &[
                ("memory.usage_in_bytes", "9000\n"),
                ("memory.stat", "inactive_file 100\n"),
                ("memory.limit_in_bytes", "4096\n"),
                ("cpuacct.usage", "2000\n"),
            ],
        );
        let info = format!(
            "{}\n",
            v1_mount_line("/", &tree.path.join("memory"), "cpuacct")
        );
        let reading = read_mounted_cgroup("1:memory:/app\n2:cpuacct:/app\n", &info);
        assert!(reading.memory.is_none());
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_nanos(2_000));
    }

    #[test]
    fn missing_memory_stat_uses_usage_and_limit_files() {
        let (v1_usage, v1_limit) = ("memory.usage_in_bytes", "memory.limit_in_bytes");
        for (usage, limit, text, bytes) in [
            ("memory.current", "memory.max", "4096", Some(4096)),
            ("memory.current", "memory.max", "max", None),
            (v1_usage, v1_limit, "4096", Some(4096)),
            (v1_usage, v1_limit, "9223372036854771712", None),
        ] {
            let tree = temp_tree();
            write_files(&tree.path, &[(usage, "1000\n"), (limit, text)]);
            let memory = read_cgroup(Some(&tree.path), None).memory.unwrap();
            let want = bytes.map_or(MemoryLimit::Unlimited, MemoryLimit::Bytes);
            assert_eq!((memory.program_bytes, memory.limit), (1000, want));
            assert_eq!(memory.source, MemorySource::CgroupUsage);
            write_files(&tree.path, &[(limit, "invalid\n")]);
            assert_eq!(read_cgroup(Some(&tree.path), None).memory, None);
        }
    }

    fn proc_fixture(cgroup_dir: &Path) -> TempTree {
        let proc = temp_tree();
        let info = format!("{}\n", v2_mount_line("/", cgroup_dir));
        write_files(
            &proc.path,
            &[("self/cgroup", "0::/\n"), ("self/mountinfo", &info)],
        );
        proc
    }

    #[test]
    fn memory_layout_is_one_capped_line_naming_the_dir_and_files() {
        let cgroup = temp_tree();
        write_files(
            &cgroup.path,
            &[("memory.current", "1\n"), ("memory.max", "max\n")],
        );
        std::fs::create_dir(cgroup.path.join("memory.stat")).unwrap();
        let proc = proc_fixture(&cgroup.path);
        let layout = memory_layout(&proc.path, &cgroup.path);
        assert_eq!(layout.dir, cgroup.path.display().to_string());
        assert_eq!(layout.files, "memory.current,memory.max,memory.stat");
        let states = "memory.stat=unreadable memory.current=readable memory.max=readable";
        assert!(layout.key_files.starts_with(states), "{}", layout.key_files);
        assert!(layout.key_files.ends_with("memory.limit_in_bytes=missing"));
        assert_eq!(layout.proc_cgroup, "0::/");
        assert!(layout.mounts.contains(" - cgroup2 "), "{}", layout.mounts);

        write_files(
            &proc.path,
            &[("self/cgroup", "0::/x\n".repeat(500).as_str())],
        );
        std::fs::remove_file(proc.path.join("self/mountinfo")).unwrap();
        let layout = memory_layout(&proc.path, &cgroup.path);
        assert!(layout.proc_cgroup.ends_with(TRUNCATED) && layout.proc_cgroup.len() == 512);
        assert!(!layout.proc_cgroup.contains('\n'));
        assert_eq!([layout.mounts, layout.dir], ["unreadable", "none"]);
        assert!(one_line(&"é\n".repeat(2000), 2048).len() <= 2048);
    }
}
