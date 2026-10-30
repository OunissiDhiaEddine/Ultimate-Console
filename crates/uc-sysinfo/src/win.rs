//! Win32 implementation of [`Sampler`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAsFileTime, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetProcessTimes, GetSystemTimes, OpenProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{Owned, PCWSTR, PWSTR, w};

use crate::{
    GpuUsage, ProcessStats, Snapshot, StatsSource, SystemStats, aggregate_gpu, process_cpu_percent,
    system_cpu_percent,
};

/// Samples real counters. Create it when the overlay opens, call
/// [`StatsSource::sample`] about once a second, drop it when the overlay
/// closes.
pub struct Sampler {
    cpus: u32,
    gpu: Option<GpuCounters>,
    prev_system: Option<SystemTimes>,
    prev_procs: HashMap<u32, ProcTimes>,
}

#[derive(Clone, Copy)]
struct SystemTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

#[derive(Clone)]
struct ProcTimes {
    /// Creation time, to notice a PID that was reused by a new process.
    created: u64,
    busy: u64,
    at: Instant,
    name: Option<String>,
}

// SAFETY: the only non-Send fields are the PDH query and counter handles.
// PDH handles are not tied to the thread that created them, and the sampler
// is used from one thread at a time (it needs `&mut self`).
unsafe impl Send for Sampler {}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        // SAFETY: plain query with no pointers.
        let cpus = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1);
        let gpu = GpuCounters::open();
        if gpu.is_none() {
            tracing::info!("GPU Engine counters unavailable; GPU % will be blank");
        }
        Self {
            cpus,
            gpu,
            prev_system: None,
            prev_procs: HashMap::new(),
        }
    }

    fn system(&mut self, gpu: &GpuUsage) -> SystemStats {
        let mut out = SystemStats {
            gpu_percent: gpu.total,
            ..Default::default()
        };
        if let Some(now) = system_times() {
            out.cpu_percent = self.prev_system.and_then(|p| {
                system_cpu_percent(
                    now.idle.wrapping_sub(p.idle),
                    now.kernel.wrapping_sub(p.kernel),
                    now.user.wrapping_sub(p.user),
                )
            });
            self.prev_system = Some(now);
        }
        let mut mem = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: `mem` is a valid MEMORYSTATUSEX with dwLength set.
        if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
            out.memory_total_bytes = mem.ullTotalPhys;
            out.memory_used_bytes = mem.ullTotalPhys.saturating_sub(mem.ullAvailPhys);
        }
        out
    }

    fn process(&self, pid: u32, gpu: &GpuUsage) -> Option<(ProcessStats, ProcTimes)> {
        // SAFETY: OpenProcess has no pointer arguments; the handle is closed
        // by `Owned` on drop.
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            // SAFETY: the handle was just returned by OpenProcess and is ours.
            Ok(h) => unsafe { Owned::new(h) },
            Err(e) => {
                tracing::debug!(pid, error = %e, "cannot open process");
                return None;
            }
        };
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: valid handle and four valid FILETIME out-pointers.
        if let Err(e) =
            unsafe { GetProcessTimes(*handle, &mut created, &mut exited, &mut kernel, &mut user) }
        {
            tracing::debug!(pid, error = %e, "GetProcessTimes failed");
            return None;
        }
        let created = ft(created);
        let busy = ft(kernel) + ft(user);
        let at = Instant::now();

        let prev = self.prev_procs.get(&pid).filter(|p| p.created == created);
        let cpu_percent = prev.and_then(|p| {
            process_cpu_percent(
                hundred_ns(busy.saturating_sub(p.busy)),
                at.duration_since(p.at),
                self.cpus,
            )
        });
        let name = match prev {
            Some(p) => p.name.clone(),
            None => image_name(*handle),
        };

        let mut mem = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: valid handle; PROCESS_MEMORY_COUNTERS_EX starts with the
        // PROCESS_MEMORY_COUNTERS layout and `cb` tells the API its real size.
        let mem_ok = unsafe {
            GetProcessMemoryInfo(
                *handle,
                (&raw mut mem).cast::<PROCESS_MEMORY_COUNTERS>(),
                mem.cb,
            )
        }
        .is_ok();
        if !mem_ok {
            tracing::debug!(pid, "GetProcessMemoryInfo failed");
        }

        // SAFETY: no arguments.
        let now = ft(unsafe { GetSystemTimeAsFileTime() });
        let stats = ProcessStats {
            pid,
            name: name.clone(),
            cpu_percent,
            working_set_bytes: mem.WorkingSetSize as u64,
            private_bytes: mem.PrivateUsage as u64,
            // A process that used no engine since the last sample has no
            // counter instance, so 0 rather than "unknown" once GPU works.
            gpu_percent: gpu
                .total
                .map(|_| gpu.per_pid.get(&pid).copied().unwrap_or(0.0)),
            uptime: hundred_ns(now.saturating_sub(created)),
        };
        let times = ProcTimes {
            created,
            busy,
            at,
            name,
        };
        Some((stats, times))
    }
}

impl StatsSource for Sampler {
    fn sample(&mut self, pids: &[u32]) -> Snapshot {
        let gpu = self.gpu.as_mut().map(GpuCounters::read).unwrap_or_default();
        let system = self.system(&gpu);
        let mut processes = Vec::with_capacity(pids.len());
        let mut next = HashMap::with_capacity(pids.len());
        for &pid in pids {
            if let Some((stats, times)) = self.process(pid, &gpu) {
                processes.push(stats);
                next.insert(pid, times);
            }
        }
        // Forget processes that weren't asked for this time.
        self.prev_procs = next;
        Snapshot { system, processes }
    }
}

/// The `\GPU Engine(*)\Utilization Percentage` wildcard counter.
struct GpuCounters {
    // Closing the query also frees its counters.
    query: Owned<PDH_HQUERY>,
    counter: PDH_HCOUNTER,
}

impl GpuCounters {
    fn open() -> Option<Self> {
        let mut query = PDH_HQUERY::default();
        // SAFETY: null data source = live data; `query` is a valid out-pointer.
        let status = unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) };
        if status != 0 {
            tracing::debug!(status = format!("{status:#x}"), "PdhOpenQueryW failed");
            return None;
        }
        // SAFETY: the query was just opened and is ours to close.
        let query = unsafe { Owned::new(query) };
        let mut counter = PDH_HCOUNTER::default();
        // SAFETY: valid query, static NUL-terminated path, valid out-pointer.
        let status = unsafe {
            PdhAddEnglishCounterW(
                *query,
                w!("\\GPU Engine(*)\\Utilization Percentage"),
                0,
                &mut counter,
            )
        };
        if status != 0 {
            tracing::debug!(
                status = format!("{status:#x}"),
                "GPU Engine counter missing"
            );
            return None;
        }
        // Utilization is a rate counter: prime it so the first real read
        // has something to diff against.
        // SAFETY: valid query.
        unsafe { PdhCollectQueryData(*query) };
        Some(Self { query, counter })
    }

    fn read(&mut self) -> GpuUsage {
        // SAFETY: valid query.
        let status = unsafe { PdhCollectQueryData(*self.query) };
        if status != 0 {
            tracing::debug!(
                status = format!("{status:#x}"),
                "PdhCollectQueryData failed"
            );
            return GpuUsage::default();
        }
        let mut items: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = Vec::new();
        let mut count = 0u32;
        let mut done = false;
        // Ask for the size, allocate, read. Retry if the instance list grew
        // in between (it can't for one collection, but be safe).
        for _ in 0..3 {
            let mut size = (items.capacity() * size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()) as u32;
            // SAFETY: valid counter; the buffer (if any) is `size` bytes of
            // suitably aligned storage, as PDH requires.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    (items.capacity() > 0).then_some(items.as_mut_ptr()),
                )
            };
            match status {
                0 => {
                    done = true;
                    break;
                }
                PDH_MORE_DATA => {
                    // `size` is in bytes and includes the instance-name
                    // strings stored after the items.
                    let n = (size as usize).div_ceil(size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());
                    items = Vec::with_capacity(n);
                    count = 0;
                }
                s => {
                    tracing::debug!(
                        status = format!("{s:#x}"),
                        "PdhGetFormattedCounterArrayW failed"
                    );
                    return GpuUsage::default();
                }
            }
        }
        if !done {
            tracing::debug!("GPU Engine instance list kept growing; skipping this sample");
            return GpuUsage::default();
        }
        if items.capacity() == 0 || count == 0 {
            // No instances: counters work but nothing used the GPU.
            return GpuUsage {
                total: Some(0.0),
                ..Default::default()
            };
        }
        let count = (count as usize).min(items.capacity());
        // SAFETY: PDH wrote `count` initialised items at the start of the
        // buffer (and their name strings after them, inside the same buffer).
        let slice = unsafe { std::slice::from_raw_parts(items.as_ptr(), count) };
        let readings: Vec<(String, f64)> = slice
            .iter()
            .filter(|it| {
                matches!(
                    it.FmtValue.CStatus,
                    PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
                )
            })
            .filter_map(|it| {
                // SAFETY: szName points at a NUL-terminated string inside
                // the buffer; CStatus says doubleValue is the active field.
                let name = unsafe { it.szName.to_string() }.ok()?;
                let value = unsafe { it.FmtValue.Anonymous.doubleValue };
                Some((name, value))
            })
            .collect();
        let mut usage = aggregate_gpu(readings.iter().map(|(n, v)| (n.as_str(), *v)));
        usage.total.get_or_insert(0.0);
        usage
    }
}

fn system_times() -> Option<SystemTimes> {
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: three valid FILETIME out-pointers.
    unsafe {
        GetSystemTimes(
            Some(&raw mut idle),
            Some(&raw mut kernel),
            Some(&raw mut user),
        )
    }
    .ok()?;
    Some(SystemTimes {
        idle: ft(idle),
        kernel: ft(kernel),
        user: ft(user),
    })
}

fn image_name(handle: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: valid handle; `buf` holds `len` u16s and the API writes at
    // most that many and updates `len`.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
    Some(name.to_owned())
}

fn ft(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

fn hundred_ns(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(100))
}
