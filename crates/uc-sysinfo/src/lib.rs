//! Per-process CPU/RAM/GPU sampling (WP-06).
//!
//! Feeds the overlay's "info for selected task" panel and its system line
//! (plan §2.9). Nothing runs in the background: the caller (the overlay, at
//! 1 Hz while it is open) calls [`StatsSource::sample`] and each call measures
//! the time since the previous one. Drop the [`Sampler`] when the overlay
//! closes and it costs nothing.
//!
//! - CPU % comes from `GetProcessTimes` / `GetSystemTimes` deltas, scaled to
//!   all logical processors like Task Manager.
//! - RAM is the working set and private bytes from `GetProcessMemoryInfo`.
//! - GPU % comes from the PDH `\GPU Engine(*)\Utilization Percentage`
//!   counters. A process's GPU % is its busiest engine, the same rule Task
//!   Manager uses.
//!
//! CPU % and GPU % are `None` on the first sample of a process (there is no
//! previous value to diff against) and GPU % is `None` when the GPU counters
//! are unavailable.
//!
//! The pure parts (percent maths, PDH instance parsing, GPU aggregation) are
//! platform independent and unit tested. The Win32 sampler lives in `win.rs`;
//! on other platforms [`Sampler`] returns empty snapshots so dependants still
//! build.

use std::collections::HashMap;
use std::time::Duration;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::Sampler;

/// One process, as measured by one [`StatsSource::sample`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessStats {
    pub pid: u32,
    /// Executable file name, e.g. `eldenring.exe`, when it could be read.
    pub name: Option<String>,
    /// Share of all logical processors, 0–100. `None` on the first sample.
    pub cpu_percent: Option<f32>,
    /// Working set (physical RAM in use), in bytes.
    pub working_set_bytes: u64,
    /// Private bytes (commit charge), in bytes.
    pub private_bytes: u64,
    /// Busiest GPU engine used by this process, 0–100. `None` when unknown.
    pub gpu_percent: Option<f32>,
    /// Time since the process started.
    pub uptime: Duration,
}

/// Whole-machine totals for the overlay's system line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemStats {
    /// 0–100. `None` on the first sample.
    pub cpu_percent: Option<f32>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    /// Busiest GPU engine across all processes, 0–100. `None` when unknown.
    pub gpu_percent: Option<f32>,
}

/// The result of one sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub system: SystemStats,
    /// One entry per requested PID that could be read, in request order.
    /// PIDs that have exited or can't be opened are left out.
    pub processes: Vec<ProcessStats>,
}

impl Snapshot {
    pub fn process(&self, pid: u32) -> Option<&ProcessStats> {
        self.processes.iter().find(|p| p.pid == pid)
    }
}

/// What the overlay talks to, so it can be built and tested against
/// [`FixedStats`] instead of real counters.
pub trait StatsSource {
    /// Measure the system and the given processes since the previous call.
    fn sample(&mut self, pids: &[u32]) -> Snapshot;
}

/// A [`StatsSource`] that always returns the same numbers. For tests and for
/// running the overlay without real counters.
#[derive(Debug, Clone, Default)]
pub struct FixedStats(pub Snapshot);

impl StatsSource for FixedStats {
    fn sample(&mut self, pids: &[u32]) -> Snapshot {
        Snapshot {
            system: self.0.system.clone(),
            processes: pids
                .iter()
                .filter_map(|&pid| self.0.process(pid).cloned())
                .collect(),
        }
    }
}

/// Stand-in on non-Windows platforms: reports nothing.
#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct Sampler;

#[cfg(not(windows))]
impl Sampler {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(windows))]
impl StatsSource for Sampler {
    fn sample(&mut self, _pids: &[u32]) -> Snapshot {
        Snapshot::default()
    }
}

/// CPU % of one process over an interval, as a share of every logical
/// processor. `busy_delta` is kernel + user time spent in the interval.
pub fn process_cpu_percent(busy_delta: Duration, wall_delta: Duration, cpus: u32) -> Option<f32> {
    let capacity = wall_delta.as_secs_f64() * f64::from(cpus.max(1));
    if capacity <= 0.0 {
        return None;
    }
    Some(clamp_percent(busy_delta.as_secs_f64() / capacity * 100.0))
}

/// System CPU % from `GetSystemTimes` deltas. On Windows the kernel time
/// includes idle time, so busy = kernel + user − idle.
pub fn system_cpu_percent(idle_delta: u64, kernel_delta: u64, user_delta: u64) -> Option<f32> {
    let total = kernel_delta.saturating_add(user_delta);
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle_delta);
    Some(clamp_percent(busy as f64 / total as f64 * 100.0))
}

fn clamp_percent(v: f64) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 100.0) as f32
    } else {
        0.0
    }
}

/// A parsed `GPU Engine` counter instance name, e.g.
/// `pid_1234_luid_0x00000000_0x0000C6F1_phys_0_eng_3_engtype_3D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEngineInstance<'a> {
    pub pid: u32,
    /// Identifies one physical engine (`luid_…_phys_…_eng_…`), shared by
    /// every process that uses it.
    pub engine: &'a str,
}

impl<'a> GpuEngineInstance<'a> {
    pub fn parse(name: &'a str) -> Option<Self> {
        let rest = name.strip_prefix("pid_")?;
        let (pid, rest) = rest.split_once('_')?;
        let pid = pid.parse().ok()?;
        let engine = match rest.find("_engtype_") {
            Some(i) => &rest[..i],
            None => rest,
        };
        if !engine.starts_with("luid_") {
            return None;
        }
        Some(Self { pid, engine })
    }
}

/// GPU usage worked out from one read of the `GPU Engine` counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuUsage {
    /// Per process: its utilisation of the busiest engine it uses.
    pub per_pid: HashMap<u32, f32>,
    /// The busiest engine overall. `None` if there were no readings.
    pub total: Option<f32>,
}

/// Aggregate `(instance name, utilisation %)` readings. Each engine's load is
/// the sum over processes; a process's GPU % and the system GPU % are the
/// maximum over engines. Unparseable names are ignored.
pub fn aggregate_gpu<'a>(readings: impl IntoIterator<Item = (&'a str, f64)>) -> GpuUsage {
    let mut per_engine: HashMap<&str, f64> = HashMap::new();
    let mut per_pid_engine: HashMap<(u32, &str), f64> = HashMap::new();
    for (name, value) in readings {
        let Some(inst) = GpuEngineInstance::parse(name) else {
            continue;
        };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        *per_engine.entry(inst.engine).or_default() += value;
        *per_pid_engine.entry((inst.pid, inst.engine)).or_default() += value;
    }
    let mut per_pid: HashMap<u32, f32> = HashMap::new();
    for ((pid, _), v) in per_pid_engine {
        let v = clamp_percent(v);
        let e = per_pid.entry(pid).or_default();
        *e = e.max(v);
    }
    let total = per_engine
        .values()
        .copied()
        .map(clamp_percent)
        .reduce(f32::max);
    GpuUsage { per_pid, total }
}

/// Format a byte count the way the overlay shows it (`1.5 GB`, `320 MB`).
pub fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

/// Format an uptime as `1h 02m`, `5m 07s` or `42s`.
pub fn format_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_cpu_is_share_of_all_cpus() {
        // One core fully busy for 1 s on an 8-thread machine = 12.5 %.
        let p = process_cpu_percent(Duration::from_secs(1), Duration::from_secs(1), 8);
        assert_eq!(p, Some(12.5));
        // Rounding noise above 100 is clamped.
        let p = process_cpu_percent(Duration::from_millis(4100), Duration::from_secs(1), 4);
        assert_eq!(p, Some(100.0));
        assert_eq!(process_cpu_percent(Duration::ZERO, Duration::ZERO, 8), None);
        // cpus = 0 is treated as 1 rather than dividing by zero.
        assert_eq!(
            process_cpu_percent(Duration::from_millis(500), Duration::from_secs(1), 0),
            Some(50.0)
        );
    }

    #[test]
    fn system_cpu_subtracts_idle_from_kernel() {
        // kernel 600 (of which idle 500) + user 400 → busy 500 of 1000.
        assert_eq!(system_cpu_percent(500, 600, 400), Some(50.0));
        assert_eq!(system_cpu_percent(0, 0, 0), None);
        assert_eq!(system_cpu_percent(900, 100, 0), Some(0.0));
    }

    #[test]
    fn parses_gpu_engine_instance_names() {
        let i =
            GpuEngineInstance::parse("pid_1234_luid_0x00000000_0x0000C6F1_phys_0_eng_3_engtype_3D")
                .unwrap();
        assert_eq!(i.pid, 1234);
        assert_eq!(i.engine, "luid_0x00000000_0x0000C6F1_phys_0_eng_3");
        // Engine type names can contain spaces and underscores.
        let i = GpuEngineInstance::parse(
            "pid_8_luid_0x00000000_0x0000C6F1_phys_0_eng_12_engtype_Video Decode",
        )
        .unwrap();
        assert_eq!(i.engine, "luid_0x00000000_0x0000C6F1_phys_0_eng_12");
        assert!(GpuEngineInstance::parse("_Total").is_none());
        assert!(GpuEngineInstance::parse("pid_x_luid_0").is_none());
        assert!(GpuEngineInstance::parse("pid_12_something").is_none());
    }

    #[test]
    fn gpu_takes_busiest_engine() {
        let r = [
            ("pid_10_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 40.0),
            ("pid_10_luid_0x0_0x1_phys_0_eng_5_engtype_Copy", 5.0),
            ("pid_20_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 30.0),
            ("pid_20_luid_0x0_0x1_phys_0_eng_9_engtype_VideoDecode", 50.0),
            ("garbage", 99.0),
            ("pid_30_luid_0x0_0x1_phys_0_eng_0_engtype_3D", f64::NAN),
        ];
        let g = aggregate_gpu(r);
        assert_eq!(g.per_pid[&10], 40.0);
        assert_eq!(g.per_pid[&20], 50.0);
        assert!(!g.per_pid.contains_key(&30));
        // 3D engine: 40 + 30 = 70, busier than video decode at 50.
        assert_eq!(g.total, Some(70.0));
    }

    #[test]
    fn gpu_total_is_clamped_and_empty_is_none() {
        let r = [
            ("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 70.0),
            ("pid_2_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 60.0),
        ];
        assert_eq!(aggregate_gpu(r).total, Some(100.0));
        assert_eq!(aggregate_gpu([]).total, None);
    }

    #[test]
    fn fixed_stats_filters_by_pid() {
        let p = |pid| ProcessStats {
            pid,
            name: None,
            cpu_percent: Some(1.0),
            working_set_bytes: 1,
            private_bytes: 1,
            gpu_percent: None,
            uptime: Duration::ZERO,
        };
        let mut s = FixedStats(Snapshot {
            system: SystemStats::default(),
            processes: vec![p(1), p(2)],
        });
        let snap = s.sample(&[2, 3]);
        assert_eq!(snap.processes, vec![p(2)]);
    }

    #[test]
    fn formats_for_the_overlay() {
        assert_eq!(format_bytes(320 * 1024 * 1024), "320 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(format_uptime(Duration::from_secs(42)), "42s");
        assert_eq!(format_uptime(Duration::from_secs(307)), "5m 07s");
        assert_eq!(format_uptime(Duration::from_secs(3720)), "1h 02m");
    }
}
