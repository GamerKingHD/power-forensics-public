//! Self-telemetry: the profiler measuring itself.
//!
//! Reads the CURRENT process (no PID lookup, no instance ambiguity) via
//! GetProcessTimes (CPU), GetProcessMemoryInfo (working set), and
//! GetProcessIoCounters (I/O bytes), plus Toolhelp thread count.
//! Rates come from deltas, like the process collector. This is the raw
//! material for the observer-effect report — not a claim by itself.

use crate::collector::{Collector, CollectorError};
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, Reading, escape_json};

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> isize;
    fn GetProcessTimes(
        h: isize,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn GetProcessIoCounters(h: isize, counters: *mut IoCounters) -> i32;
    fn GetActiveProcessorCount(group: u16) -> u32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Thread32First(h: isize, e: *mut ThreadEntry) -> i32;
    fn Thread32Next(h: isize, e: *mut ThreadEntry) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetCurrentProcessId() -> u32;
}

#[cfg(windows)]
#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(h: isize, counters: *mut MemCounters, cb: u32) -> i32;
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

#[cfg(windows)]
#[repr(C)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    tid: u32,
    owner: u32,
    base_pri: i32,
    delta_pri: i32,
    flags: u32,
}

#[derive(Debug, Clone)]
pub struct SelfSample {
    /// CPU % of total machine capacity (same normalization as proc).
    pub cpu_pct: Reading<f64>,
    pub ws_mb: Reading<f64>,
    pub priv_mb: Reading<f64>,
    pub io_read_b: Reading<u64>,
    pub io_write_b: Reading<u64>,
    pub threads: Reading<u32>,
    /// System-wide context-switch rate (ctx/s) copied from the cpu
    /// collector's latest sample by the monitor and labeled Estimated:
    /// it is informational context for the observer-effect report, NOT a
    /// measurement of this process (per-process ctx data needs an elevated
    /// helper). Unavailable until the first cpu sample arrives — never
    /// fabricated. Session parsers ignore unknown fields, so this stays
    /// JSON-compatible without touching session.rs.
    pub ctx_switches: Reading<f64>,
}

fn u64_100ns(lo: u32, hi: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub struct SelfCollector {
    #[cfg(windows)]
    // (cpu_100ns, monotonic Instant). The rate denominator is a monotonic
    // interval; wall time is never used for durations (see telemetry::Clock).
    prev_cpu: Option<(u64, std::time::Instant)>,
    #[cfg(windows)]
    ncpu: u32,
}

impl SelfCollector {
    /// Canonical self cadence (matches every FieldMeta::interval_ms below
    /// and `sched::canonical_interval("self", _)`).
    pub const INTERVAL_MS: u64 = 5000;

    pub fn new() -> Self {
        #[cfg(windows)]
        // SAFETY: trivial getter.
        let ncpu = unsafe { GetActiveProcessorCount(0xFFFF) }.max(1);
        SelfCollector {
            #[cfg(windows)]
            prev_cpu: None,
            #[cfg(windows)]
            ncpu,
        }
    }

    pub fn read(&mut self) -> Result<SelfSample, String> {
        #[cfg(windows)]
        // SAFETY: current-process pseudo-handle needs no cleanup; all
        // out-params checked.
        unsafe {
            let h = GetCurrentProcess();
            let (mut c, mut e, mut k, mut u) = (
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
            );
            if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
                return Err("GetProcessTimes(self) failed".to_string());
            }
            let cpu_total = u64_100ns(k.lo, k.hi).saturating_add(u64_100ns(u.lo, u.hi));
            // Monotonic denominator: a wall-clock step must not distort the
            // self CPU rate. The displayed wall timestamp still comes from
            // the ClockStamp the caller passes in.
            let now = std::time::Instant::now();
            let cpu_pct = match self.prev_cpu {
                Some((prev_cpu, prev_at)) => {
                    let dt = now.saturating_duration_since(prev_at);
                    let dwall_100ns = dt.as_nanos() as u64 / 100 * self.ncpu as u64;
                    if dwall_100ns > 0 {
                        Reading::Measured(
                            cpu_total.saturating_sub(prev_cpu) as f64 / dwall_100ns as f64 * 100.0,
                        )
                    } else {
                        Reading::Unavailable("zero monotonic delta")
                    }
                }
                _ => Reading::Unavailable("warming up (first tick)"),
            };
            self.prev_cpu = Some((cpu_total, now));

            let mut m = std::mem::zeroed::<MemCounters>();
            m.cb = std::mem::size_of::<MemCounters>() as u32;
            let (ws_mb, priv_mb) = if GetProcessMemoryInfo(h, &mut m, m.cb) != 0 {
                (
                    Reading::Measured(m.ws as f64 / (1024.0 * 1024.0)),
                    Reading::Measured(m.private as f64 / (1024.0 * 1024.0)),
                )
            } else {
                (
                    Reading::Unavailable("GetProcessMemoryInfo failed"),
                    Reading::Unavailable("GetProcessMemoryInfo failed"),
                )
            };
            let mut io = IoCounters::default();
            let (io_r, io_w) = if GetProcessIoCounters(h, &mut io) != 0 {
                (
                    Reading::Measured(io.read_bytes),
                    Reading::Measured(io.write_bytes),
                )
            } else {
                (
                    Reading::Unavailable("GetProcessIoCounters failed"),
                    Reading::Unavailable("GetProcessIoCounters failed"),
                )
            };
            // Thread count for our own PID.
            let me = GetCurrentProcessId();
            let snap = CreateToolhelp32Snapshot(0x4, 0);
            let mut threads = Reading::Unavailable("thread snapshot failed");
            if snap != -1 {
                let mut count = 0u32;
                let mut te = std::mem::zeroed::<ThreadEntry>();
                te.size = std::mem::size_of::<ThreadEntry>() as u32;
                if Thread32First(snap, &mut te) != 0 {
                    loop {
                        if te.owner == me {
                            count += 1;
                        }
                        if Thread32Next(snap, &mut te) == 0 {
                            break;
                        }
                    }
                    threads = Reading::Measured(count);
                }
                CloseHandle(snap);
            }
            Ok(SelfSample {
                cpu_pct,
                ws_mb,
                priv_mb,
                io_read_b: io_r,
                io_write_b: io_w,
                threads,
                // Filled in by the monitor from the cpu collector's latest
                // system ctx_switches/s (labeled Estimated there). The
                // collector itself must not fabricate it.
                ctx_switches: Reading::Unavailable(
                    "no cpu sample yet (monitor fills from cpu collector)",
                ),
            })
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }
}

impl Default for SelfCollector {
    fn default() -> Self {
        Self::new()
    }
}

fn jnum<T: std::fmt::Display>(r: &Reading<T>) -> String {
    use pf_core::telemetry::{UnavailKind, quality_for_kind};
    match r {
        Reading::Measured(v) | Reading::Derived(v) | Reading::Estimated(v) => {
            format!(
                "{{\"v\":{v},\"p\":\"{}\",\"q\":0}}",
                r.provenance().as_str()
            )
        }
        Reading::Unavailable(reason) => {
            // Self-telemetry absence is always a read failure (transient).
            let kind = UnavailKind::TransientError;
            format!(
                "{{\"v\":null,\"p\":\"unavailable\",\"k\":{},\"q\":{},\"reason\":\"{}\"}}",
                kind as u8,
                quality_for_kind(kind) as u8,
                escape_json(reason)
            )
        }
    }
}

impl Collector for SelfCollector {
    fn name(&self) -> &'static str {
        "self"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "self.cpu_pct",
                unit: "% machine",
                source: "GetProcessTimes(self) deltas",
                provenance: Provenance::Measured,
                interval_ms: 5000,
                requires_admin: false,
                notes: "The profiler's own CPU share; input to the overhead report",
            },
            FieldMeta {
                name: "self.mem_mb",
                unit: "MB",
                source: "GetProcessMemoryInfo(self)",
                provenance: Provenance::Measured,
                interval_ms: 5000,
                requires_admin: false,
                notes: "Working set + private bytes",
            },
            FieldMeta {
                name: "self.io_bytes",
                unit: "B cumulative",
                source: "GetProcessIoCounters(self)",
                provenance: Provenance::Measured,
                interval_ms: 5000,
                requires_admin: false,
                notes: "All process I/O incl. session writes themselves",
            },
            FieldMeta {
                name: "self.threads",
                unit: "count",
                source: "Toolhelp thread walk (own PID)",
                provenance: Provenance::Measured,
                interval_ms: 5000,
                requires_admin: false,
                notes: "Grows with worker-thread count by design",
            },
            FieldMeta {
                name: "self.ctx_switches_s",
                unit: "ctx/s system",
                source: "cpu collector system ctx_switches/s, copied by monitor",
                provenance: Provenance::Estimated,
                interval_ms: 5000,
                requires_admin: false,
                notes: "Informational estimate only (system-wide, not this process); Unavailable until first cpu sample",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let s = self.read().map_err(|e| CollectorError::new("self", e))?;
        let t1 = clock.stamp();
        Ok(Self::format_json(&s, t1, t0.mono_millis, t1.mono_millis))
    }
}

impl SelfCollector {
    /// Render a previously-read snapshot (collect once, reuse).
    pub fn format_json(
        sample: &SelfSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        let s = sample;
        format!(
            "{{\"collector\":\"self\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"cpu_pct\":{},\"ws_mb\":{},\"priv_mb\":{},\
             \"io_read_b\":{},\"io_write_b\":{},\"threads\":{},\
             \"ctx_switches_s\":{}}}",
            stamp.wall_millis,
            stamp.mono_millis,
            t_start_ms,
            t_end_ms,
            jnum(&s.cpu_pct),
            jnum(&s.ws_mb),
            jnum(&s.priv_mb),
            jnum(&s.io_read_b),
            jnum(&s.io_write_b),
            jnum(&s.threads),
            jnum(&s.ctx_switches),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_math() {
        assert_eq!(u64_100ns(0, 0), 0);
        assert_eq!(u64_100ns(0xFFFF_FFFF, 0), 0xFFFF_FFFF);
        assert_eq!(u64_100ns(0, 1), 0x1_0000_0000);
    }

    #[test]
    fn envelope_kinds() {
        let s = jnum(&Reading::Measured(1.5f64));
        assert!(s.contains("\"p\":\"measured\"") && s.contains("\"q\":0"));
        let s: String = jnum(&Reading::<f64>::Unavailable("x"));
        assert!(s.contains("\"k\":1") && s.contains("\"q\":3"));
    }

    #[cfg(windows)]
    #[test]
    fn self_read_works() {
        let mut c = SelfCollector::new();
        let s = c.read().unwrap();
        assert!(s.ws_mb.value().is_some());
        assert!(s.threads.value().is_some());
        // Second read yields a CPU rate (first is warming up).
        let s2 = c.read().unwrap();
        assert!(s2.cpu_pct.value().is_some() || s2.cpu_pct.reason().is_some());
    }
}
