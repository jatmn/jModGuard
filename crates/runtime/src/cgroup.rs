//! This process's own cgroup CPU and memory.
//!
//! Readings come from cgroup v2 (`memory.current`, `memory.stat`,
//! `memory.max`, `cpu.stat`) or the v1 files when those are absent.
//! Membership is `/proc/self/cgroup`. Host-wide memory and processor totals
//! are not consulted. Nothing here writes a database or calls a provider.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

const MAX_CGROUP_FILE_BYTES: usize = 64 * 1024;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// cgroup v1 reports "no limit" as a page-rounded value at least this large.
const V1_UNLIMITED_BYTES: u64 = 1 << 62;

static LATEST: Mutex<ContainerSample> = Mutex::new(ContainerSample::unavailable());

/// Real program memory: `memory.current` minus `inactive_file`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryUse {
    pub program_bytes: u64,
    pub limit: MemoryLimit,
}

/// `memory.max`. The v2 token `max`, and a v1 limit of at least 2^62 bytes,
/// mean the cgroup has no limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    let text = read_limited(Path::new("/proc/self/cgroup"));
    read_at_mount(text.as_deref(), Path::new("/sys/fs/cgroup"))
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

struct CandidateDirs {
    memory: Vec<PathBuf>,
    cpu: Vec<PathBuf>,
}

struct Observation {
    reading: CgroupReading,
    at: Instant,
}

#[derive(Default)]
struct CpuRate {
    previous: Option<(Duration, Instant)>,
}

enum CpuStep {
    Percent(u32),
    Pending,
    Unchanged,
}

impl CpuRate {
    fn reset(&mut self) {
        self.previous = None;
    }

    fn observe(&mut self, usage: Duration, at: Instant) -> CpuStep {
        let Some((previous_usage, previous_at)) = self.previous else {
            self.previous = Some((usage, at));
            return CpuStep::Pending;
        };
        let wall = at.saturating_duration_since(previous_at);
        if wall.is_zero() {
            return CpuStep::Unchanged;
        }
        if usage < previous_usage {
            self.previous = Some((usage, at));
            return CpuStep::Pending;
        }
        let tenths = usage_tenths(usage.saturating_sub(previous_usage), wall);
        self.previous = Some((usage, at));
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
    let reading = read_process_cgroup();
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

fn read_at_mount(proc_text: Option<&str>, mount: &Path) -> CgroupReading {
    if let Some(text) = proc_text {
        let candidates = candidates_from_proc(text, mount);
        return CgroupReading {
            memory: candidates.memory.iter().find_map(|dir| read_memory(dir)),
            cpu: candidates.cpu.iter().find_map(|dir| read_cpu(dir)),
        };
    }
    CgroupReading {
        memory: read_memory(mount),
        cpu: read_cpu(mount),
    }
}

fn candidates_from_proc(text: &str, mount: &Path) -> CandidateDirs {
    let mut memory = Vec::new();
    let mut cpu = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some((id, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((controllers, path)) = rest.split_once(':') else {
            continue;
        };
        let Some(unified) = join_cgroup(mount, path) else {
            continue;
        };
        if id == "0" && controllers.is_empty() {
            memory.push(unified.clone());
            cpu.push(unified);
            continue;
        }
        let names: Vec<&str> = controllers
            .split(',')
            .filter(|name| !name.is_empty())
            .collect();
        if names.contains(&"memory") {
            if let Some(v1) = join_cgroup(&mount.join("memory"), path) {
                memory.push(v1);
            }
            memory.push(unified.clone());
        }
        if names.contains(&"cpu") || names.contains(&"cpuacct") {
            if let Some(acct) = join_cgroup(&mount.join("cpuacct"), path) {
                cpu.push(acct);
            }
            if let Some(combined) = join_cgroup(&mount.join("cpu,cpuacct"), path) {
                cpu.push(combined);
            }
            cpu.push(unified);
        }
    }
    CandidateDirs { memory, cpu }
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
        return None;
    }
    let text = read_limited(&dir.join("memory.usage_in_bytes"))?;
    parse_v1_memory(dir, &text)
}

fn parse_v2_memory(dir: &Path, current_text: &str) -> Option<MemoryUse> {
    memory_use(
        current_text,
        &read_limited(&dir.join("memory.stat"))?,
        &read_limited(&dir.join("memory.max"))?,
    )
}

fn parse_v1_memory(dir: &Path, current_text: &str) -> Option<MemoryUse> {
    memory_use(
        current_text,
        &read_limited(&dir.join("memory.stat"))?,
        &read_limited(&dir.join("memory.limit_in_bytes"))?,
    )
}

fn memory_use(current_text: &str, stat_text: &str, limit_text: &str) -> Option<MemoryUse> {
    let current = parse_u64_token(current_text)?;
    let inactive = stat_value(stat_text, "inactive_file")?;
    Some(MemoryUse {
        program_bytes: current.saturating_sub(inactive),
        limit: parse_memory_max(limit_text)?,
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
    fn missing_inactive_file_does_not_pretend_current_is_program_use() {
        let tree = temp_tree();
        write_files(
            &tree.path,
            &[
                ("memory.current", "1000\n"),
                ("memory.stat", "anon 1000\n"),
                ("memory.max", "max\n"),
            ],
        );
        assert!(read_cgroup(Some(&tree.path), None).memory.is_none());
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
        let reading = read_at_mount(Some("0::/app\n"), &tree.path);
        assert_eq!(reading.memory.unwrap().program_bytes, 900);
        assert_eq!(reading.cpu.unwrap().usage, Duration::from_micros(9));

        let elsewhere = read_at_mount(Some("0::/other\n"), &tree.path);
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
        let reading = read_at_mount(Some("2:cpu,cpuacct:/app\n1:memory:/app\n"), &tree.path);
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
        let reading = read_at_mount(Some("0::/../outside\n"), &tree.path);
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
        let reading = read_at_mount(None, &tree.path);
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
}
