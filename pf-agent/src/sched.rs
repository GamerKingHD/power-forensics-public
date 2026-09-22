//! Collector scheduler: independent workers, timestamped samples,
//! bounded channel, central writer.
//!
//! Each collector runs in its own thread at its own cadence and stamps
//! its own acquisition window (start/end mono + wall). Samples flow over
//! a BOUNDED channel to the single writer thread (the monitor loop).
//! A slow or stalled collector blocks only its own worker: every other
//! cadence continues, and the timestamps expose the laggard.
//!
//! The scheduler is generic over the payload type so it stays testable
//! without hardware: production wires typed samples, tests use counters.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One collected sample with true acquisition timing. `t_start_ms` and
/// `t_end_ms` bound the actual collection work; `mono_ms` is the
/// effective timestamp (midpoint); `wall_ms` is UTC for correlation.
#[derive(Debug)]
pub struct Timed<P> {
    pub collector: &'static str,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub mono_ms: u64,
    pub wall_ms: u64,
    /// Wall time spent inside this collection (observer-effect input).
    pub collect_ms: f64,
    pub payload: P,
}

/// One scheduled source. `interval_ms` is fixed unless `adaptive` is set,
/// in which case the worker re-reads it every iteration (the monitor uses
/// this for anomaly-boosted battery sampling).
pub struct Spec<P> {
    pub name: &'static str,
    pub interval_ms: u64,
    pub adaptive: Option<std::sync::Arc<AtomicU64>>,
    pub collect: Box<dyn FnMut() -> P + Send>,
}

/// Guard that records worker completion even if the body panics (unwind), so
/// `WorkerSet::finish_bounded` can tell a finished worker from a stuck one
/// without blocking on `join`.
struct FinishedGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Handles + completion counter for a spawned worker set. A collector call
/// can block forever inside a Win32/PDH/vendor API that Rust cannot safely
/// kill; joining such a worker would hang shutdown. `finish_bounded` waits a
/// fixed grace for every worker to finish, joins the ones that did, and
/// abandons (detaches) only those still blocked. Because a collector owns
/// exactly one long-lived worker, a permanently blocked call leaks at most
/// one thread per collector — never one per timeout.
pub struct WorkerSet {
    handles: Vec<JoinHandle<()>>,
    finished: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    total: usize,
}

impl WorkerSet {
    /// Wait up to `grace` for all workers to finish. Returns the number still
    /// running (0 on a clean shutdown). A positive return means at least one
    /// collector call is blocked; those threads are detached and reclaimed by
    /// process exit, never by an unbounded join.
    pub fn finish_bounded(self, grace: Duration) -> usize {
        let deadline = Instant::now() + grace;
        loop {
            let done = self.finished.load(Ordering::Relaxed);
            if done >= self.total {
                for h in self.handles {
                    let _ = h.join();
                }
                return 0;
            }
            if Instant::now() >= deadline {
                return self.total - done;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn finished(&self) -> usize {
        self.finished.load(Ordering::Relaxed)
    }
}

/// Align a completion wall stamp to the acquisition midpoint, matching
/// `mono_ms = (t_start + t_end)/2`. Keeps wall and monotonic referencing the
/// same sample moment so a slow collection cannot inject a false
/// wall-vs-mono skew (which the discontinuity scan would read as suspend).
pub fn aligned_wall_ms(end_wall_ms: u64, collect_ms: f64) -> u64 {
    let half = if collect_ms.is_finite() && collect_ms > 0.0 {
        (collect_ms / 2.0) as u64
    } else {
        0
    };
    end_wall_ms.saturating_sub(half)
}

/// Live scheduler telemetry: unread messages in flight (queue backlog).
/// Workers increment before a blocking send, the writer decrements on
/// receive; the high-water mark per session lands in the footer.
#[derive(Debug, Default)]
pub struct SchedStats {
    pub in_flight: AtomicU64,
    pub max_in_flight: AtomicU64,
}

impl SchedStats {
    pub fn before_send(&self) {
        let n = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        let mut max = self.max_in_flight.load(Ordering::Relaxed);
        while n > max {
            match self.max_in_flight.compare_exchange_weak(
                max,
                n,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => max = cur,
            }
        }
    }

    pub fn after_recv(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Canonical per-collector cadence, driven by FieldMeta::interval_ms.
///
/// Each collector module declares its cadence in `capabilities()` via
/// `FieldMeta::interval_ms`; this table is the scheduler's copy of that
/// policy (kept here because the scheduler owns timing, and several
/// collector modules are owned elsewhere so their constants cannot be
/// moved without cross-ownership edits). Sources:
/// - battery/cpu/gpu/storage/net: fast FieldMeta fields are 1000 ms and
///   follow the user `--interval-ms` base directly.
/// - proc: base*2 clamped to [500, 2000] (heavier enumeration).
/// - display: base*2 clamped to [1000, 5000]; brightness/mode/HDR changes
///   are ALSO emitted as timeline events, so polling is a backstop.
/// - usb: 30_000 tick. The collector caches its expensive enumeration and
///   refreshes it every 5th tick (~150 s), which is what usb.rs now
///   advertises for the device/change fields.
/// - os: 30_000 — power-policy/registry values change on human timescales
///   (spec 30–60 s); the monitor keeps 5 s only for the first 2 ticks so
///   the initial snapshot lands fast, then relaxes to this. os_power now
///   advertises the 30 s steady state.
///
/// `scheduler_intervals_agree_with_every_collector_fieldmeta` below asserts
/// this table stays honest against every collector's `capabilities()`.
/// - self: 5_000 (selfmon.rs FieldMeta is 5000; matches
///   `SelfCollector::INTERVAL_MS`).
/// - elevated: no FieldMeta (external helper binary); the caller passes
///   the configured `helper_every` as `base_ms` and it passes through.
pub fn canonical_interval(collector: &str, base_ms: u64) -> u64 {
    match collector {
        "battery" | "cpu" | "gpu" | "storage" | "net" => base_ms,
        "proc" => base_ms.saturating_mul(2).clamp(500, 2000),
        "display" => base_ms.saturating_mul(2).clamp(1000, 5000),
        "usb" => 30_000,
        "os" => 30_000,
        "self" => 5000,
        _ => base_ms, // elevated (config) + unknown: caller-provided cadence
    }
}

/// Scheduler entry point for per-collector intervals. Same as
/// [`canonical_interval`]; kept as a separate name so call sites read as
/// policy ("interval for X") rather than as a table lookup.
pub fn interval_ms_for(collector: &str, base_ms: u64) -> u64 {
    canonical_interval(collector, base_ms)
}
/// True when a collector's next arrival is far enough past its cadence to be
/// treated as a blocked/stalled worker rather than normal jitter. A provider
/// failure still delivers an `Err` payload on cadence, so it never trips this.
pub fn arrival_overdue(now_mono: u64, last_mono: u64, interval_ms: u64) -> bool {
    now_mono.saturating_sub(last_mono) > interval_ms.saturating_mul(3).saturating_add(2000)
}

/// Tracks currently-overdue collectors so each timeout episode is reported
/// exactly once and recovery is observable. Pure bookkeeping: the caller
/// supplies the clock and cadence, and decides what evidence to emit.
#[derive(Default)]
pub struct TimeoutWatch {
    timed_out: std::collections::BTreeSet<&'static str>,
    episodes: u64,
}

impl TimeoutWatch {
    /// Fold one collector's current lateness into the watch. Returns true on
    /// the transition into a new timeout episode (so the caller emits one
    /// event), false otherwise (still overdue, or not overdue).
    pub fn poll(
        &mut self,
        name: &'static str,
        now_mono: u64,
        last_mono: u64,
        interval_ms: u64,
    ) -> bool {
        if arrival_overdue(now_mono, last_mono, interval_ms) && self.timed_out.insert(name) {
            self.episodes += 1;
            return true;
        }
        false
    }

    /// Mark a collector as having produced a sample. Returns true if it was
    /// previously timed out (a recovery worth recording).
    pub fn note_arrival(&mut self, name: &'static str) -> bool {
        self.timed_out.remove(name)
    }

    pub fn episodes(&self) -> u64 {
        self.episodes
    }

    pub fn is_timed_out(&self, name: &str) -> bool {
        self.timed_out.contains(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &&'static str> {
        self.timed_out.iter()
    }
}

/// Spawn one worker thread per spec. Workers share `stop` and `base`
/// (session monotonic clock) and send into a bounded channel of
/// `capacity`. A full channel applies backpressure to the producing
/// worker only — never to its siblings. Returns a bounded [`WorkerSet`],
/// the receiver, and shared queue telemetry.
pub fn spawn<P: Send + 'static>(
    specs: Vec<Spec<P>>,
    stop: std::sync::Arc<AtomicBool>,
    base: Instant,
    capacity: usize,
) -> (WorkerSet, Receiver<Timed<P>>, std::sync::Arc<SchedStats>) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    let stats = std::sync::Arc::new(SchedStats::default());
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = specs.len();
    let mut handles = Vec::with_capacity(total);
    for mut spec in specs {
        let tx = tx.clone();
        let stop = stop.clone();
        let adaptive = spec.adaptive.clone();
        let stats = stats.clone();
        let finished = finished.clone();
        handles.push(std::thread::spawn(move || {
            // Completion is recorded on any exit path, including a panic
            // while unwinding, so a finished worker is never mistaken for a
            // blocked one by `finish_bounded`.
            let _done = FinishedGuard(finished);
            // Stagger workers so ten collectors do not thundering-herd
            // the first tick (deterministic small offsets by name hash).
            let stagger = spec
                .name
                .bytes()
                .fold(0u64, |a, b| a.wrapping_add(b as u64))
                % 97;
            let mut first = true;
            while !stop.load(Ordering::Relaxed) {
                if first {
                    first = false;
                    if stagger > 0 {
                        sleep_checked(&stop, stagger, None);
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
                let interval = adaptive
                    .as_ref()
                    .map(|a| a.load(Ordering::Relaxed).max(50))
                    .unwrap_or(spec.interval_ms);
                let t0 = Instant::now();
                let payload = (spec.collect)();
                let dt = t0.elapsed();
                let end = base.elapsed();
                let start_ms = end.saturating_sub(dt).as_millis() as u64;
                let end_ms = end.as_millis() as u64;
                let msg = Timed {
                    collector: spec.name,
                    t_start_ms: start_ms,
                    t_end_ms: end_ms,
                    mono_ms: start_ms.saturating_add(end_ms) / 2,
                    // Wall is stamped at the SAME sample moment as
                    // monotonic (the acquisition midpoint), not at collector
                    // completion. Otherwise a slow collection would inject a
                    // wall-vs-mono skew and could masquerade as suspend.
                    wall_ms: aligned_wall_ms(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                        dt.as_secs_f64() * 1000.0,
                    ),
                    collect_ms: dt.as_secs_f64() * 1000.0,
                    payload,
                };
                // Bounded send with shutdown observation. A blocking
                // `send` parks the worker with no way to see `stop`, so if
                // the writer exits early (write failure, `--samples`,
                // stop-file) while the channel is full, the parked worker
                // would deadlock the final join. `try_send` never parks:
                // when the channel is full we back off briefly and re-check
                // `stop`, preserving per-worker backpressure. On shutdown a
                // sample already collected but not yet queued is dropped
                // (bounded loss), and the send counter is reconciled.
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                stats.before_send();
                let mut pending = msg;
                let mut queued = false;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match tx.try_send(pending) {
                        Ok(()) => {
                            queued = true;
                            break;
                        }
                        Err(mpsc::TrySendError::Full(m)) => {
                            pending = m;
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => break, // writer gone
                    }
                }
                if !queued {
                    stats.after_recv();
                    break;
                }
                let elapsed = t0.elapsed().as_millis() as u64;
                let wait = interval.saturating_sub(elapsed);
                sleep_checked(&stop, wait, adaptive.as_ref().map(|a| (a, interval)));
            }
        }));
    }
    (
        WorkerSet {
            handles,
            finished,
            total,
        },
        rx,
        stats,
    )
}

/// Sleep in slices so shutdown stays responsive even for slow cadences.
/// When an adaptive knob is present, wake early if it changed: boost
/// decisions must take effect within ~50 ms, not after a full sleep.
fn sleep_checked(stop: &AtomicBool, mut ms: u64, knob: Option<(&std::sync::Arc<AtomicU64>, u64)>) {
    while ms > 0 && !stop.load(Ordering::Relaxed) {
        let step = ms.min(50);
        std::thread::sleep(Duration::from_millis(step));
        ms = ms.saturating_sub(step);
        if let Some((k, was)) = &knob
            && k.load(Ordering::Relaxed).max(50) != *was
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_collectors::collector::Collector;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn staggered_slow_collector_does_not_stall_fast() {
        // Fast worker every 50 ms; slow worker every 50 ms but each read
        // takes 300 ms. Battery-equivalent cadence must continue.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let fast_n = std::sync::Arc::new(AtomicU64::new(0));
        let slow_n = std::sync::Arc::new(AtomicU64::new(0));
        let fast_c = fast_n.clone();
        let slow_c = slow_n.clone();
        let specs: Vec<Spec<u64>> = vec![
            Spec {
                name: "fast",
                interval_ms: 50,
                adaptive: None,
                collect: Box::new(move || {
                    fast_c.fetch_add(1, Ordering::Relaxed);
                    fast_c.load(Ordering::Relaxed)
                }),
            },
            Spec {
                name: "slow",
                interval_ms: 50,
                adaptive: None,
                collect: Box::new(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    slow_c.fetch_add(1, Ordering::Relaxed);
                    slow_c.load(Ordering::Relaxed)
                }),
            },
        ];
        let (workers, rx, _stats) = spawn(specs, stop.clone(), base, 64);
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut fast_times = Vec::new();
        let mut slow_seen = 0u32;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => {
                    if m.collector == "fast" {
                        fast_times.push(m.mono_ms);
                    } else {
                        slow_seen += 1;
                    }
                    // Slow worker's own timestamps must expose its lag:
                    // its collection window is ~300 ms wide.
                    if m.collector == "slow" {
                        assert!(m.t_end_ms >= m.t_start_ms);
                        assert!(m.t_end_ms - m.t_start_ms >= 250);
                    }
                }
                Err(_) => break,
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            workers.finish_bounded(Duration::from_secs(5)),
            0,
            "fast/slow workers must all exit cleanly"
        );
        // Fast cadence continued (~50 ms spacing) despite the slow sibling.
        assert!(fast_times.len() >= 6, "fast samples: {}", fast_times.len());
        for w in fast_times.windows(2) {
            let gap = w[1].saturating_sub(w[0]);
            assert!(gap < 400, "fast gap too large: {gap}");
        }
        // Slow worker produced a couple of (correctly timestamped) samples.
        assert!(slow_seen <= 4, "slow samples: {slow_seen}");
        assert_eq!(fast_n.load(Ordering::Relaxed) as usize, fast_times.len());
    }

    #[test]
    fn scheduler_intervals_agree_with_fieldmeta() {
        // FieldMeta sources (see canonical_interval docs): fast collector
        // fields tick at the user base; usb enumerates at 30 s; self at 5 s.
        assert_eq!(canonical_interval("battery", 1000), 1000);
        assert_eq!(canonical_interval("cpu", 1000), 1000);
        assert_eq!(canonical_interval("gpu", 1000), 1000);
        assert_eq!(canonical_interval("storage", 1000), 1000);
        assert_eq!(canonical_interval("net", 1000), 1000);
        assert_eq!(canonical_interval("proc", 1000), 2000);
        assert_eq!(canonical_interval("proc", 100), 500); // clamp floor
        assert_eq!(canonical_interval("proc", 5000), 2000); // clamp ceiling
        assert_eq!(canonical_interval("display", 1000), 2000);
        assert_eq!(canonical_interval("display", 100), 1000);
        assert_eq!(canonical_interval("display", 5000), 5000);
        assert_eq!(canonical_interval("usb", 1000), 30_000);
        assert_eq!(canonical_interval("os", 1000), 30_000);
        assert_eq!(canonical_interval("self", 1000), 5000);
        assert_eq!(canonical_interval("self", 250), 5000);
        // interval_ms_for is the call-site alias: must agree exactly.
        for name in [
            "battery", "cpu", "gpu", "storage", "net", "proc", "display", "usb", "os", "self",
        ] {
            assert_eq!(interval_ms_for(name, 1000), canonical_interval(name, 1000));
        }
        // Self collector's exported cadence constant agrees with both its
        // FieldMeta table and the scheduler.
        assert_eq!(pf_collectors::selfmon::SelfCollector::INTERVAL_MS, 5000);
        for f in pf_collectors::selfmon::SelfCollector::new().capabilities() {
            if f.name == "self.ctx_switches_s" {
                continue; // informational estimate piggybacked on the self tick
            }
            assert_eq!(
                f.interval_ms,
                pf_collectors::selfmon::SelfCollector::INTERVAL_MS,
                "{}",
                f.name
            );
        }
        assert_eq!(
            canonical_interval("self", 1000),
            pf_collectors::selfmon::SelfCollector::INTERVAL_MS
        );
    }

    #[test]
    fn scheduler_intervals_agree_with_every_collector_fieldmeta() {
        // The scheduler must not poll a collector slower than that collector's
        // fastest advertised field: that would over-promise freshness in the
        // session metadata. For collectors whose fields share the tick cadence,
        // the fastest field must pin the scheduler interval exactly. USB is the
        // documented exception: its tick is 30 s while its cached enumeration
        // refreshes every 5th tick (~150 s), which is what it advertises.
        let cases: Vec<(&str, Vec<pf_core::telemetry::FieldMeta>)> = vec![
            (
                "battery",
                pf_collectors::battery::BatteryCollector::new().capabilities(),
            ),
            (
                "cpu",
                pf_collectors::cpu::CpuCollector::new().capabilities(),
            ),
            (
                "gpu",
                pf_collectors::gpu::GpuCollector::new().capabilities(),
            ),
            (
                "storage",
                pf_collectors::storage::StorageCollector::new().capabilities(),
            ),
            (
                "net",
                pf_collectors::network::NetCollector::new().capabilities(),
            ),
            (
                "proc",
                pf_collectors::proc::ProcCollector::new().capabilities(),
            ),
            (
                "display",
                pf_collectors::display::DisplayCollector::new().capabilities(),
            ),
            (
                "usb",
                pf_collectors::usb::UsbCollector::new().capabilities(),
            ),
            (
                "os",
                pf_collectors::system::OsPowerCollector::new().capabilities(),
            ),
            (
                "self",
                pf_collectors::selfmon::SelfCollector::new().capabilities(),
            ),
        ];
        for (name, fields) in &cases {
            let tick = canonical_interval(name, 1000);
            let mut active: Vec<u64> = fields
                .iter()
                .map(|f| f.interval_ms)
                .filter(|m| *m > 0)
                .collect();
            active.sort_unstable();
            assert!(
                !active.is_empty(),
                "{name}: no sampled FieldMeta advertised"
            );
            assert!(
                tick <= active[0],
                "{name}: scheduler {tick} ms polls slower than the fastest advertised field {} ms",
                active[0]
            );
            if *name != "usb" {
                assert_eq!(
                    active[0], tick,
                    "{name}: fastest advertised field must match the scheduler tick"
                );
            }
        }
        // USB's decoupled enumeration is explicit: 30 s tick, ~150 s refresh.
        let usb: Vec<u64> = pf_collectors::usb::UsbCollector::new()
            .capabilities()
            .into_iter()
            .filter(|f| {
                f.name == "usb.devices" || f.name == "usb.changes" || f.name == "usb.power_devices"
            })
            .map(|f| f.interval_ms)
            .collect();
        assert_eq!(usb, vec![150_000, 150_000, 150_000]);
    }

    #[test]
    fn adaptive_interval_is_honored() {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let knob = std::sync::Arc::new(AtomicU64::new(500));
        let knob2 = knob.clone();
        let specs: Vec<Spec<u64>> = vec![Spec {
            name: "adapt",
            interval_ms: 500,
            adaptive: Some(knob2),
            collect: Box::new(|| 1u64),
        }];
        let (workers, rx, _stats) = spawn(specs, stop.clone(), base, 16);
        // One slow sample, then tighten the knob and expect faster ones.
        rx.recv_timeout(Duration::from_millis(800)).unwrap();
        knob.store(60, Ordering::Relaxed);
        let t0 = Instant::now();
        let mut extra = 0;
        while t0.elapsed() < Duration::from_millis(400) {
            if rx.recv_timeout(Duration::from_millis(200)).is_ok() {
                extra += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            workers.finish_bounded(Duration::from_secs(5)),
            0,
            "adaptive worker must exit cleanly"
        );
        assert!(extra >= 3, "adaptive samples: {extra}");
    }

    #[test]
    fn stopped_writer_does_not_deadlock_producer() {
        // Regression: a worker parked in a blocking `send` with a full channel
        // never observed `stop`, so a writer that exited early (write failure,
        // `--samples`, stop-file) and then joined its workers hung forever.
        // Capacity 1 + a fast producer fills the queue; we then set stop while
        // keeping `rx` alive and undrained — the exact state of a writer loop
        // that has broken out but not yet dropped its receiver — and require
        // the worker to terminate promptly.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let specs: Vec<Spec<u64>> = vec![Spec {
            name: "full",
            interval_ms: 0,
            adaptive: None,
            collect: Box::new(|| 1u64),
        }];
        let (workers, rx, stats) = spawn(specs, stop.clone(), base, 1);
        // A parked producer on a full channel shows up as in_flight >= 2:
        // one queued message plus the one counted but not yet delivered.
        let deadline = Instant::now() + Duration::from_secs(5);
        while stats.in_flight.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            stats.in_flight.load(Ordering::Relaxed) >= 2,
            "producer never filled the bounded channel"
        );
        // Writer early exit: request stop but keep `rx` alive/undrained.
        stop.store(true, Ordering::Relaxed);
        // A watchdog that joins in a helper thread, so a regression fails the
        // assertion after a bound instead of hanging the test run forever.
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(workers.finish_bounded(Duration::from_secs(3)));
        });
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(4)).unwrap(),
            0,
            "worker deadlocked after the writer stopped draining"
        );
        drop(rx);
    }

    /// A permanently blocked collector call must not block shutdown: the
    /// healthy sibling keeps producing, `finish_bounded` returns within the
    /// grace, and it reports the blocked worker instead of hanging on join.
    #[test]
    fn blocked_collector_does_not_block_shutdown_or_siblings() {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let (block_tx, block_rx) = mpsc::channel::<()>();
        let healthy = std::sync::Arc::new(AtomicU64::new(0));
        let healthy_c = healthy.clone();
        let specs: Vec<Spec<u64>> = vec![
            Spec {
                name: "blocked",
                interval_ms: 50,
                adaptive: None,
                collect: Box::new(move || {
                    // Block until the test drops the sender, then return;
                    // but shutdown must not wait for that.
                    let _ = block_rx.recv();
                    0u64
                }),
            },
            Spec {
                name: "healthy",
                interval_ms: 50,
                adaptive: None,
                collect: Box::new(move || {
                    healthy_c.fetch_add(1, Ordering::Relaxed);
                    1u64
                }),
            },
        ];
        let (workers, rx, _stats) = spawn(specs, stop.clone(), base, 64);
        assert_eq!(workers.total(), 2);
        // Healthy sibling keeps producing while `blocked` is stuck inside its call.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut healthy_samples = 0u32;
        while healthy_samples < 3 && Instant::now() < deadline {
            if let Ok(m) = rx.recv_timeout(Duration::from_millis(200))
                && m.collector == "healthy"
            {
                healthy_samples += 1;
            }
        }
        assert!(healthy_samples >= 3, "healthy sibling stalled");
        // Shutdown is bounded even though `blocked` never returns.
        stop.store(true, Ordering::Relaxed);
        let t0 = Instant::now();
        let stuck = workers.finish_bounded(Duration::from_millis(500));
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "shutdown was not bounded"
        );
        assert_eq!(stuck, 1, "the blocked worker must be reported, not joined");
        drop(block_tx); // release the blocked thread so the test leaks nothing
        drop(rx);
    }

    #[test]
    fn aligned_wall_matches_mono_midpoint() {
        assert_eq!(aligned_wall_ms(4200, 4200.0), 2100);
        assert_eq!(aligned_wall_ms(1000, 0.0), 1000);
        assert_eq!(aligned_wall_ms(100, 50.0), 75);
    }

    /// A slow collection followed by a fast one must not look like a
    /// wall-vs-mono discontinuity once wall is stamped at the midpoint.
    #[test]
    fn slow_collection_does_not_masquerade_as_skew() {
        use pf_core::analysis::MAX_CLOCK_SKEW_SECS;
        // mono = midpoints; aligned wall = midpoints; completion wall = ends.
        let mono = [2.1, 4.25];
        let aligned = [2.1, 4.25];
        let completion = [4.2, 4.3];
        assert!(
            !pf_core::stats::discontinuities(&mono, &completion, 5.0, MAX_CLOCK_SKEW_SECS)
                .is_empty(),
            "fixture should reproduce the completion-stamp false skew"
        );
        assert!(
            pf_core::stats::discontinuities(&mono, &aligned, 5.0, MAX_CLOCK_SKEW_SECS).is_empty(),
            "aligned wall stamp must not register a discontinuity"
        );
    }

    #[test]
    fn overdue_threshold_separates_jitter_from_a_stall() {
        // 1 s cadence: 3 s + 2 s grace (5000 ms).
        assert!(!arrival_overdue(5_001, 1_000, 1_000));
        assert!(!arrival_overdue(6_000, 1_000, 1_000));
        assert!(arrival_overdue(6_001, 1_000, 1_000));
        // A failed read that arrives on cadence is never overdue, even though
        // its payload is an error.
        assert!(!arrival_overdue(2_000, 1_000, 1_000));
    }

    #[test]
    fn timeout_watch_reports_one_episode_then_recovers_and_can_repeat() {
        let mut w = TimeoutWatch::default();
        // Still within cadence: no episode.
        assert!(!w.poll("battery", 2_000, 1_000, 1_000));
        assert_eq!(w.episodes(), 0);
        // Stalled: exactly one episode across repeated polls.
        assert!(w.poll("battery", 6_001, 1_000, 1_000));
        assert!(!w.poll("battery", 7_000, 1_000, 1_000));
        assert!(!w.poll("battery", 60_000, 1_000, 1_000));
        assert_eq!(w.episodes(), 1);
        assert!(w.is_timed_out("battery"));
        // Recovery clears it and is observable once.
        assert!(w.note_arrival("battery"));
        assert!(!w.note_arrival("battery"));
        assert!(!w.is_timed_out("battery"));
        // A later stall is a new episode (counted, not duplicated).
        assert!(w.poll("battery", 205_001, 199_000, 1_000));
        assert_eq!(w.episodes(), 2);
    }

    /// `--samples` must complete even when the chosen primary collector is
    /// permanently blocked: the first producing collector takes over the
    /// bound, and shutdown stays bounded.
    #[test]
    fn samples_bound_completes_with_blocked_primary_and_healthy_secondary() {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let base = Instant::now();
        let (_block_tx, block_rx) = mpsc::channel::<()>();
        let specs: Vec<Spec<u64>> = vec![
            Spec {
                name: "battery",
                interval_ms: 50,
                adaptive: None,
                collect: Box::new(move || {
                    let _ = block_rx.recv(); // never returns until released
                    0u64
                }),
            },
            Spec {
                name: "cpu",
                interval_ms: 50,
                adaptive: None,
                collect: Box::new(|| 1u64),
            },
        ];
        let (workers, rx, _stats) = spawn(specs, stop.clone(), base, 64);
        // Writer-side policy: primary = battery; after a short grace, the
        // first producing collector (cpu) becomes the bound, then 3 samples
        // complete the session.
        let mut primary: &'static str = "battery";
        let mut tick = 0u32;
        let start = Instant::now();
        let mut reassigned = false;
        while tick < 3 && Instant::now() < start + Duration::from_secs(10) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => {
                    if !reassigned
                        && tick == 0
                        && m.collector != primary
                        && start.elapsed() > Duration::from_millis(300)
                    {
                        primary = m.collector;
                        reassigned = true;
                    }
                    if m.collector == primary {
                        tick += 1;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(reassigned, "healthy secondary must take over the bound");
        assert_eq!(tick, 3, "--samples bound must complete despite the block");
        // Shutdown is bounded despite the blocked primary.
        stop.store(true, Ordering::Relaxed);
        drop(rx);
        assert_eq!(
            workers.finish_bounded(Duration::from_millis(500)),
            1,
            "only the blocked primary should remain"
        );
    }
}
