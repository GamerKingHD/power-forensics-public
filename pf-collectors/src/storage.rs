//! Storage collector: disk utilization, throughput, IOPS, queue depth,
//! latency (PDH PhysicalDisk), plus a sustained-activity watchdog for the
//! timeline — continuous background disk churn is a classic deep-idle
//! blocker and deserves its own event, not just a graph line.
//!
//! No admin required. NVMe power states need admin IOCTLs to the physical
//! drive (verified: ACCESS_DENIED non-admin); SMART WMI classes return no
//! instances for this NVMe drive (verified). Both stay Unavailable with
//! the empirical reason. Disk-specific per-process IO needs ETW; the
//! process collector's total IO counters are the cross-reference.

use crate::collector::{Collector, CollectorError};
#[cfg(windows)]
use crate::cpu::InitRetry;
use crate::pdh::PdhQuery;
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};

/// Per-instance counters. Latency counters are seconds (shown as ms).
const DISK_COUNTERS: &[(&str, &str)] = &[
    ("disk_time", "% Disk Time"),
    ("idle_time", "% Idle Time"),
    ("avg_queue", "Avg. Disk Queue Length"),
    ("cur_queue", "Current Disk Queue Length"),
    ("reads", "Disk Reads/sec"),
    ("writes", "Disk Writes/sec"),
    ("read_bps", "Disk Read Bytes/sec"),
    ("write_bps", "Disk Write Bytes/sec"),
    ("read_lat_s", "Avg. Disk sec/Read"),
    ("write_lat_s", "Avg. Disk sec/Write"),
];

/// Convert a PDH "Avg. Disk sec/Read|Write" counter (seconds) to
/// milliseconds. Unit conversion: the seconds were measured, the ms figure
/// is Derived. A measured zero stays a real derived zero; a missing counter
/// stays None (never 0).
pub fn latency_ms(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|s| s * 1000.0)
}

/// Fires once per episode when disk busyness stays over threshold for
/// `needed` consecutive samples. Pure state machine, unit-tested.
pub struct ActivityWatch {
    threshold_pct: f64,
    needed: u32,
    count: u32,
    firing: bool,
}

impl ActivityWatch {
    pub fn new(threshold_pct: f64, needed: u32) -> Self {
        ActivityWatch {
            threshold_pct,
            needed: needed.max(1),
            count: 0,
            firing: false,
        }
    }

    /// Returns an event description on the rising edge only.
    pub fn update(&mut self, busy_pct: f64) -> Option<String> {
        if busy_pct > self.threshold_pct {
            self.count += 1;
            if !self.firing && self.count >= self.needed {
                self.firing = true;
                return Some(format!(
                    "sustained disk activity {busy_pct:.1}% over {} samples (>{:.0}% threshold) — likely blocking deep idle",
                    self.count, self.threshold_pct
                ));
            }
        } else {
            self.count = 0;
            self.firing = false;
        }
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiskStats {
    pub instance: String,
    pub is_total: bool,
    pub disk_time: Option<f64>,
    pub idle_time: Option<f64>,
    pub avg_queue: Option<f64>,
    pub cur_queue: Option<f64>,
    pub reads: Option<f64>,
    pub writes: Option<f64>,
    pub read_bps: Option<f64>,
    pub write_bps: Option<f64>,
    pub read_lat_ms: Option<f64>,
    pub write_lat_ms: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct StorageSample {
    pub disks: Vec<DiskStats>,
}

pub struct StorageCollector {
    #[cfg(windows)]
    pdh: Option<PdhQuery>,
    #[cfg(windows)]
    counters: Vec<(String, bool, Vec<usize>)>,
    #[cfg(windows)]
    init_retry: InitRetry,
    #[cfg(windows)]
    fresh: bool,
    #[cfg(windows)]
    tick: u64,
}

#[cfg(windows)]
const REBUILD_EVERY: u64 = 60;

impl StorageCollector {
    pub fn new() -> Self {
        StorageCollector {
            #[cfg(windows)]
            pdh: None,
            #[cfg(windows)]
            counters: Vec::new(),
            #[cfg(windows)]
            init_retry: InitRetry::new(),
            #[cfg(windows)]
            fresh: false,
            #[cfg(windows)]
            tick: 0,
        }
    }

    /// Feed _Total busyness; returns a timeline event on rising edge.
    /// Takes the watch state as a parameter so worker-owned collectors
    /// stay stateless across the scheduler boundary.
    pub fn observe_watch(watch: &mut ActivityWatch, sample: &StorageSample) -> Option<String> {
        // Missing busyness is absence, not 0% busy: skip the update so the
        // watchdog never treats a missing counter as a quiet period (which
        // would fabricate a reset / hide a real sustained-activity episode).
        match sample
            .disks
            .iter()
            .find(|d| d.is_total)
            .and_then(|d| d.disk_time)
        {
            Some(busy) => watch.update(busy),
            None => None,
        }
    }

    #[cfg(windows)]
    fn ensure_init(&mut self) -> Result<(), String> {
        if self.pdh.is_some() {
            return Ok(());
        }
        if !self.init_retry.may_attempt(self.tick) {
            return Err(self
                .init_retry
                .cached()
                .unwrap_or_else(|| "initialization retry pending".to_string()));
        }
        match self.build_counters() {
            Ok(()) => {
                self.init_retry.note_success();
                Ok(())
            }
            Err(e) => Err(self.init_retry.note_failure(self.tick, e)),
        }
    }

    #[cfg(windows)]
    fn build_counters(&mut self) -> Result<(), String> {
        let pdh = PdhQuery::open()?;
        let mut counters = Vec::new();
        if let Ok(insts) = PdhQuery::enum_instances("PhysicalDisk") {
            for inst in insts {
                let mut handles = Vec::new();
                let mut added = 0;
                for (_, counter) in DISK_COUNTERS {
                    let h = pdh.add(&format!("\\PhysicalDisk({inst})\\{counter}"));
                    if h != 0 {
                        added += 1;
                    }
                    handles.push(h);
                }
                if added > 0 {
                    counters.push((inst.clone(), inst.eq_ignore_ascii_case("_total"), handles));
                }
            }
        }
        if counters.is_empty() {
            return Err("no PhysicalDisk counters could be added".to_string());
        }
        pdh.collect()?;
        std::thread::sleep(std::time::Duration::from_millis(150));
        pdh.collect()?;
        self.pdh = Some(pdh);
        self.counters = counters;
        self.fresh = true;
        Ok(())
    }

    pub fn read(&mut self) -> Result<StorageSample, String> {
        #[cfg(windows)]
        {
            // Advance before init so a failed attempt still counts down the
            // retry backoff.
            self.tick = self.tick.wrapping_add(1);
            self.ensure_init()?;
            if self.tick.is_multiple_of(REBUILD_EVERY) {
                let mut probe = StorageCollector::new();
                if let Ok(()) = probe.build_counters() {
                    self.pdh = probe.pdh;
                    self.counters = probe.counters;
                }
            }
            let pdh = self.pdh.as_ref().ok_or("PDH query not initialized")?;
            if self.fresh {
                self.fresh = false;
            } else {
                pdh.collect()?;
            }
            let get = |h: &[usize], i: usize| -> Option<f64> {
                h.get(i).copied().and_then(|hh| pdh.read_double(hh))
            };
            let mut disks = Vec::with_capacity(self.counters.len());
            for (inst, is_total, handles) in &self.counters {
                disks.push(DiskStats {
                    instance: inst.clone(),
                    is_total: *is_total,
                    disk_time: get(handles, 0),
                    idle_time: get(handles, 1),
                    avg_queue: get(handles, 2),
                    cur_queue: get(handles, 3),
                    reads: get(handles, 4),
                    writes: get(handles, 5),
                    read_bps: get(handles, 6),
                    write_bps: get(handles, 7),
                    read_lat_ms: latency_ms(get(handles, 8)),
                    write_lat_ms: latency_ms(get(handles, 9)),
                });
            }
            Ok(StorageSample { disks })
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }

    /// Render a previously-read sample (collect once, reuse).
    pub fn format_json(
        sample: &StorageSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        fn opt(v: Option<f64>) -> String {
            pf_core::telemetry::json_num(
                v,
                pf_core::telemetry::Provenance::Measured,
                pf_core::telemetry::UnavailKind::NotSampled,
                "counter not ready",
            )
        }
        // Latency is a seconds->milliseconds conversion, so it is Derived
        // even when the PDH counter was measured.
        fn opt_derived(v: Option<f64>) -> String {
            pf_core::telemetry::json_num(
                v,
                pf_core::telemetry::Provenance::Derived,
                pf_core::telemetry::UnavailKind::NotSampled,
                "counter not ready",
            )
        }
        let mut disks = String::new();
        for (i, d) in sample.disks.iter().enumerate() {
            if i > 0 {
                disks.push(',');
            }
            disks.push_str(&format!(
                "{{\"instance\":\"{}\",\"total\":{},\"disk_time_pct\":{},\"idle_pct\":{},\
                 \"avg_queue\":{},\"cur_queue\":{},\"reads_s\":{},\"writes_s\":{},\
                 \"read_bps\":{},\"write_bps\":{},\"read_lat_ms\":{},\"write_lat_ms\":{}}}",
                escape_json(&d.instance),
                d.is_total,
                opt(d.disk_time),
                opt(d.idle_time),
                opt(d.avg_queue),
                opt(d.cur_queue),
                opt(d.reads),
                opt(d.writes),
                opt(d.read_bps),
                opt(d.write_bps),
                opt_derived(d.read_lat_ms),
                opt_derived(d.write_lat_ms),
            ));
        }
        // The former hardcoded `stale:false` field was dead capability (the
        // read path never set it) and is no longer emitted: fictional stale
        // evidence is worse than none. See the semantic gate classification.
        format!(
            "{{\"collector\":\"storage\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\"disks\":[{disks}]}}",
            stamp.wall_millis, stamp.mono_millis, t_start_ms, t_end_ms,
        )
    }
}

impl Default for StorageCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for StorageCollector {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "storage.disk.util_pct",
                unit: "%",
                source: "PDH PhysicalDisk(*) % Disk Time / % Idle Time",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Sustained non-idle feeds the deep-idle watchdog",
            },
            FieldMeta {
                name: "storage.disk.throughput_bps",
                unit: "B/s",
                source: "PDH PhysicalDisk(*) Disk Read/Write Bytes per sec",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Integrated per session into read/write MB totals",
            },
            FieldMeta {
                name: "storage.disk.iops",
                unit: "/s",
                source: "PDH PhysicalDisk(*) Disk Reads/Writes per sec",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "High IOPS at low throughput = small random IO (indexers, telemetry)",
            },
            FieldMeta {
                name: "storage.disk.queue_latency",
                unit: "queue/ms",
                source: "PDH Avg/Current Disk Queue Length, Avg Disk sec per Read/Write",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Saturation signal; latency fields are Derived (seconds->ms conversion)",
            },
            FieldMeta {
                name: "storage.watchdog",
                unit: "events",
                source: ">5% busyness for 10 consecutive samples",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Fires once per episode; the deep-idle-blocker alarm",
            },
            FieldMeta {
                name: "storage.nvme_power",
                unit: "state",
                source: "NVMe IOCTL to physical drive — needs admin (ACCESS_DENIED verified)",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "APST/power-state query needs an admin drive handle",
            },
            FieldMeta {
                name: "storage.temperature_c",
                unit: "C",
                source: "none on this drive (WMI SMART classes empty for NVMe, IOCTL needs admin)",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "Verified absent 2026-09-17 on WD SN740; revisit per drive model",
            },
            FieldMeta {
                name: "storage.proc_io",
                unit: "B/s",
                source: "per-process disk attribution needs ETW (admin); totals in proc collector",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "proc.top.io_bps covers all file/device IO incl. paging",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let sample = self.read().map_err(|e| CollectorError::new("storage", e))?;
        let t1 = clock.stamp();
        Ok(Self::format_json(
            &sample,
            t1,
            t0.mono_millis,
            t1.mono_millis,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_fires_once_per_episode() {
        let mut w = ActivityWatch::new(5.0, 3);
        assert_eq!(w.update(1.0), None);
        assert_eq!(w.update(6.0), None);
        assert_eq!(w.update(7.0), None);
        let fire = w.update(8.0);
        assert!(fire.is_some());
        assert!(fire.unwrap().contains("sustained disk activity"));
        // Stays firing without repeats.
        assert_eq!(w.update(9.0), None);
        assert_eq!(w.update(9.0), None);
        // Quiet re-arms.
        assert_eq!(w.update(0.5), None);
        assert_eq!(w.update(6.0), None);
        assert_eq!(w.update(6.0), None);
        assert!(w.update(6.0).is_some());
    }

    #[test]
    fn watchdog_skips_missing_busyness_instead_of_zero() {
        let mut w = ActivityWatch::new(5.0, 2);
        // Two hot samples, then a sample with no _Total busyness at all.
        assert_eq!(w.update(9.0), None);
        let hot = StorageSample {
            disks: vec![DiskStats {
                instance: "_Total".to_string(),
                is_total: true,
                disk_time: Some(9.0),
                ..Default::default()
            }],
        };
        // Missing counter must not be fed as 0 (which would reset the run).
        let missing = StorageSample {
            disks: vec![DiskStats {
                instance: "_Total".to_string(),
                is_total: true,
                disk_time: None,
                ..Default::default()
            }],
        };
        assert_eq!(StorageCollector::observe_watch(&mut w, &missing), None);
        // The run survives the gap: next hot sample is the 2nd consecutive.
        assert!(StorageCollector::observe_watch(&mut w, &hot).is_some());
        // No _Total row at all is likewise not 0.
        let mut w2 = ActivityWatch::new(5.0, 2);
        let none = StorageSample { disks: vec![] };
        assert_eq!(StorageCollector::observe_watch(&mut w2, &none), None);
    }

    #[test]
    fn watchdog_edge_cases() {
        let mut w = ActivityWatch::new(5.0, 1);
        assert!(w.update(5.1).is_some()); // first sample can fire with needed=1
        let mut w2 = ActivityWatch::new(5.0, 3);
        assert_eq!(w2.update(5.0), None); // threshold is strict >
        // Missing data (NaN-safe: NaN comparisons are false → resets).
        assert_eq!(w2.update(f64::NAN), None);
    }

    #[test]
    fn latency_units() {
        // 400us service time renders as 0.4ms.
        assert_eq!(latency_ms(Some(0.0004)), Some(0.4));
        // A measured zero stays a real zero; missing stays None, never 0.
        assert_eq!(latency_ms(Some(0.0)), Some(0.0));
        assert_eq!(latency_ms(None), None);
    }

    #[test]
    fn latency_json_is_derived_not_measured() {
        let sample = StorageSample {
            disks: vec![DiskStats {
                instance: "_Total".to_string(),
                is_total: true,
                read_bps: Some(1000.0),
                read_lat_ms: latency_ms(Some(0.0004)),
                ..Default::default()
            }],
        };
        let stamp = ClockStamp {
            wall_millis: 1,
            mono_millis: 2,
        };
        let json = StorageCollector::format_json(&sample, stamp, 0, 0);
        assert!(json.contains("\"read_lat_ms\":{\"v\":0.4,\"p\":\"derived\""));
        // Raw byte-rate stays measured.
        assert!(json.contains("\"read_bps\":{\"v\":1000,\"p\":\"measured\""));
        // A missing latency is Unavailable, not a 0.
        let missing = StorageSample {
            disks: vec![DiskStats {
                instance: "_Total".to_string(),
                is_total: true,
                ..Default::default()
            }],
        };
        let json = StorageCollector::format_json(&missing, stamp, 0, 0);
        assert!(json.contains("\"read_lat_ms\":{\"v\":null"));
    }

    #[test]
    fn format_json_has_no_dead_stale_field() {
        // `stale` was always false on every read path and never persisted, so
        // it was removed from the emitted schema rather than kept as
        // permanently-false evidence.
        let sample = StorageSample {
            disks: vec![DiskStats {
                instance: "_Total".to_string(),
                is_total: true,
                disk_time: Some(2.0),
                ..Default::default()
            }],
        };
        let json = StorageCollector::format_json(
            &sample,
            ClockStamp {
                wall_millis: 1,
                mono_millis: 2,
            },
            0,
            0,
        );
        assert!(!json.contains("\"stale\""), "{json}");
    }
}
