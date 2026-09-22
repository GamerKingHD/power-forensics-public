//! Process collector: identity + CPU + memory + IO per process.
//!
//! Sources: Toolhelp32 snapshot (PID/exe/PPID/threads, always available)
//! plus limited-info handles for times, memory, IO counters, and handle
//! counts (same-user processes; protected/system processes fail OpenProcess
//! and degrade per-process, never aborting the enumeration).
//!
//! Rates come from deltas keyed by (PID, start time) so PID reuse can never
//! corrupt them. First tick after start has no deltas and honestly reports
//! warming-up None instead of zeros.

use crate::collector::{Collector, CollectorError};
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};

#[cfg(windows)]
use std::collections::HashMap;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetProcessTimes(
        h: isize,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn GetProcessHandleCount(h: isize, count: *mut u32) -> i32;
    fn GetProcessIoCounters(h: isize, counters: *mut IoCounters) -> i32;
    fn GetActiveProcessorCount(group: u16) -> u32;
    fn CloseHandle(h: isize) -> i32;
}

#[cfg(windows)]
#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(h: isize, counters: *mut MemCounters, cb: u32) -> i32;
}

#[cfg(windows)]
const TH32CS_SNAPPROCESS: u32 = 0x2;
#[cfg(windows)]
const QUERY_LIMITED: u32 = 0x1000;
#[cfg(windows)]
const ALL_GROUPS: u16 = 0xFFFF;

#[cfg(windows)]
#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    pid: u32,
    heap: usize,
    module: u32,
    threads: u32,
    ppid: u32,
    pri: i32,
    flags: u32,
    exe: [u16; 260],
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    lo: u32,
    hi: u32,
}

#[cfg(windows)]
#[repr(C)]
struct MemCounters {
    cb: u32,
    page_faults: u32,
    peak_ws: usize,
    ws: usize,
    quota_peak_paged: usize,
    quota_paged: usize,
    quota_peak_nonpaged: usize,
    quota_nonpaged: usize,
    private: usize,
    peak_private: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_ops: u64,
    write_ops: u64,
    other_ops: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

pub use pf_core::analysis::exit_hint;

/// Top-N cap: full-fidelity for the hottest processes, totals for the rest.
pub const TOP_N: usize = 15;

#[derive(Debug, Clone, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub threads: u32,
    pub accessible: bool,
    pub start_unix_ms: Option<u64>,
    /// % of total machine capacity (sums to ~system utility). None while
    /// warming up or when the process is inaccessible.
    pub cpu_pct: Option<f64>,
    pub cpu_100ns_total: Option<u64>,
    pub ws_mb: Option<f64>,
    pub priv_mb: Option<f64>,
    pub io_r_bps: Option<f64>,
    pub io_w_bps: Option<f64>,
    pub io_r_total: Option<u64>,
    pub io_w_total: Option<u64>,
    pub handles: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcSample {
    pub total_procs: usize,
    pub total_threads: u64,
    pub inaccessible: usize,
    pub truncated: bool,
    pub top: Vec<ProcInfo>,
}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct Prev {
    cpu_100ns: u64,
    r_bytes: u64,
    w_bytes: u64,
    /// Monotonic milliseconds (ClockStamp::mono_millis). Rate denominators
    /// use this, never wall time: an NTP step or DST change must not turn a
    /// real interval into a bogus rate. The human-facing wall timestamp is
    /// unaffected.
    mono_ms: u64,
}

/// FILETIME (100ns since 1601) to Unix millis. None for implausible values.
pub fn filetime_to_unix_ms(lo: u32, hi: u32) -> Option<u64> {
    let t = ((hi as u64) << 32) | lo as u64;
    const EPOCH_DIFF: u64 = 116444736000000000;
    if t < EPOCH_DIFF {
        return None;
    }
    let ms = (t - EPOCH_DIFF) / 10_000;
    // Sanity: 2000-01-01 .. 2100-01-01.
    if !(946_684_800_000..=4_102_444_800_000).contains(&ms) {
        return None;
    }
    Some(ms)
}

/// CPU % of total machine capacity from cumulative 100ns counters.
/// None on zero/negative wall delta (never a fake 0 or infinity).
pub fn cpu_pct(delta_cpu_100ns: u64, delta_wall_ms: u64, ncpu: u32) -> Option<f64> {
    if delta_wall_ms == 0 || ncpu == 0 {
        return None;
    }
    let capacity_100ns = delta_wall_ms * 10_000 * ncpu as u64;
    if capacity_100ns == 0 {
        return None;
    }
    Some(delta_cpu_100ns as f64 / capacity_100ns as f64 * 100.0)
}

pub struct ProcCollector {
    #[cfg(windows)]
    prev: HashMap<(u32, u64), Prev>,
    #[cfg(windows)]
    ncpu: u32,
}

impl ProcCollector {
    pub fn new() -> Self {
        #[cfg(windows)]
        // SAFETY: trivial getter.
        let ncpu = unsafe { GetActiveProcessorCount(ALL_GROUPS) }.max(1);
        ProcCollector {
            #[cfg(windows)]
            prev: HashMap::new(),
            #[cfg(windows)]
            ncpu,
        }
    }

    #[cfg(windows)]
    fn decode_exe(raw: &[u16; 260]) -> String {
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        raw[..end]
            .iter()
            .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
            .collect()
    }

    /// One typed read; feeds both store and dashboard (collect once).
    pub fn read_with_stamp(&mut self, stamp: ClockStamp) -> Result<ProcSample, String> {
        #[cfg(windows)]
        // SAFETY: Toolhelp + limited-info handle queries throughout; every
        // handle closed, all return values checked, per-process failures
        // degrade to inaccessible entries.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == -1 {
                return Err(format!(
                    "CreateToolhelp32Snapshot failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut infos = Vec::new();
            let mut entry = std::mem::zeroed::<ProcessEntry>();
            entry.size = std::mem::size_of::<ProcessEntry>() as u32;
            let mut ok = Process32FirstW(snap, &mut entry);
            while ok != 0 {
                infos.push(self.query_one(&entry, stamp.mono_millis));
                ok = Process32NextW(snap, &mut entry);
            }
            CloseHandle(snap);

            let total_procs = infos.len();
            let total_threads: u64 = infos.iter().map(|p| p.threads as u64).sum();
            let inaccessible = infos.iter().filter(|p| !p.accessible).count();
            // Rank: hottest CPU first, unknown CPU last (by working set).
            infos.sort_by(|a, b| match (a.cpu_pct, b.cpu_pct) {
                (Some(x), Some(y)) => y.total_cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => b.ws_mb.unwrap_or(0.0).total_cmp(&a.ws_mb.unwrap_or(0.0)),
            });
            let truncated = infos.len() > TOP_N;
            infos.truncate(TOP_N);
            Ok(ProcSample {
                total_procs,
                total_threads,
                inaccessible,
                truncated,
                top: infos,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = stamp;
            Err("Windows required".to_string())
        }
    }

    #[cfg(windows)]
    fn query_one(&mut self, entry: &ProcessEntry, mono_ms: u64) -> ProcInfo {
        let name = Self::decode_exe(&entry.exe);
        let mut info = ProcInfo {
            pid: entry.pid,
            ppid: entry.ppid,
            name,
            threads: entry.threads,
            accessible: false,
            ..Default::default()
        };
        // SAFETY: handle checked for failure, closed on every path below.
        let h = unsafe { OpenProcess(QUERY_LIMITED, 0, entry.pid) };
        if h == 0 || h == -1 {
            return info;
        }
        struct Guard(isize);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _guard = Guard(h);
        unsafe {
            let (mut c, mut e, mut k, mut u) = (
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
            );
            if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
                return info;
            }
            let start_100ns = ((c.hi as u64) << 32) | c.lo as u64;
            let cpu_total = (((k.hi as u64) << 32) | k.lo as u64)
                .saturating_add(((u.hi as u64) << 32) | u.lo as u64);
            info.accessible = true;
            info.start_unix_ms = filetime_to_unix_ms(c.lo, c.hi);
            info.cpu_100ns_total = Some(cpu_total);

            let mut m = std::mem::zeroed::<MemCounters>();
            m.cb = std::mem::size_of::<MemCounters>() as u32;
            if GetProcessMemoryInfo(h, &mut m, m.cb) != 0 {
                info.ws_mb = Some(m.ws as f64 / (1024.0 * 1024.0));
                info.priv_mb = Some(m.private as f64 / (1024.0 * 1024.0));
            }
            let mut io = IoCounters::default();
            let io_ok = GetProcessIoCounters(h, &mut io) != 0;
            if io_ok {
                info.io_r_total = Some(io.read_bytes);
                info.io_w_total = Some(io.write_bytes);
            }
            let mut hc: u32 = 0;
            if GetProcessHandleCount(h, &mut hc) != 0 {
                info.handles = Some(hc);
            }
            // Deltas keyed by (pid, start) defeat PID reuse corruption.
            let key = (entry.pid, start_100ns);
            if let Some(p) = self.prev.get(&key) {
                let dmono = mono_ms.saturating_sub(p.mono_ms);
                info.cpu_pct = cpu_pct(cpu_total.saturating_sub(p.cpu_100ns), dmono, self.ncpu);
                if io_ok && dmono > 0 {
                    let dt = dmono as f64 / 1000.0;
                    info.io_r_bps = Some(io.read_bytes.saturating_sub(p.r_bytes) as f64 / dt);
                    info.io_w_bps = Some(io.write_bytes.saturating_sub(p.w_bytes) as f64 / dt);
                }
            }
            self.prev.insert(
                key,
                Prev {
                    cpu_100ns: cpu_total,
                    r_bytes: io.read_bytes,
                    w_bytes: io.write_bytes,
                    mono_ms,
                },
            );
        }
        info
    }

    /// Render a previously-read sample (collect once, reuse for store +
    /// dashboard).
    pub fn format_json(
        sample: &ProcSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        use pf_core::telemetry::{Provenance, UnavailKind, json_num};
        fn opt(v: Option<f64>, kind: UnavailKind, reason: &'static str) -> String {
            json_num(v, Provenance::Measured, kind, reason)
        }
        fn opt_u(v: Option<u64>, kind: UnavailKind, reason: &'static str) -> String {
            json_num(v.map(|x| x as f64), Provenance::Measured, kind, reason)
        }
        fn opt_d(v: Option<f64>, kind: UnavailKind, reason: &'static str) -> String {
            json_num(v, Provenance::Derived, kind, reason)
        }
        let mut top = String::new();
        for (i, p) in sample.top.iter().enumerate() {
            if i > 0 {
                top.push(',');
            }
            // Absence kind is known here: protected processes are
            // Unsupported; accessible ones without rates are warming up.
            let (kind, reason) = if p.accessible {
                (UnavailKind::NotSampled, "warming up (first tick)")
            } else {
                (
                    UnavailKind::Unsupported,
                    "process inaccessible (protected/system)",
                )
            };
            let (io_r, io_w) = match (p.io_r_bps, p.io_w_bps) {
                (Some(_), Some(_)) => (
                    opt_d(p.io_r_bps, kind, reason),
                    opt_d(p.io_w_bps, kind, reason),
                ),
                _ => (opt(None, kind, reason), opt(None, kind, reason)),
            };
            top.push_str(&format!(
                "{{\"pid\":{},\"ppid\":{},\"name\":\"{}\",\"threads\":{},\"accessible\":{},\
                 \"start_unix_ms\":{},\"cpu_pct\":{},\"cpu_100ns_total\":{},\
                 \"ws_mb\":{},\"priv_mb\":{},\"io_r_bps\":{io_r},\"io_w_bps\":{io_w},\
                 \"io_r_total\":{},\"io_w_total\":{},\"handles\":{}}}",
                p.pid,
                p.ppid,
                escape_json(&p.name),
                p.threads,
                p.accessible,
                p.start_unix_ms
                    .map(|v| v.to_string())
                    .unwrap_or("null".to_string()),
                opt_d(p.cpu_pct, kind, reason),
                opt_u(p.cpu_100ns_total, kind, reason),
                opt(p.ws_mb, kind, reason),
                opt(p.priv_mb, kind, reason),
                opt_u(p.io_r_total, kind, reason),
                opt_u(p.io_w_total, kind, reason),
                p.handles
                    .map(|v| v.to_string())
                    .unwrap_or("null".to_string()),
            ));
        }
        format!(
            "{{\"collector\":\"proc\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"total_procs\":{},\"total_threads\":{},\"inaccessible\":{},\
             \"truncated\":{},\"top\":[{top}]}}",
            stamp.wall_millis,
            stamp.mono_millis,
            t_start_ms,
            t_end_ms,
            sample.total_procs,
            sample.total_threads,
            sample.inaccessible,
            sample.truncated,
        )
    }
}

impl Default for ProcCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for ProcCollector {
    fn name(&self) -> &'static str {
        "proc"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "proc.list",
                unit: "counts",
                source: "Toolhelp32 snapshot",
                provenance: Provenance::Measured,
                interval_ms: 2000,
                requires_admin: false,
                notes: "Total processes/threads; top 15 by CPU in full fidelity",
            },
            FieldMeta {
                name: "proc.top.identity",
                unit: "pid/ppid/name",
                source: "Toolhelp32 PROCESSENTRY32",
                provenance: Provenance::Measured,
                interval_ms: 2000,
                requires_admin: false,
                notes: "Resolves GPU top-PIDs to names; PPID gives child relationships",
            },
            FieldMeta {
                name: "proc.top.cpu_pct",
                unit: "% machine",
                source: "GetProcessTimes deltas / (wall x logical CPUs)",
                provenance: Provenance::Derived,
                interval_ms: 2000,
                requires_admin: false,
                notes: "Sums to ~system utility; first tick warming-up None; keyed by (pid,start)",
            },
            FieldMeta {
                name: "proc.top.mem_mb",
                unit: "MB",
                source: "GetProcessMemoryInfo (working set + private)",
                provenance: Provenance::Measured,
                interval_ms: 2000,
                requires_admin: false,
                notes: "Protected processes report no memory (inaccessible, not zero)",
            },
            FieldMeta {
                name: "proc.top.io_bps",
                unit: "B/s",
                source: "GetProcessIoCounters deltas",
                provenance: Provenance::Derived,
                interval_ms: 2000,
                requires_admin: false,
                notes: "All file/device IO incl. paging; not disk-only",
            },
            FieldMeta {
                name: "proc.top.handles_threads",
                unit: "counts",
                source: "GetProcessHandleCount + Toolhelp cntThreads",
                provenance: Provenance::Measured,
                interval_ms: 2000,
                requires_admin: false,
                notes: "Handle leaks and thread explosions are idle-drain suspects",
            },
            FieldMeta {
                name: "proc.top.energy_impact",
                unit: "-",
                source: "Energy Estimation Engine per-app — planned, needs admin",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "Windows' own per-app energy figure lives behind privileged APIs",
            },
            FieldMeta {
                name: "proc.top.wakeups",
                unit: "/s",
                source: "ETW context-switch attribution — planned, needs admin",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "The '2% CPU but no deep idle' smoking gun; kernel tracing required",
            },
            FieldMeta {
                name: "proc.top.net_bps",
                unit: "B/s",
                source: "ETW network events — planned, needs admin",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "Adapter totals (os collector phase) come first; per-proc needs ETW",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let stamp = clock.stamp();
        let t0mono = stamp.mono_millis;
        let sample = self
            .read_with_stamp(stamp)
            .map_err(|e| CollectorError::new("proc", e))?;
        let t1 = clock.stamp();
        Ok(Self::format_json(&sample, t1, t0mono, t1.mono_millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_epoch_converts() {
        assert_eq!(filetime_to_unix_ms(0, 0), None); // before epoch
        // The 1970 epoch itself is below the 2000 sanity floor: no real
        // process started then, so None is correct, not 0.
        assert_eq!(filetime_to_unix_ms(0xD53E8000, 0x019DB1DE), None);
        // 2026-01-01T00:00:00Z = 1767225600000 ms.
        let t100ns = 1767225600000u64 * 10_000 + 116444736000000000u64;
        assert_eq!(
            filetime_to_unix_ms(t100ns as u32, (t100ns >> 32) as u32),
            Some(1767225600000)
        );
        assert_eq!(filetime_to_unix_ms(0xFFFF_FFFF, 0xFFFF_FFFF), None); // year 60056
    }

    #[test]
    fn cpu_pct_math() {
        // 1 core fully busy for 1s on a 12-CPU box = 8.33% of machine.
        assert!((cpu_pct(10_000_000, 1000, 12).unwrap() - 100.0 / 12.0).abs() < 1e-9);
        // All 12 busy = 100%.
        assert!((cpu_pct(12 * 10_000_000, 1000, 12).unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(cpu_pct(100, 0, 12), None);
        assert_eq!(cpu_pct(100, 1000, 0), None);
    }

    #[test]
    fn rate_denominator_is_monotonic_millis() {
        // The delta passed for the denominator must be the monotonic
        // ClockStamp::mono_millis difference. A backward wall step cannot
        // reach the math: saturating_sub yields 0, which cpu_pct rejects as
        // None instead of fabricating a rate.
        let prev_mono = 5_000u64;
        let cur_mono = 6_500u64;
        assert_eq!(cur_mono.saturating_sub(prev_mono), 1500);
        assert!((cpu_pct(15_000_000, 1500, 1).unwrap() - 100.0).abs() < 1e-9);
        // Same monotonic instant: no interval, no rate.
        assert!(cpu_pct(1_000, cur_mono.saturating_sub(cur_mono), 1).is_none());
        // io rate from a mono delta would be bytes / (mono_delta/1000).
        let dt = 1500f64 / 1000.0;
        assert!((3000.0 / dt - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn pid_reuse_keying() {
        // Same PID, different start time: no delta must be fabricated.
        let mut prev = HashMap::new();
        prev.insert(
            (1234u32, 111u64),
            Prev {
                cpu_100ns: 1_000_000,
                r_bytes: 0,
                w_bytes: 0,
                mono_ms: 5000,
            },
        );
        // New process reused PID 1234 with a different start: key misses.
        assert!(!prev.contains_key(&(1234u32, 222u64)));
        // Same process: delta computes.
        let p = prev.get(&(1234u32, 111u64)).unwrap();
        assert_eq!(p.cpu_100ns, 1_000_000);
    }

    #[test]
    fn top_n_truncation() {
        assert_eq!(TOP_N, 15);
    }

    #[test]
    fn exit_hint_needs_sustained_absence() {
        // Absent 4+ ticks: estimated exit one interval after last sighting.
        assert_eq!(exit_hint(100.0, 1.0, 4), Some(101.0));
        assert_eq!(exit_hint(100.0, 2.5, 10), Some(102.5));
        // Short gaps are churn, not exits.
        assert_eq!(exit_hint(100.0, 1.0, 3), None);
        assert_eq!(exit_hint(100.0, 1.0, 0), None);
        assert_eq!(exit_hint(100.0, 0.0, 9), None);
    }
}
