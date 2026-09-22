//! CPU collector: utilization, frequency, C-state residency, wake activity.
//!
//! Source: PDH "Processor Information" counters — no admin required.
//! This one object yields % Processor Utility/Performance, frequency,
//! % Idle / % C1-C3 Time, C-state transitions, idle-break events, and
//! interrupt/DPC rates, which is why no ETW is needed for the MVP.
//! Per-core instances are enumerated at runtime (never hard-coded).

use crate::collector::{Collector, CollectorError};
use crate::pdh::PdhQuery;
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};
use std::sync::LazyLock;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetActiveProcessorCount(group: u16) -> u32;
}

#[cfg(windows)]
const ALL_PROCESSOR_GROUPS: u16 = 0xFFFF;

/// Keep "_Total" plus numeric "group,cpu" instances; drop group totals
/// like "0,_Total" (redundant on single-group machines, confusing otherwise).
fn is_cpu_instance(inst: &str) -> bool {
    if inst.eq_ignore_ascii_case("_total") {
        return true;
    }
    if inst.to_lowercase().ends_with(",_total") {
        return false;
    }
    let parts: Vec<&str> = inst.split(',').collect();
    parts.len() == 2 && parts.iter().all(|p| p.parse::<u32>().is_ok())
}

/// (field name, PDH counter name) for the _Total instance.
const TOTAL_COUNTERS: &[(&str, &str)] = &[
    ("utility", "% Processor Utility"),
    ("priv_utility", "% Privileged Utility"),
    ("performance", "% Processor Performance"),
    ("of_max_freq", "% of Maximum Frequency"),
    ("freq_mhz", "Processor Frequency"),
    ("idle_breaks", "Idle Break Events/sec"),
    ("idle_pct", "% Idle Time"),
    ("c1_pct", "% C1 Time"),
    ("c2_pct", "% C2 Time"),
    ("c3_pct", "% C3 Time"),
    ("c1_trans", "C1 Transitions/sec"),
    ("c2_trans", "C2 Transitions/sec"),
    ("c3_trans", "C3 Transitions/sec"),
    ("interrupts", "Interrupts/sec"),
    ("dpc_rate", "DPC Rate"),
    ("dpc_pct", "% DPC Time"),
    ("interrupt_pct", "% Interrupt Time"),
    ("user_time", "% User Time"),
    ("processor_time", "% Processor Time"),
    ("perf_limit_flags", "Performance Limit Flags"),
    ("perf_limit_pct", "% Performance Limit"),
];

/// Counters outside "Processor Information": (field, full PDH path, unit, notes).
const SYS_COUNTERS: &[(&str, &str, &str, &str)] = &[
    (
        "ctx_switches",
        "\\System\\Context Switches/sec",
        "/s",
        "Scheduler activity; storms implicate runaway wakeups or DPCs",
    ),
    (
        "proc_queue",
        "\\System\\Processor Queue Length",
        "threads",
        "Saturation signal: sustained queue with idle CPU is anomalous",
    ),
];

/// Per-core counters (kept small to bound handle count).
const CORE_COUNTERS: &[(&str, &str)] = &[
    ("utility", "% Processor Utility"),
    ("performance", "% Processor Performance"),
    ("freq_mhz", "Processor Frequency"),
    ("parking", "Parking Status"),
];

pub use pf_core::analysis::aggregate_cpu_mismatch;

/// P(W) = dEnergy(pWh) / dTime(ms) x 3.6e-6. Validated 2026-09-17 on ASUS
/// Ryzen 5 7430U: dE=2836750000, dT=3082ms -> 3.31W vs Power counter 3.33W.
const ENERGY_PWH_PER_MS_TO_W: f64 = 3.6e-6;

/// Package-energy watts from two Energy/Time samples. None on zero/negative
/// time delta or energy going backwards (reset/wrap) — never negative watts.
fn energy_derived_w(e0: f64, t0_ms: f64, e1: f64, t1_ms: f64) -> Option<f64> {
    let dt = t1_ms - t0_ms;
    let de = e1 - e0;
    if dt.is_nan() || dt <= 0.0 || de < 0.0 {
        return None;
    }
    Some(de / dt * ENERGY_PWH_PER_MS_TO_W)
}

/// Energy Meter instances are platform-specific ("rapl_package0_pkg" here);
/// pick the package instance without hard-coding its full name.
fn is_package_instance(inst: &str) -> bool {
    inst.to_lowercase().contains("pkg")
}

#[derive(Debug, Clone, Default)]
pub struct CoreSample {
    pub instance: String,
    pub utility: Option<f64>,
    pub performance: Option<f64>,
    pub freq_mhz: Option<f64>,
    pub parking: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CpuSample {
    pub totals: Vec<(String, Option<f64>)>,
    pub cores: Vec<CoreSample>,
    pub energy_instance: Option<String>,
    pub pkg_power_w: Option<f64>,
    pub pkg_derived_w: Option<f64>,
}

impl CpuSample {
    pub fn total(&self, field: &str) -> Option<f64> {
        self.totals
            .iter()
            .find(|(f, _)| f == field)
            .and_then(|(_, v)| *v)
    }

    /// Mean utility across online cores. None when no core reported.
    pub fn mean_core_utility(&self) -> Option<f64> {
        let vals: Vec<f64> = self.cores.iter().filter_map(|c| c.utility).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    }

    /// Count of cores reporting nonzero Parking Status. None when no core
    /// reports parking at all. Status semantics (which value = parked) are
    /// not fully documented; raw per-core values are kept in the sample.
    pub fn parked_cores(&self) -> Option<usize> {
        let mut any = false;
        let mut n = 0usize;
        for c in &self.cores {
            if let Some(v) = c.parking {
                any = true;
                if v >= 1.0 {
                    n += 1;
                }
            }
        }
        any.then_some(n)
    }
}

#[cfg(windows)]
struct EnergyHandles {
    power: usize,
    energy: usize,
    time: usize,
    instance: String,
    prev: Option<(f64, f64)>, // (energy, time_ms) from the previous window
}

/// Why a collector's first initialization failed, at the granularity that
/// decides whether retrying can ever help. Shared by the PDH-backed
/// collectors (cpu/gpu/storage/net) so their retry policy stays identical.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitClass {
    /// The counter/object is genuinely absent on this machine.
    PermanentUnsupported,
    /// Access denied: an in-process retry cannot elevate.
    PermissionElevation,
    /// PDH provider busy/not ready: a later attempt may succeed.
    TransientProvider,
    /// Device/interface temporarily gone (unplugged, driver restart).
    TemporaryDeviceGone,
}

#[cfg(windows)]
impl InitClass {
    pub(crate) fn retryable(self) -> bool {
        matches!(
            self,
            InitClass::TransientProvider | InitClass::TemporaryDeviceGone
        )
    }
}

/// Classify an init error string. PDH surfaces provider faults as opaque
/// status strings, so recognized permanent/permission patterns are matched
/// explicitly and everything else defaults to retryable (with a capped
/// backoff) — defaulting to permanent would re-create the sticky-failure bug.
#[cfg(windows)]
pub(crate) fn classify_init_error(msg: &str) -> InitClass {
    let m = msg.to_ascii_lowercase();
    // Permission first: Win32 ERROR_ACCESS_DENIED / PDH_ACCESS_DENIED.
    if m.contains("0x80070005")
        || m.contains("0xc0000bdb")
        || m.contains("access is denied")
        || m.contains("access denied")
        || m.contains("denied")
        || m.contains("privilege")
        || m.contains("elevat")
    {
        return InitClass::PermissionElevation;
    }
    // Explicitly unsupported on this platform/machine: never changes by retrying.
    if m.contains("windows required")
        || m.contains("no representative")
        || m.contains("unsupported")
        || m.contains("not supported")
        || m.contains("no such object")
    {
        return InitClass::PermanentUnsupported;
    }
    // Temporary device/interface disappearance (unplug, driver restart,
    // object instance not present this instant).
    if m.contains("0x800007d5")
        || m.contains("0xc0000bbf")
        || m.contains("no data")
        || m.contains("not found")
        || m.contains("no instance")
        || m.contains("no such")
        || m.contains("device")
        || m.contains("unplug")
        || m.contains("no instances enumerated")
    {
        return InitClass::TemporaryDeviceGone;
    }
    InitClass::TransientProvider
}

/// Bounded, capped backoff for init retries: 5, 10, 20, 40, ... ticks,
/// capped at 320 so a persistently failing provider is retried rarely, never
/// in a hot loop, while a one-off transient failure recovers within a few ticks.
#[cfg(windows)]
fn backoff_ticks(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(6);
    (5u64 << shift).min(320)
}

/// Sticky-init guard: remembers the first failure and only permits a new
/// init attempt for retryable classes after the backoff deadline. While
/// waiting, `cached()` supplies the honest error the caller surfaces as
/// Unavailable(reason).
#[cfg(windows)]
pub(crate) struct InitRetry {
    error: Option<String>,
    class: InitClass,
    attempts: u32,
    retry_at_tick: u64,
}

#[cfg(windows)]
impl InitRetry {
    pub(crate) fn new() -> Self {
        InitRetry {
            error: None,
            class: InitClass::PermanentUnsupported,
            attempts: 0,
            retry_at_tick: 0,
        }
    }

    /// Decide whether init may run now. `tick` must advance on every read,
    /// including failed ones, or the backoff deadline never elapses.
    pub(crate) fn may_attempt(&self, tick: u64) -> bool {
        match &self.error {
            None => true,
            Some(_) => self.class.retryable() && tick >= self.retry_at_tick,
        }
    }

    /// Record a failed init at `tick`; returns the message to surface.
    pub(crate) fn note_failure(&mut self, tick: u64, msg: String) -> String {
        self.class = classify_init_error(&msg);
        self.attempts = self.attempts.saturating_add(1);
        self.retry_at_tick = tick.saturating_add(backoff_ticks(self.attempts));
        self.error = Some(msg.clone());
        msg
    }

    pub(crate) fn note_success(&mut self) {
        self.error = None;
        self.attempts = 0;
        self.retry_at_tick = 0;
    }

    pub(crate) fn cached(&self) -> Option<String> {
        self.error.clone()
    }

    #[cfg(test)]
    pub(crate) fn class(&self) -> InitClass {
        self.class
    }
}

pub struct CpuCollector {
    #[cfg(windows)]
    pdh: Option<PdhQuery>,
    #[cfg(windows)]
    totals: Vec<(String, usize)>,
    #[cfg(windows)]
    cores: Vec<(String, usize, usize, usize, usize)>,
    #[cfg(windows)]
    energy: Option<EnergyHandles>,
    #[cfg(windows)]
    init_retry: InitRetry,
    /// Advances on every read, including failed ones, so init backoff elapses.
    #[cfg(windows)]
    tick: u64,
    /// True until the first read after init. The init warmup ends with a
    /// fresh collection, so the first read must format WITHOUT collecting
    /// again — otherwise percentage/rate counters would be computed over a
    /// ~0ms window and quantize to exact 0.
    #[cfg(windows)]
    fresh: bool,
}

impl CpuCollector {
    pub fn new() -> Self {
        CpuCollector {
            #[cfg(windows)]
            pdh: None,
            #[cfg(windows)]
            totals: Vec::new(),
            #[cfg(windows)]
            cores: Vec::new(),
            #[cfg(windows)]
            energy: None,
            #[cfg(windows)]
            init_retry: InitRetry::new(),
            #[cfg(windows)]
            tick: 0,
            #[cfg(windows)]
            fresh: false,
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
        match self.init_inner() {
            Ok(()) => {
                self.init_retry.note_success();
                Ok(())
            }
            Err(e) => Err(self.init_retry.note_failure(self.tick, e)),
        }
    }

    /// Build all counter state into locals and only publish it on success, so
    /// a failed init never leaves partial handles behind for a retry to
    /// duplicate or mis-address.
    #[cfg(windows)]
    fn init_inner(&mut self) -> Result<(), String> {
        let pdh = PdhQuery::open()?;

        let instances = PdhQuery::enum_instances("Processor Information").unwrap_or_else(|_| {
            // Fallback: synthesize group-0 instances from the active
            // processor count. Nonexistent instances fail AddCounter and
            // are skipped, so over-provisioning is safe.
            // SAFETY: trivial getter, no invalid inputs.
            let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1);
            let mut v = vec!["_Total".to_string()];
            for cpu in 0..n {
                v.push(format!("0,{cpu}"));
            }
            v
        });
        // Full-path counters outside "Processor Information".
        let mut added = 0usize;
        let mut totals: Vec<(String, usize)> = Vec::new();
        let mut cores: Vec<(String, usize, usize, usize, usize)> = Vec::new();
        let mut energy: Option<EnergyHandles> = None;
        for (field, path, _, _) in SYS_COUNTERS {
            let h = pdh.add(path);
            if h != 0 {
                added += 1;
            }
            totals.push((field.to_string(), h));
        }
        for inst in instances.iter().filter(|i| is_cpu_instance(i)) {
            let is_total = inst.eq_ignore_ascii_case("_total");
            let defs: &[(&str, &str)] = if is_total {
                TOTAL_COUNTERS
            } else {
                CORE_COUNTERS
            };
            let mut handles: Vec<usize> = Vec::new();
            for (_, counter) in defs {
                let path = format!("\\Processor Information({inst})\\{counter}");
                let h = pdh.add(&path);
                if h != 0 {
                    added += 1;
                }
                handles.push(h);
            }
            if is_total {
                // Extend: SYS_COUNTERS entries were pushed above and must
                // survive (a plain assignment here once silently dropped
                // ctx_switches / proc_queue).
                totals.extend(
                    TOTAL_COUNTERS
                        .iter()
                        .zip(handles)
                        .map(|((f, _), h)| (f.to_string(), h)),
                );
            } else if handles.len() == CORE_COUNTERS.len() {
                cores.push((inst.clone(), handles[0], handles[1], handles[2], handles[3]));
            }
        }
        // Package energy (Energy Meter): pick the platform's pkg instance.
        if let Ok(einsts) = PdhQuery::enum_instances("Energy Meter")
            && let Some(pkg) = einsts.iter().find(|i| is_package_instance(i))
        {
            let mut eh = EnergyHandles {
                power: 0,
                energy: 0,
                time: 0,
                instance: pkg.clone(),
                prev: None,
            };
            for (slot, counter) in [
                (&mut eh.power, "Power"),
                (&mut eh.energy, "Energy"),
                (&mut eh.time, "Time"),
            ] {
                let path = format!("\\Energy Meter({pkg})\\{counter}");
                let h = pdh.add(&path);
                if h != 0 {
                    *slot = h;
                    added += 1;
                }
            }
            if eh.power != 0 || eh.energy != 0 {
                energy = Some(eh);
            }
        }
        if added == 0 {
            return Err("no PDH CPU counters could be added".to_string());
        }
        // Warmup: rate counters need two collections before valid data.
        pdh.collect()?;
        // Baseline the energy window at collect #1 so the first read spans
        // the full 300ms warmup instead of a ~0ms window.
        if let Some(eh) = energy.as_ref() {
            let e = pdh.read_double(eh.energy);
            let t = pdh.read_double(eh.time);
            if let (Some(e), Some(t)) = (e, t)
                && let Some(eh) = energy.as_mut()
            {
                eh.prev = Some((e, t));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        pdh.collect()?;
        self.pdh = Some(pdh);
        self.totals = totals;
        self.cores = cores;
        self.energy = energy;
        self.fresh = true;
        Ok(())
    }

    pub fn read(&mut self) -> Result<CpuSample, String> {
        #[cfg(windows)]
        {
            self.tick = self.tick.wrapping_add(1);
            self.ensure_init()?;
            let pdh = self.pdh.as_ref().ok_or("PDH query not initialized")?;
            if self.fresh {
                // Use the warmup window (init collect #1 -> #2, 300ms).
                self.fresh = false;
            } else {
                pdh.collect()?;
            }
            // Package energy: read current Energy/Time, derive watts against
            // the previous window, then advance the baseline.
            let (e1, t1) = match self.energy.as_ref() {
                Some(eh) => (pdh.read_double(eh.energy), pdh.read_double(eh.time)),
                None => (None, None),
            };
            let pkg_power_w = self
                .energy
                .as_ref()
                .and_then(|eh| pdh.read_double(eh.power))
                .map(|mw| mw / 1000.0);
            let pkg_derived_w = match (self.energy.as_ref(), e1, t1) {
                (Some(eh), Some(e), Some(t)) => {
                    eh.prev.and_then(|(e0, t0)| energy_derived_w(e0, t0, e, t))
                }
                _ => None,
            };
            if let Some(eh) = self.energy.as_mut()
                && let (Some(e), Some(t)) = (e1, t1)
            {
                eh.prev = Some((e, t));
            }
            let energy_instance = self.energy.as_ref().map(|eh| eh.instance.clone());
            Ok(CpuSample {
                totals: self
                    .totals
                    .iter()
                    .map(|(f, h)| (f.clone(), pdh.read_double(*h)))
                    .collect(),
                cores: self
                    .cores
                    .iter()
                    .map(|(inst, hu, hp, hf, hpark)| CoreSample {
                        instance: inst.clone(),
                        utility: pdh.read_double(*hu),
                        performance: pdh.read_double(*hp),
                        freq_mhz: pdh.read_double(*hf),
                        parking: pdh.read_double(*hpark),
                    })
                    .collect(),
                energy_instance,
                pkg_power_w,
                pkg_derived_w,
            })
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

// No Drop impl: the owned PdhQuery closes the query itself.

#[cfg(windows)]
fn opt_num(v: Option<f64>) -> String {
    pf_core::telemetry::json_num(
        v,
        pf_core::telemetry::Provenance::Measured,
        pf_core::telemetry::UnavailKind::NotSampled,
        "counter missing or not ready",
    )
}

/// Estimated values are never labeled measured, even when they come from a
/// real counter (Energy Meter is a model, not a sensor).
#[cfg(windows)]
fn opt_est(v: Option<f64>) -> String {
    pf_core::telemetry::json_num(
        v,
        pf_core::telemetry::Provenance::Estimated,
        pf_core::telemetry::UnavailKind::NotSampled,
        "energy instance missing or warming up",
    )
}

/// Dynamic PDH field names/sources, built once per process (not per call).
static CPU_TOTAL_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    TOTAL_COUNTERS
        .iter()
        .map(|(f, _)| Box::leak(format!("cpu.total.{f}").into_boxed_str()) as &str)
        .collect()
});

static CPU_TOTAL_SOURCES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    TOTAL_COUNTERS
        .iter()
        .map(|(_, c)| {
            Box::leak(format!("PDH Processor Information(_Total)\\{c}").into_boxed_str()) as &str
        })
        .collect()
});

static CPU_SYS_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    SYS_COUNTERS
        .iter()
        .map(|(f, _, _, _)| Box::leak(format!("cpu.total.{f}").into_boxed_str()) as &str)
        .collect()
});

impl Collector for CpuCollector {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        let mut caps: Vec<FieldMeta> = TOTAL_COUNTERS
            .iter()
            .enumerate()
            .map(|(i, &(f, _))| FieldMeta {
                name: CPU_TOTAL_NAMES[i],
                unit: if f == "freq_mhz" {
                    "MHz"
                } else if f == "perf_limit_flags" {
                    "bitmask"
                } else if f.ends_with("_trans") || f == "interrupts" || f == "dpc_rate" || f == "idle_breaks" {
                    "/s"
                } else if f.ends_with("_pct")
                    || f == "utility"
                    || f == "priv_utility"
                    || f == "performance"
                    || f == "of_max_freq"
                    || f == "user_time"
                    || f == "processor_time"
                {
                    "%"
                } else {
                    "?"
                },
                source: CPU_TOTAL_SOURCES[i],
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: if f.starts_with('c') && f != "ctx_switches" {
                    "C-state residency without ETW; % C3 on modern standby-capable SoCs may read low"
                } else if f == "idle_breaks" {
                    "Key deep-idle diagnostic: high rate at low util => wakeup storm"
                } else if f == "performance" {
                    "Can exceed 100 under turbo; ratio to utility reveals frequency scaling"
                } else if f == "perf_limit_flags" {
                    "Raw bitmask; 0 observed while uncapped, bit meanings unverified"
                } else if f == "perf_limit_pct" {
                    "100 observed while uncapped; read as % of allowed performance (semantics uncertain)"
                } else {
                    ""
                },
            })
            .collect();
        for (i, (_field, _path, unit, notes)) in SYS_COUNTERS.iter().enumerate() {
            caps.push(FieldMeta {
                name: CPU_SYS_NAMES[i],
                unit,
                source: "PDH \\System object",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes,
            });
        }
        caps.push(FieldMeta {
            name: "cpu.core.utility",
            unit: "%",
            source: "PDH Processor Information(group,cpu)\\% Processor Utility",
            provenance: Provenance::Measured,
            interval_ms: 1000,
            requires_admin: false,
            notes: "One entry per enumerated core instance; count varies by machine",
        });
        caps.push(FieldMeta {
            name: "cpu.core.performance",
            unit: "%",
            source: "PDH Processor Information(group,cpu)\\% Processor Performance",
            provenance: Provenance::Measured,
            interval_ms: 1000,
            requires_admin: false,
            notes: "Per-core turbo/frequency behavior",
        });
        caps.push(FieldMeta {
            name: "cpu.core.freq_mhz",
            unit: "MHz",
            source: "PDH Processor Information(group,cpu)\\Processor Frequency",
            provenance: Provenance::Measured,
            interval_ms: 1000,
            requires_admin: false,
            notes: "Per-core clock; heterogeneity reveals scheduling/preferred-core behavior",
        });
        caps.push(FieldMeta {
            name: "cpu.core.parking",
            unit: "raw",
            source: "PDH Processor Information(group,cpu)\\Parking Status",
            provenance: Provenance::Measured,
            interval_ms: 1000,
            requires_admin: false,
            notes: "0 observed unparked on High-performance; nonzero semantics unverified",
        });
        caps.push(FieldMeta {
            name: "cpu.energy.pkg_power_w",
            unit: "W",
            source: "PDH Energy Meter(<pkg>)\\Power / 1000",
            provenance: Provenance::Estimated,
            interval_ms: 1000,
            requires_admin: false,
            notes: "E3 estimate incl. uncore; instance name varies by platform; validate vs battery",
        });
        caps.push(FieldMeta {
            name: "cpu.energy.pkg_derived_w",
            unit: "W",
            source: "dEnergy(pWh)/dTime(ms) x 3.6e-6 over sample window",
            provenance: Provenance::Estimated,
            interval_ms: 1000,
            requires_admin: false,
            notes: "Independent derivation from Energy/Time deltas; cross-checks pkg_power_w",
        });
        caps
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let sample = self.read().map_err(|e| CollectorError::new("cpu", e))?;
        let t1 = clock.stamp();
        Ok(Self::format_json(
            &sample,
            t1,
            t0.mono_millis,
            t1.mono_millis,
        ))
    }
}

impl CpuCollector {
    /// Render a previously-read sample. Separated from `read()` so callers
    /// (e.g. the monitor dashboard) can collect once and reuse the sample.
    /// `t_start_ms`/`t_end_ms` bound the actual acquisition work.
    pub fn format_json(
        sample: &CpuSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        let mut totals = String::new();
        for (i, (f, v)) in sample.totals.iter().enumerate() {
            if i > 0 {
                totals.push(',');
            }
            #[cfg(windows)]
            totals.push_str(&format!("\"{f}\":{}", opt_num(*v)));
            #[cfg(not(windows))]
            totals.push_str(&format!("\"{f}\":{{\"v\":null,\"p\":\"unavailable\"}}"));
        }
        let mut cores = String::new();
        for (i, c) in sample.cores.iter().enumerate() {
            if i > 0 {
                cores.push(',');
            }
            #[cfg(windows)]
            cores.push_str(&format!(
                "{{\"inst\":\"{}\",\"utility\":{},\"performance\":{},\"freq_mhz\":{},\"parking\":{}}}",
                escape_json(&c.instance),
                opt_num(c.utility),
                opt_num(c.performance),
                opt_num(c.freq_mhz),
                opt_num(c.parking)
            ));
            #[cfg(not(windows))]
            cores.push_str("{\"inst\":\"?\",\"utility\":null,\"performance\":null,\"freq_mhz\":null,\"parking\":null}");
        }
        #[cfg(windows)]
        let energy = match &sample.energy_instance {
            Some(inst) => format!(
                "{{\"instance\":\"{}\",\"pkg_power_w\":{},\"pkg_derived_w\":{}}}",
                escape_json(inst),
                opt_est(sample.pkg_power_w),
                opt_est(sample.pkg_derived_w)
            ),
            None => "{\"instance\":null,\"pkg_power_w\":{\"v\":null,\"p\":\"unavailable\",\"reason\":\"no package energy instance\"},\"pkg_derived_w\":{\"v\":null,\"p\":\"unavailable\",\"reason\":\"no package energy instance\"}}".to_string(),
        };
        #[cfg(not(windows))]
        let energy = "{\"instance\":null}".to_string();
        format!(
            "{{\"collector\":\"cpu\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\"totals\":{{{totals}}},\"cores\":[{cores}],\"energy\":{energy}}}",
            stamp.wall_millis, stamp.mono_millis, t_start_ms, t_end_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_filter_keeps_total_and_cores() {
        assert!(is_cpu_instance("_Total"));
        assert!(is_cpu_instance("_TOTAL"));
        assert!(is_cpu_instance("0,0"));
        assert!(is_cpu_instance("0,11"));
        assert!(!is_cpu_instance("0,_Total"));
        assert!(!is_cpu_instance(" averaging"));
        assert!(!is_cpu_instance("foo"));
    }

    #[test]
    fn core_utility_mean_ignores_missing() {
        let s = CpuSample {
            totals: vec![],
            cores: vec![
                CoreSample {
                    instance: "0,0".into(),
                    utility: Some(10.0),
                    performance: Some(100.0),
                    freq_mhz: Some(2300.0),
                    parking: Some(0.0),
                },
                CoreSample {
                    instance: "0,1".into(),
                    utility: None,
                    performance: None,
                    freq_mhz: None,
                    parking: None,
                },
                CoreSample {
                    instance: "0,2".into(),
                    utility: Some(30.0),
                    performance: Some(120.0),
                    freq_mhz: Some(2400.0),
                    parking: Some(0.0),
                },
            ],
            ..Default::default()
        };
        assert_eq!(s.mean_core_utility(), Some(20.0));
        assert_eq!(s.total("utility"), None);
        assert_eq!(s.parked_cores(), Some(0));
        let empty = CpuSample {
            totals: vec![("utility".into(), Some(5.5))],
            cores: vec![],
            ..Default::default()
        };
        assert_eq!(empty.mean_core_utility(), None);
        assert_eq!(empty.total("utility"), Some(5.5));
        assert_eq!(empty.parked_cores(), None);
        let parked = CpuSample {
            totals: vec![],
            cores: vec![CoreSample {
                instance: "0,3".into(),
                utility: Some(0.0),
                performance: Some(0.0),
                freq_mhz: Some(0.0),
                parking: Some(2.0),
            }],
            ..Default::default()
        };
        assert_eq!(parked.parked_cores(), Some(1));
    }

    #[test]
    fn energy_derivation_matches_live_probe() {
        // 2026-09-17 probe: dE=2836750000 pWh, dT=3082 ms -> ~3.31 W,
        // Power counter read 3.33 W in the same window.
        let w = energy_derived_w(11140853054166.0, 86571062.0, 11143689829166.0, 86574144.0);
        assert!((w.unwrap() - 3.31).abs() < 0.05);
        assert_eq!(energy_derived_w(0.0, 0.0, 100.0, 0.0), None);
        assert_eq!(energy_derived_w(200.0, 0.0, 100.0, 1000.0), None);
    }

    #[test]
    fn package_instance_picker() {
        assert!(is_package_instance("rapl_package0_pkg"));
        assert!(is_package_instance("RAPL_Package0_PKG"));
        assert!(!is_package_instance("rapl_package0_core0_core"));
        assert!(!is_package_instance("_total"));
    }

    #[test]
    fn cpu_mismatch_needs_both_sides() {
        assert_eq!(
            aggregate_cpu_mismatch(&[4.0, 5.0], &[14.0, 15.0]),
            Some(10.0)
        );
        assert_eq!(aggregate_cpu_mismatch(&[5.0], &[5.0]), Some(0.0));
        assert_eq!(aggregate_cpu_mismatch(&[], &[5.0]), None);
        assert_eq!(aggregate_cpu_mismatch(&[5.0], &[]), None);
    }

    #[cfg(windows)]
    #[test]
    fn init_errors_classify_by_cause() {
        // Permanent: the object genuinely does not exist here.
        assert_eq!(
            classify_init_error("PdhAddEnglishCounterW failed: \\No Such Object(*)\\X"),
            InitClass::PermanentUnsupported
        );
        assert_eq!(
            classify_init_error("no representative English counter for object Foo"),
            InitClass::PermanentUnsupported
        );
        // Permission: an in-process retry can never elevate.
        assert_eq!(
            classify_init_error("PdhOpenQueryW failed: 0x80070005 (Access is denied)"),
            InitClass::PermissionElevation
        );
        // Transient provider fault.
        assert_eq!(
            classify_init_error("PdhCollectQueryData failed: 0x800007D2"),
            InitClass::TransientProvider
        );
        // Temporary device disappearance.
        assert_eq!(
            classify_init_error(
                "no instances enumerated for \\GPU Engine(*)\\Utilization Percentage"
            ),
            InitClass::TemporaryDeviceGone
        );
        assert!(InitClass::TransientProvider.retryable());
        assert!(InitClass::TemporaryDeviceGone.retryable());
        assert!(!InitClass::PermanentUnsupported.retryable());
        assert!(!InitClass::PermissionElevation.retryable());
    }

    #[cfg(windows)]
    #[test]
    fn transient_init_failure_is_retried_and_recovers() {
        let mut r = InitRetry::new();
        let msg = "PdhCollectQueryData failed: 0x800007D2".to_string();
        assert_eq!(r.note_failure(1, msg.clone()), msg);
        assert_eq!(r.class(), InitClass::TransientProvider);
        // Backoff: first retry is 5 ticks after the failure, not immediate.
        assert!(!r.may_attempt(2));
        assert!(!r.may_attempt(5));
        assert!(r.may_attempt(6));
        // A second failure doubles the delay (10 ticks).
        r.note_failure(6, "PdhCollectQueryData failed: 0x800007D2".into());
        assert!(!r.may_attempt(11));
        assert!(r.may_attempt(16));
        // A later successful attempt resets the guard entirely.
        r.note_success();
        assert!(r.may_attempt(16));
        assert_eq!(r.cached(), None);
    }

    #[cfg(windows)]
    #[test]
    fn backoff_is_capped() {
        // 5, 10, 20, 40, 80, 160, then capped at 320 — never unbounded.
        assert_eq!(backoff_ticks(1), 5);
        assert_eq!(backoff_ticks(2), 10);
        assert_eq!(backoff_ticks(3), 20);
        assert_eq!(backoff_ticks(7), 320);
        assert_eq!(backoff_ticks(50), 320);
    }

    #[cfg(windows)]
    #[test]
    fn permanent_init_failure_is_not_retried() {
        let mut r = InitRetry::new();
        r.note_failure(1, "no representative English counter for object Foo".into());
        assert_eq!(r.class(), InitClass::PermanentUnsupported);
        for t in [2u64, 100, 100_000, u64::MAX] {
            assert!(!r.may_attempt(t), "permanent error retried at tick {t}");
        }
        assert!(r.cached().is_some(), "cached reason must stay available");
    }

    #[cfg(windows)]
    #[test]
    fn permission_init_failure_is_not_retried() {
        let mut r = InitRetry::new();
        r.note_failure(
            1,
            "PdhOpenQueryW failed: 0x80070005 Access is denied".into(),
        );
        assert_eq!(r.class(), InitClass::PermissionElevation);
        assert!(!r.may_attempt(1_000_000));
    }
}
