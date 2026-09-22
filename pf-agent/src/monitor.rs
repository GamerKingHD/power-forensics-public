//! Monitor agent: scheduler wiring, per-collector handlers, session writer.
//!
//! Phase-4 architecture: every collector runs in its own worker thread at
//! its own cadence and stamps its own acquisition window. Samples flow over
//! a bounded channel to this single writer loop, which formats, stores,
//! prints, and analyzes. A stalled collector delays only itself; the
//! timestamps expose the laggard instead of shifting the whole session.
//!
//! Handler bodies are preserved from the sequential loop; only the data
//! flow changed (channel messages instead of direct reads).

use crate::sched;
use crate::store::SessionWriter;
use pf_collectors::batinfo::BatteryStatic;
use pf_collectors::battery::{BatteryCollector, BatteryRaw};
use pf_collectors::collector::Collector;
use pf_collectors::cpu::{CpuCollector, CpuSample};
use pf_collectors::display::{DisplayCollector, DisplaySample};
use pf_collectors::gpu::{GpuCollector, GpuSample};
use pf_collectors::network::{self, NetCollector, NetSample};
use pf_collectors::proc::{ProcCollector, ProcSample};
use pf_collectors::selfmon::{SelfCollector, SelfSample};
use pf_collectors::storage::{ActivityWatch, StorageCollector, StorageSample};
use pf_collectors::system::{OsPowerCollector, OsPowerRaw};
use pf_collectors::usb::{UsbCollector, UsbSample};
use pf_core::analysis::MultiBaseline;
use pf_core::stats;
use pf_core::telemetry::{Clock, ClockStamp, escape_json};

pub const VERSION: &str = "0.1.0";

/// Phase C live-snapshot slot (no behavior change when `None`). The writer
/// loop publishes the latest battery/live state into this slot after each
/// received sample, unless paused. Thin wrapper over
/// `crate::service::set_live_snapshot` so the loop never touches `Service`
/// directly; `snapshot_from_sessiondata` conversion lives in `crate::service`.
pub fn set_live_snapshot(
    slot: &Option<std::sync::Arc<std::sync::Mutex<crate::ipc_server::StateSnapshot>>>,
    snap: crate::ipc_server::StateSnapshot,
) {
    crate::service::set_live_snapshot(slot, snap);
}

/// Read an optional pause flag; `None` means "never paused". While set the
/// writer loop drops samples without advancing recording state.
pub fn is_paused_flag(paused: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) -> bool {
    crate::service::is_paused_flag(paused)
}

/// Human rate for a possibly-unavailable total: `n/a` (never `0B/s`) when no
/// interface produced a usable counter. Keeps the NET TOTAL line honest.
pub fn human_bps_opt(v: Option<f64>) -> String {
    v.map(network::human_bps)
        .unwrap_or_else(|| "n/a".to_string())
}

pub struct MonitorOpts {
    pub interval_ms: u64,
    pub samples: Option<u64>,
    pub out: Option<String>,
    pub note: String,
    pub helper: Option<String>,
    pub helper_every: u64,
    /// Comma-separated collector subset (e.g. "battery,cpu,os").
    pub collectors: Option<String>,
    /// Preset: normal (default), low (battery+cpu+os @5s), deep (all @250ms).
    pub preset: Option<String>,
}

/// Resolve the effective base monitoring cadence from defaults, an explicit
/// `--interval-ms`, and preset overrides. This is the SINGLE source of truth
/// for the base cadence: it is written into the session header and passed to
/// the scheduler and the live energy gap threshold, so a recovered session
/// derives the same temporal semantics from the header as the clean
/// finalization used. Do not recompute an effective cadence elsewhere.
pub fn effective_base_interval(opts: &MonitorOpts) -> u64 {
    match opts.preset.as_deref() {
        Some("low") => 5000,
        // Deep keeps the user's interval when it is already faster than 250 ms.
        Some("deep") => opts.interval_ms.min(250),
        _ => opts.interval_ms,
    }
}

fn collectors() -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(BatteryCollector::new()),
        Box::new(CpuCollector::new()),
        Box::new(GpuCollector::new()),
        Box::new(ProcCollector::new()),
        Box::new(DisplayCollector::new()),
        Box::new(NetCollector::new()),
        Box::new(StorageCollector::new()),
        Box::new(UsbCollector::new()),
        Box::new(OsPowerCollector::new()),
        Box::new(SelfCollector::new()),
    ]
}

pub fn capabilities_json() -> String {
    let collectors = collectors();
    let mut out = String::from("{\"collectors\":[");
    for (ci, c) in collectors.iter().enumerate() {
        if ci > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"name\":\"{}\",\"fields\":[", c.name()));
        for (fi, f) in c.capabilities().iter().enumerate() {
            if fi > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"unit\":\"{}\",\"source\":\"{}\",\
                 \"provenance\":\"{}\",\"interval_ms\":{},\
                 \"requires_admin\":{},\"notes\":\"{}\"}}",
                escape_json(f.name),
                escape_json(f.unit),
                escape_json(f.source),
                f.provenance.as_str(),
                f.interval_ms,
                f.requires_admin,
                escape_json(f.notes)
            ));
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    out
}

pub fn cmd_sample() -> i32 {
    let clock = Clock::new();
    let mut collectors = collectors();
    let mut failed = 0;
    for c in collectors.iter_mut() {
        match c.sample_json(&clock) {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("error: {e}");
                failed += 1;
            }
        }
    }
    if failed > 0 { 1 } else { 0 }
}
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ipc_server::StateSnapshot;

/// Typed sample from a worker. Formatting stays centralized here so the
/// session, dashboard, and analysis all see identical values.
///
/// `Battery` is much larger than the other variants, but these messages are
/// short-lived (bounded channel, consumed immediately) so the size difference
/// is not worth an allocation/indirection on the hot path.
#[allow(clippy::large_enum_variant)]
pub enum Payload {
    Battery(Result<(BatteryRaw, Vec<(usize, BatteryStatic)>), String>),
    Cpu(Result<CpuSample, String>),
    Gpu(Result<GpuSample, String>),
    Proc(Result<ProcSample, String>),
    Display(Result<DisplaySample, String>),
    Net(Result<NetSample, String>),
    Storage(Result<StorageSample, String>),
    Usb(Result<UsbSample, String>),
    Os(OsPowerRaw),
    Elevated(Result<String, String>),
    SelfMon(Result<SelfSample, String>),
}

fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn worker_stamp(base: Instant) -> ClockStamp {
    ClockStamp {
        wall_millis: now_wall_ms(),
        mono_millis: base.elapsed().as_millis() as u64,
    }
}

/// Upper bound on retained recent battery samples. At the fastest adaptive
/// cadence (250 ms) this covers >1000 s, comfortably more than the 300 s
/// runtime window and the 30-sample live ring; memory is fixed regardless of
/// session length.
const RECENT_BATTERY_CAP: usize = 4096;
/// Fixed histogram (lo, hi, bins) for streaming percentiles. Resolution is
/// 0.1 W / 0.1 pct. Percentiles are a bucket-quantile APPROXIMATION: there is
/// NO universal error bound. For a series concentrated within the target
/// rank's bin (constant or dense-smooth) the estimate is within one bin width
/// (<= 0.1); for spread/bimodal series the interpolation can be off by a large
/// fraction of the range. Out-of-range values clamp into the edge bin and
/// remain counted, but with unbounded absolute error. Do not quote a general
/// bound for arbitrary distributions.
const DISCHARGE_HIST: (f64, f64, usize) = (0.0, 300.0, 3000);
const CPU_UTIL_HIST: (f64, f64, usize) = (0.0, 100.0, 1000);
const PKG_HIST: (f64, f64, usize) = (0.0, 200.0, 2000);
/// Hard cap for the streaming summary state, asserted by the long-run test.
#[cfg(test)]
const SUMMARY_STATE_CAP_BYTES: usize = 1 << 20;

/// Fixed-bin histogram with O(1) insertion and bounded size. Percentiles are
/// approximated by locating the target rank's bin and interpolating linearly
/// inside it (bin width = (hi-lo)/bins). This replaces the exact session-wide
/// sort, which required retaining every sample for the whole session.
/// `count`/`sum`/`min`/`max` remain exact; non-finite readings are dropped.
#[derive(Debug, Clone)]
struct FixedHistogram {
    lo: f64,
    inv_width: f64,
    bins: Vec<u64>,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl FixedHistogram {
    fn new(lo: f64, hi: f64, n: usize) -> Self {
        let n = n.max(1);
        FixedHistogram {
            lo,
            inv_width: n as f64 / (hi - lo).max(f64::MIN_POSITIVE),
            bins: vec![0; n],
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    fn push(&mut self, v: f64) {
        if !v.is_finite() {
            return;
        }
        self.count += 1;
        self.sum += v;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
        let idx = ((v - self.lo) * self.inv_width).floor();
        let idx = idx.clamp(0.0, (self.bins.len() - 1) as f64) as usize;
        self.bins[idx] += 1;
    }

    /// Linear-interpolated percentile (p in 0..=100), None when empty.
    fn percentile(&self, p: f64) -> Option<f64> {
        if self.count == 0 || !(0.0..=100.0).contains(&p) {
            return None;
        }
        let width = 1.0 / self.inv_width;
        let target = p / 100.0 * (self.count - 1) as f64;
        let mut cum = 0u64;
        for (i, &c) in self.bins.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let next = cum + c;
            if target < next as f64 || i == self.bins.len() - 1 {
                let frac = if c > 1 {
                    ((target - cum as f64) / (c as f64 - 1.0)).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                return Some(self.lo + (i as f64 + frac) * width);
            }
            cum = next;
        }
        None
    }

    #[cfg(test)]
    fn heap_bytes(&self) -> usize {
        self.bins.capacity() * std::mem::size_of::<u64>()
    }
}

/// Streaming count/sum/max for lateness and collection-time series: the old
/// per-collector `Vec<f64>` grew for the whole session only for these numbers.
#[derive(Debug, Clone, Copy, Default)]
struct TimeStats {
    count: u64,
    sum: f64,
    max: f64,
}

impl TimeStats {
    fn push(&mut self, v: f64) {
        self.count += 1;
        self.sum += v;
        if v > self.max {
            self.max = v;
        }
    }

    fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RecentBattery {
    mono_secs: f64,
    discharge_w: Option<f64>,
}

/// Streaming dual-clock segmented energy accumulator. Mirrors
/// `stats::integrate_segmented_clocks` pair-by-pair: a boundary is forced on a
/// backwards clock, a gap beyond `max_gap`, or wall/mono divergence beyond
/// `max_skew`, and energy is never integrated across it. Only the previous
/// sample is retained, so a suspend/clock jump cannot contribute phantom Wh
/// while memory stays constant.
#[derive(Debug, Clone, Copy, Default)]
struct EnergyStream {
    discharge: stats::SegmentEnergy,
    charge: stats::SegmentEnergy,
    /// Per-kind discontinuity tally, indexed by `DiscontinuityKind::index`.
    disc_kinds: [u64; 4],
    last: Option<(f64, f64, Option<f64>, Option<f64>)>,
}

impl EnergyStream {
    fn push(
        &mut self,
        mono: f64,
        wall: f64,
        discharge: Option<f64>,
        charge: Option<f64>,
        max_gap: f64,
        max_skew: f64,
    ) {
        if let Some((pm, pw, pd, pc)) = self.last {
            let mono_dt = mono - pm;
            let wall_dt = (pw > 0.0 && wall > 0.0).then_some(wall - pw);
            // Single source of truth with the batch/recovery scan, so a clean
            // footer and a recovered footer cannot disagree on boundaries.
            if let Some(kind) = stats::classify_step(mono_dt, wall_dt, max_gap, max_skew) {
                let unknown = if mono_dt > 0.0 { mono_dt } else { 0.0 };
                self.discharge.unknown_secs += unknown;
                self.charge.unknown_secs += unknown;
                self.discharge.discontinuities += 1;
                self.charge.discontinuities += 1;
                self.disc_kinds[kind.index()] += 1;
                let unobserved = wall_dt.map(|w| mono_dt.max(w)).unwrap_or(mono_dt).max(0.0);
                self.discharge.unobserved_secs += unobserved;
                self.charge.unobserved_secs += unobserved;
            } else {
                Self::pair(&mut self.discharge, mono_dt, pd, discharge, max_gap);
                Self::pair(&mut self.charge, mono_dt, pc, charge, max_gap);
            }
        }
        self.last = Some((mono, wall, discharge, charge));
    }

    fn pair(seg: &mut stats::SegmentEnergy, dt: f64, a: Option<f64>, b: Option<f64>, max_gap: f64) {
        match (a, b) {
            (Some(a), Some(b)) if dt <= max_gap => {
                seg.energy_wh += 0.5 * (a + b) * dt / 3600.0;
                seg.covered_secs += dt;
            }
            _ => seg.unknown_secs += dt,
        }
    }
}

/// Streaming accumulator for a byte-rate total over a sampled timeline.
/// Distinguishes a *measured* zero from *no observation*: a pair contributes
/// `mb += rate*dt` only when both an interval and a usable rate exist and the
/// gap is within `max_gap`; missing rates and oversized gaps accumulate
/// `unknown_secs` and are never bridged as if activity were zero.
#[derive(Debug, Clone, Copy, Default)]
struct ThroughputAccum {
    mb: f64,
    covered_secs: f64,
    unknown_secs: f64,
    /// Pairs that actually contributed a usable rate (a measured 0 counts).
    samples: u64,
    discontinuities: usize,
}

impl ThroughputAccum {
    fn push(&mut self, dt: Option<f64>, rate_bps: Option<f64>, max_gap: f64) {
        let Some(dt) = dt else {
            return;
        };
        if dt < 0.0 {
            return;
        }
        if dt > max_gap {
            self.unknown_secs += dt;
            self.discontinuities += 1;
            return;
        }
        match rate_bps {
            Some(r) => {
                self.mb += r * dt / (1024.0 * 1024.0);
                self.covered_secs += dt;
                self.samples += 1;
            }
            None => self.unknown_secs += dt,
        }
    }

    /// Total in MiB, or None when no interval carried a usable rate (so a
    /// caller can say "no usable disk measurement" rather than "0 MB").
    fn total_mb(&self) -> Option<f64> {
        (self.samples > 0).then_some(self.mb)
    }

    fn coverage(&self) -> Option<f64> {
        let span = self.covered_secs + self.unknown_secs;
        if span <= 0.0 {
            None
        } else {
            Some(self.covered_secs / span)
        }
    }
}

/// All per-session summary accumulators, bounded by construction. Session
/// length changes only counts/sums/histogram tallies; it never increases
/// retained memory, so the footer can cover arbitrarily long runs.
struct SummaryAccum {
    discharge: FixedHistogram,
    cpu_util: FixedHistogram,
    pkg_w: FixedHistogram,
    recent: std::collections::VecDeque<RecentBattery>,
    energy: EnergyStream,
    charge_n: u64,
    collect_ms: HashMap<&'static str, TimeStats>,
    counts: HashMap<&'static str, u64>,
    late: HashMap<&'static str, TimeStats>,
    max_gap: f64,
    max_skew: f64,
}

impl SummaryAccum {
    fn new(base_interval: u64) -> Self {
        SummaryAccum {
            discharge: FixedHistogram::new(DISCHARGE_HIST.0, DISCHARGE_HIST.1, DISCHARGE_HIST.2),
            cpu_util: FixedHistogram::new(CPU_UTIL_HIST.0, CPU_UTIL_HIST.1, CPU_UTIL_HIST.2),
            pkg_w: FixedHistogram::new(PKG_HIST.0, PKG_HIST.1, PKG_HIST.2),
            recent: std::collections::VecDeque::with_capacity(RECENT_BATTERY_CAP),
            energy: EnergyStream::default(),
            charge_n: 0,
            collect_ms: HashMap::new(),
            counts: HashMap::new(),
            late: HashMap::new(),
            max_gap: stats::energy_max_gap_secs(base_interval),
            max_skew: pf_core::analysis::MAX_CLOCK_SKEW_SECS,
        }
    }

    fn record_time(&mut self, name: &'static str, collect_ms: f64) {
        self.collect_ms.entry(name).or_default().push(collect_ms);
        *self.counts.entry(name).or_default() += 1;
    }

    fn record_late(&mut self, name: &'static str, late_ms: f64) {
        self.late.entry(name).or_default().push(late_ms);
    }

    fn record_battery(
        &mut self,
        mono_secs: f64,
        wall_secs: f64,
        discharge: Option<f64>,
        charge: Option<f64>,
    ) {
        self.energy.push(
            mono_secs,
            wall_secs,
            discharge,
            charge,
            self.max_gap,
            self.max_skew,
        );
        if let Some(w) = discharge {
            self.discharge.push(w);
        }
        if charge.is_some() {
            self.charge_n += 1;
        }
        if self.recent.len() >= RECENT_BATTERY_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back(RecentBattery {
            mono_secs,
            discharge_w: discharge,
        });
    }

    fn record_cpu(&mut self, utility: f64) {
        self.cpu_util.push(utility);
    }

    fn record_pkg(&mut self, w: f64) {
        self.pkg_w.push(w);
    }

    /// Last `n` discharge readings (including `None` gaps), newest last.
    fn recent_discharges(&self, n: usize) -> Vec<Option<f64>> {
        self.recent
            .iter()
            .rev()
            .take(n)
            .rev()
            .map(|s| s.discharge_w)
            .collect()
    }

    /// Mean discharge over retained readings within `window_secs` of `now`.
    fn window_mean_discharge(&self, now: f64, window_secs: f64) -> Option<f64> {
        let cutoff = now - window_secs;
        let mut sum = 0.0;
        let mut n = 0usize;
        for s in &self.recent {
            if s.mono_secs >= cutoff
                && let Some(w) = s.discharge_w
            {
                sum += w;
                n += 1;
            }
        }
        if n == 0 { None } else { Some(sum / n as f64) }
    }

    /// Approximate retained heap footprint, used by the long-run bound test.
    #[cfg(test)]
    fn stored_bytes(&self) -> usize {
        use std::mem::size_of;
        size_of::<SummaryAccum>()
            + self.discharge.heap_bytes()
            + self.cpu_util.heap_bytes()
            + self.pkg_w.heap_bytes()
            + self.recent.capacity() * size_of::<RecentBattery>()
            + self.collect_ms.capacity() * (size_of::<&'static str>() + size_of::<TimeStats>())
            + self.counts.capacity() * (size_of::<&'static str>() + size_of::<u64>())
            + self.late.capacity() * (size_of::<&'static str>() + size_of::<TimeStats>())
    }
}

struct MonitorState {
    session: SessionWriter,
    path: String,
    /// Bounded streaming summary (histograms, energy, lateness, counters).
    summary: SummaryAccum,
    baselines: MultiBaseline,
    dch_count: u64,
    disk_read: ThroughputAccum,
    disk_write: ThroughputAccum,
    prev_disk_mono: Option<f64>,
    /// Expected cadence per collector + last arrival, for lateness.
    sched_iv: HashMap<&'static str, u64>,
    expected: HashMap<&'static str, u64>,
    active: Vec<&'static str>,
    boosted: u32,
    battery_interval: Arc<AtomicU64>,
    base_interval: u64,
    /// Latest system-wide ctx_switches/s from the cpu collector, copied
    /// into each self sample as an Estimated informational field.
    last_cpu_ctx: Option<f64>,
    /// Last ctx value actually stamped into a self sample (footer `self_ctx_s`).
    last_self_ctx: Option<f64>,
    watch: ActivityWatch,
    n_battery: u64,
    /// Latest live state served over IPC; refreshed on battery samples.
    live: StateSnapshot,
    live_slot: Option<Arc<Mutex<StateSnapshot>>>,
    /// Shared pause cell: while set, samples are dropped and `live` is not
    /// published. `None` means "never paused" (legacy monitor path).
    pause_slot: Option<Arc<AtomicBool>>,
    /// Pending markers drained into the session as `event` records. `None`
    /// for the legacy path.
    marker_slot: Option<crate::service::MarkerQueue>,
    /// Count of timeline events recorded (mirrors `StateSnapshot::events`).
    events: u64,
    /// Primary collector whose arrivals bound `--samples`: battery when
    /// active, otherwise the first selected collector.
    tick_collector: &'static str,
    /// Number of primary-collector samples recorded (the `--samples` unit).
    tick: u64,
    /// Session-start monotonic ms, for the primary-collector fallback grace.
    start_mono_ms: u64,
    /// Whether the `--samples` primary was already reassigned because the
    /// chosen collector never produced a sample.
    primary_fallback_done: bool,
    /// Collectors whose next arrival is currently overdue beyond their
    /// cadence (a blocked worker or transient stall), plus episode count.
    /// Populated by `poll_collector_timeouts`; cleared when the collector
    /// delivers again.
    timeouts: sched::TimeoutWatch,
}

impl MonitorState {
    fn record_time(&mut self, name: &'static str, collect_ms: f64) {
        self.summary.record_time(name, collect_ms);
    }

    fn write_line(&mut self, line: &str) -> bool {
        if let Err(e) = self.session.write_line(line) {
            eprintln!("error: session write failed: {e}");
            return false;
        }
        true
    }

    fn event(&mut self, wall_ms: u64, mono_ms: u64, kind: &str, detail: &str) {
        self.events += 1;
        let _ = self.session.event(wall_ms, mono_ms, kind, detail);
    }

    /// Drain queued markers into the session as `marker` events. Called each
    /// writer-loop iteration (including while paused) and once more before
    /// finalizing, so a marker queued just before stop is still recorded. The
    /// monotonic stamp is derived at drain time by subtracting the wall time
    /// elapsed since the marker was queued, keeping both clocks aligned.
    fn drain_markers(&mut self, stamp: ClockStamp) {
        let Some(slot) = self.marker_slot.clone() else {
            return;
        };
        let pending: Vec<(u64, String)> = match slot.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return,
        };
        for (wall, text) in pending {
            let elapsed = stamp.wall_millis.saturating_sub(wall);
            let mono = stamp.mono_millis.saturating_sub(elapsed);
            self.event(wall, mono, "marker", &text);
        }
    }

    /// Record the arrival of `collector`, clearing any active timeout and
    /// emitting a truthful recovery event. Returns nothing; best-effort.
    fn note_arrival(&mut self, collector: &'static str, wall_ms: u64, mono_ms: u64) {
        if self.timeouts.note_arrival(collector) {
            self.event(
                wall_ms,
                mono_ms,
                "collector_recovered",
                &format!("collector '{collector}' produced a sample again"),
            );
        }
    }

    /// Detect collectors whose next sample is overdue far beyond their
    /// cadence. This is distinct from a failed read: a provider failure still
    /// delivers an `Err` payload on time, while a blocked/stalled worker
    /// delivers nothing. Each collector can be in at most one timeout episode
    /// at a time, so repeated polls never duplicate evidence.
    fn poll_collector_timeouts(&mut self, now_mono: u64, wall_ms: u64) {
        for name in self.active.clone() {
            let Some(iv) = self.sched_iv.get(name).copied() else {
                continue;
            };
            let base = self
                .expected
                .get(name)
                .copied()
                .unwrap_or(self.start_mono_ms);
            if self.timeouts.poll(name, now_mono, base, iv) {
                let overdue = now_mono.saturating_sub(base);
                self.event(
                    wall_ms,
                    now_mono,
                    "collector_timeout",
                    &format!(
                        "collector '{name}' produced no sample for {}s (cadence {}ms); treating as blocked, not failed",
                        overdue / 1000,
                        iv
                    ),
                );
            }
        }
    }

    fn handle_battery(
        &mut self,
        res: Result<(BatteryRaw, Vec<(usize, BatteryStatic)>), String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        let mono_secs = stamp.mono_millis as f64 / 1000.0;
        let (power_txt, charge_txt, remain_txt, runtime_txt, boost_txt) = match res {
            Ok((raw, statics)) => {
                let line = BatteryCollector::format_json(&raw, &statics, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                let pct = raw
                    .charge_percent
                    .value()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".to_string());
                let rem = raw
                    .remaining_wh()
                    .value()
                    .map(|v| format!("{v:.1}"))
                    .unwrap_or_else(|| "n/a".to_string());
                let dch = raw.discharge_w().value().copied();
                let chg = raw.charge_w().value().copied();
                // Bounded streaming accumulation: dual-clock segmented energy,
                // discharge histogram, recent ring, and charge count.
                self.summary
                    .record_battery(mono_secs, stamp.wall_millis as f64 / 1000.0, dch, chg);
                // Refresh the live snapshot served over IPC (published by the
                // writer loop unless paused). Provenance-free latest readings.
                self.live.watts = dch;
                self.live.charge_w = chg;
                self.live.pct = raw.charge_percent.value().map(|v| *v as f64);
                self.live.remaining_wh = raw.remaining_wh().value().copied();
                self.live.recent = self.summary.recent_discharges(30);
                let (pwr, kind) = match dch {
                    Some(w) => (format!("{w:.2} dch"), "dch"),
                    None => match chg {
                        Some(w) => (format!("{w:.2} chg"), "chg"),
                        None => ("n/a".to_string(), "idle"),
                    },
                };
                let mut boost_txt = String::new();
                if kind == "dch"
                    && let Some(last) = dch
                {
                    self.baselines.push(last);
                    self.dch_count += 1;
                    if self.dch_count >= 10
                        && self.baselines.should_boost(last, 1.0, 0.2)
                        && self.boosted == 0
                    {
                        self.boosted = 8;
                        self.battery_interval
                            .store((self.base_interval / 4).max(250), Ordering::Relaxed);
                        let sm = self.baselines.short_med().unwrap_or(f64::NAN);
                        let mm = self.baselines.medium_med().unwrap_or(f64::NAN);
                        let lm = self.baselines.long_med().unwrap_or(f64::NAN);
                        self.event(
                                stamp.wall_millis,
                                stamp.mono_millis,
                                "anomaly_hint",
                                &format!(
                                    "discharge deviated to {last:.2} W vs short-baseline median {sm:.2} W (medium {mm:.2}, long {lm:.2}); boosting sample rate"
                                ),
                            );
                        boost_txt = "boost".to_string();
                    }
                }
                if self.boosted > 0 {
                    self.boosted -= 1;
                    if self.boosted == 0 {
                        self.battery_interval
                            .store(self.base_interval, Ordering::Relaxed);
                    }
                }
                let rt = |secs: u64| -> String {
                    let avg = self.summary.window_mean_discharge(mono_secs, secs as f64);
                    match (avg, raw.remaining_wh().value()) {
                        (Some(avg), Some(rem)) => stats::runtime_hours(*rem, avg)
                            .map(|h| format!("{:.0}h{:02.0}m", h.floor(), (h % 1.0) * 60.0))
                            .unwrap_or_else(|| "n/a".to_string()),
                        _ => "n/a".to_string(),
                    }
                };
                (pwr, pct, rem, format!("{}/{}", rt(60), rt(300)), boost_txt)
            }
            Err(e) => {
                eprintln!("error: battery read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[battery] {e}"),
                );
                (
                    "err".to_string(),
                    "err".to_string(),
                    "err".to_string(),
                    "n/a".to_string(),
                    String::new(),
                )
            }
        };
        println!(
            "{power_txt:>10}  {charge_txt:>7}  {remain_txt:>9}  {runtime_txt:>13}  {boost_txt}",
        );
        true
    }

    fn handle_cpu(
        &mut self,
        res: Result<CpuSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(s) => {
                // Cache the system-wide ctx rate for selfmon attribution
                // (informational Estimated copy; see handle_selfmon).
                self.last_cpu_ctx = s.total("ctx_switches");
                let line = CpuCollector::format_json(&s, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                if let Some(u) = s.total("utility") {
                    self.summary.record_cpu(u);
                }
                let f = |k: &str| {
                    s.total(k)
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_else(|| "n/a".to_string())
                };
                let c123 = format!("{}/{}/{}", f("c1_pct"), f("c2_pct"), f("c3_pct"));
                let mean_core = s
                    .mean_core_utility()
                    .map(|v| format!("{v:.1}"))
                    .unwrap_or_else(|| "n/a".to_string());
                let park = s
                    .parked_cores()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".to_string());
                if let Some(w) = s.pkg_derived_w {
                    self.summary.record_pkg(w);
                }
                let pkg = s
                    .pkg_derived_w
                    .map(|v| format!("~{v:.1}"))
                    .unwrap_or_else(|| {
                        s.pkg_power_w
                            .map(|v| format!("~{v:.1}"))
                            .unwrap_or_else(|| "n/a".to_string())
                    });
                println!(
                    "{:>9}  {:>5}  {:>8}  {:>8}  {c123:>10}  {mean_core:>10}  {park:>4}  {:>7}  {pkg:>5}",
                    f("utility"),
                    f("performance"),
                    f("freq_mhz"),
                    f("idle_breaks"),
                    f("ctx_switches"),
                );
            }
            Err(e) => {
                eprintln!("error: cpu read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[cpu] {e}"),
                );
                println!("{:>9}  cpu_error", "n/a");
            }
        }
        true
    }

    fn handle_gpu(
        &mut self,
        res: Result<GpuSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(gs) => {
                let line = GpuCollector::format_json(&gs, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                if gs.stale {
                    println!("note: gpu instance list stale");
                }
                for v in &gs.adapters {
                    let s = &v.stats;
                    let top = s
                        .top_pids
                        .first()
                        .map(|(pid, _)| pid.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let state = if v.awake.active { "ACTIVE" } else { "idle" };
                    let vendor = if v.discrete {
                        format!("{}*", pf_collectors::gpu::vendor_name(v.adapter.vendor_id))
                    } else {
                        pf_collectors::gpu::vendor_name(v.adapter.vendor_id).to_string()
                    };
                    println!(
                        "GPU{}  {:<9}  {:>5.1}  {:>5.1}  {:>5.1}  {:>5.1}  {:>6.0}  {:>4}  {:>7}  {}",
                        v.adapter.index,
                        vendor,
                        s.total_util,
                        s.util_3d,
                        s.util_decode,
                        s.util_copy,
                        s.mem_dedicated_mb,
                        s.top_pids.len(),
                        top,
                        state,
                    );
                }
            }
            Err(e) => {
                eprintln!("error: gpu read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[gpu] {e}"),
                );
            }
        }
        true
    }

    fn handle_proc(
        &mut self,
        res: Result<ProcSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(ps) => {
                let line = ProcCollector::format_json(&ps, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                let f2 = |v: Option<f64>| {
                    v.map(|x| format!("{x:.1}"))
                        .unwrap_or_else(|| "n/a".to_string())
                };
                for p in ps.top.iter().take(5) {
                    println!(
                        "PROC  {:>6}  {:<20}  {:>5}  {:>7}  {:>7}  {:>7}  {:>4}",
                        p.pid,
                        p.name.chars().take(20).collect::<String>(),
                        f2(p.cpu_pct),
                        f2(p.ws_mb),
                        p.io_r_bps
                            .map(|x| format!("{:.0}", x))
                            .unwrap_or_else(|| "n/a".to_string()),
                        p.io_w_bps
                            .map(|x| format!("{:.0}", x))
                            .unwrap_or_else(|| "n/a".to_string()),
                        p.threads,
                    );
                }
            }
            Err(e) => {
                eprintln!("error: proc read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[proc] {e}"),
                );
            }
        }
        true
    }

    fn handle_storage(
        &mut self,
        res: Result<StorageSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        let mono_secs = stamp.mono_millis as f64 / 1000.0;
        match res {
            Ok(ss) => {
                let line = StorageCollector::format_json(&ss, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                if let Some(event) = StorageCollector::observe_watch(&mut self.watch, &ss) {
                    self.event(stamp.wall_millis, stamp.mono_millis, "storage", &event);
                    println!("EVENT storage: {event}");
                }
                if let Some(t) = ss.disks.iter().find(|d| d.is_total) {
                    // dt since the previous storage sample; None on the first.
                    let dt = self.prev_disk_mono.map(|prev| (mono_secs - prev).max(0.0));
                    let max_gap = self.summary.max_gap;
                    self.disk_read.push(dt, t.read_bps, max_gap);
                    self.disk_write.push(dt, t.write_bps, max_gap);
                }
                self.prev_disk_mono = Some(mono_secs);
                let f3 = |v: Option<f64>| {
                    v.map(|x| format!("{x:.1}"))
                        .unwrap_or_else(|| "n/a".to_string())
                };
                for d in &ss.disks {
                    let lat = match (d.read_lat_ms, d.write_lat_ms) {
                        (Some(r), Some(w)) => format!("{r:.1}/{w:.1}"),
                        _ => "n/a".to_string(),
                    };
                    println!(
                        "DISK  {:<10}  {:>6}  {:>6}  {:>7}  {:>7}  {:>5}  {:>9}  {:>5}",
                        if d.is_total {
                            "TOTAL".to_string()
                        } else {
                            d.instance.chars().take(10).collect::<String>()
                        },
                        f3(d.reads),
                        f3(d.writes),
                        d.read_bps
                            .map(|x| format!("{:.2}", x / (1024.0 * 1024.0)))
                            .unwrap_or_else(|| "n/a".to_string()),
                        d.write_bps
                            .map(|x| format!("{:.2}", x / (1024.0 * 1024.0)))
                            .unwrap_or_else(|| "n/a".to_string()),
                        f3(d.avg_queue),
                        lat,
                        f3(d.idle_time),
                    );
                }
            }
            Err(e) => {
                eprintln!("error: storage read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[storage] {e}"),
                );
            }
        }
        true
    }

    fn handle_usb(
        &mut self,
        res: Result<pf_collectors::usb::UsbSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(us) => {
                let line = pf_collectors::usb::UsbCollector::format_json(&us, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                let mut summary = String::new();
                for g in &us.groups {
                    let on = g.devices.iter().filter(|d| d.started).count();
                    summary.push_str(&format!("{}:{glen}({on}) ", g.key, glen = g.devices.len()));
                }
                let pw = if us.power_error.is_some() {
                    "n/a".to_string()
                } else {
                    us.power_devices.len().to_string()
                };
                println!("USB  {summary} PWR-REQ:{pw}");
                for c in &us.changes {
                    self.event(stamp.wall_millis, stamp.mono_millis, "usb", c);
                    println!("EVENT usb: {c}");
                }
            }
            Err(e) => {
                eprintln!("error: usb read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[usb] {e}"),
                );
            }
        }
        true
    }

    fn handle_net(
        &mut self,
        res: Result<NetSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(ns) => {
                let line = NetCollector::format_json(&ns, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                for c in &ns.changes {
                    self.event(stamp.wall_millis, stamp.mono_millis, "net", c);
                    println!("EVENT net: {c}");
                }
                for (i, a) in ns.adapters.iter().enumerate() {
                    if a.class == "loopback" {
                        continue;
                    }
                    let tp = ns.throughput.iter().find(|t| {
                        matches!(network::match_adapter(&t.instance, &ns.adapters), Some((j, _)) if j == i)
                    });
                    let carries = tp
                        .map(|t| t.rx_bps.unwrap_or(0.0) + t.tx_bps.unwrap_or(0.0))
                        .unwrap_or(0.0);
                    if carries == 0.0 && network::is_virtual_noise(&a.alias, &a.descr) {
                        continue;
                    }
                    let (rx, tx) = match tp {
                        Some(t) => (
                            t.rx_bps
                                .map(network::human_bps)
                                .unwrap_or_else(|| "n/a".to_string()),
                            t.tx_bps
                                .map(network::human_bps)
                                .unwrap_or_else(|| "n/a".to_string()),
                        ),
                        None => ("n/a".to_string(), "n/a".to_string()),
                    };
                    let wifi = ns
                        .wifi
                        .iter()
                        .find(|w| w.guid == a.guid)
                        .map(|w| {
                            let sig = w
                                .signal_pct
                                .map(|s| format!("{s}%"))
                                .unwrap_or_else(|| "n/a".to_string());
                            if w.ssid.is_empty() {
                                sig
                            } else {
                                format!("{} ({sig})", w.ssid)
                            }
                        })
                        .unwrap_or_default();
                    println!(
                        "NET  {:<28}  {:<9}  {}/{}  {:>9}  {:>9}  {:>8}  {}",
                        a.alias.chars().take(28).collect::<String>(),
                        a.class,
                        a.oper_name,
                        a.media,
                        rx,
                        tx,
                        format!("{:.0}Mbps", a.rx_mbps.max(a.tx_mbps)),
                        wifi,
                    );
                }
                println!(
                    "NET  TOTAL  ↓{}  ↑{}",
                    human_bps_opt(ns.total_rx_bps_opt),
                    human_bps_opt(ns.total_tx_bps_opt),
                );
            }
            Err(e) => {
                eprintln!("error: net read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[net] {e}"),
                );
            }
        }
        true
    }

    fn handle_display(
        &mut self,
        res: Result<DisplaySample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(ds) => {
                let line = DisplayCollector::format_json(&ds, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                for d in &ds.displays {
                    let bri = d
                        .brightness
                        .map(|b| format!("{b}%"))
                        .unwrap_or_else(|| "n/a".to_string());
                    println!(
                        "DISP  {:<24}  {}x{}@{}Hz  {:>4}  {}",
                        d.name.chars().take(24).collect::<String>(),
                        d.width,
                        d.height,
                        d.freq_hz,
                        bri,
                        if d.primary { "primary" } else { "" },
                    );
                }
                let hdr_on = ds.hdr.iter().filter(|t| t.enabled).count();
                println!("HDR   {}/{} targets enabled", hdr_on, ds.hdr.len());
                for c in &ds.changes {
                    self.event(stamp.wall_millis, stamp.mono_millis, "display", c);
                    println!("EVENT display: {c}");
                }
            }
            Err(e) => {
                eprintln!("error: display read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[display] {e}"),
                );
            }
        }
        true
    }

    fn handle_os(&mut self, raw: OsPowerRaw, stamp: ClockStamp, ts: u64, te: u64) -> bool {
        let line = OsPowerCollector::format_json(&raw, stamp, ts, te);
        if !self.write_line(&line) {
            return false;
        }
        {
            let u = |r: &pf_collectors::system::AcDc<pf_core::telemetry::Reading<u32>>| {
                let one = |v: &pf_core::telemetry::Reading<u32>| match v.value() {
                    Some(x) => x.to_string(),
                    None => "n/a".to_string(),
                };
                format!("{}/{}", one(&r.ac), one(&r.dc))
            };
            let t = |r: &pf_collectors::system::AcDc<pf_core::telemetry::Reading<u32>>| {
                let one = |v: &pf_core::telemetry::Reading<u32>| match v.value() {
                    None => "n/a".to_string(),
                    Some(0) => "never".to_string(),
                    Some(x) => pf_collectors::system::format_timeout(*x),
                };
                format!("{}/{}", one(&r.ac), one(&r.dc))
            };
            println!(
                "OS  {}  {}  {}  {}  {}  {}  {}  {}  {}/{}  {:.1}ms",
                raw.scheme_name,
                u(&raw.cpu_min),
                u(&raw.cpu_max),
                u(&raw.epp),
                t(&raw.display_timeout_s),
                u(&raw.brightness_pct),
                t(&raw.sleep_timeout_s),
                t(&raw.hibernate_timeout_s),
                u(&raw.low_batt_pct),
                u(&raw.crit_batt_pct),
                raw.timer_resolution_ms.value().unwrap_or(&f64::NAN),
            );
        }
        true
    }

    fn handle_elevated(&mut self, res: Result<String, String>, stamp: ClockStamp) -> bool {
        match res {
            Ok(line) => {
                if !self.write_line(&line) {
                    return false;
                }
                println!("ELEV  snapshot stored");
            }
            Err(e) if e == ELEVATED_PENDING => {
                // Non-blocking poll while the helper is still running:
                // not an error, not an event, just wait for the next tick.
            }
            Err(e) => {
                self.event(stamp.wall_millis, stamp.mono_millis, "helper_error", &e);
                eprintln!("helper: {e}");
            }
        }
        true
    }

    fn handle_selfmon(
        &mut self,
        res: Result<SelfSample, String>,
        stamp: ClockStamp,
        ts: u64,
        te: u64,
    ) -> bool {
        match res {
            Ok(mut s) => {
                // Attribute the system ctx rate as an informational estimate.
                // Never fabricated: Unavailable until a cpu sample exists.
                s.ctx_switches = match self.last_cpu_ctx {
                    Some(v) => pf_core::telemetry::Reading::Estimated(v),
                    None => pf_core::telemetry::Reading::Unavailable("no cpu sample yet"),
                };
                self.last_self_ctx = self.last_cpu_ctx;
                let line = SelfCollector::format_json(&s, stamp, ts, te);
                if !self.write_line(&line) {
                    return false;
                }
                let f = |v: &pf_core::telemetry::Reading<f64>| {
                    v.value()
                        .map(|x| format!("{x:.2}"))
                        .unwrap_or_else(|| "n/a".to_string())
                };
                println!(
                    "SELF  cpu%:{}  ws:{}MB  ioW:{}B  thr:{}",
                    f(&s.cpu_pct),
                    f(&s.ws_mb),
                    match &s.io_write_b {
                        pf_core::telemetry::Reading::Measured(x) => format!("{x}"),
                        _ => "n/a".to_string(),
                    },
                    match &s.threads {
                        pf_core::telemetry::Reading::Measured(x) => format!("{x}"),
                        _ => "n/a".to_string(),
                    },
                );
                true
            }
            Err(e) => {
                eprintln!("error: self read failed: {e}");
                self.event(
                    stamp.wall_millis,
                    stamp.mono_millis,
                    "collector_error",
                    &format!("[self] {e}"),
                );
                true
            }
        }
    }

    fn write_footer(&mut self, queue_max: u64) -> bool {
        // Bounded streaming histograms replace the old exact session-wide
        // sorts: percentiles are within one 0.1 W/0.1 pct bin (see
        // `FixedHistogram`). Energy was accumulated pair-by-pair with the same
        // dual-clock discontinuity rules as the analysis path, so a
        // suspend/clock jump cannot bridge a segment and invent phantom Wh.
        let discharge_n = self.summary.discharge.count;
        let q = |h: &FixedHistogram, p: f64| {
            h.percentile(p)
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "null".to_string())
        };
        let discharge_median = q(&self.summary.discharge, 50.0);
        let discharge_p10 = q(&self.summary.discharge, 10.0);
        let discharge_p90 = q(&self.summary.discharge, 90.0);
        let cpu_median = self
            .summary
            .cpu_util
            .percentile(50.0)
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "null".to_string());
        let pkg_median = self
            .summary
            .pkg_w
            .percentile(50.0)
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "null".to_string());
        let dseg = self.summary.energy.discharge;
        let cseg = self.summary.energy.charge;
        let dwh = (dseg.covered_secs > 0.0).then_some(dseg.energy_wh);
        let cwh = (cseg.covered_secs > 0.0).then_some(cseg.energy_wh);
        let coverage_pct = dseg
            .coverage()
            .map(|c| format!("{:.1}", c * 100.0))
            .unwrap_or_else(|| "null".to_string());
        let charge_cov = cseg
            .coverage()
            .map(|c| format!("{:.1}", c * 100.0))
            .unwrap_or_else(|| "null".to_string());
        let disc_kinds = {
            let mut s = String::from("{");
            for (i, k) in stats::DiscontinuityKind::ALL.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    "\"{}\":{}",
                    k.as_str(),
                    self.summary.energy.disc_kinds[k.index()]
                ));
            }
            s.push('}');
            s
        };
        let mut collect_ms = String::from("{");
        let mut counts = String::from("{");
        let mut names: Vec<&&str> = self.summary.collect_ms.keys().collect();
        names.sort();
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                collect_ms.push(',');
                counts.push(',');
            }
            let ts = &self.summary.collect_ms[*name];
            collect_ms.push_str(&format!(
                "\"{name}\":{{\"mean_ms\":{:.3},\"max_ms\":{:.3}}}",
                ts.mean(),
                ts.max
            ));
            counts.push_str(&format!(
                "\"{name}\":{}",
                self.summary.counts.get(*name).copied().unwrap_or(0)
            ));
        }
        collect_ms.push('}');
        counts.push('}');
        // Scheduler lateness: mean/max gap beyond expected cadence.
        let mut late_json = String::from("{");
        let mut late_names: Vec<&&str> = self.summary.late.keys().collect();
        late_names.sort();
        for (i, name) in late_names.iter().enumerate() {
            if i > 0 {
                late_json.push(',');
            }
            let ts = &self.summary.late[*name];
            late_json.push_str(&format!(
                "\"{name}\":{{\"mean_ms\":{:.0},\"max_ms\":{:.0},\"events\":{}}}",
                ts.mean(),
                ts.max,
                ts.count
            ));
        }
        late_json.push('}');
        let mut active_json = String::from("[");
        let mut active_sorted = self.active.clone();
        active_sorted.sort();
        for (i, name) in active_sorted.iter().enumerate() {
            if i > 0 {
                active_json.push(',');
            }
            active_json.push_str(&format!("\"{name}\""));
        }
        active_json.push(']');
        // Truthful collector timeout evidence (blocked/stalled workers), not
        // to be confused with provider failures, which arrive as errors.
        let timeout_episodes = self.timeouts.episodes();
        let mut timed_out_sorted: Vec<&&str> = self.timeouts.names().collect();
        timed_out_sorted.sort();
        let timed_out_json = format!(
            "[{}]",
            timed_out_sorted
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let session_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let self_ctx_s = self
            .last_self_ctx
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "null".to_string());
        let self_cpu = self
            .summary
            .collect_ms
            .get("self")
            .map(|ts| format!("{:.3}", ts.mean()))
            .unwrap_or_else(|| "null".to_string());
        // Disk totals keep the measured-vs-unobserved distinction and are
        // qualified (coverage / unknown / sample count) when coverage is
        // incomplete. A missing rate never contributes phantom zero bytes.
        let mb = |v: Option<f64>| {
            v.map(|x| format!("{x:.1}"))
                .unwrap_or_else(|| "null".to_string())
        };
        let pct = |v: Option<f64>| {
            v.map(|x| format!("{:.1}", x * 100.0))
                .unwrap_or_else(|| "null".to_string())
        };
        let disk_read_mb = mb(self.disk_read.total_mb());
        let disk_write_mb = mb(self.disk_write.total_mb());
        let disk_read_cov = pct(self.disk_read.coverage());
        let disk_write_cov = pct(self.disk_write.coverage());
        let summary_json = format!(
            "{{\"samples\":{},\"discharge_n\":{discharge_n},\"charge_n\":{},\
             \"discharge_median_w\":{discharge_median},\"discharge_p10_w\":{discharge_p10},\"discharge_p90_w\":{discharge_p90},\
             \"discharge_wh\":{},\"charge_wh\":{},\
             \"discharge_coverage_pct\":{coverage_pct},\"charge_coverage_pct\":{charge_cov},\
             \"discharge_unknown_s\":{:.0},\"discharge_unobserved_s\":{:.0},\
             \"discharge_discontinuities\":{},\"discharge_discontinuity_kinds\":{disc_kinds},\
             \"cpu_utility_median_pct\":{cpu_median},\
             \"cpu_pkg_derived_median_w\":{pkg_median},\
             \"disk_read_mb\":{disk_read_mb},\"disk_write_mb\":{disk_write_mb},\
             \"disk_read_coverage_pct\":{disk_read_cov},\"disk_write_coverage_pct\":{disk_write_cov},\
             \"disk_read_unknown_s\":{:.0},\"disk_write_unknown_s\":{:.0},\
             \"disk_read_samples\":{},\"disk_write_samples\":{},\
             \"collection_ms\":{collect_ms},\"collector_samples\":{counts},\
             \"lateness_ms\":{late_json},\"queue_max\":{queue_max},\
             \"collector_timeouts\":{timeout_episodes},\"timed_out_collectors\":{timed_out_json},\
             \"self_ctx_s\":{self_ctx_s},\"self_collect_mean_ms\":{self_cpu},\
             \"session_bytes\":{session_bytes},\"active_collectors\":{active_json},\
             \"unattributed_note\":\"no GPU power sensor exists (vendor APIs pending); cpu package power is estimated-only, so discharge minus estimates remains unattributed; overhead phase A is a low-load (battery+self) proxy for profiler-off, true profiler-off requires external measurement\"}}",
            self.n_battery,
            self.summary.charge_n,
            dwh.map(|v| format!("{v:.4}")).unwrap_or("null".to_string()),
            cwh.map(|v| format!("{v:.4}")).unwrap_or("null".to_string()),
            dseg.unknown_secs,
            dseg.unobserved_secs,
            dseg.discontinuities,
            self.disk_read.unknown_secs,
            self.disk_write.unknown_secs,
            self.disk_read.samples,
            self.disk_write.samples,
        );
        let end = now_wall_ms();
        if self.session.footer(end, &summary_json).is_err() {
            eprintln!("error: cannot write session footer");
            return false;
        }
        println!("wrote {} lines to {}", self.session.lines, self.path);
        println!("summary: {summary_json}");
        let mut parts: Vec<String> = Vec::new();
        for name in names {
            let mean = self.summary.collect_ms[name].mean();
            parts.push(format!("{name} {mean:.2}ms"));
        }
        println!(
            "collection cost per sample (wall time in collectors): {}",
            parts.join(", ")
        );
        println!(
            "note: 'self' collection cost above is profiler-only (GetProcessTimes(self)); system load is reported separately per collector; overhead phase A is a low-load (battery+self) proxy for profiler-off, true profiler-off requires external measurement"
        );
        true
    }
}

/// Sentinel for "helper still running; poll again next tick". Handled
/// silently by `handle_elevated` (no event, no error line).
pub const ELEVATED_PENDING: &str = "helper pending (non-blocking poll)";

/// Non-blocking elevated-helper poller. The old code ran a synchronous
/// 20 s `try_wait` loop that blocked its scheduler worker; this instead
/// spawns once, then does at most ~100 ms of `try_wait` polling per
/// `poll()` call so the worker thread never blocks >250 ms. While a child
/// is in flight the scheduler knob is tightened to 250 ms so completion
/// is noticed promptly; it is restored to the steady cadence on settle.
/// Late children are killed at the deadline.
pub struct ElevatedPoller {
    cmd: String,
    args: Vec<String>,
    timeout: Duration,
    steady_ms: u64,
    knob: Option<Arc<AtomicU64>>,
    child: Option<(std::process::Child, Instant)>,
}

impl ElevatedPoller {
    pub fn new(helper: &str, every_ms: u64, knob: Option<Arc<AtomicU64>>) -> Self {
        ElevatedPoller {
            cmd: helper.to_string(),
            args: vec!["snapshot".to_string()],
            timeout: Duration::from_secs(20),
            steady_ms: every_ms,
            knob,
            child: None,
        }
    }

    #[cfg(test)]
    fn test(cmd: &str, args: &[&str], timeout_ms: u64) -> Self {
        ElevatedPoller {
            cmd: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            timeout: Duration::from_millis(timeout_ms),
            steady_ms: 60_000,
            knob: None,
            child: None,
        }
    }

    fn settle_output(
        &mut self,
        child: std::process::Child,
        status: std::process::ExitStatus,
    ) -> Result<String, String> {
        self.restore();
        if !status.success() {
            return Err(format!(
                "helper exited with status {status} (admin? try elevated shell)"
            ));
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("helper read failed: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or("").trim().to_string();
        if line.contains("\"collector\":\"elevated\"") {
            return Ok(line);
        }
        if line.contains("\"error\"") {
            return Err(format!("helper reports: {line}"));
        }
        Err("helper returned unrecognized output".to_string())
    }

    fn restore(&mut self) {
        if let Some(k) = &self.knob {
            k.store(self.steady_ms.max(50), Ordering::Relaxed);
        }
    }

    /// Poll once. Budgets: spawn + immediate `try_wait`, else bounded
    /// polling with 10 ms slices up to 100 ms total. Never sleeps >100 ms.
    pub fn poll(&mut self) -> Result<String, String> {
        if self.child.is_none() {
            let mut child = std::process::Command::new(&self.cmd)
                .args(&self.args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("helper spawn failed ({}): {e}", self.cmd))?;
            // Fast path: already exited (or exits within the 100 ms budget).
            let deadline_poll = Instant::now() + Duration::from_millis(100);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return self.settle_output(child, status);
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline_poll {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        let _ = child.kill();
                        self.restore();
                        return Err(format!("helper wait failed: {e}"));
                    }
                }
            }
            // Still running: keep the child, tighten the knob so the next
            // poll comes soon, and yield without further blocking.
            let deadline = Instant::now() + self.timeout;
            if let Some(k) = &self.knob {
                k.store(250, Ordering::Relaxed);
            }
            self.child = Some((child, deadline));
            return Err(ELEVATED_PENDING.to_string());
        }
        let done: Option<std::process::ExitStatus> = match &mut self.child {
            Some((child, deadline)) => match child.try_wait() {
                Ok(Some(status)) => Some(status),
                Ok(None) => {
                    if Instant::now() >= *deadline {
                        let (mut c, _) = self.child.take().unwrap();
                        let _ = c.kill();
                        self.restore();
                        return Err("helper timed out after 20s".to_string());
                    }
                    None
                }
                Err(e) => {
                    let (mut c, _) = self.child.take().unwrap();
                    let _ = c.kill();
                    self.restore();
                    return Err(format!("helper wait failed: {e}"));
                }
            },
            None => None,
        };
        match done {
            Some(status) => {
                let (taken, _) = self.child.take().unwrap();
                self.settle_output(taken, status)
            }
            None if self.child.is_some() => Err(ELEVATED_PENDING.to_string()),
            None => Err(ELEVATED_PENDING.to_string()),
        }
    }
}

/// Raw Windows console control handler (kernel32, no crate). Ctrl-C,
/// Ctrl-Break and console-close flip the running monitor's stop flag so the
/// writer loop can finalize exactly once and exit 0. The handler runs on a
/// system thread, so it must only touch the `AtomicBool` whose pointer is
/// published here for the lifetime of the monitor call.
#[cfg(windows)]
mod console {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    #[allow(non_camel_case_types)]
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(non_camel_case_types)]
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;

    static TARGET: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(DWORD) -> BOOL>,
            add: BOOL,
        ) -> BOOL;
    }

    unsafe extern "system" fn handler(ctrl_type: DWORD) -> BOOL {
        // CTRL_C_EVENT=0, CTRL_BREAK_EVENT=1, CTRL_CLOSE_EVENT=2. All map to
        // a graceful stop; close also gets a limited (~5 s) grace window, and
        // this finalize path is far faster than that.
        let p = TARGET.load(Ordering::SeqCst);
        if !p.is_null() {
            // SAFETY: TARGET is published by `install` and cleared by
            // `uninstall` before the owning monitor drops its Arc.
            unsafe { (*p).store(true, Ordering::SeqCst) };
        }
        if ctrl_type <= 2 { 1 } else { 0 } // 1 = handled, do not terminate
    }

    pub fn install(target: &Arc<AtomicBool>) {
        TARGET.store(Arc::as_ptr(target) as *mut AtomicBool, Ordering::SeqCst);
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }

    pub fn uninstall() {
        TARGET.store(std::ptr::null_mut(), Ordering::SeqCst);
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 0);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn handler_sets_target_and_reports_handled() {
            // Exercises the extern entry point (SetConsoleCtrlHandler link)
            // and the pointer-publish path even though no console event can
            // be injected in-process.
            let flag = Arc::new(AtomicBool::new(false));
            install(&flag);
            assert_eq!(unsafe { handler(0) }, 1, "CTRL_C must be handled");
            assert!(flag.load(Ordering::SeqCst), "handler did not set stop flag");
            assert_eq!(
                unsafe { handler(3) },
                0,
                "unknown event must not be claimed"
            );
            uninstall();
            assert!(TARGET.load(Ordering::SeqCst).is_null());
        }
    }
}

/// Legacy monitor entry point: no live-snapshot slot, never paused. Kept
/// byte-identical to the pre-Phase-C behavior.
pub fn cmd_monitor(opts: MonitorOpts) -> i32 {
    cmd_monitor_inner(opts, None, None, None, None, None, None)
}

/// Monitor with optional Phase C wiring. `live_slot` receives the latest
/// battery snapshot after each received sample; `pause_slot` gates the
/// writer. **Pause semantics:** while paused the received sample is dropped
/// without advancing recording state (no session line, no counters, no live
/// publish) and no data is fabricated; collectors keep running. Resume
/// continues from the next sample (a truthful gap, integrated separately).
pub fn cmd_monitor_with(
    opts: MonitorOpts,
    live_slot: Option<Arc<Mutex<StateSnapshot>>>,
    pause_slot: Option<Arc<AtomicBool>>,
) -> i32 {
    cmd_monitor_inner(opts, live_slot, pause_slot, None, None, None, None)
}

/// Agent entry point: `stop_slot` is the authoritative stop cell owned by a
/// `Service` (`Service::stop_flag`). When it is set (a pipe `stop` verb)
/// the writer loop finalizes: it flushes, writes exactly one footer, and
/// returns 0. The process-wide Ctrl-C handler stays installed because the
/// caller does not own the trigger (unlike `cmd_monitor_with_signal`).
pub fn cmd_monitor_with_stop(
    opts: MonitorOpts,
    live_slot: Option<Arc<Mutex<StateSnapshot>>>,
    pause_slot: Option<Arc<AtomicBool>>,
    stop_slot: Arc<AtomicBool>,
) -> i32 {
    cmd_monitor_inner(
        opts,
        live_slot,
        pause_slot,
        Some(stop_slot),
        None,
        None,
        None,
    )
}

/// Full agent entry point: like [`cmd_monitor_with_stop`] plus the shared
/// marker queue (whose entries are persisted as session `event` records) and
/// the shared session-path cell the writer loop publishes its output path into
/// (`Service::session_path_handle`). The path cell is the single authority a
/// consumer uses to identify the active session; it is set only once the
/// output file has actually been created.
pub fn cmd_monitor_with_control(
    opts: MonitorOpts,
    live_slot: Option<Arc<Mutex<StateSnapshot>>>,
    pause_slot: Option<Arc<AtomicBool>>,
    stop_slot: Arc<AtomicBool>,
    marker_slot: crate::service::MarkerQueue,
    session_path_slot: Option<Arc<Mutex<Option<String>>>>,
) -> i32 {
    cmd_monitor_inner(
        opts,
        live_slot,
        pause_slot,
        Some(stop_slot),
        None,
        Some(marker_slot),
        session_path_slot,
    )
}

/// Embedded/test hook: `signal` is honored exactly like the console
/// handler's Ctrl-C (a graceful stop) without touching the process-wide
/// handler. Used by the graceful-termination regression test.
pub fn cmd_monitor_with_signal(
    opts: MonitorOpts,
    live_slot: Option<Arc<Mutex<StateSnapshot>>>,
    pause_slot: Option<Arc<AtomicBool>>,
    signal: Arc<AtomicBool>,
) -> i32 {
    cmd_monitor_inner(opts, live_slot, pause_slot, None, Some(signal), None, None)
}

/// RAII guard that clears the shared active-session path cell when the writer
/// loop exits for any reason, so `status` never advertises a session the
/// process no longer owns.
struct SessionPathGuard(Option<Arc<Mutex<Option<String>>>>);

impl Drop for SessionPathGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.0.as_ref() {
            *slot.lock().unwrap() = None;
        }
    }
}

fn cmd_monitor_inner(
    opts: MonitorOpts,
    live_slot: Option<Arc<Mutex<StateSnapshot>>>,
    pause_slot: Option<Arc<AtomicBool>>,
    stop_slot: Option<Arc<AtomicBool>>,
    ext_signal: Option<Arc<AtomicBool>>,
    marker_slot: Option<crate::service::MarkerQueue>,
    session_path_slot: Option<Arc<Mutex<Option<String>>>>,
) -> i32 {
    let clock = Clock::new();
    let first = clock.stamp();
    // Clear the published active-session identity on every exit path
    // (including early errors) so a consumer never sees a stale path after the
    // writer stops owning it.
    let _session_path_guard = SessionPathGuard(session_path_slot.clone());
    // Sessions live next to the program (sessions/), never in TEMP or
    // other hidden locations. Falls back to CWD only if sessions/ cannot
    // be created.
    let dir_ok = std::fs::create_dir_all("sessions").is_ok();
    if !dir_ok {
        eprintln!("warning: cannot create sessions dir; using current directory");
    }
    let path = opts.out.clone().unwrap_or_else(|| {
        if dir_ok {
            format!("sessions/pf-session-{}.jsonl", first.wall_millis)
        } else {
            format!("pf-session-{}.jsonl", first.wall_millis)
        }
    });
    let mut session = match SessionWriter::create(path.clone().into(), 10) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot create session file {path}: {e}");
            return 2;
        }
    };
    // Publish the exact output path only after the file exists: consumers get
    // authoritative active-session identity instead of guessing by mtime. The
    // path is made absolute so identity survives a differing working directory.
    if let Some(slot) = session_path_slot.as_ref() {
        let published = std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.clone());
        *slot.lock().unwrap() = Some(published);
    }
    // Collector set + effective base cadence, resolved ONCE here, before the
    // header records the cadence or the scheduler consumes it. `--collectors`
    // wins the set; `--preset` otherwise selects the set and the cadence.
    // Header, scheduler, live energy gap threshold, and (via the header)
    // recovery all use this exact `base_iv`.
    const KNOWN: &[&str] = &[
        "battery", "cpu", "gpu", "storage", "net", "proc", "display", "usb", "os", "self",
        "elevated",
    ];
    let base_iv = effective_base_interval(&opts);
    let mut active: Vec<&str> =
        if opts.preset.as_deref() == Some("low") && opts.collectors.is_none() {
            vec!["battery", "cpu", "os", "self"]
        } else {
            KNOWN[..10].to_vec()
        };
    if session
        .header(first.wall_millis, VERSION, base_iv, &opts.note)
        .is_err()
    {
        eprintln!("error: cannot write session header");
        return 2;
    }
    let _ = session.event(
        first.wall_millis,
        first.mono_millis,
        "capabilities",
        &capabilities_json(),
    );

    let base = clock.mono_base();
    let stop = Arc::new(AtomicBool::new(false));
    // Ctrl-C/Ctrl-Break/console-close -> `stop`, which the writer loop below
    // observes to flush and write the single footer before exiting 0. When an
    // embedded signal is supplied the caller owns the trigger, so skip the
    // process-wide handler.
    #[cfg(windows)]
    if ext_signal.is_none() {
        console::install(&stop);
    }
    if let Some(list) = opts.collectors.as_deref() {
        active.clear();
        for name in list.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if !KNOWN.contains(&name) {
                eprintln!(
                    "error: unknown collector '{name}' (known: {})",
                    KNOWN.join(",")
                );
                return 2;
            }
            // 'static lifetime: names come from KNOWN or the arg string —
            // re-resolve to the static entry so state maps stay simple.
            active.push(KNOWN.iter().find(|k| **k == name).unwrap());
        }
        if active.is_empty() {
            eprintln!("error: --collectors selected nothing");
            return 2;
        }
    }
    // Elevated helper joins the set when configured (unless an explicit
    // --collectors list excludes it).
    if opts.helper.is_some()
        && (opts.collectors.is_none() || active.contains(&"elevated"))
        && !active.contains(&"elevated")
    {
        active.push("elevated");
    }
    let has = |n: &str| active.contains(&n);
    let battery_interval = Arc::new(AtomicU64::new(base_iv));
    // OS policy cadence: 5 s for the first 2 ticks (fast initial snapshot),
    // then 30 s (spec 30–60 s; power-policy values change on human
    // timescales). Driven by the same adaptive knob the scheduler honors.
    let os_interval = Arc::new(AtomicU64::new(5000));
    // Expected cadence per collector, for scheduler-lateness accounting.
    // Sourced from sched::interval_ms_for (FieldMeta-driven) instead of
    // hardcoded numbers.
    let mut sched_iv: HashMap<&'static str, u64> = HashMap::new();
    sched_iv.insert("battery", sched::interval_ms_for("battery", base_iv));
    sched_iv.insert("cpu", sched::interval_ms_for("cpu", base_iv));
    sched_iv.insert("gpu", sched::interval_ms_for("gpu", base_iv));
    sched_iv.insert("storage", sched::interval_ms_for("storage", base_iv));
    sched_iv.insert("net", sched::interval_ms_for("net", base_iv));
    sched_iv.insert("proc", sched::interval_ms_for("proc", base_iv));
    sched_iv.insert("display", sched::interval_ms_for("display", base_iv));
    sched_iv.insert("usb", sched::interval_ms_for("usb", base_iv));
    sched_iv.insert("os", sched::interval_ms_for("os", base_iv));
    sched_iv.insert("self", sched::interval_ms_for("self", base_iv));

    let battery = BatteryCollector::new();
    let mut cpu = CpuCollector::new();
    let mut gpu = GpuCollector::new();
    let mut proc = ProcCollector::new();
    let mut display = DisplayCollector::new();
    let mut net = NetCollector::new();
    let mut storage = StorageCollector::new();
    let mut usb = UsbCollector::new();
    let mut os = OsPowerCollector::new();
    let mut selfmon = SelfCollector::new();

    // Battery static metadata is cached ~60 ticks: enumeration + IOCTLs
    // on every tick was pure observer effect for data that never changes.
    // NOTE (sample-path gap, not fixable from here): battery.rs
    // `sample_json` still calls `batinfo::query_all()` on every call, and
    // main.rs `cmd_sample` routes through it — battery.rs/main.rs are
    // outside this module's ownership, so the one-shot `sample` path keeps
    // re-enumerating. Only the monitor worker path (below) is cached.
    let mut cached_stat: Vec<(usize, BatteryStatic)> = Vec::new();
    let mut battery_ticks = 0u64;
    let battery_iv = battery_interval.clone();
    let os_iv = os_interval.clone();
    let mut os_ticks = 0u64;
    let mut specs: Vec<sched::Spec<Payload>> = Vec::new();
    // Helper macro would hide moves; explicit conditional pushes keep
    // ownership visible per collector.
    if has("battery") {
        specs.push(sched::Spec {
            name: "battery",
            interval_ms: sched::interval_ms_for("battery", base_iv),
            adaptive: Some(battery_iv),
            collect: Box::new(move || {
                battery_ticks += 1;
                if cached_stat.is_empty() || battery_ticks % 60 == 1 {
                    cached_stat = pf_collectors::batinfo::query_all();
                }
                Payload::Battery(match battery.read() {
                    Ok(raw) => Ok((raw, cached_stat.clone())),
                    Err(e) => Err(e),
                })
            }),
        });
    }
    if has("cpu") {
        specs.push(sched::Spec {
            name: "cpu",
            interval_ms: sched::interval_ms_for("cpu", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Cpu(cpu.read())),
        });
    }
    if has("gpu") {
        specs.push(sched::Spec {
            name: "gpu",
            interval_ms: sched::interval_ms_for("gpu", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Gpu(gpu.read())),
        });
    }
    if has("storage") {
        specs.push(sched::Spec {
            name: "storage",
            interval_ms: sched::interval_ms_for("storage", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Storage(storage.read())),
        });
    }
    if has("net") {
        specs.push(sched::Spec {
            name: "net",
            interval_ms: sched::interval_ms_for("net", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Net(net.read())),
        });
    }
    if has("proc") {
        specs.push(sched::Spec {
            name: "proc",
            interval_ms: sched::interval_ms_for("proc", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Proc(proc.read_with_stamp(worker_stamp(base)))),
        });
    }
    if has("display") {
        // Display polls at 1–5 s as a backstop; brightness/mode/HDR
        // transitions are ALSO event-driven (DisplayCollector::diff_state
        // emits a change string — including any brightness delta — which
        // handle_display forwards as a timeline event), so a slow poll
        // never loses the transition, only delays its timestamp.
        specs.push(sched::Spec {
            name: "display",
            interval_ms: sched::interval_ms_for("display", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Display(display.read())),
        });
    }
    if has("usb") {
        specs.push(sched::Spec {
            name: "usb",
            interval_ms: sched::interval_ms_for("usb", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::Usb(usb.read())),
        });
    }
    if has("os") {
        specs.push(sched::Spec {
            name: "os",
            interval_ms: 5000, // first 2 ticks fast; then os_iv relaxes to 30 s
            adaptive: Some(os_iv),
            collect: Box::new(move || {
                os_ticks += 1;
                if os_ticks == 3 {
                    os_interval.store(sched::interval_ms_for("os", base_iv), Ordering::Relaxed);
                }
                Payload::Os(os.read(worker_stamp(base)))
            }),
        });
    }
    if has("self") {
        specs.push(sched::Spec {
            name: "self",
            interval_ms: sched::interval_ms_for("self", base_iv),
            adaptive: None,
            collect: Box::new(move || Payload::SelfMon(selfmon.read())),
        });
    }
    if has("elevated")
        && let Some(helper) = opts.helper.clone()
    {
        let every = opts.helper_every.max(10) * 1000;
        sched_iv.insert("elevated", every);
        let elev_knob = Arc::new(AtomicU64::new(every));
        let elev_iv = elev_knob.clone();
        let mut poller = ElevatedPoller::new(&helper, every, Some(elev_knob));
        specs.push(sched::Spec {
            name: "elevated",
            interval_ms: every,
            adaptive: Some(elev_iv),
            collect: Box::new(move || Payload::Elevated(poller.poll())),
        });
    }
    if specs.is_empty() {
        eprintln!("error: no collectors selected");
        return 2;
    }

    // `--samples N` counts the primary collector's arrivals. Battery is the
    // natural primary when active; otherwise the first selected collector,
    // so the limit bounds the session for every collector subset instead of
    // silently doing nothing.
    let tick_collector: &'static str = if has("battery") { "battery" } else { active[0] };

    let (workers, rx, sched_stats) = sched::spawn(specs, stop.clone(), base, 128);

    let mut st = MonitorState {
        session,
        path: path.clone(),
        summary: SummaryAccum::new(base_iv),
        baselines: MultiBaseline::new(),
        dch_count: 0,
        disk_read: ThroughputAccum::default(),
        disk_write: ThroughputAccum::default(),
        prev_disk_mono: None,
        sched_iv,
        expected: HashMap::new(),
        active: active.clone(),
        boosted: 0,
        battery_interval: battery_interval.clone(),
        base_interval: base_iv,
        last_cpu_ctx: None,
        last_self_ctx: None,
        watch: ActivityWatch::new(5.0, 10),
        n_battery: 0,
        live: StateSnapshot {
            label: opts.note.clone(),
            ..Default::default()
        },
        live_slot,
        pause_slot,
        marker_slot,
        events: 0,
        tick_collector,
        tick: 0,
        start_mono_ms: first.mono_millis,
        primary_fallback_done: false,
        timeouts: sched::TimeoutWatch::default(),
    };

    println!("session: {path}");
    println!("BATTERY_W  CHARGE%  REMAIN_WH  RUNTIME(1m/5m)  NOTE");
    println!("CPU_UTIL%  PERF%  FREQ_MHz  IDLEBRK/s  C1/C2/C3%  MEAN_CORE%  PARK  CTX/s  PKG_W");
    println!("GPU  VENDOR(*)  U%  3D  DEC  CPY  MEM_MB  PIDS  TOP_PID  STATE");
    println!("PROC  PID  NAME  CPU%  WS_MB  IO_R/s  IO_W/s  THR");
    println!("OS  SCHEME  CPUmin  CPUmax  EPP  DISP  BRI  SLP  HIB  LB/CR  TIMER");
    println!("DISP  NAME  MODE  BRI  HDR");
    println!("NET  ALIAS  CLASS  OPER  RX/s  TX/s  LINK  WIFI");
    println!("DISK  NAME  R/s  W/s  R_MB/s  W_MB/s  Q  LATms  IDLE%");
    println!("USB  class:count(on)  PWR-REQ  EVENTS");

    // Graceful-stop sentinel: <out-dir>/stop.requested.
    let stop_file = std::path::Path::new(&path)
        .parent()
        .map(|p| p.join("stop.requested"))
        .unwrap_or_else(|| std::path::PathBuf::from("stop.requested"));

    let mut ok = true;
    'recv: loop {
        // Persist any queued markers first, so a marker that arrives between
        // samples (or while paused) lands before the next record and cannot be
        // lost to an immediately following stop.
        let now = clock.stamp();
        st.drain_markers(now);
        // Truthful timeout evidence: a collector whose worker is blocked or
        // stalled emits nothing, so detect overdue arrivals here (a failed
        // read still delivers an `Err` payload on cadence and is not flagged).
        st.poll_collector_timeouts(now.mono_millis, now.wall_millis);
        // Console handler (or injected test signal): finalize through the one
        // footer path below. Checked each iteration so a graceful stop is
        // noticed within one 100 ms receive timeout.
        if stop.load(Ordering::Relaxed)
            || ext_signal
                .as_ref()
                .map(|s| s.load(Ordering::Relaxed))
                .unwrap_or(false)
        {
            println!("interrupt received; finalizing session");
            break 'recv;
        }
        // Authoritative agent stop (pipe `stop` via `Service::stop_flag`).
        if stop_slot
            .as_ref()
            .map(|s| s.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            println!("stop requested; finalizing session");
            break 'recv;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => {
                st.note_arrival(msg.collector, msg.wall_ms, msg.mono_ms);
                if is_paused_flag(&st.pause_slot) {
                    // Paused: drop the sample without advancing recording
                    // state (no session line, counters, or live publish).
                    // Keep the expected arrival current so resume is not
                    // scored as scheduler lateness.
                    st.expected.insert(msg.collector, msg.mono_ms);
                    sched_stats.after_recv();
                } else {
                    st.record_time(msg.collector, msg.collect_ms);
                    sched_stats.after_recv();
                    // Scheduler lateness: gap beyond the expected cadence.
                    // Negative (early, e.g. boost) clamps to zero.
                    if let Some(iv) = st.sched_iv.get(msg.collector).copied() {
                        if let Some(last) = st.expected.get(msg.collector).copied() {
                            let gap = msg.mono_ms.saturating_sub(last);
                            if gap > iv + 150 {
                                st.summary.record_late(msg.collector, (gap - iv) as f64);
                            }
                        }
                        st.expected.insert(msg.collector, msg.mono_ms);
                    }
                    // `--samples N` bounds N samples of the primary collector.
                    // If that collector never arrives (a stalled worker, not a
                    // failed read: failures still deliver a payload), the bound
                    // would never advance. After a grace period, let the first
                    // collector that IS producing become the bound, and record
                    // the reassignment so the reason is in the evidence.
                    if opts.samples.is_some()
                        && st.tick == 0
                        && !st.primary_fallback_done
                        && msg.collector != st.tick_collector
                    {
                        let grace = (base_iv.saturating_mul(30)).max(30_000);
                        if msg.mono_ms.saturating_sub(st.start_mono_ms) > grace {
                            let old = st.tick_collector;
                            st.tick_collector = msg.collector;
                            st.primary_fallback_done = true;
                            st.event(
                                msg.wall_ms,
                                msg.mono_ms,
                                "primary_fallback",
                                &format!(
                                    "primary collector '{old}' produced no sample within {}s; --samples bound by '{}'",
                                    grace / 1000,
                                    msg.collector
                                ),
                            );
                        }
                    }
                    let is_tick = msg.collector == st.tick_collector;
                    let stamp = ClockStamp {
                        wall_millis: msg.wall_ms,
                        mono_millis: msg.mono_ms,
                    };
                    let (ts, te) = (msg.t_start_ms, msg.t_end_ms);
                    let cont = match msg.payload {
                        Payload::Battery(res) => {
                            st.n_battery += 1;
                            st.handle_battery(res, stamp, ts, te)
                        }
                        Payload::Cpu(res) => st.handle_cpu(res, stamp, ts, te),
                        Payload::Gpu(res) => st.handle_gpu(res, stamp, ts, te),
                        Payload::Proc(res) => st.handle_proc(res, stamp, ts, te),
                        Payload::Display(res) => st.handle_display(res, stamp, ts, te),
                        Payload::Net(res) => st.handle_net(res, stamp, ts, te),
                        Payload::Storage(res) => st.handle_storage(res, stamp, ts, te),
                        Payload::Usb(res) => st.handle_usb(res, stamp, ts, te),
                        Payload::Os(raw) => st.handle_os(raw, stamp, ts, te),
                        Payload::Elevated(res) => st.handle_elevated(res, stamp),
                        Payload::SelfMon(res) => st.handle_selfmon(res, stamp, ts, te),
                    };
                    if !cont {
                        ok = false;
                        break 'recv;
                    }
                    // Publish the latest live state over IPC. No-op (and no
                    // clone) when no slot is wired, so the legacy path is
                    // unchanged.
                    if st.live_slot.is_some() {
                        st.live.events = st.events as usize;
                        set_live_snapshot(&st.live_slot, st.live.clone());
                    }
                    if is_tick {
                        st.tick += 1;
                        if let Some(limit) = opts.samples
                            && st.tick >= limit
                        {
                            break 'recv;
                        }
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'recv,
        }
        if stop_file.exists() {
            let _ = std::fs::remove_file(&stop_file);
            println!("stop requested; finalizing session");
            break 'recv;
        }
    }
    stop.store(true, Ordering::Relaxed);
    #[cfg(windows)]
    console::uninstall();
    // Disconnect before joining: a worker mid-`try_send` wakes with
    // Disconnected instead of waiting out the backoff, and the channel's
    // queued messages are released. This is the writer-side half of the
    // deadlock fix in `sched` (worker-side half: stop-aware `try_send`).
    drop(rx);
    // Bounded shutdown: a collector call blocked forever in a Win32/PDH/vendor
    // API cannot be killed safely, but it must not block finalization. Wait a
    // fixed grace for workers to observe `stop`; abandon only the still-blocked
    // ones (one long-lived thread per collector, never one per timeout).
    let stuck = workers.finish_bounded(Duration::from_secs(2));
    if stuck > 0 {
        eprintln!(
            "warning: {stuck} collector worker(s) still blocked; abandoning them (session finalized)"
        );
    }
    if !ok {
        return 2;
    }
    let queue_max = sched_stats.max_in_flight.load(Ordering::Relaxed);
    st.drain_markers(clock.stamp());
    if !st.write_footer(queue_max) {
        return 2;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_fast_helper_returns_quickly() {
        // Fake helper as a batch file: avoids cmd.exe quote-mangling of
        // nested double quotes (a batch `echo` preserves them verbatim).
        let bat = std::env::temp_dir().join(format!("pf-fake-helper-{}.bat", std::process::id()));
        std::fs::write(&bat, "@echo {\"collector\":\"elevated\"}\r\n").unwrap();
        let mut p = ElevatedPoller::test("cmd", &["/c", bat.to_str().unwrap()], 5000);
        let t0 = Instant::now();
        let mut r = p.poll();
        while matches!(&r, Err(e) if e == ELEVATED_PENDING) && t0.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(10));
            r = p.poll();
        }
        let dt = t0.elapsed();
        std::fs::remove_file(&bat).ok();
        assert!(
            dt < Duration::from_secs(2),
            "helper did not settle within {dt:?}"
        );
        assert!(r.is_ok(), "fast helper should settle quickly: {r:?}");
        assert!(r.unwrap().contains("\"collector\":\"elevated\""));
    }

    #[test]
    fn elevated_slow_helper_never_blocks_worker() {
        // Fake helper that sleeps ~5 s; each poll must return in ~100 ms
        // (worker budget 250 ms), first as pending, then as timeout with a
        // short test deadline.
        let mut p = ElevatedPoller::test("cmd", &["/c", "ping -n 6 127.0.0.1 >nul"], 400);
        let t0 = Instant::now();
        let r1 = p.poll();
        let d1 = t0.elapsed();
        assert!(
            d1 < Duration::from_millis(1000),
            "first poll blocked {d1:?}"
        );
        assert_eq!(r1.unwrap_err(), ELEVATED_PENDING);
        // Keep polling (fast, non-blocking) until the deadline fires.
        let mut last: String;
        let t1 = Instant::now();
        loop {
            let t = Instant::now();
            match p.poll() {
                Ok(_) => panic!("slow helper should not succeed"),
                Err(e) => {
                    last = e.clone();
                    if e != ELEVATED_PENDING {
                        break;
                    }
                }
            }
            assert!(t.elapsed() < Duration::from_millis(1000), "poll blocked");
            if t1.elapsed() > Duration::from_secs(10) {
                panic!("poller never settled: last={last}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(last.contains("timed out"), "expected timeout, got: {last}");
    }

    #[test]
    fn elevated_spawn_failure_is_an_error_not_pending() {
        let mut p = ElevatedPoller::test("no-such-helper-binary-xyz", &[], 5000);
        let r = p.poll();
        let e = r.unwrap_err();
        assert!(e.contains("spawn failed"), "got: {e}");
    }

    #[test]
    fn net_total_humanizes_absent_as_na_not_zero() {
        // No usable interface counter -> honest "n/a", never a fake "0B/s".
        assert_eq!(human_bps_opt(None), "n/a");
        // A genuine measured zero stays a real zero.
        assert_eq!(human_bps_opt(Some(0.0)), "0B/s");
        assert_eq!(human_bps_opt(Some(12_300.0)), "12.3KB/s");
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    fn line_count(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    /// Pause must stop the writer from advancing recording state (no session
    /// lines) and stop live-snapshot publishes; resume continues both. Runs
    /// the real monitor loop with real collectors over an optional slot.
    #[test]
    fn monitor_pause_freezes_recording_then_resume_continues() {
        let dir = std::env::temp_dir().join(format!("pf-monitor-pause-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("pause.jsonl");
        let stop_file = dir.join("stop.requested");
        let live = Arc::new(Mutex::new(StateSnapshot::default()));
        let pause = Arc::new(AtomicBool::new(false));
        let opts = MonitorOpts {
            interval_ms: 100,
            samples: None,
            out: Some(out.to_string_lossy().to_string()),
            note: "pause-test".to_string(),
            helper: None,
            helper_every: 60,
            // cpu ticks at the base cadence and always writes a line; os is a
            // fallback so the test is not hostage to a single PDH counter.
            collectors: Some("cpu,os".to_string()),
            preset: None,
        };
        let live2 = live.clone();
        let pause2 = pause.clone();
        let handle = std::thread::spawn(move || cmd_monitor_with(opts, Some(live2), Some(pause2)));

        // The wired loop publishes the labelled snapshot and records samples.
        assert!(
            wait_until(
                || live.lock().unwrap().label == "pause-test",
                Duration::from_secs(10)
            ),
            "live snapshot never published"
        );
        assert!(
            wait_until(|| line_count(&out) >= 1, Duration::from_secs(10)),
            "no samples recorded before pause"
        );

        // Pause, let any in-flight tick drain, then prove the writer is frozen.
        pause.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        let frozen_lines = line_count(&out);
        live.lock().unwrap().label = "sentinel".to_string();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            line_count(&out),
            frozen_lines,
            "session advanced while paused"
        );
        assert_eq!(
            live.lock().unwrap().label,
            "sentinel",
            "live snapshot published while paused"
        );

        // Resume: both recording and publishing continue.
        pause.store(false, Ordering::Relaxed);
        assert!(
            wait_until(|| line_count(&out) > frozen_lines, Duration::from_secs(10)),
            "recording did not resume"
        );
        assert!(
            wait_until(
                || live.lock().unwrap().label == "pause-test",
                Duration::from_secs(10)
            ),
            "live snapshot did not resume"
        );

        // Clean stop via the stop-file sentinel; footer must be written.
        std::fs::write(&stop_file, b"stop").unwrap();
        assert_eq!(
            handle.join().unwrap(),
            0,
            "monitor did not finalize cleanly"
        );
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(
            text.contains("session_footer"),
            "missing footer after resume+stop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ctrl-C-equivalent graceful stop: the writer must finish, flush, write
    /// exactly one footer, and return 0. Uses the injected signal so the
    /// process-wide console handler is not touched (parallel-test safe).
    #[test]
    fn signal_stop_finalizes_with_exactly_one_footer() {
        let dir = std::env::temp_dir().join(format!("pf-monitor-signal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("signal.jsonl");
        let opts = MonitorOpts {
            interval_ms: 100,
            samples: None,
            out: Some(out.to_string_lossy().to_string()),
            note: "signal-test".to_string(),
            helper: None,
            helper_every: 60,
            collectors: Some("cpu,os".to_string()),
            preset: None,
        };
        let signal = Arc::new(AtomicBool::new(false));
        let sig2 = signal.clone();
        let handle = std::thread::spawn(move || cmd_monitor_with_signal(opts, None, None, sig2));
        assert!(
            wait_until(|| line_count(&out) >= 2, Duration::from_secs(10)),
            "no samples recorded before the signal"
        );
        signal.store(true, Ordering::Relaxed);
        assert_eq!(handle.join().unwrap(), 0, "graceful stop did not exit 0");
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            text.matches("\"type\":\"session_footer\"").count(),
            1,
            "footer must be written exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Agent stop path: the shared `Service::stop_flag` cell is what a pipe
    /// `stop` flips. Setting it must finalize the session exactly once and
    /// return 0, with the process-wide Ctrl-C handler still installed.
    #[test]
    fn stop_slot_finalizes_with_exactly_one_footer() {
        let dir = std::env::temp_dir().join(format!("pf-monitor-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("stop.jsonl");
        let opts = MonitorOpts {
            interval_ms: 100,
            samples: None,
            out: Some(out.to_string_lossy().to_string()),
            note: "stop-test".to_string(),
            helper: None,
            helper_every: 60,
            collectors: Some("cpu,os".to_string()),
            preset: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || cmd_monitor_with_stop(opts, None, None, stop2));
        assert!(
            wait_until(|| line_count(&out) >= 2, Duration::from_secs(10)),
            "no samples recorded before the stop"
        );
        stop.store(true, Ordering::Relaxed);
        assert_eq!(handle.join().unwrap(), 0, "stop-slot stop did not exit 0");
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            text.matches("\"type\":\"session_footer\"").count(),
            1,
            "footer must be written exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--samples` must bound the session even when battery is not active:
    /// the primary collector (here cpu, since battery is excluded) is counted.
    #[test]
    fn samples_bounds_session_without_battery() {
        let dir = std::env::temp_dir().join(format!("pf-monitor-samples-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("samples.jsonl");
        let opts = MonitorOpts {
            interval_ms: 100,
            samples: Some(3),
            out: Some(out.to_string_lossy().to_string()),
            note: "samples-test".to_string(),
            helper: None,
            helper_every: 60,
            collectors: Some("cpu".to_string()),
            preset: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let code = cmd_monitor_with(opts, None, None);
            let _ = tx.send(code);
        });
        let code = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("--samples did not stop a battery-less session");
        handle.join().unwrap();
        assert_eq!(code, 0);
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            text.matches("\"type\":\"session_footer\"").count(),
            1,
            "a samples-bounded session writes exactly one footer"
        );
        // cpu samples are the unit: at least the three requested ticks.
        let cpu_lines = text
            .lines()
            .filter(|l| l.contains("\"collector\":\"cpu\""))
            .count();
        assert!(cpu_lines >= 3, "expected >=3 cpu samples, got {cpu_lines}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 6A: the old MonitorState retained every sample for the whole
    /// session (discharge/charge histories, battery timeline, cpu/pkg series,
    /// per-collector collection and lateness vectors). 200k samples must now
    /// leave the streaming summary bounded while its reported values stay
    /// correct within one histogram bin.
    #[test]
    fn long_synthetic_run_stays_bounded_and_summary_correct() {
        const N: usize = 200_000;
        const CHECKPOINT: usize = 50_000;
        let base = 1000u64;
        let mut acc = SummaryAccum::new(base);
        let mut small = SummaryAccum::new(base);
        let record = |a: &mut SummaryAccum, i: usize| {
            let t = i as f64;
            a.record_battery(t, t, Some(5.0), None);
            a.record_cpu(40.0);
            a.record_pkg(12.0);
            a.record_time("cpu", 0.5);
            a.record_late("cpu", 10.0);
        };
        for i in 0..N {
            record(&mut acc, i);
            if i < CHECKPOINT {
                record(&mut small, i);
            }
        }

        // Bounded: fixed cap, and independent of how many samples were seen.
        assert!(
            acc.stored_bytes() <= SUMMARY_STATE_CAP_BYTES,
            "summary state {} B exceeds cap {} B",
            acc.stored_bytes(),
            SUMMARY_STATE_CAP_BYTES
        );
        assert_eq!(
            small.stored_bytes(),
            acc.stored_bytes(),
            "retained summary state grew between {CHECKPOINT} and {N} samples"
        );
        assert_eq!(
            acc.recent.len(),
            RECENT_BATTERY_CAP,
            "recent ring is capped"
        );

        // Values remain correct within one bin (0.1 W / 0.1 pct).
        assert_eq!(acc.discharge.count, N as u64);
        assert_eq!(acc.cpu_util.count, N as u64);
        assert_eq!(acc.collect_ms["cpu"].count, N as u64);
        assert_eq!(acc.late["cpu"].count, N as u64);
        for (name, got, want) in [
            (
                "discharge median",
                acc.discharge.percentile(50.0).unwrap(),
                5.0,
            ),
            (
                "discharge p10",
                acc.discharge.percentile(10.0).unwrap(),
                5.0,
            ),
            (
                "discharge p90",
                acc.discharge.percentile(90.0).unwrap(),
                5.0,
            ),
            ("cpu median", acc.cpu_util.percentile(50.0).unwrap(), 40.0),
            ("pkg median", acc.pkg_w.percentile(50.0).unwrap(), 12.0),
            ("cpu collect mean", acc.collect_ms["cpu"].mean(), 0.5),
            ("cpu collect max", acc.collect_ms["cpu"].max, 0.5),
            ("late mean", acc.late["cpu"].mean(), 10.0),
        ] {
            assert!((got - want).abs() < 0.11, "{name}: got {got}, want {want}");
        }
        // Trapezoid over N-1 one-second pairs at 5 W.
        let want_wh = 5.0 * (N as f64 - 1.0) / 3600.0;
        assert!(
            (acc.energy.discharge.energy_wh - want_wh).abs() < 1e-9,
            "energy {} != {want_wh}",
            acc.energy.discharge.energy_wh
        );
        assert_eq!(acc.energy.discharge.coverage(), Some(1.0));
        assert_eq!(acc.charge_n, 0);
    }

    /// Phase 3 follow-up: the footer now integrates energy with both clocks.
    /// A wall jump with continuous monotonic time forces a segment boundary in
    /// the core dual-clock path; the streaming accumulator must match it and
    /// refuse the phantom Wh the old mono-only footer would have reported.
    #[test]
    fn footer_energy_dual_clock_ignores_wall_jump() {
        let (n, jump_at) = (200usize, 100usize);
        let mut acc = SummaryAccum::new(1000);
        let mut times = Vec::new();
        let mut walls = Vec::new();
        let mut watts = Vec::new();
        let mut t = 0.0f64;
        let mut w = 0.0f64;
        for i in 0..n {
            if i == jump_at {
                // Suspend/clock step: wall leaps 1 h, mono advances only 2 s.
                t += 2.0;
                w += 3600.0;
            }
            times.push(t);
            walls.push(w);
            watts.push(Some(5.0));
            acc.record_battery(t, w, Some(5.0), None);
            t += 1.0;
            w += 1.0;
        }
        let mono_only = stats::integrate_segmented(&times, &watts, 5.0);
        let dual = stats::integrate_segmented_clocks(
            &times,
            &walls,
            &watts,
            5.0,
            pf_core::analysis::MAX_CLOCK_SKEW_SECS,
        );
        // The old mono-only footer bridges the jump and invents 5 W * 2 s.
        assert!(
            mono_only.energy_wh - dual.energy_wh > 0.004,
            "expected the mono-only path to invent energy: mono={} dual={}",
            mono_only.energy_wh,
            dual.energy_wh
        );
        // The streaming footer path matches the dual-clock reference exactly.
        assert!(
            (acc.energy.discharge.energy_wh - dual.energy_wh).abs() < 1e-9,
            "streaming {} != dual {}",
            acc.energy.discharge.energy_wh,
            dual.energy_wh
        );
        assert!((acc.energy.discharge.covered_secs - dual.covered_secs).abs() < 1e-9);
        assert_eq!(acc.energy.discharge.discontinuities, dual.discontinuities);
        assert!(
            acc.energy.discharge.discontinuities >= 1,
            "wall jump was not detected"
        );
        assert!(
            (acc.energy.discharge.unobserved_secs - dual.unobserved_secs).abs() < 1e-6,
            "unobserved {} != {}",
            acc.energy.discharge.unobserved_secs,
            dual.unobserved_secs
        );
    }

    #[test]
    fn histogram_percentile_is_approximate_and_clipping_is_documented() {
        let h = |vals: &[f64]| {
            let mut hist =
                FixedHistogram::new(DISCHARGE_HIST.0, DISCHARGE_HIST.1, DISCHARGE_HIST.2);
            for &v in vals {
                hist.push(v);
            }
            hist
        };
        // Constant/concentrated series: within one bin width of the exact value.
        for vals in [
            vec![5.0; 100],
            (0..100).map(|i| 5.0 + i as f64 * 0.003).collect(),
        ] {
            let hist = h(&vals);
            let mut sorted = vals.clone();
            let got = hist.percentile(50.0).unwrap();
            let want = stats::percentile_sorted(&mut sorted, 50.0).unwrap();
            assert!(
                (got - want).abs() <= 0.1 + 1e-6,
                "concentrated series should be within a bin: got {got}, want {want}"
            );
        }
        // A spread/bimodal series has NO one-bin guarantee: the approximation
        // can be far off, which is exactly why no universal bound is claimed.
        let bimodal: Vec<f64> = (0..100)
            .map(|i| if i % 2 == 0 { 0.0 } else { 299.9 })
            .collect();
        let hist = h(&bimodal);
        let mut sorted = bimodal.clone();
        let got = hist.percentile(50.0).unwrap();
        let want = stats::percentile_sorted(&mut sorted, 50.0).unwrap();
        assert!(
            (got - want).abs() > 0.1,
            "expected the documented approximation error on a bimodal series: got {got}, want {want}"
        );
        // Clipping/negative values are retained and counted, reported at the
        // edge, never dropped.
        assert_eq!(h(&[1000.0; 50]).count, 50);
        assert!((299.0..=300.0).contains(&h(&[1000.0; 50]).percentile(50.0).unwrap()));
        assert_eq!(h(&[-5.0; 50]).count, 50);
        assert!(h(&[-5.0; 50]).percentile(50.0).unwrap() <= 0.1);
    }

    #[test]
    fn disk_throughput_distinguishes_zero_from_unobserved() {
        let mib = 1024.0 * 1024.0;
        // Measured zero throughput contributes a real 0, not "unknown".
        let mut z = ThroughputAccum::default();
        for _ in 0..3 {
            z.push(Some(1.0), Some(0.0), 5.0);
        }
        assert_eq!(z.total_mb(), Some(0.0));
        assert_eq!(z.coverage(), Some(1.0));
        assert_eq!(z.samples, 3);

        // All unavailable => no total, all unknown.
        let mut u = ThroughputAccum::default();
        for _ in 0..3 {
            u.push(Some(1.0), None, 5.0);
        }
        assert_eq!(u.total_mb(), None);
        assert_eq!(u.coverage(), Some(0.0));
        assert_eq!(u.samples, 0);

        // Partial availability => qualified partial total.
        let mut p = ThroughputAccum::default();
        p.push(Some(1.0), Some(2.0 * mib), 5.0); // 2 MiB/s for 1 s = 2 MiB
        p.push(Some(1.0), None, 5.0);
        assert_eq!(p.total_mb(), Some(2.0));
        assert_eq!(p.samples, 1);
        assert!((p.coverage().unwrap() - 0.5).abs() < 1e-9);

        // A large gap is not bridged as zero activity.
        let mut g = ThroughputAccum::default();
        g.push(Some(1.0), Some(1.0 * mib), 5.0);
        g.push(Some(600.0), Some(1.0 * mib), 5.0);
        assert_eq!(g.total_mb(), Some(1.0));
        assert_eq!(g.discontinuities, 1);
        assert!((g.unknown_secs - 600.0).abs() < 1e-9);

        // First sample has no interval yet: no claim.
        let mut f = ThroughputAccum::default();
        f.push(None, Some(9.0 * mib), 5.0);
        assert_eq!(f.total_mb(), None);
        assert_eq!(f.coverage(), None);
    }

    // -- D2: effective cadence is the single source of truth ----------------

    fn opts_with(interval_ms: u64, preset: Option<&str>) -> MonitorOpts {
        MonitorOpts {
            interval_ms,
            samples: None,
            out: None,
            note: String::new(),
            helper: None,
            helper_every: 60,
            collectors: None,
            preset: preset.map(|s| s.to_string()),
        }
    }

    #[test]
    fn effective_base_interval_resolves_defaults_and_presets() {
        assert_eq!(effective_base_interval(&opts_with(1000, None)), 1000);
        assert_eq!(effective_base_interval(&opts_with(2000, None)), 2000);
        assert_eq!(
            effective_base_interval(&opts_with(1000, Some("normal"))),
            1000
        );
        assert_eq!(effective_base_interval(&opts_with(1000, Some("low"))), 5000);
        // Preset low overrides the raw interval outright.
        assert_eq!(effective_base_interval(&opts_with(250, Some("low"))), 5000);
        assert_eq!(effective_base_interval(&opts_with(1000, Some("deep"))), 250);
        // Deep keeps an already-faster explicit interval.
        assert_eq!(effective_base_interval(&opts_with(100, Some("deep"))), 100);
    }

    /// The header must record the effective cadence the scheduler actually
    /// uses, not the raw `--interval-ms`, so recovery derives the same gap
    /// threshold. Runs the real monitor for a deterministic one-sample session.
    #[test]
    fn header_records_effective_preset_cadence() {
        for (preset, raw, expected) in [
            ("low", 1000u64, 5000u64),
            ("deep", 1000, 250),
            ("normal", 1500, 1500),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "pf-hdr-{preset}-{}-{}",
                std::process::id(),
                raw
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let out = dir.join("h.jsonl");
            let mut opts = opts_with(raw, Some(preset));
            opts.samples = Some(1);
            opts.collectors = Some("cpu".to_string());
            opts.out = Some(out.to_string_lossy().to_string());
            opts.note = format!("hdr-{preset}");
            let signal = Arc::new(AtomicBool::new(false));
            assert_eq!(
                cmd_monitor_with_signal(opts, None, None, signal),
                0,
                "one-sample {preset} session did not finalize"
            );
            let first = std::fs::read_to_string(&out).unwrap();
            let v = pf_core::json::parse(first.lines().next().unwrap()).unwrap();
            assert_eq!(
                v.get("interval_ms").and_then(|x| x.num()),
                Some(expected as f64),
                "{preset}: header must record the effective cadence"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The strongest guarantee: for the same raw samples, the live streaming
    /// accumulator (clean finalization) and `recover` (dirty termination)
    /// interpret energy, coverage, unknown/unobserved seconds and
    /// discontinuities identically, for every cadence including presets. A gap
    /// is placed in the former disagreement band (5 s < gap <= 15 s) so a
    /// stale header would diverge.
    #[test]
    fn clean_and_recovered_agree_on_effective_cadence() {
        let mono = [0.0, 1.0, 2.0, 12.0, 13.0, 14.0];
        let watt = Some(6.0);
        let w = [watt; 6];
        let wall: Vec<f64> = mono.iter().map(|m| 1_000_000.0 + m).collect();

        let live = |effective: u64| -> (pf_core::stats::SegmentEnergy, [u64; 4]) {
            let gap = pf_core::stats::energy_max_gap_secs(effective);
            let mut s = EnergyStream::default();
            for i in 0..mono.len() {
                s.push(
                    mono[i],
                    wall[i],
                    w[i],
                    None,
                    gap,
                    pf_core::analysis::MAX_CLOCK_SKEW_SECS,
                );
            }
            (s.discharge, s.disc_kinds)
        };
        // Recovers a footer from a session whose header records `header_iv`.
        let recovered = |header_iv: u64| {
            let mut jsonl = format!(
                "{{\"type\":\"session_header\",\"wall_ms\":1000000,\"interval_ms\":{header_iv}}}\n"
            );
            for i in 0..mono.len() {
                jsonl.push_str(&format!(
                    "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                     \"discharge_w\":{{\"v\":6.0,\"p\":\"measured\"}}}}\n",
                    (wall[i] * 1000.0) as u64,
                    (mono[i] * 1000.0) as u64
                ));
            }
            let text = crate::store::recover_session_text(&jsonl, 2_000_000).unwrap();
            let s = pf_core::session::load_session("r", &text).unwrap();
            let f = s.footer.unwrap();
            let n = |k: &str| f.get(k).and_then(|v| v.num());
            let kinds = pf_core::stats::DiscontinuityKind::ALL.map(|k| {
                f.get("discharge_discontinuity_kinds")
                    .and_then(|o| o.get(k.as_str()))
                    .and_then(|v| v.num())
                    .unwrap_or(0.0) as u64
            });
            (
                n("discharge_wh"),
                n("discharge_coverage_pct"),
                n("discharge_unknown_s").unwrap(),
                n("discharge_unobserved_s").unwrap(),
                n("discharge_discontinuities").unwrap() as u64,
                kinds,
            )
        };

        for preset in [None, Some("low"), Some("deep")] {
            let raw = 1000u64;
            let effective = effective_base_interval(&opts_with(raw, preset));
            let (l, lkinds) = live(effective);
            let r = recovered(effective);
            let lwh = (l.covered_secs > 0.0).then_some(l.energy_wh);
            match (lwh, r.0) {
                (Some(a), Some(b)) => {
                    assert!((a - b).abs() < 1e-4, "{preset:?}: energy {a} vs {b}")
                }
                (None, None) => {}
                other => panic!("{preset:?}: energy presence mismatch {other:?}"),
            }
            assert_eq!(l.discontinuities as u64, r.4, "{preset:?}: discontinuities");
            // The recovered footer rounds seconds to whole values, so compare
            // with a sub-second tolerance.
            assert!(
                (l.unknown_secs - r.2).abs() < 0.5,
                "{preset:?}: unknown seconds {} vs {}",
                l.unknown_secs,
                r.2
            );
            assert!(
                (l.unobserved_secs - r.3).abs() < 0.5,
                "{preset:?}: unobserved seconds {} vs {}",
                l.unobserved_secs,
                r.3
            );
            assert_eq!(lkinds, r.5, "{preset:?}: discontinuity kinds must match");
            assert_eq!(
                l.coverage().map(|c| format!("{:.1}", c * 100.0)),
                r.1.map(|c| format!("{c:.1}")),
                "{preset:?}: coverage"
            );
        }

        // Proof the test is meaningful: the former stale header (raw 1000 ms
        // written for a preset-low run whose effective cadence was 5000 ms)
        // makes clean and recovered disagree on the 10 s gap.
        let stale = recovered(1000);
        let (low_live, _) = live(5000);
        assert_eq!(low_live.discontinuities, 0);
        assert_eq!(stale.4, 1, "stale header must misclassify the gap");
        assert_ne!(low_live.unknown_secs, stale.2);
    }
}
